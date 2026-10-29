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

/// Live progress from a running loop, one JSON object per event:
/// `run_started`, `observing`, `observed`, `thinking`, `planned`,
/// `acting`, `acted`, `result` (what the next screenshot showed, i.e.
/// what is fed back to the brain), plus `failed`/`approval` notices.
/// The web layer appends a terminal `final` event (the [`step_view`]
/// fields) when the runner returns, so a client that only reads to the
/// end still gets the old answer — but a client that reads as events
/// arrive can narrate the run in chat while it is still happening.
/// Without this the whole loop was one blocking request: the user saw
/// nothing until it finished, and a mid-loop failure lost every step.
pub type StepEventSender = tokio::sync::mpsc::UnboundedSender<serde_json::Value>;

/// Emit one progress event. Never blocks and never fails the run:
/// a slow or gone reader simply misses events.
pub fn emit(tx: &StepEventSender, event: serde_json::Value) {
    let _ = tx.send(event);
}

/// Single prototype step: capture -> brain -> gate -> act. Implemented in
/// `apps/rosed`; the full loop (budgets, store, events) arrives in M2.
/// Returns a JSON value built by [`step_view`]; `Err` is a user-facing message.
#[async_trait]
pub trait StepRunner: Send + Sync {
    async fn run_step(
        &self,
        goal: &str,
        force: bool,
        chat_id: Option<&str>,
        tx: &StepEventSender,
    ) -> Result<serde_json::Value, String>;
    /// Ask the in-flight run (if any) to stop at its next checkpoint.
    /// Returns whether a run was active. Cooperative, not a kill: the
    /// current atomic action/brain call may finish, nothing new starts.
    fn request_stop(&self) -> bool {
        false
    }
}

/// Adapt an mpsc receiver into the NDJSON response body stream.
struct EventStream {
    rx: tokio::sync::mpsc::UnboundedReceiver<serde_json::Value>,
}

impl futures_core::Stream for EventStream {
    type Item = Result<axum::body::Bytes, std::convert::Infallible>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        match self.rx.poll_recv(cx) {
            std::task::Poll::Ready(Some(value)) => {
                std::task::Poll::Ready(Some(Ok(axum::body::Bytes::from(format!("{value}\n")))))
            }
            std::task::Poll::Ready(None) => std::task::Poll::Ready(None),
            std::task::Poll::Pending => std::task::Poll::Pending,
        }
    }
}

/// Read-only access to persisted run traces. Implemented in
/// `apps/rosed` over the trace store; the web layer never touches
/// the filesystem itself.
#[async_trait]
pub trait TraceSource: Send + Sync {
    /// Recent run summaries, newest first (JSON values).
    async fn runs(&self, limit: u32) -> Vec<serde_json::Value>;
    /// `{summary, events}` for one run, or `None` if unknown.
    async fn run(&self, run_id: &str) -> Option<serde_json::Value>;
    /// The screenshot the brain decided on at `step`, if saved.
    async fn frame(&self, run_id: &str, step: u32) -> Option<Vec<u8>>;
    /// Persisted chat history (user goals + assistant replies),
    /// oldest first, as JSON values.
    async fn chat(&self, limit: u32) -> Vec<serde_json::Value> {
        let _ = limit;
        Vec::new()
    }
    /// Sidebar chats (conversations), most recently updated first.
    async fn chats(&self, limit: u32) -> Vec<serde_json::Value>;
    /// All runs (turns) of one chat, oldest first, as summaries.
    async fn chat_runs(&self, chat_id: &str) -> Option<Vec<serde_json::Value>>;
    async fn rename_chat(&self, chat_id: &str, title: &str) -> bool;
    async fn delete_chat(&self, chat_id: &str) -> bool;
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
    pub traces: Option<Arc<dyn TraceSource>>,
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
        .route("/api/stop", post(api_stop))
        .route("/api/chat", get(api_chat))
        .route("/api/chats", get(api_chats))
        .route(
            "/api/chats/{id}",
            axum::routing::patch(api_chat_rename).delete(api_chat_delete),
        )
        .route("/api/chats/{id}/runs", get(api_chat_runs))
        .route("/api/runs", get(api_runs))
        .route("/api/runs/{id}", get(api_run))
        .route("/api/runs/{id}/frames/{step}", get(api_run_frame))
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
    let chat_id = body
        .get("chat_id")
        .and_then(|v| v.as_str())
        .filter(|id| !id.trim().is_empty())
        .map(str::to_string);
    match &s.stepper {
        None => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"error": "step runner not configured"})),
        )
            .into_response(),
        Some(runner) => {
            // Stream NDJSON live: the run can take minutes (one model call
            // per step), so progress events are flushed as they happen and
            // a terminal `final` event carries the step_view result — or
            // the error, in-band, after whatever steps already succeeded.
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            let runner = Arc::clone(runner);
            tokio::spawn(async move {
                let final_view = match runner.run_step(&goal, force, chat_id.as_deref(), &tx).await
                {
                    Ok(mut view) => {
                        if view.get("type").is_none() {
                            view["type"] = serde_json::json!("final");
                        }
                        view
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "step failed");
                        serde_json::json!({
                            "type": "final",
                            "say": e,
                            "acted": false,
                            "approval_needed": false,
                            "done": false,
                            "error": e,
                        })
                    }
                };
                emit(&tx, final_view);
            });
            (
                [
                    (header::CONTENT_TYPE, "application/x-ndjson"),
                    (header::CACHE_CONTROL, "no-store"),
                ],
                axum::body::Body::from_stream(EventStream { rx }),
            )
                .into_response()
        }
    }
}

