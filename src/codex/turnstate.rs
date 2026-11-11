//! Opaque `X-Codex-Turn-State` handling.
//!
//! Shape is a heuristic, never a signature check and never a quality metric.
//! The state is issued by the upstream; this module only parses, ages and
//! stores it. It does not attempt to synthesize a token.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Result};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const HEADER: &str = "X-Codex-Turn-State";
pub const TTL_SECONDS: u64 = 3600;
pub const REFRESH_SECONDS: u64 = 1200;
const MAX_TOKEN_LEN: usize = 2048;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Token {
    pub value: String,
    pub fingerprint: String,
    pub issued: u64,
    pub blocks: usize,
}

#[derive(Debug, Clone)]
pub struct Policy {
    pub accepted_blocks: Vec<usize>,
    pub ttl: Duration,
    pub refresh: Duration,
}

impl Policy {
    pub fn new(accepted_blocks: Vec<usize>, ttl_seconds: u64, refresh_seconds: u64) -> Self {
        Self {
            accepted_blocks,
            ttl: Duration::from_secs(ttl_seconds),
            refresh: Duration::from_secs(refresh_seconds),
        }
    }

    pub fn accept(&self, token: &Token, now: SystemTime) -> bool {
        if token.value.is_empty() {
            return false;
        }
        if !self.accepted_blocks.contains(&token.blocks) {
            return false;
        }
        let issued = UNIX_EPOCH + Duration::from_secs(token.issued);
        if issued > now + Duration::from_secs(30) {
            return false;
        }
        let expiry = issued + self.ttl;
        now + Duration::from_secs(30) < expiry
    }

    pub fn remaining_seconds(&self, token: &Token, now: SystemTime) -> u64 {
        let issued = UNIX_EPOCH + Duration::from_secs(token.issued);
        issued
            .checked_add(self.ttl)
            .and_then(|expiry| expiry.duration_since(now).ok())
            .map(|left| left.as_secs())
            .unwrap_or(0)
    }
}

