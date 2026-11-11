//! Public-IP detection over HTTP and STUN.
//!
//! The guard is fail-closed: a missing result is never treated as an allowed
//! exit. HTTP and STUN are independent so a proxy that only carries TCP cannot
//! silently satisfy a `both` policy.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use rand::RngCore;
use serde::{Deserialize, Serialize};

use crate::config::{ip_allowed, Config, IpCheckMode};

const HTTP_ENDPOINTS: [&str; 4] = [
    "https://api.ipify.org",
    "https://ifconfig.me/ip",
    "https://icanhazip.com",
    "https://ipinfo.io/ip",
];

const STUN_SERVERS: [&str; 3] = [
    "stun.cloudflare.com:3478",
    "stun.l.google.com:19302",
    "stun1.l.google.com:19302",
];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IpReport {
    pub mode: IpCheckMode,
    pub http_ip: Option<IpAddr>,
    pub stun_ip: Option<IpAddr>,
    pub checked_at: String,
    /// Detection itself succeeded (both required transports answered).
    pub detected: bool,
    /// Every detected exit is on the whitelist and at least one exit exists.
    pub allowed: bool,
    pub detail: String,
}

impl IpReport {
    pub fn effective_ip(&self) -> Option<IpAddr> {
        self.http_ip.or(self.stun_ip)
    }
}

pub async fn check(config: &Config) -> IpReport {
    let mode = config.protection.ip_check_mode.clone();
    let proxy = config
        .protection
        .http_proxy
        .as_deref()
        .filter(|p| !p.trim().is_empty());

    let http_task = async {
        if matches!(mode, IpCheckMode::Stun) {
            None
        } else {
            Some(check_http(proxy).await)
        }
    };
    let stun_task = async {
        if matches!(mode, IpCheckMode::Http) {
            None
        } else {
            Some(check_stun().await)
        }
    };
    let (http_result, stun_result) = tokio::join!(http_task, stun_task);

    let http_ip = match http_result {
        None => None,
        Some(Ok(ip)) => Some(ip),
        Some(Err(err)) => {
            crate::util::warn(format!("HTTP exit check failed: {err:#}"));
            None
        }
    };
    let stun_ip = match stun_result {
        None => None,
        Some(Ok(ip)) => Some(ip),
        Some(Err(err)) => {
            crate::util::warn(format!("STUN exit check failed: {err:#}"));
            None
        }
    };

    let detected = match mode {
        IpCheckMode::Http => http_ip.is_some(),
        IpCheckMode::Stun => stun_ip.is_some(),
        IpCheckMode::Both => http_ip.is_some() && stun_ip.is_some(),
    };
    let allowed = detected
        && [http_ip, stun_ip]
            .into_iter()
            .flatten()
            .all(|ip| ip_allowed(ip, &config.protection.whitelist));

    let detail = if !detected {
        "无法确认出口 IP；按失败处理".to_string()
    } else if config.protection.whitelist.is_empty() {
        "IP 白名单为空；按失败处理".to_string()
    } else if allowed {
        "出口在白名单内".to_string()
    } else {
        "出口不在白名单内".to_string()
    };

    IpReport {
        mode,
        http_ip,
        stun_ip,
        checked_at: crate::util::now_iso(),
        detected,
        allowed,
        detail,
    }
}

async fn check_http(proxy: Option<&str>) -> Result<IpAddr> {
    let mut builder = reqwest::Client::builder()
        .timeout(Duration::from_secs(8))
        .user_agent(concat!("AIGuard/", env!("CARGO_PKG_VERSION")));
    if let Some(proxy) = proxy {
        builder = builder.proxy(reqwest::Proxy::all(proxy).context("invalid HTTP proxy")?);
    }
    let client = builder.build()?;
    let mut errors = Vec::new();
    for endpoint in HTTP_ENDPOINTS {
        match client.get(endpoint).send().await {
            Ok(response) if response.status().is_success() => match response.text().await {
                Ok(text) => {
                    let text = text.trim();
                    if let Ok(ip) = text.parse::<IpAddr>() {
                        return Ok(ip);
                    }
                    errors.push(format!("{endpoint}: unexpected body"));
                }
                Err(err) => errors.push(format!("{endpoint}: {err}")),
            },
            Ok(response) => errors.push(format!("{endpoint}: HTTP {}", response.status())),
            Err(err) => errors.push(format!("{endpoint}: {err}")),
        }
    }
    bail!("all HTTP exit services failed: {}", errors.join("; "))
}

async fn check_stun() -> Result<IpAddr> {
    let mut errors = Vec::new();
    for server in STUN_SERVERS {
        match stun_query(server).await {
            Ok(ip) => return Ok(ip),
            Err(err) => errors.push(format!("{server}: {err}")),
        }
    }
    bail!("all STUN servers failed: {}", errors.join("; "))
}