async fn api_stop(axum::extract::State(s): axum::extract::State<AppState>) -> impl IntoResponse {
    match &s.stepper {
        None => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"error": "step runner not configured"})),
        )
            .into_response(),
        Some(runner) => {
            let stopping = runner.request_stop();
            tracing::info!(stopping, "stop requested from UI");
            Json(serde_json::json!({"stopping": stopping})).into_response()
        }
    }
}

async fn api_chat(
    axum::extract::State(s): axum::extract::State<AppState>,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    let limit = q
        .get("limit")
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(50)
        .clamp(1, 200);
    match &s.traces {
        None => Json(serde_json::json!({"messages": []})).into_response(),
        Some(t) => Json(serde_json::json!({"messages": t.chat(limit).await})).into_response(),
    }
}

async fn api_chats(
    axum::extract::State(s): axum::extract::State<AppState>,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    let limit = q
        .get("limit")
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(30)
        .clamp(1, 100);
    match &s.traces {
        None => Json(serde_json::json!({"chats": []})).into_response(),
        Some(t) => Json(serde_json::json!({"chats": t.chats(limit).await})).into_response(),
    }
}

async fn api_chat_runs(
    axum::extract::State(s): axum::extract::State<AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> impl IntoResponse {
    match &s.traces {
        None => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"error": "trace store not configured"})),
        )
            .into_response(),
        Some(t) => match t.chat_runs(&id).await {
            Some(runs) => Json(serde_json::json!({"runs": runs})).into_response(),
            None => (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "unknown chat"})),
            )
                .into_response(),
        },
    }
}

async fn api_chat_rename(
    axum::extract::State(s): axum::extract::State<AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let title = body.get("title").and_then(|v| v.as_str()).unwrap_or("");
    if title.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "title missing or empty"})),
        )
            .into_response();
    }
    match &s.traces {
        None => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"error": "trace store not configured"})),
        )
            .into_response(),
        Some(t) if t.rename_chat(&id, title).await => {
            Json(serde_json::json!({"renamed": true})).into_response()
        }
        Some(_) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "unknown chat"})),
        )
            .into_response(),
    }
}

async fn api_chat_delete(
    axum::extract::State(s): axum::extract::State<AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> impl IntoResponse {
    match &s.traces {
        None => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"error": "trace store not configured"})),
        )
            .into_response(),
        Some(t) if t.delete_chat(&id).await => {
            Json(serde_json::json!({"deleted": true})).into_response()
        }
        Some(_) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "unknown chat"})),
        )
            .into_response(),
    }
}

async fn api_runs(
    axum::extract::State(s): axum::extract::State<AppState>,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    let limit = q
        .get("limit")
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(20)
        .clamp(1, 100);
    match &s.traces {
        None => Json(serde_json::json!({"runs": []})).into_response(),
        Some(t) => Json(serde_json::json!({"runs": t.runs(limit).await})).into_response(),
    }
}