pub fn parse(value: &str) -> Result<Token> {
    let value = value.trim();
    if value.len() > MAX_TOKEN_LEN || value.contains(['\r', '\n', '\t', ' ']) {
        bail!("invalid state encoding");
    }
    let core = value.trim_end_matches('=');
    if value.len() - core.len() > 2 {
        bail!("invalid state padding");
    }
    let raw = URL_SAFE_NO_PAD
        .decode(core)
        .map_err(|_| anyhow::anyhow!("invalid base64url state"))?;
    if raw.len() < 73 || raw[0] != 0x80 || (raw.len() - 57) % 16 != 0 {
        bail!("unrecognized state envelope");
    }
    let issued = u64::from_be_bytes(raw[1..9].try_into().expect("fixed slice"));
    if !(1_577_836_800..4_102_444_800).contains(&issued) {
        bail!("state timestamp out of range");
    }
    let digest = Sha256::digest(value.as_bytes());
    let fingerprint = digest[..8].iter().map(|b| format!("{b:02x}")).collect();
    Ok(Token {
        value: value.to_string(),
        fingerprint,
        issued,
        blocks: (raw.len() - 57) / 16,
    })
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Snapshot {
    pub token: Option<Token>,
    pub route: usize,
    pub version: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoreStatus {
    pub usable: bool,
    pub version: u64,
    pub remaining_seconds: u64,
    pub ready: bool,
    pub strikes: u32,
    pub observations: u64,
}

/// One publisher. Response observations cannot overwrite the active token.
#[derive(Debug)]
pub struct Store {
    policy: Policy,
    hold_active: bool,
    active: Snapshot,
    ready: Snapshot,
    version: u64,
    strikes: u32,
    observations: u64,
}

impl Store {
    pub fn new(policy: Policy) -> Self {
        Self {
            policy,
            hold_active: false,
            active: Snapshot::default(),
            ready: Snapshot::default(),
            version: 0,
            strikes: 0,
            observations: 0,
        }
    }

    pub fn policy(&self) -> &Policy {
        &self.policy
    }

    pub fn acquire(&mut self, now: SystemTime) -> (Snapshot, bool) {
        self.promote(now);
        let usable = self
            .active
            .token
            .as_ref()
            .is_some_and(|token| self.policy.accept(token, now));
        (self.active.clone(), usable)
    }

    fn promote(&mut self, now: SystemTime) {
        let Some(ready) = self.ready.token.clone() else {
            return;
        };
        let active_token = self.active.token.clone();
        let active_usable = active_token
            .as_ref()
            .is_some_and(|token| self.policy.accept(token, now));
        let different = active_token
            .as_ref()
            .map(|token| token.fingerprint != ready.fingerprint)
            .unwrap_or(true);
        let ready_usable = self.policy.accept(&ready, now);
        let near_expiry = active_token.as_ref().is_some_and(|token| {
            let issued = UNIX_EPOCH + Duration::from_secs(token.issued);
            !self.hold_active && now + self.policy.refresh > issued + self.policy.ttl
        });
        let newer = active_token
            .as_ref()
            .map(|token| ready.issued > token.issued)
            .unwrap_or(true);
        if ready_usable
            && different
            && (!active_usable || self.strikes >= 2 || (near_expiry && newer))
        {
            self.version += 1;
            self.ready.version = self.version;
            self.active = self.ready.clone();
            self.ready = Snapshot::default();
            self.strikes = 0;
        }
    }

    pub fn offer(&mut self, token: Token, route: usize, now: SystemTime) -> bool {
        self.observations += 1;
        if !self.policy.accept(&token, now) {
            return false;
        }
        if self
            .active
            .token
            .as_ref()
            .is_some_and(|active| active.fingerprint == token.fingerprint)
        {
            return true;
        }
        let active_usable = self
            .active
            .token
            .as_ref()
            .is_some_and(|active| self.policy.accept(active, now));
        if !active_usable {
            self.version += 1;
            self.active = Snapshot {
                token: Some(token),
                route,
                version: self.version,
            };
            self.strikes = 0;
            return true;
        }
        if self.ready.token.is_none()
            || self
                .ready
                .token
                .as_ref()
                .is_some_and(|ready| token.issued >= ready.issued)
        {
            self.ready = Snapshot {
                token: Some(token),
                route,
                version: 0,
            };
        }
        self.promote(now);
        true
    }

    pub fn observe(&mut self, value: &str, used: &Snapshot, now: SystemTime) -> bool {
        if value.is_empty() {
            return false;
        }
        self.observations += 1;
        let suspect = parse(value)
            .map(|token| !self.policy.accept(&token, now))
            .unwrap_or(true);
        let same = used.version == self.active.version
            && used.token.as_ref().map(|t| &t.fingerprint)
                == self.active.token.as_ref().map(|t| &t.fingerprint);
        if same {
            if suspect {
                self.strikes += 1;
            } else {
                self.strikes = 0;
            }
        }
        suspect
    }

    pub fn needs_refresh(&mut self, now: SystemTime) -> bool {
        self.promote(now);
        let active_ok = self
            .active
            .token
            .as_ref()
            .is_some_and(|token| self.policy.accept(token, now));
        let near_expiry = self.active.token.as_ref().is_some_and(|token| {
            let issued = UNIX_EPOCH + Duration::from_secs(token.issued);
            now + self.policy.refresh > issued + self.policy.ttl
        });
        !active_ok || self.strikes >= 2 || near_expiry
    }

    pub fn status(&self, now: SystemTime) -> StoreStatus {
        let usable = self
            .active
            .token
            .as_ref()
            .is_some_and(|token| self.policy.accept(token, now));
        let remaining = self
            .active
            .token
            .as_ref()
            .map(|token| self.policy.remaining_seconds(token, now))
            .unwrap_or(0);
        let ready = self
            .ready
            .token
            .as_ref()
            .is_some_and(|token| self.policy.accept(token, now));
        StoreStatus {
            usable,
            version: self.active.version,
            remaining_seconds: remaining,
            ready,
            strikes: self.strikes,
            observations: self.observations,
        }
    }

    pub fn reject_and_promote(&mut self, used: &Snapshot, now: SystemTime) -> bool {
        let same = used.version == self.active.version
            && used.token.as_ref().map(|t| &t.fingerprint)
                == self.active.token.as_ref().map(|t| &t.fingerprint);
        if !same {
            return false;
        }
        self.active = Snapshot::default();
        self.strikes = 0;
        self.promote(now);
        self.active
            .token
            .as_ref()
            .is_some_and(|token| self.policy.accept(token, now))
    }

    pub fn hold_active(&mut self, hold: bool) {
        self.hold_active = hold;
    }
}

pub fn synthetic(blocks: usize, issued: u64) -> String {
    let mut raw = vec![0u8; 57 + 16 * blocks];
    raw[0] = 0x80;
    raw[1..9].copy_from_slice(&issued.to_be_bytes());
    for (index, byte) in raw[9..].iter_mut().enumerate() {
        *byte = (index % 251) as u8;
    }
    URL_SAFE_NO_PAD.encode(raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(1_800_000_000)
    }

    #[test]
    fn parses_10_12_and_33_blocks() {
        for blocks in [10usize, 12, 33] {
            let value = synthetic(blocks, 1_800_000_000);
            let token = parse(&value).unwrap();
            assert_eq!(token.blocks, blocks);
            assert_eq!(token.issued, 1_800_000_000);
        }
    }

    #[test]
    fn accepts_expanded_only_for_matching_policy() {
        let token = parse(&synthetic(33, 1_800_000_000)).unwrap();
        assert!(Policy::new(vec![10, 33], 3600, 1200).accept(&token, now()));
        assert!(Policy::new(vec![12, 33], 3600, 1200).accept(&token, now()));
        assert!(!Policy::new(vec![10], 3600, 1200).accept(&token, now()));
    }

    #[test]
    fn rejects_future_expired_and_malformed() {
        let future = parse(&synthetic(10, 1_800_000_040)).unwrap();
        assert!(!Policy::new(vec![10], 3600, 1200).accept(&future, now()));
        let old = parse(&synthetic(10, 1_799_000_000)).unwrap();
        assert!(!Policy::new(vec![10], 3600, 1200).accept(&old, now()));
        assert!(parse("not base64").is_err());
        assert!(parse(&"A".repeat(2100)).is_err());
    }

    #[test]
    fn store_promotes_ready_and_keeps_observation_separate() {
        let policy = Policy::new(vec![10, 33], 3600, 1200);
        let mut store = Store::new(policy);
        let first = parse(&synthetic(10, 1_799_999_900)).unwrap();
        assert!(store.offer(first.clone(), 0, now()));
        let (snapshot, usable) = store.acquire(now());
        assert!(usable);
        assert_eq!(snapshot.token.as_ref().unwrap().blocks, 10);

        let ready = parse(&synthetic(33, 1_800_000_000)).unwrap();
        assert_eq!(ready.blocks, 33);
        assert!(store.policy().accept(&ready, now()));
        assert!(store.offer(ready, 1, now()));
        let (snapshot, usable) = store.acquire(now());
        assert!(usable);
        assert_eq!(
            snapshot.token.as_ref().unwrap().blocks,
            10,
            "active stays until promotion"
        );

        // A response that disagrees with the active shape only records a strike.
        assert!(store.observe(&synthetic(11, 1_800_000_200), &snapshot, now()));
        assert_eq!(store.status(now()).strikes, 1);
    }

    #[test]
    fn expired_active_is_replaced_immediately() {
        let policy = Policy::new(vec![10, 33], 3600, 1200);
        let mut store = Store::new(policy);
        let old = parse(&synthetic(10, 1_799_000_000)).unwrap();
        store.offer(old, 0, now());
        let fresh = parse(&synthetic(33, 1_800_000_000)).unwrap();
        assert!(store.offer(fresh, 1, now()));
        let (snapshot, usable) = store.acquire(now());
        assert!(usable);
        assert_eq!(snapshot.token.unwrap().blocks, 33);
    }
}
