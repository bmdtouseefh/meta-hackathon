//! HTTP API + embedded static UI (IMPLEMENTATION.md T1.4, shell first).
//!
//! Calls agent/store traits only — never the desktop or models directly.
//! The one exception is [`FrameSource`]: a narrow snapshot seam the
//! composition root (`apps/rosed`) adapts a backend to. Input injection
//! (click/type via the page) is NOT exposed here; it waits for the T1.4
//! takeover lease, which gates every input through the agent.

use async_trait::async_trait;
use axum::{
    http::{header, StatusCode},
    response::{Html, IntoResponse},
    routing::{get, post},
    Json, Router,
};
use std::sync::Arc;

/// Read-only snapshot of the desktop for the live view. Implemented in
/// `apps/rosed` (the one place allowed to touch backends).
#[async_trait]
pub trait FrameSource: Send + Sync {
    async fn jpeg(&self) -> std::result::Result<Vec<u8>, String>;
    fn display(&self) -> String;
    fn size(&self) -> (u32, u32);
}

/// Single prototype step: capture -> brain -> gate -> act. Implemented in
/// `apps/rosed`; the full loop (budgets, store, events) arrives in M2.
/// Returns a JSON value built by [`step_view`]; `Err` is a user-facing message.
#[async_trait]
pub trait StepRunner: Send + Sync {
    async fn run_step(&self, goal: &str, force: bool) -> Result<serde_json::Value, String>;
}

/// A step result card for the chat thread.
pub fn step_view(say: &str, acted: bool, approval_needed: bool, done: bool) -> serde_json::Value {
    serde_json::json!({
        "say": say,
        "acted": acted,
        "approval_needed": approval_needed,
        "done": done,
    })
}

/// Server-memory API key slot. Set via POST /api/keys (Setup modal) or left
/// empty when the key comes from env. Never logged, never persisted.
#[derive(Clone, Default)]
pub struct KeySlot {
    inner: Arc<std::sync::Mutex<Option<String>>>,
}

impl KeySlot {
    pub fn set(&self, key: String) {
        *self.inner.lock().expect("key slot") = Some(key);
    }

    pub fn clear(&self) {
        *self.inner.lock().expect("key slot") = None;
    }

    pub fn get(&self) -> Option<String> {
        self.inner.lock().expect("key slot").clone()
    }

    pub fn is_set(&self) -> bool {
        self.inner.lock().expect("key slot").is_some()
    }
}

#[derive(Clone)]
pub struct AppState {
    pub source: Option<Arc<dyn FrameSource>>,
    pub stepper: Option<Arc<dyn StepRunner>>,
    pub keys: KeySlot,
    pub env_key_present: bool,
    pub backend: String,
    pub brain: String,
    pub version: String,
    pub started: std::time::SystemTime,
}

const INDEX_HTML: &str = include_str!("../../../ui/index.html");
const APP_JS: &str = include_str!("../../../ui/app.js");
const STYLE_CSS: &str = include_str!("../../../ui/style.css");

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(|| async { Html(INDEX_HTML) }))
        .route(
            "/app.js",
            get(|| async { ([(header::CONTENT_TYPE, "text/javascript")], APP_JS) }),
        )
        .route(
            "/style.css",
            get(|| async { ([(header::CONTENT_TYPE, "text/css")], STYLE_CSS) }),
        )
        .route("/api/status", get(api_status))
        .route("/api/health", get(api_health))
        .route("/api/frame", get(api_frame))
        .route(
            "/api/keys",
            get(api_keys_get)
                .post(api_keys_post)
                .delete(api_keys_delete),
        )
        .route("/api/step", post(api_step))
        .with_state(state)
}

async fn api_status(axum::extract::State(s): axum::extract::State<AppState>) -> impl IntoResponse {
    tracing::debug!(backend = %s.backend, brain = %s.brain, "status");
    let (width, height) = s.source.as_ref().map(|f| f.size()).unwrap_or((0, 0));
    Json(serde_json::json!({
        "status": "idle",
        "backend": s.backend,
        "brain": s.brain,
        "display": s.source.as_ref().map(|f| f.display()).unwrap_or_default(),
        "width": width,
        "height": height,
        "version": s.version,
    }))
}

async fn api_health(axum::extract::State(s): axum::extract::State<AppState>) -> impl IntoResponse {
    let uptime_secs = s.started.elapsed().map(|d| d.as_secs()).unwrap_or(0);
    Json(serde_json::json!({
        "ok": true,
        "version": s.version,
        "uptime_secs": uptime_secs,
    }))
}

