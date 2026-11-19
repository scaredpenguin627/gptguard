//! Exercise the installed desktop client's HTTP contract against a real daemon.
//! Ports, configuration, Codex home and the deliberately failing proxy are local fixtures.
use gptguard::config::{AppPaths, Config, IpCheckMode, RouteConfig};
use reqwest::{Client, StatusCode};
use serde_json::{json, Value};
use std::{path::Path, time::Duration};
use tokio::net::TcpListener;

fn paths(dir: &Path) -> AppPaths {
    AppPaths {
        config_dir: dir.into(),
        data_dir: dir.into(),
        config_file: dir.join("config.json"),
        state_file: dir.join("state.json"),
        log_dir: dir.join("logs"),
        backups_dir: dir.join("backups"),
        lock_file: dir.join("guard.lock"),
    }
}

#[test]
fn reading_a_legacy_empty_whitelist_keeps_existing_protection() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = paths(tmp.path());
    let mut cfg = Config::load_or_create(&paths).unwrap();
    cfg.protection.enabled = true;
    cfg.protection.whitelist.clear();
    cfg.save(&paths).unwrap();
    let original = std::fs::read(&paths.config_file).unwrap();
    assert!(Config::load_or_create(&paths).unwrap().protection.enabled);
    assert_eq!(original, std::fs::read(&paths.config_file).unwrap());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn activation_rejects_empty_rules_and_blocks_unverified_clash_requests() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = paths(tmp.path());
    let mut cfg = Config::load_or_create(&paths).unwrap();
    let refused = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_url = format!("http://{}", refused.local_addr().unwrap());
    let rejecting_proxy = tokio::spawn(async move {
        while let Ok((stream, _)) = refused.accept().await {
            drop(stream);
        }
    });
    let api = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let relay = TcpListener::bind("127.0.0.1:0").await.unwrap();
    cfg.ui.web_port = api.local_addr().unwrap().port();
    cfg.codex.listen = relay.local_addr().unwrap().to_string();
    cfg.codex.codex_home = Some(tmp.path().join("codex").display().to_string());
    cfg.codex.direct = false;
    cfg.codex.routes = vec![RouteConfig {
        id: "auto-local".into(),
        label: "fixture".into(),
        url: proxy_url.clone(),
        enabled: true,
        mihomo: None,
    }];
    cfg.mihomo.enabled = false;
    cfg.protection.network_guard = false;
    cfg.protection.kill_on_block = false;
    cfg.protection.targets.clear();
    cfg.protection.ip_check_mode = IpCheckMode::Http;
    cfg.protection.http_proxy = Some(proxy_url.clone());
    cfg.save(&paths).unwrap();
    drop((api, relay));
    let worker = tokio::spawn(gptguard::service::run(cfg.clone(), paths.clone(), false));
    let client = Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    let base = format!("http://127.0.0.1:{}/api", cfg.ui.web_port);
    let token = &cfg.ui.admin_token;
    let post = |path: &str, body: Value| {
        client
            .post(format!("{base}/{path}"))
            .header("x-gptguard-token", token)
            .json(&body)
            .send()
    };
    let snapshot = tokio::time::timeout(Duration::from_secs(12), async {
        loop {
            if let Ok(resp) = client
                .get(format!("{base}/status"))
                .header("x-gptguard-token", token)
                .send()
                .await
            {
                break resp.json::<Value>().await.unwrap();
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
    assert!(snapshot["daemon_elevated"].is_boolean());
    assert_eq!(snapshot["local_proxy"], format!("{proxy_url}/"));
    let rejected = post("protection", json!({"enabled":true})).await.unwrap();
    assert_eq!(rejected.status(), StatusCode::CONFLICT);
    assert_eq!(
        rejected.json::<Value>().await.unwrap()["error"]["code"],
        "whitelist_required"
    );
    assert!(!Config::load_or_create(&paths).unwrap().protection.enabled);
    let trust = post(
        "codex/trust-local-exit",
        json!({"proxy": format!("{proxy_url}/"),"ip":"203.0.113.8"}),
    )
    .await
    .unwrap();
    assert_eq!(trust.status(), StatusCode::CONFLICT);
    assert!(Config::load_or_create(&paths)
        .unwrap()
        .protection
        .whitelist
        .is_empty());
    assert!(post(
        "whitelist",
        json!({"entries":["203.0.113.7"],"append":true})
    )
    .await
    .unwrap()
    .status()
    .is_success());
    assert_eq!(
        post("protection", json!({"enabled":true}))
            .await
            .unwrap()
            .json::<Value>()
            .await
            .unwrap()["enabled"],
        true
    );
    assert_eq!(
        post("restart-idle", json!({})).await.unwrap().status(),
        StatusCode::CONFLICT
    );
    let check = post("check", json!({}))
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(check["runtime"]["allowed"], false);
    let denied = client
        .get(format!(
            "http://{}/backend-api/codex/models",
            cfg.codex.listen
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        denied.json::<Value>().await.unwrap()["error"]["code"],
        "egress_unverified"
    );
    assert_eq!(
        post("protection", json!({"enabled":false}))
            .await
            .unwrap()
            .json::<Value>()
            .await
            .unwrap()["enabled"],
        false
    );
    assert!(!Config::load_or_create(&paths).unwrap().protection.enabled);
    assert!(post("shutdown", json!({}))
        .await
        .unwrap()
        .status()
        .is_success());
    tokio::time::timeout(Duration::from_secs(8), worker)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    rejecting_proxy.abort();
}
