//! Local-only integration checks. Real core tests use GPTGUARD_TEST_MIHOMO.
use std::sync::{Arc, Mutex};
use std::time::Duration;

use gptguard::codex::proxy::ProxyRuntime;
use gptguard::codex::turnstate;
use gptguard::config::{AppPaths, Config, IpCheckMode, RouteConfig};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

fn paths(dir: &std::path::Path) -> AppPaths {
    AppPaths {
        config_dir: dir.join("config"),
        data_dir: dir.join("data"),
        config_file: dir.join("config/config.json"),
        state_file: dir.join("data/state.json"),
        log_dir: dir.join("data/logs"),
        backups_dir: dir.join("data/backups"),
        lock_file: dir.join("data/gptguard.lock"),
    }
}

fn free_pair() -> u16 {
    loop {
        let first = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = first.local_addr().unwrap().port();
        if port < 65535 && std::net::TcpListener::bind(("127.0.0.1", port + 1)).is_ok() {
            return port;
        }
    }
}

async fn gateway(
    mut cfg: Config,
    paths: AppPaths,
) -> (ProxyRuntime, u16, tokio::task::JoinHandle<()>) {
    let mut reserved = Vec::new();
    let listener = loop {
        let candidate = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = candidate.local_addr().unwrap().port();
        if cfg.mihomo.enabled
            && (u32::from(cfg.mihomo.base_port)
                ..u32::from(cfg.mihomo.base_port) + cfg.codex.routes.len() as u32)
                .contains(&u32::from(port))
        {
            reserved.push(candidate);
        } else {
            break candidate;
        }
    };
    drop(reserved);
    let port = listener.local_addr().unwrap().port();
    cfg.codex.listen = format!("127.0.0.1:{port}");
    let proxy = ProxyRuntime::new(cfg, paths).unwrap();
    let router = proxy.clone().router();
    let task = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    (proxy, port, task)
}

async fn request(port: u16, model: &str) -> reqwest::Response {
    reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap()
        .post(format!(
            "http://127.0.0.1:{port}/backend-api/codex/responses"
        ))
        .header("authorization", "Bearer local-test-credential")
        .json(&serde_json::json!({"model": model, "input": []}))
        .send()
        .await
        .unwrap()
}

fn sidecar_config() -> Config {
    let mut cfg = Config::default();
    cfg.mihomo.enabled = true;
    cfg.mihomo.base_port = free_pair();
    cfg.codex.direct = false;
    cfg.protection.ip_check_mode = IpCheckMode::Http;
    cfg.codex.upstream = Some("http://127.0.0.1:9/v1".into());
    cfg.codex.upstream_kind = "official".into();
    cfg.codex.routes.push(RouteConfig {
        id: "one".into(),
        label: "one".into(),
        enabled: true,
        mihomo: None,
        url: "socks5://127.0.0.1:9".into(),
    });
    cfg
}

#[tokio::test]
async fn missing_core_and_empty_pool_fail_closed() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = sidecar_config();
    cfg.mihomo.executable = Some(dir.path().join("missing-core.exe").display().to_string());
    let (proxy, port, task) = gateway(cfg.clone(), paths(dir.path())).await;
    let response = request(port, "codex-auto-review").await;
    assert_eq!(response.status(), 503);
    assert_eq!(
        response.json::<serde_json::Value>().await.unwrap()["error"]["code"],
        "mihomo_unavailable"
    );
    let check = proxy.check_egress(&cfg).await;
    assert!(!check.detected && !check.allowed);
    cfg.codex.routes[0].enabled = false;
    assert!(proxy.check_reload(&cfg).is_err());
    task.abort();
    let _ = task.await;
    drop(proxy);

    let mut cfg = Config::default();
    cfg.codex.direct = false;
    let (proxy, port, task) = gateway(cfg, paths(dir.path())).await;
    let response = request(port, "codex-auto-review").await;
    assert_eq!(response.status(), 503);
    assert_eq!(
        response.json::<serde_json::Value>().await.unwrap()["error"]["code"],
        "no_routes"
    );
    task.abort();
    let _ = task.await;
    drop(proxy);
}