async fn api_frame(axum::extract::State(s): axum::extract::State<AppState>) -> impl IntoResponse {
    match &s.source {
        None => {
            tracing::debug!("frame: no backend configured");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({"error": "no desktop backend (T1.4)"})),
            )
                .into_response()
        }
        Some(f) => match f.jpeg().await {
            Ok(bytes) => {
                tracing::debug!(bytes = bytes.len(), "frame served");
                (
                    [
                        (header::CONTENT_TYPE, "image/jpeg"),
                        (header::CACHE_CONTROL, "no-store"),
                    ],
                    bytes,
                )
                    .into_response()
            }
            Err(e) => {
                tracing::warn!(error = %e, "frame capture failed");
                (
                    StatusCode::BAD_GATEWAY,
                    Json(serde_json::json!({"error": e})),
                )
                    .into_response()
            }
        },
    }
}

async fn api_keys_get(
    axum::extract::State(s): axum::extract::State<AppState>,
) -> impl IntoResponse {
    let web = s.keys.is_set();
    let source = if web {
        "web"
    } else if s.env_key_present {
        "env"
    } else {
        "none"
    };
    Json(serde_json::json!({ "set": web || s.env_key_present, "source": source }))
}

async fn api_keys_delete(
    axum::extract::State(s): axum::extract::State<AppState>,
) -> impl IntoResponse {
    s.keys.clear();
    tracing::info!("muse api key cleared via UI");
    Json(serde_json::json!({ "set": false }))
}

async fn api_keys_post(
    axum::extract::State(s): axum::extract::State<AppState>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    match body.get("muse_api_key").and_then(|v| v.as_str()) {
        Some(k) if !k.trim().is_empty() => {
            s.keys.set(k.trim().to_string());
            tracing::info!("muse api key set via UI (memory only)");
            Json(serde_json::json!({ "set": true })).into_response()
        }
        _ => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "muse_api_key missing or empty"})),
        )
            .into_response(),
    }
}

async fn api_step(
    axum::extract::State(s): axum::extract::State<AppState>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let goal = body
        .get("goal")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if goal.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "goal missing or empty"})),
        )
            .into_response();
    }
    let force = body.get("force").and_then(|v| v.as_bool()).unwrap_or(false);
    match &s.stepper {
        None => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"error": "step runner not configured"})),
        )
            .into_response(),
        Some(runner) => match runner.run_step(&goal, force).await {
            Ok(view) => Json(view).into_response(),
            Err(e) => {
                tracing::warn!(error = %e, "step failed");
                (
                    StatusCode::BAD_GATEWAY,
                    Json(serde_json::json!({"error": e})),
                )
                    .into_response()
            }
        },
    }
}