async fn stun_query(server: &str) -> Result<IpAddr> {
    let address = tokio::net::lookup_host(server)
        .await
        .with_context(|| format!("resolve {server}"))?
        .next()
        .ok_or_else(|| anyhow!("no address for {server}"))?;

    let bind: SocketAddr = if address.is_ipv4() {
        SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0))
    } else {
        SocketAddr::from((Ipv6Addr::UNSPECIFIED, 0))
    };
    let socket = tokio::net::UdpSocket::bind(bind)
        .await
        .context("bind UDP socket")?;
    socket.connect(address).await?;

    let mut transaction = [0u8; 12];
    rand::thread_rng().fill_bytes(&mut transaction);
    let mut request = Vec::with_capacity(20);
    request.extend_from_slice(&0x0001u16.to_be_bytes());
    request.extend_from_slice(&0u16.to_be_bytes());
    request.extend_from_slice(&0x2112A442u32.to_be_bytes());
    request.extend_from_slice(&transaction);

    socket.send(&request).await.context("send STUN request")?;
    let mut buf = [0u8; 1500];
    let len = tokio::time::timeout(Duration::from_secs(4), socket.recv(&mut buf))
        .await
        .context("STUN timeout")?
        .context("receive STUN response")?;
    parse_stun_response(&buf[..len], &transaction)
}

fn parse_stun_response(data: &[u8], transaction: &[u8; 12]) -> Result<IpAddr> {
    if data.len() < 20 {
        bail!("short STUN response");
    }
    if u16::from_be_bytes([data[0], data[1]]) != 0x0101 {
        bail!("not a STUN success response");
    }
    if u32::from_be_bytes([data[4], data[5], data[6], data[7]]) != 0x2112A442 {
        bail!("bad STUN magic cookie");
    }
    if &data[8..20] != transaction {
        bail!("STUN transaction mismatch");
    }
    let declared = u16::from_be_bytes([data[2], data[3]]) as usize;
    let end = (20 + declared).min(data.len());
    let mut offset = 20;
    while offset + 4 <= end {
        let kind = u16::from_be_bytes([data[offset], data[offset + 1]]);
        let length = u16::from_be_bytes([data[offset + 2], data[offset + 3]]) as usize;
        let value_start = offset + 4;
        let value_end = value_start.saturating_add(length);
        if value_end > end {
            break;
        }
        let value = &data[value_start..value_end];
        match kind {
            0x0020 => {
                if let Some(ip) = parse_xor_mapped(value) {
                    return Ok(ip);
                }
            }
            0x0001 => {
                if let Some(ip) = parse_mapped(value) {
                    return Ok(ip);
                }
            }
            _ => {}
        }
        offset = value_end + ((4 - (length % 4)) % 4);
    }
    bail!("no mapped address in STUN response")
}

fn parse_mapped(value: &[u8]) -> Option<IpAddr> {
    if value.len() < 8 {
        return None;
    }
    match value[1] {
        0x01 if value.len() >= 8 => Some(IpAddr::V4(Ipv4Addr::new(
            value[4], value[5], value[6], value[7],
        ))),
        0x02 if value.len() >= 20 => {
            let mut octets = [0u8; 16];
            octets.copy_from_slice(&value[4..20]);
            Some(IpAddr::V6(Ipv6Addr::from(octets)))
        }
        _ => None,
    }
}

fn parse_xor_mapped(value: &[u8]) -> Option<IpAddr> {
    if value.len() < 8 {
        return None;
    }
    match value[1] {
        0x01 if value.len() >= 8 => {
            let cookie = 0x2112A442u32.to_be_bytes();
            Some(IpAddr::V4(Ipv4Addr::new(
                value[4] ^ cookie[0],
                value[5] ^ cookie[1],
                value[6] ^ cookie[2],
                value[7] ^ cookie[3],
            )))
        }
        0x02 if value.len() >= 20 => {
            let mut mask = [0u8; 16];
            mask[..4].copy_from_slice(&0x2112A442u32.to_be_bytes());
            let mut octets = [0u8; 16];
            for index in 0..16 {
                octets[index] = value[4 + index] ^ mask[index];
            }
            Some(IpAddr::V6(Ipv6Addr::from(octets)))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_xor_mapped_ipv4() {
        let mut tx = [0u8; 12];
        tx.copy_from_slice(&[7u8; 12]);
        let mut packet = Vec::new();
        packet.extend_from_slice(&0x0101u16.to_be_bytes());
        packet.extend_from_slice(&12u16.to_be_bytes());
        packet.extend_from_slice(&0x2112A442u32.to_be_bytes());
        packet.extend_from_slice(&tx);
        packet.extend_from_slice(&0x0020u16.to_be_bytes());
        packet.extend_from_slice(&8u16.to_be_bytes());
        packet.push(0);
        packet.push(0x01);
        let port = 12345u16;
        packet.extend_from_slice(&(port ^ 0x2112).to_be_bytes());
        let ip = [203u8, 0, 113, 9];
        let cookie = 0x2112A442u32.to_be_bytes();
        for i in 0..4 {
            packet.push(ip[i] ^ cookie[i]);
        }
        let parsed = parse_stun_response(&packet, &tx).unwrap();
        assert_eq!(parsed, "203.0.113.9".parse::<IpAddr>().unwrap());
    }
}