async fn mock_socks(
    records: Arc<Mutex<Vec<String>>>,
    token: Option<String>,
) -> (u16, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let task = tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            let records = records.clone();
            let token = token.clone();
            tokio::spawn(async move {
                let mut hello = [0; 2];
                stream.read_exact(&mut hello).await.unwrap();
                let mut methods = vec![0; hello[1] as usize];
                stream.read_exact(&mut methods).await.unwrap();
                stream.write_all(&[5, 0]).await.unwrap();
                let mut request = [0; 4];
                stream.read_exact(&mut request).await.unwrap();
                let size = match request[3] {
                    1 => 4,
                    4 => 16,
                    3 => stream.read_u8().await.unwrap() as usize,
                    _ => panic!("address"),
                };
                let mut addr = vec![0; size + 2];
                stream.read_exact(&mut addr).await.unwrap();
                stream
                    .write_all(&[5, 0, 0, 1, 127, 0, 0, 1, 0, 1])
                    .await
                    .unwrap();
                let mut bytes = Vec::new();
                loop {
                    bytes.push(stream.read_u8().await.unwrap());
                    if bytes.ends_with(b"\r\n\r\n") {
                        break;
                    }
                    assert!(bytes.len() < 65536);
                }
                let headers = String::from_utf8(bytes.clone()).unwrap();
                let length = headers
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .map(|s| s.trim().parse::<usize>().unwrap())
                    })
                    .unwrap_or(0);
                let mut body = vec![0; length];
                stream.read_exact(&mut body).await.unwrap();
                bytes.extend(body);
                records
                    .lock()
                    .unwrap()
                    .push(String::from_utf8(bytes).unwrap());
                let body = "data: {\"type\":\"response.completed\"}\n\n";
                let token = token
                    .map(|t| format!("X-Codex-Turn-State: {t}\r\n"))
                    .unwrap_or_default();
                stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n{token}\r\n{body}", body.len()).as_bytes()).await.unwrap();
            });
        }
    });
    (port, task)
}