/// Serve forever on an already-bound listener.
pub async fn serve(listener: tokio::net::TcpListener, state: AppState) -> std::io::Result<()> {
    axum::serve(listener, router(state)).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    struct Stub;
    #[async_trait]
    impl FrameSource for Stub {
        async fn jpeg(&self) -> std::result::Result<Vec<u8>, String> {
            Ok(vec![0xff, 0xd8, 0xff, 0xd9])
        }
        fn display(&self) -> String {
            ":stub".to_string()
        }
        fn size(&self) -> (u32, u32) {
            (1600, 900)
        }
    }

    fn state(source: Option<Arc<dyn FrameSource>>) -> AppState {
        AppState {
            source,
            stepper: None,
            keys: KeySlot::default(),
            env_key_present: false,
            backend: "fake".to_string(),
            brain: "fake".to_string(),
            version: "0.1.0".to_string(),
            started: std::time::SystemTime::now(),
        }
    }

    async fn get(port: u16, path: &str) -> (String, Vec<u8>) {
        let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .expect("connect");
        let req = format!("GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n");
        stream.write_all(req.as_bytes()).await.expect("write");
        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).await.expect("read");
        let split = buf
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .expect("header/body split");
        (
            String::from_utf8_lossy(&buf[..split]).into_owned(),
            buf[split + 4..].to_vec(),
        )
    }

    async fn spawn(state: AppState) -> u16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let port = listener.local_addr().expect("addr").port();
        tokio::spawn(async move { serve(listener, state).await });
        port
    }

    #[tokio::test]
    async fn index_and_status_serve() {
        let port = spawn(state(Some(Arc::new(Stub)))).await;
        let (head, body) = get(port, "/").await;
        assert!(head.contains("200 OK"), "{head}");
        assert!(String::from_utf8_lossy(&body).contains("Rose"), "has UI");
        let (head, body) = get(port, "/api/status").await;
        assert!(head.contains("200 OK"), "{head}");
        let v: serde_json::Value = serde_json::from_slice(&body).expect("json");
        assert_eq!(v["backend"], "fake");
        assert_eq!(v["width"], 1600);
    }

    #[tokio::test]
    async fn frame_returns_jpeg_without_backend_gives_503() {
        let port = spawn(state(Some(Arc::new(Stub)))).await;
        let (head, body) = get(port, "/api/frame").await;
        assert!(head.contains("200 OK"), "{head}");
        assert!(head.contains("image/jpeg"), "{head}");
        assert_eq!(body, vec![0xff, 0xd8, 0xff, 0xd9]);

        let port = spawn(state(None)).await;
        let (head, _) = get(port, "/api/frame").await;
        assert!(head.contains("503"), "{head}");
    }

    #[tokio::test]
    async fn health_reports_version_and_uptime() {
        let port = spawn(state(None)).await;
        let (head, body) = get(port, "/api/health").await;
        assert!(head.contains("200 OK"), "{head}");
        let v: serde_json::Value = serde_json::from_slice(&body).expect("json");
        assert_eq!(v["ok"], true);
        assert_eq!(v["version"], "0.1.0");
        assert!(v["uptime_secs"].as_u64().is_some());
    }

    async fn post(port: u16, path: &str, body: &str) -> (String, Vec<u8>) {
        request(port, "POST", path, Some(body)).await
    }

    async fn delete(port: u16, path: &str) -> (String, Vec<u8>) {
        request(port, "DELETE", path, None).await
    }

    async fn request(port: u16, method: &str, path: &str, body: Option<&str>) -> (String, Vec<u8>) {
        let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .expect("connect");
        let payload = body.unwrap_or("");
        let req = format!(
            "{method} {path} HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
            payload.len()
        );
        stream.write_all(req.as_bytes()).await.expect("write");
        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).await.expect("read");
        let split = buf
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .expect("header/body split");
        (
            String::from_utf8_lossy(&buf[..split]).into_owned(),
            buf[split + 4..].to_vec(),
        )
    }

    #[tokio::test]
    async fn keys_roundtrip_memory_only() {
        let port = spawn(state(None)).await;
        let (_, body) = get(port, "/api/keys").await;
        let v: serde_json::Value = serde_json::from_slice(&body).expect("json");
        assert_eq!(v["set"], false);
        assert_eq!(v["source"], "none");

        let (head, _) = post(port, "/api/keys", r#"{"muse_api_key":""}"#).await;
        assert!(head.contains("400"), "{head}");
        let (head, _) = post(port, "/api/keys", r#"{"muse_api_key":"sk-test"}"#).await;
        assert!(head.contains("200 OK"), "{head}");

        let (_, body) = get(port, "/api/keys").await;
        let v: serde_json::Value = serde_json::from_slice(&body).expect("json");
        assert_eq!(v["set"], true);
        assert_eq!(v["source"], "web");
        // The value itself is never exposed.
        assert!(!String::from_utf8_lossy(&body).contains("sk-test"));

        // DELETE clears the slot; status flips back to missing.
        let (head, _) = delete(port, "/api/keys").await;
        assert!(head.contains("200 OK"), "{head}");
        let (_, body) = get(port, "/api/keys").await;
        let v: serde_json::Value = serde_json::from_slice(&body).expect("json");
        assert_eq!(v["set"], false);
    }

    struct StubRunner;
    #[async_trait]
    impl StepRunner for StubRunner {
        async fn run_step(&self, goal: &str, force: bool) -> Result<serde_json::Value, String> {
            if goal == "boom" {
                return Err("kaboom".to_string());
            }
            Ok(step_view("did a step", true, force, false))
        }
    }

    #[tokio::test]
    async fn step_needs_runner_and_validates_goal() {
        let port = spawn(state(None)).await;
        let (head, _) = post(port, "/api/step", r#"{"goal":"hi"}"#).await;
        assert!(head.contains("503"), "{head}");

        let mut st = state(None);
        st.stepper = Some(Arc::new(StubRunner));
        let port = spawn(st).await;
        let (head, _) = post(port, "/api/step", r#"{"goal":""}"#).await;
        assert!(head.contains("400"), "{head}");
        let (head, body) = post(port, "/api/step", r#"{"goal":"hi"}"#).await;
        assert!(head.contains("200 OK"), "{head}");
        let v: serde_json::Value = serde_json::from_slice(&body).expect("json");
        assert_eq!(v["acted"], true);
        let (head, body) = post(port, "/api/step", r#"{"goal":"boom"}"#).await;
        assert!(head.contains("502"), "{head}");
        assert!(String::from_utf8_lossy(&body).contains("kaboom"));
    }
}