async fn api_run(
    axum::extract::State(s): axum::extract::State<AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> impl IntoResponse {
    match &s.traces {
        None => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"error": "trace store not configured"})),
        )
            .into_response(),
        Some(t) => match t.run(&id).await {
            Some(view) => Json(view).into_response(),
            None => (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "unknown run"})),
            )
                .into_response(),
        },
    }
}

async fn api_run_frame(
    axum::extract::State(s): axum::extract::State<AppState>,
    axum::extract::Path((id, step)): axum::extract::Path<(String, u32)>,
) -> impl IntoResponse {
    match &s.traces {
        None => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"error": "trace store not configured"})),
        )
            .into_response(),
        Some(t) => match t.frame(&id, step).await {
            Some(bytes) => (
                [
                    (header::CONTENT_TYPE, "image/jpeg"),
                    (header::CACHE_CONTROL, "no-store"),
                ],
                bytes,
            )
                .into_response(),
            None => (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "unknown frame"})),
            )
                .into_response(),
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
            traces: None,
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
        async fn run_step(
            &self,
            goal: &str,
            force: bool,
            _chat_id: Option<&str>,
            tx: &StepEventSender,
        ) -> Result<serde_json::Value, String> {
            if goal == "boom" {
                return Err("kaboom".to_string());
            }
            emit(
                tx,
                serde_json::json!({"type": "planned", "step": 0, "say": "did a step"}),
            );
            Ok(step_view("did a step", true, force, false))
        }
        fn request_stop(&self) -> bool {
            true
        }
    }

    #[tokio::test]
    async fn stop_endpoint_reports_and_needs_a_runner() {
        let port = spawn(state(None)).await;
        let (head, _) = post(port, "/api/stop", "{}").await;
        assert!(head.contains("503"), "{head}");

        let mut st = state(None);
        st.stepper = Some(Arc::new(StubRunner));
        let port = spawn(st).await;
        let (head, body) = post(port, "/api/stop", "{}").await;
        assert!(head.contains("200 OK"), "{head}");
        let v: serde_json::Value = serde_json::from_slice(&body).expect("json");
        assert_eq!(v["stopping"], true);
    }

    /// Decode a chunked transfer body (the NDJSON stream) into raw bytes.
    fn dechunk(body: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut rest = body;
        loop {
            let eol = rest
                .windows(2)
                .position(|w| w == b"\r\n")
                .expect("chunk size line");
            let size = usize::from_str_radix(
                std::str::from_utf8(&rest[..eol])
                    .expect("chunk size")
                    .trim(),
                16,
            )
            .expect("chunk size hex");
            rest = &rest[eol + 2..];
            if size == 0 {
                break;
            }
            out.extend_from_slice(&rest[..size]);
            rest = &rest[size + 2..];
        }
        out
    }

    fn ndjson_events(body: &[u8]) -> Vec<serde_json::Value> {
        String::from_utf8_lossy(body)
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).expect("ndjson line"))
            .collect()
    }

    struct StubTraces;
    #[async_trait]
    impl TraceSource for StubTraces {
        async fn runs(&self, limit: u32) -> Vec<serde_json::Value> {
            vec![
                serde_json::json!({"run_id": "run-1", "goal": "g", "state": "done", "steps": limit}),
            ]
        }
        async fn run(&self, run_id: &str) -> Option<serde_json::Value> {
            (run_id == "run-1").then(|| {
                serde_json::json!({
                    "summary": {"run_id": "run-1", "state": "done"},
                    "events": [{"type": "final", "say": "ok"}],
                })
            })
        }
        async fn frame(&self, run_id: &str, step: u32) -> Option<Vec<u8>> {
            (run_id == "run-1" && step == 2).then(|| vec![0xff, 0xd8, 0xff, 0xd9])
        }
        async fn chat(&self, _limit: u32) -> Vec<serde_json::Value> {
            vec![
                serde_json::json!({"role": "user", "text": "hi", "run_id": "run-1"}),
                serde_json::json!({"role": "assistant", "text": "hello", "run_id": "run-1"}),
            ]
        }
        async fn chats(&self, _limit: u32) -> Vec<serde_json::Value> {
            vec![
                serde_json::json!({"chat_id": "chat-1", "title": "hi", "turns": 2, "state": "done"}),
            ]
        }
        async fn chat_runs(&self, chat_id: &str) -> Option<Vec<serde_json::Value>> {
            (chat_id == "chat-1")
                .then(|| vec![serde_json::json!({"run_id": "run-1", "goal": "hi"})])
        }
        async fn rename_chat(&self, chat_id: &str, _title: &str) -> bool {
            chat_id == "chat-1"
        }
        async fn delete_chat(&self, chat_id: &str) -> bool {
            chat_id == "chat-1"
        }
    }

    #[tokio::test]
    async fn runs_endpoints_serve_persisted_traces() {
        // Without a trace source the list is empty, detail is 503.
        let port = spawn(state(None)).await;
        let (_, body) = get(port, "/api/runs").await;
        let v: serde_json::Value = serde_json::from_slice(&body).expect("json");
        assert_eq!(v["runs"], serde_json::json!([]));
        let (head, _) = get(port, "/api/runs/run-1").await;
        assert!(head.contains("503"), "{head}");

        let mut st = state(None);
        st.traces = Some(Arc::new(StubTraces));
        let port = spawn(st).await;
        let (_, body) = get(port, "/api/runs?limit=7").await;
        let v: serde_json::Value = serde_json::from_slice(&body).expect("json");
        assert_eq!(v["runs"][0]["run_id"], "run-1");
        assert_eq!(v["runs"][0]["steps"], 7, "limit reaches the source");
        let (_, body) = get(port, "/api/runs/run-1").await;
        let v: serde_json::Value = serde_json::from_slice(&body).expect("json");
        assert_eq!(v["events"][0]["type"], "final");
        let (head, _) = get(port, "/api/runs/nope").await;
        assert!(head.contains("404"), "{head}");
        let (_, body) = get(port, "/api/chat").await;
        let v: serde_json::Value = serde_json::from_slice(&body).expect("json");
        assert_eq!(v["messages"][0]["role"], "user");
        assert_eq!(v["messages"][1]["text"], "hello");
        let (_, body) = get(port, "/api/chats").await;
        let v: serde_json::Value = serde_json::from_slice(&body).expect("json");
        assert_eq!(v["chats"][0]["chat_id"], "chat-1");
        assert_eq!(v["chats"][0]["turns"], 2);
        let (_, body) = get(port, "/api/chats/chat-1/runs").await;
        let v: serde_json::Value = serde_json::from_slice(&body).expect("json");
        assert_eq!(v["runs"][0]["run_id"], "run-1");
        let (head, _) = get(port, "/api/chats/nope/runs").await;
        assert!(head.contains("404"), "{head}");
        let (head, body) = get(port, "/api/runs/run-1/frames/2").await;
        assert!(head.contains("200 OK"), "{head}");
        assert_eq!(body, vec![0xff, 0xd8, 0xff, 0xd9]);
        let (head, _) = get(port, "/api/runs/run-1/frames/9").await;
        assert!(head.contains("404"), "{head}");
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
        // Success streams progress events then a terminal `final`.
        let (head, body) = post(port, "/api/step", r#"{"goal":"hi"}"#).await;
        assert!(head.contains("200 OK"), "{head}");
        assert!(head.contains("x-ndjson"), "{head}");
        let events = ndjson_events(&dechunk(&body));
        assert_eq!(events.len(), 2, "{events:?}");
        assert_eq!(events[0]["type"], "planned");
        assert_eq!(events[1]["type"], "final");
        assert_eq!(events[1]["acted"], true);
        // A failure arrives in-band as a final event (not a bare 502), so
        // steps streamed before it are not lost.
        let (head, body) = post(port, "/api/step", r#"{"goal":"boom"}"#).await;
        assert!(head.contains("200 OK"), "{head}");
        let events = ndjson_events(&dechunk(&body));
        assert_eq!(events.len(), 1, "{events:?}");
        assert_eq!(events[0]["type"], "final");
        assert!(events[0]["error"]
            .as_str()
            .expect("error")
            .contains("kaboom"));
    }
}
