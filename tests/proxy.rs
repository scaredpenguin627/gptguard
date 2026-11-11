//! End-to-end relay tests against a local mock upstream. No real OpenAI
//! request is made.

use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::routing::post;
use axum::Router;
use serde_json::json;

use gptguard::codex::proxy::ProxyRuntime;
use gptguard::codex::turnstate;
use gptguard::config::{AppPaths, Config};

#[derive(Clone)]
struct MockState {
    records: Arc<Mutex<Vec<(HeaderMap, String)>>>,
    token: String,
}

async fn mock(State(state): State<MockState>, headers: HeaderMap, body: String) -> Response {
    state.records.lock().unwrap().push((headers, body));
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream")
        .header(turnstate::HEADER, &state.token)
        .body(Body::from("data: {\"type\":\"response.completed\"}\n\n"))
        .unwrap()
}

async fn mock_models() -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "application/json")
        .body(Body::from(
            r#"{"data":[{"id":"gpt-6.3-nova"},{"id":"gpt-6.2-sol"}]}"#,
        ))
        .unwrap()
}

fn temp_paths(root: &std::path::Path) -> AppPaths {
    AppPaths {
        config_dir: root.join("config"),
        data_dir: root.join("data"),
        config_file: root.join("config/config.json"),
        state_file: root.join("data/state.json"),
        log_dir: root.join("data/logs"),
        backups_dir: root.join("data/backups"),
        lock_file: root.join("data/gptguard.lock"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forwards_aux_models_and_injects_33_block_state() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mock_port = listener.local_addr().unwrap().port();
    let records = Arc::new(Mutex::new(Vec::new()));
    let token = turnstate::synthetic(33, chrono::Utc::now().timestamp() as u64 - 5);
    let mock_state = MockState {
        records: records.clone(),
        token: token.clone(),
    };
    let mock_router = Router::new()
        .route("/v1/responses", post(mock))
        .route("/v1/models", axum::routing::get(mock_models))
        .with_state(mock_state);
    tokio::spawn(async move {
        axum::serve(listener, mock_router).await.unwrap();
    });

    let probe = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_port = probe.local_addr().unwrap().port();
    drop(probe);

    let dir = tempfile::tempdir().unwrap();
    let paths = temp_paths(dir.path());
    paths.ensure().unwrap();
    let mut config = Config::default();
    config.codex.listen = format!("127.0.0.1:{proxy_port}");
    config.codex.upstream = Some(format!("http://127.0.0.1:{mock_port}/v1"));
    config.codex.upstream_kind = "official".into();
    config.codex.direct = true;
    config.codex.injection = true;
    config.codex.state_fallback = gptguard::config::StateFallback::Strict;
    let proxy = ProxyRuntime::new(config, paths.clone()).unwrap();
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let running = proxy.clone();
    tokio::spawn(async move {
        let _ = running
            .serve(async {
                let _ = shutdown_rx.await;
            })
            .await;
    });
    tokio::time::sleep(std::time::Duration::from_millis(250)).await;

    let client = reqwest::Client::new();
    let base = format!("http://127.0.0.1:{proxy_port}/backend-api/codex/responses");
    let auth = "Bearer test-token-1234567890";

    let models = client
        .get(format!(
            "http://127.0.0.1:{proxy_port}/backend-api/codex/models"
        ))
        .header("authorization", auth)
        .header("chatgpt-account-id", "acct")
        .header("host", format!("127.0.0.1:{proxy_port}"))
        .send()
        .await
        .unwrap();
    assert_eq!(models.status(), 200);
    let _ = models.text().await.unwrap();
    let saved: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&paths.config_file).unwrap()).unwrap();
    let discovered = saved["codex"]["models"].as_array().unwrap();
    assert!(discovered.iter().any(|model| model == "gpt-6.3-nova"));
    assert!(discovered.iter().any(|model| model == "gpt-6.2-sol"));

    let aux = client
        .post(&base)
        .header("authorization", auth)
        .header("chatgpt-account-id", "acct")
        .header("host", format!("127.0.0.1:{proxy_port}"))
        .json(&json!({"model": "codex-auto-review", "input": []}))
        .send()
        .await
        .unwrap();
    assert_eq!(aux.status(), 200);
    let _ = aux.text().await.unwrap();
    assert!(records
        .lock()
        .unwrap()
        .iter()
        .any(|(_, body)| body.contains("codex-auto-review")));

    let generation = client
        .post(&base)
        .header("authorization", auth)
        .header("chatgpt-account-id", "acct")
        .header("host", format!("127.0.0.1:{proxy_port}"))
        .json(&json!({"model": "gpt-6-astra", "input": []}))
        .send()
        .await
        .unwrap();
    assert_eq!(generation.status(), 200);
    let text = generation.text().await.unwrap();
    assert!(text.contains("response.completed"));

    let records = records.lock().unwrap();
    let generation_record = records
        .iter()
        .find(|(_, body)| body.contains("gpt-6-astra") && body.contains("Reply with OK") == false)
        .or_else(|| {
            records
                .iter()
                .find(|(_, body)| body.contains("gpt-6-astra"))
        })
        .expect("generation reached mock");
    assert_eq!(
        generation_record
            .0
            .get(turnstate::HEADER)
            .and_then(|value| value.to_str().ok()),
        Some(token.as_str())
    );

    let _ = shutdown_tx.send(());
}