async fn ready(proxy: &ProxyRuntime, previous: Option<u32>) -> u32 {
    for _ in 0..300 {
        let state = proxy.status().mihomo;
        if state.ready && state.pid != previous {
            return state.pid.unwrap();
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("sidecar did not become ready: {:?}", proxy.status().mihomo);
}

fn kill(pid: u32) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        assert!(std::process::Command::new("taskkill.exe")
            .creation_flags(0x08000000)
            .args(["/PID", &pid.to_string(), "/F"])
            .output()
            .unwrap()
            .status
            .success());
    }
    #[cfg(unix)]
    {
        assert!(std::process::Command::new("kill")
            .args(["-KILL", &pid.to_string()])
            .status()
            .unwrap()
            .success());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires GPTGUARD_TEST_MIHOMO pointing to a real Mihomo executable"]
async fn real_core_affinity_restart_and_cleanup() {
    let executable = std::env::var("GPTGUARD_TEST_MIHOMO").expect("GPTGUARD_TEST_MIHOMO");
    let dir = tempfile::tempdir().unwrap();
    let first = Arc::new(Mutex::new(Vec::new()));
    let second = Arc::new(Mutex::new(Vec::new()));
    let token = turnstate::synthetic(33, chrono::Utc::now().timestamp() as u64 - 5);
    let (one, mock1) = mock_socks(first.clone(), None).await;
    let (two, mock2) = mock_socks(second.clone(), Some(token.clone())).await;
    let mut cfg = sidecar_config();
    cfg.mihomo.executable = Some(executable);
    cfg.codex.state_fallback = gptguard::config::StateFallback::Strict;
    cfg.codex.routes[0].url = format!("socks5://127.0.0.1:{one}");
    cfg.codex.routes.push(RouteConfig {
        id: "two".into(),
        label: "two".into(),
        enabled: true,
        mihomo: None,
        url: format!("socks5://127.0.0.1:{two}"),
    });
    let base = cfg.mihomo.base_port;
    let (proxy, port, task) = gateway(cfg.clone(), paths(dir.path())).await;
    let pid = ready(&proxy, None).await;

    // Local unauthenticated access must be rejected too.
    let response = reqwest::Client::builder()
        .no_proxy()
        .proxy(reqwest::Proxy::all(format!("http://127.0.0.1:{base}")).unwrap())
        .build()
        .unwrap()
        .get("http://example.invalid/")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 407);
    for _ in 0..2 {
        let response = request(port, "gpt-6-astra").await;
        assert_eq!(response.status(), 200, "{}", response.text().await.unwrap());
    }
    assert_eq!(
        first.lock().unwrap().len(),
        1,
        "only first route's failed probe"
    );
    assert_eq!(
        second.lock().unwrap().len(),
        3,
        "probe and two pinned generations"
    );
    assert!(second.lock().unwrap()[1].contains(&token));
    assert!(second.lock().unwrap()[2].contains(&token));

    kill(pid);
    for _ in 0..40 {
        if !proxy.status().mihomo.ready {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(!proxy.status().mihomo.ready);
    assert_eq!(request(port, "codex-auto-review").await.status(), 503);
    let new_pid = ready(&proxy, Some(pid)).await;
    assert_ne!(pid, new_pid);
    assert_eq!(request(port, "codex-auto-review").await.status(), 200);

    cfg.protection.enabled = true;
    cfg.codex.listen = format!("127.0.0.1:{port}");
    // Protection starts closed until all exits have been checked.
    proxy.reload(cfg).unwrap();
    assert_eq!(request(port, "codex-auto-review").await.status(), 503);
    proxy.shutdown();
    task.abort();
    let _ = task.await;
    drop(proxy);
    assert!(std::net::TcpStream::connect(("127.0.0.1", base)).is_err());
    assert!(std::net::TcpStream::connect(("127.0.0.1", base + 1)).is_err());
    assert_eq!(
        std::fs::read_dir(dir.path().join("data/mihomo"))
            .unwrap()
            .count(),
        0
    );
    mock1.abort();
    mock2.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires GPTGUARD_TEST_MIHOMO"]
async fn real_core_rejects_occupied_port_and_invalid_config() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = sidecar_config();
    cfg.mihomo.executable = Some(std::env::var("GPTGUARD_TEST_MIHOMO").unwrap());
    let occupied = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    cfg.mihomo.base_port = occupied.local_addr().unwrap().port();
    let (proxy, port, task) = gateway(cfg.clone(), paths(dir.path())).await;
    for _ in 0..100 {
        if proxy.status().mihomo.detail.contains("占用") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(proxy.status().mihomo.detail.contains("占用"));
    assert_eq!(request(port, "codex-auto-review").await.status(), 503);
    proxy.shutdown();
    task.abort();
    let _ = task.await;
    drop(proxy);
    drop(occupied);

    cfg.mihomo.base_port = free_pair();
    cfg.codex.routes[0].url.clear();
    cfg.codex.routes[0].mihomo = Some(serde_json::json!({
        "type": "ss", "server": "127.0.0.1", "port": 9,
        "cipher": "invalid-cipher", "password": "SECRET-NEVER-LOGGED"
    }));
    let (proxy, port, task) = gateway(cfg, paths(dir.path())).await;
    for _ in 0..100 {
        if proxy.status().mihomo.detail.contains("校验失败") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        proxy.status().mihomo.detail.contains("校验失败"),
        "{:?}",
        proxy.status().mihomo
    );
    assert!(!proxy.status().mihomo.detail.contains("SECRET-NEVER-LOGGED"));
    assert_eq!(request(port, "codex-auto-review").await.status(), 503);
    proxy.shutdown();
    task.abort();
    let _ = task.await;
}

// A separate test process owns the job. Killing only that parent must close
// its Job Object and reap Mihomo without relying on taskkill /T.
#[cfg(windows)]
#[tokio::test]
#[ignore = "internal helper for the parent-death test"]
async fn parent_job_helper() {
    let Ok(root) = std::env::var("GPTGUARD_JOB_TEST_ROOT") else {
        return;
    };
    let root = std::path::PathBuf::from(root);
    let mut cfg = sidecar_config();
    cfg.mihomo.executable = Some(std::env::var("GPTGUARD_TEST_MIHOMO").unwrap());
    let base = cfg.mihomo.base_port;
    let proxy = ProxyRuntime::new(cfg, paths(&root)).unwrap();
    let pid = ready(&proxy, None).await;
    std::fs::write(root.join("ready"), format!("{pid}:{base}")).unwrap();
    loop {
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

#[cfg(windows)]
#[test]
#[ignore = "requires GPTGUARD_TEST_MIHOMO"]
fn real_core_dies_with_parent_job() {
    use std::os::windows::process::CommandExt;
    struct Helper(std::process::Child);
    impl Drop for Helper {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let mut child = Helper(
        std::process::Command::new(std::env::current_exe().unwrap())
            .creation_flags(0x08000000)
            .args(["--exact", "parent_job_helper", "--ignored", "--nocapture"])
            .env("GPTGUARD_JOB_TEST_ROOT", dir.path())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap(),
    );
    for _ in 0..300 {
        if dir.path().join("ready").is_file() {
            break;
        }
        assert!(
            child.0.try_wait().unwrap().is_none(),
            "helper exited before ready"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let record = std::fs::read_to_string(dir.path().join("ready")).expect("helper ready");
    let port: u16 = record.split(':').nth(1).unwrap().parse().unwrap();
    child.0.kill().unwrap();
    child.0.wait().unwrap();
    for _ in 0..100 {
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("Mihomo remained alive after its owning process was killed");
}
