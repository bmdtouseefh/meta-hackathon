//! HTTP API + embedded static UI (IMPLEMENTATION.md T1.4, shell first).
//!
//! Calls agent/store traits only — never the desktop or models directly.
//! The exceptions are [`FrameSource`] (a narrow snapshot seam) and
//! [`HumanInput`] (human takeover input): both are implemented in
//! `apps/rosed` (the one place allowed to touch backends). Takeover input
//! executes only while the [`Lease`] holder is human; agent steps pause
//! for the duration and resume (with a fresh observation) on hand-back.

use async_trait::async_trait;
use axum::{
    body::Bytes,
    http::{header, HeaderMap, StatusCode},
    response::{Html, IntoResponse},
    routing::{get, post},
    Json, Router,
};
use rose_agent::{Driver, Lease};
use rose_core::RoseConfig;
use serde::{Deserialize, Serialize};
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
/// Human takeover input: the composition root adapts a backend to this.
/// Calls execute with `Actor::Human` through the same `act()` path as the
/// agent (one input channel, one audit log). The endpoints below only call
/// this while the [`Lease`] holder is human; the adapter re-checks.
#[async_trait]
pub trait HumanInput: Send + Sync {
    /// Map live-view fractions (0..1) onto a fresh frame, then act.
    async fn click_frac(&self, x: f64, y: f64, double: bool) -> Result<(), String>;
    async fn type_text(&self, text: &str) -> Result<(), String>;
    async fn press_keys(&self, keys: Vec<String>) -> Result<(), String>;
    async fn scroll_frac(&self, x: f64, y: f64, dy_frac: f64) -> Result<(), String>;
}

pub type StepEventSender = tokio::sync::mpsc::UnboundedSender<serde_json::Value>;

/// Emit one progress event. Never blocks and never fails the run:
/// a slow or gone reader simply misses events.
pub fn emit(tx: &StepEventSender, event: serde_json::Value) {
    let _ = tx.send(event);
}

/// Single prototype step: capture -> brain -> gate -> act. Implemented in
/// `apps/rosed`; the full loop (budgets, store, events) arrives in M2.
/// Returns a JSON value built by [`step_view`]; `Err` is a user-facing message.
///
/// `force` answers a pending approval: the run resumes where it paused
/// (single-use — it passes that one gate, later gates still ask).
/// `auto_approve` is "don't ask again for this run": every further gate
/// auto-passes, each logged as an `approval` event with `"auto": true`.
#[async_trait]
pub trait StepRunner: Send + Sync {
    async fn run_step(
        &self,
        goal: &str,
        force: bool,
        auto_approve: bool,
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
    /// The SoM-annotated screenshot (boxes + badges) the brain actually
    /// saw at `step`, if grounding was on. Defaults to `None` so older
    /// runs and test doubles without overlays keep working.
    async fn grounded_frame(&self, run_id: &str, step: u32) -> Option<Vec<u8>> {
        let _ = (run_id, step);
        None
    }
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

/// Live config: GET shows the desired config, PUT validates + persists.
/// Policy + brain hot-reload into the running loop; backend/display/size/
/// bind/data/cdp need a `rosed` restart (reported as `restart_needed`).
#[async_trait]
pub trait ConfigSource: Send + Sync {
    async fn get(&self) -> RoseConfig;
    /// Persist + apply. Returns `(applied_config, restart_needed)`.
    async fn set(&self, cfg: RoseConfig) -> Result<(RoseConfig, bool), String>;
    fn source_name(&self) -> String {
        "rosed (/api/config)".to_string()
    }
}

/// Validate a full config the same way PUT does. Pure: no I/O.
pub fn validate_config(cfg: &RoseConfig) -> Result<(), String> {
    let addr: std::net::SocketAddr = cfg
        .server
        .bind
        .parse()
        .map_err(|_| "bind must be host:port (e.g. 127.0.0.1:8787)".to_string())?;
    if !addr.ip().is_loopback() && !cfg.server.allow_remote {
        return Err("refusing non-loopback bind without allow_remote = true".to_string());
    }
    match cfg.desktop.backend.as_str() {
        "auto" | "x11" | "cdp" | "fake" => {}
        other => return Err(format!("unknown desktop backend: {other}")),
    }
    if !(640..=7680).contains(&cfg.desktop.width) || !(360..=4320).contains(&cfg.desktop.height) {
        return Err("desktop size looks wrong (want 640..7680 x 360..4320)".to_string());
    }
    match cfg.models.brain.as_str() {
        "muse" | "fake" => {}
        other => return Err(format!("unknown brain: {other}")),
    }
    if cfg.models.endpoint.trim().is_empty() {
        return Err("models.endpoint must not be empty".to_string());
    }
    if cfg.models.model.trim().is_empty() {
        return Err("models.model must not be empty".to_string());
    }
    if !(1..=200).contains(&cfg.policy.max_steps) {
        return Err("policy.max_steps must be 1..200".to_string());
    }
    if !(1..=120).contains(&cfg.policy.max_minutes) {
        return Err("policy.max_minutes must be 1..120".to_string());
    }
    if !(1..=500).contains(&cfg.policy.max_model_calls) {
        return Err("policy.max_model_calls must be 1..500".to_string());
    }
    if cfg.data.dir.trim().is_empty() {
        return Err("data.dir must not be empty".to_string());
    }
    if cfg.cdp.endpoint.trim().is_empty() {
        return Err("cdp.endpoint must not be empty".to_string());
    }
    Ok(())
}

/// True when applying `next` needs a restart to take effect (anything the
/// running loop cannot hot-reload: bind, backend, geometry, cdp, data dir).
/// Policy + models hot-reload, so they never force a restart.
pub fn config_restart_needed(old: &RoseConfig, next: &RoseConfig) -> bool {
    old.server != next.server
        || old.desktop != next.desktop
        || old.cdp != next.cdp
        || old.data != next.data
}

/// One draft instruction derived from a video (Skill Studio step).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VideoStep {
    pub ts: u64,
    pub say: String,
    pub intent: String,
    pub params: String,
    pub approval: bool,
    pub conf: f32,
    pub quote: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VideoMeta {
    pub id: String,
    pub name: String,
    pub size: u64,
    pub mime: String,
    pub created_at: String,
    pub status: String,
    pub steps: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VideoDetail {
    pub meta: VideoMeta,
    pub steps: Vec<VideoStep>,
}

/// File-backed video store seam (implemented in `apps/rosed`; the web
/// layer never touches the filesystem). Uploads live under
/// `<data>/videos/<id>/`, alongside `job.json` (`{video_path, out_dir}`).
/// `generate` calls Muse Spark native video (frames + narration in one
/// call) — never a mock, never SAM.
#[async_trait]
pub trait VideoSource: Send + Sync {
    async fn list(&self) -> Vec<VideoMeta>;
    async fn upload(&self, name: &str, mime: &str, bytes: Vec<u8>) -> Result<VideoMeta, String>;
    async fn file(&self, id: &str) -> Option<(Vec<u8>, String)>;
    async fn detail(&self, id: &str) -> Option<VideoDetail>;
    async fn generate(&self, id: &str) -> Result<VideoDetail, String>;
    async fn save_draft(&self, id: &str, steps: Vec<VideoStep>) -> Result<VideoDetail, String>;
    async fn remove(&self, id: &str) -> bool;
}

pub fn sanitize_video_name(name: &str) -> String {
    let base = name
        .rsplit('/')
        .next()
        .unwrap_or(name)
        .rsplit('\\')
        .next()
        .unwrap_or(name)
        .trim();
    let cleaned: String = base
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' || c == ' ' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let cleaned = cleaned.trim().trim_matches('.').to_string();
    if cleaned.is_empty() {
        "video.mp4".to_string()
    } else if cleaned.contains('.') {
        cleaned
    } else {
        format!("{cleaned}.mp4")
    }
}

pub fn valid_video_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

#[derive(Clone)]
pub struct AppState {
    pub source: Option<Arc<dyn FrameSource>>,
    pub stepper: Option<Arc<dyn StepRunner>>,
    pub traces: Option<Arc<dyn TraceSource>>,
    pub keys: KeySlot,
    pub lease: Lease,
    pub control: Option<Arc<dyn HumanInput>>,
    pub config: Option<Arc<dyn ConfigSource>>,
    pub videos: Option<Arc<dyn VideoSource>>,
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
    // UI assets carry no-store: the JS/CSS are embedded in the binary and
    // change with it, but browsers otherwise cache /app.js across deploys
    // and new UI behaviour never arrives.
    Router::new()
        .route(
            "/",
            get(|| async { ([(header::CACHE_CONTROL, "no-store")], Html(INDEX_HTML)) }),
        )
        .route(
            "/app.js",
            get(|| async {
                (
                    [(header::CACHE_CONTROL, "no-store")],
                    ([(header::CONTENT_TYPE, "text/javascript")], APP_JS),
                )
            }),
        )
        .route(
            "/style.css",
            get(|| async {
                (
                    [(header::CACHE_CONTROL, "no-store")],
                    ([(header::CONTENT_TYPE, "text/css")], STYLE_CSS),
                )
            }),
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
        .route(
            "/api/runs/{id}/frames/{step}/grounded",
            get(api_run_grounded_frame),
        )
        .route("/desktop/takeover", post(api_takeover))
        .route("/desktop/handback", post(api_handback))
        .route("/desktop/input", post(api_input))
        .route("/api/config", get(api_config_get).put(api_config_put))
        .merge(videos_router())
        .with_state(state)
}

/// Video routes carry their own body limit: axum's global default is 2 MB,
/// which rejects every real screen recording with 413 (browsers surface
/// the torn-down upload as a bare network error). The upload handler caps
/// at 200 MB itself, so the framework limit is lifted here only — every
/// other route keeps the 2 MB default.
fn videos_router() -> Router<AppState> {
    use axum::extract::DefaultBodyLimit;
    Router::new()
        .route("/api/videos", get(api_videos_list).post(api_videos_upload))
        .route(
            "/api/videos/{id}",
            get(api_video_get).delete(api_video_delete),
        )
        .route("/api/videos/{id}/file", get(api_video_file))
        .route(
            "/api/videos/{id}/instructions",
            post(api_video_instructions),
        )
        .route(
            "/api/videos/{id}/draft",
            post(api_video_draft).put(api_video_draft),
        )
        .layer(DefaultBodyLimit::disable())
}

async fn api_status(axum::extract::State(s): axum::extract::State<AppState>) -> impl IntoResponse {
    tracing::debug!(backend = %s.backend, brain = %s.brain, "status");
    let (width, height) = s.source.as_ref().map(|f| f.size()).unwrap_or((0, 0));
    Json(serde_json::json!({
        "status": "idle",
        "backend": s.backend,
        "brain": s.brain,
        "lease": s.lease.holder().as_str(),
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
    let auto_approve = body
        .get("auto_approve")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
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
                let final_view = match runner
                    .run_step(&goal, force, auto_approve, chat_id.as_deref(), &tx)
                    .await
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

/// Human takes the desktop. The agent loop pauses at its next checkpoint
/// (an in-flight atomic action still finishes); input below only works
/// while the lease is held here. Idempotent.
async fn api_takeover(
    axum::extract::State(s): axum::extract::State<AppState>,
) -> impl IntoResponse {
    if s.control.is_none() {
        return err(
            StatusCode::SERVICE_UNAVAILABLE,
            "takeover input not configured",
        );
    }
    s.lease.take_human();
    tracing::info!("takeover: human holds the desktop, agent paused");
    Json(serde_json::json!({"holder": Driver::Human.as_str()})).into_response()
}

/// Human hands back. The agent loop resumes and re-observes before acting.
async fn api_handback(
    axum::extract::State(s): axum::extract::State<AppState>,
) -> impl IntoResponse {
    s.lease.hand_back();
    tracing::info!("handback: agent holds the desktop, loop resumes");
    Json(serde_json::json!({"holder": Driver::Agent.as_str()})).into_response()
}

fn frac(v: &serde_json::Value) -> Option<f64> {
    v.as_f64().filter(|f| f.is_finite())
}

fn err(status: StatusCode, msg: impl Into<String>) -> axum::response::Response {
    (status, Json(serde_json::json!({"error": msg.into()}))).into_response()
}

/// Live-view input while the lease is human-held. Coordinates arrive as
/// fractions of the live image (0..1); the adapter maps them onto a fresh
/// frame, so X11 pixels, CDP viewports and the auto router all work without
/// the UI knowing the current backend or its size. Anything else is 409:
/// agent steps keep their exclusive input while the agent holds the lease.
async fn api_input(
    axum::extract::State(s): axum::extract::State<AppState>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    if s.lease.holder() != Driver::Human {
        return err(
            StatusCode::CONFLICT,
            "agent holds the desktop - take control first",
        );
    }
    let control = match &s.control {
        Some(c) => Arc::clone(c),
        None => {
            return err(
                StatusCode::SERVICE_UNAVAILABLE,
                "takeover input not configured",
            );
        }
    };
    let kind = body.get("kind").and_then(|v| v.as_str()).unwrap_or("");
    let result = match kind {
        "click" | "double_click" => {
            let (Some(x), Some(y)) = (body.get("x").and_then(frac), body.get("y").and_then(frac))
            else {
                return err(StatusCode::BAD_REQUEST, "click needs x/y fractions 0..1");
            };
            control.click_frac(x, y, kind == "double_click").await
        }
        "type" => match body.get("text").and_then(|v| v.as_str()) {
            Some(t) if !t.is_empty() && t.chars().count() <= 200 => control.type_text(t).await,
            _ => {
                return err(StatusCode::BAD_REQUEST, "type needs 1..200 chars of text");
            }
        },
        "key" => match body.get("keys").and_then(|v| v.as_array()) {
            Some(keys) if !keys.is_empty() && keys.len() <= 4 => {
                let keys: Vec<String> = keys
                    .iter()
                    .filter_map(|k| k.as_str().map(str::to_string))
                    .collect();
                if keys.is_empty() {
                    return err(StatusCode::BAD_REQUEST, "key needs 1..4 key names");
                }
                control.press_keys(keys).await
            }
            _ => {
                return err(StatusCode::BAD_REQUEST, "key needs 1..4 key names");
            }
        },
        "scroll" => {
            let (Some(x), Some(y), Some(dy)) = (
                body.get("x").and_then(frac),
                body.get("y").and_then(frac),
                body.get("dy").and_then(frac),
            ) else {
                return err(StatusCode::BAD_REQUEST, "scroll needs x/y/dy fractions");
            };
            control.scroll_frac(x, y, dy).await
        }
        _ => {
            return err(
                StatusCode::BAD_REQUEST,
                "kind: click | double_click | type | key | scroll",
            )
        }
    };
    match result {
        Ok(()) => Json(serde_json::json!({"ok": true})).into_response(),
        Err(e) => err(StatusCode::BAD_GATEWAY, e),
    }
}

/// GET /api/config — the desired config (what the Setup pane edits).
/// Shape: `{config: RoseConfig, source}`. Accepts no input.
async fn api_config_get(
    axum::extract::State(s): axum::extract::State<AppState>,
) -> impl IntoResponse {
    match &s.config {
        None => err(
            StatusCode::NOT_FOUND,
            "no /api/config yet (config backend not wired)",
        ),
        Some(c) => {
            let cfg = c.get().await;
            Json(serde_json::json!({"config": cfg, "source": c.source_name()})).into_response()
        }
    }
}

/// PUT /api/config — validate + persist + hot-reload policy/models.
/// Accepts the nested RoseConfig shape the UI sends (or `{config: {...}}`).
/// Backend/display/size/bind/data/cdp changes report `restart_needed: true`.
async fn api_config_put(
    axum::extract::State(s): axum::extract::State<AppState>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let store = match &s.config {
        None => {
            return err(
                StatusCode::NOT_FOUND,
                "no /api/config yet (config backend not wired)",
            );
        }
        Some(c) => c.clone(),
    };
    let inner = body.get("config").unwrap_or(&body);
    let cfg: RoseConfig = match serde_json::from_value(inner.clone()) {
        Ok(c) => c,
        Err(e) => return err(StatusCode::BAD_REQUEST, format!("invalid config: {e}")),
    };
    if let Err(e) = validate_config(&cfg) {
        return err(StatusCode::BAD_REQUEST, e);
    }
    match store.set(cfg).await {
        Ok((applied, restart_needed)) => Json(serde_json::json!({
            "config": applied,
            "restart_needed": restart_needed,
            "hot_reloaded": ["policy", "models"],
            "source": store.source_name(),
        }))
        .into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, e),
    }
}

const MAX_VIDEO_BYTES: usize = 200 * 1024 * 1024;

fn video_mime(name: &str, headers: &HeaderMap) -> String {
    if let Some(ct) = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
    {
        let ct = ct.split(';').next().unwrap_or("").trim().to_string();
        if !ct.is_empty() {
            return ct;
        }
    }
    let lower = name.to_ascii_lowercase();
    if lower.ends_with(".mp4") {
        "video/mp4".to_string()
    } else if lower.ends_with(".webm") {
        "video/webm".to_string()
    } else if lower.ends_with(".mov") {
        "video/quicktime".to_string()
    } else if lower.ends_with(".mkv") {
        "video/x-matroska".to_string()
    } else {
        "video/mp4".to_string()
    }
}

/// GET /api/videos — list uploads, newest first.
async fn api_videos_list(
    axum::extract::State(s): axum::extract::State<AppState>,
) -> impl IntoResponse {
    match &s.videos {
        None => err(
            StatusCode::NOT_FOUND,
            "no /api/videos yet (video backend not wired)",
        ),
        Some(v) => Json(serde_json::json!({"videos": v.list().await})).into_response(),
    }
}

/// POST /api/videos?filename=foo.mp4 — raw bytes body (no multipart dep).
/// The UI sends one file per request; the filename query carries the name.
async fn api_videos_upload(
    axum::extract::State(s): axum::extract::State<AppState>,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    let store = match &s.videos {
        None => {
            return err(
                StatusCode::NOT_FOUND,
                "no /api/videos yet (video backend not wired)",
            );
        }
        Some(v) => v.clone(),
    };
    let raw = q
        .get("filename")
        .or_else(|| q.get("name"))
        .cloned()
        .unwrap_or_else(|| "video.mp4".to_string());
    let name = sanitize_video_name(&raw);
    let lower = name.to_ascii_lowercase();
    if ![".mp4", ".webm", ".mov", ".mkv"]
        .iter()
        .any(|ext| lower.ends_with(ext))
    {
        return err(
            StatusCode::BAD_REQUEST,
            "only mp4 / webm / mov / mkv uploads",
        );
    }
    if body.is_empty() {
        return err(StatusCode::BAD_REQUEST, "empty upload");
    }
    if body.len() > MAX_VIDEO_BYTES {
        return err(
            StatusCode::PAYLOAD_TOO_LARGE,
            "video too large (max 200 MB)",
        );
    }
    let mime = video_mime(&name, &headers);
    match store.upload(&name, &mime, body.to_vec()).await {
        Ok(meta) => Json(serde_json::json!({
            "video": meta,
            "videos": store.list().await,
        }))
        .into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, e),
    }
}

async fn api_video_get(
    axum::extract::State(s): axum::extract::State<AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> impl IntoResponse {
    match &s.videos {
        None => err(
            StatusCode::NOT_FOUND,
            "no /api/videos yet (video backend not wired)",
        ),
        Some(v) => match v.detail(&id).await {
            Some(d) => Json(serde_json::json!({"video": d.meta, "steps": d.steps})).into_response(),
            None => err(StatusCode::NOT_FOUND, "unknown video"),
        },
    }
}

async fn api_video_delete(
    axum::extract::State(s): axum::extract::State<AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> impl IntoResponse {
    match &s.videos {
        None => err(
            StatusCode::NOT_FOUND,
            "no /api/videos yet (video backend not wired)",
        ),
        Some(v) => {
            if v.remove(&id).await {
                Json(serde_json::json!({"deleted": true})).into_response()
            } else {
                err(StatusCode::NOT_FOUND, "unknown video")
            }
        }
    }
}

/// GET /api/videos/:id/file — the original bytes for the <video> player.
async fn api_video_file(
    axum::extract::State(s): axum::extract::State<AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> impl IntoResponse {
    match &s.videos {
        None => err(
            StatusCode::NOT_FOUND,
            "no /api/videos yet (video backend not wired)",
        ),
        Some(v) => match v.file(&id).await {
            Some((bytes, mime)) => (
                [
                    (header::CONTENT_TYPE, mime),
                    (header::CACHE_CONTROL, "no-store".to_string()),
                ],
                bytes,
            )
                .into_response(),
            None => err(StatusCode::NOT_FOUND, "unknown video"),
        },
    }
}

/// POST /api/videos/:id/instructions — draft steps from the video.
/// Muse Spark reads the video (frames + narration) natively via the
/// Files + Responses APIs — no SAM stage, no separate transcription.
/// The draft is persisted next to the upload (`steps.json`) so
/// Save/Send work against the backend.
async fn api_video_instructions(
    axum::extract::State(s): axum::extract::State<AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> impl IntoResponse {
    match &s.videos {
        None => err(
            StatusCode::NOT_FOUND,
            "no /api/videos yet (video backend not wired)",
        ),
        Some(v) => match v.generate(&id).await {
            Ok(d) => {
                let avg: f32 = if d.steps.is_empty() {
                    0.0
                } else {
                    d.steps.iter().map(|st| st.conf).sum::<f32>() / d.steps.len() as f32
                };
                Json(serde_json::json!({
                    "video_id": d.meta.id,
                    "steps": d.steps,
                    "avg_confidence": avg,
                    "stages": ["upload", "spark"],
                }))
                .into_response()
            }
            // Unknown id is 404; a missing key is 400 (fixable in Setup);
            // anything else is the model/upstream failing (502), never a
            // silent mock — the UI surfaces the message as-is.
            Err(e) => {
                let status = if e.contains("unknown video") {
                    StatusCode::NOT_FOUND
                } else if e.contains("API key") {
                    StatusCode::BAD_REQUEST
                } else {
                    StatusCode::BAD_GATEWAY
                };
                err(status, e)
            }
        },
    }
}

/// PUT/POST /api/videos/:id/draft `{steps: [...]}` — persist edited steps.
async fn api_video_draft(
    axum::extract::State(s): axum::extract::State<AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let store = match &s.videos {
        None => {
            return err(
                StatusCode::NOT_FOUND,
                "no /api/videos yet (video backend not wired)",
            );
        }
        Some(v) => v.clone(),
    };
    let steps_value = body.get("steps").unwrap_or(&body);
    let steps: Vec<VideoStep> = match serde_json::from_value(steps_value.clone()) {
        Ok(st) => st,
        Err(e) => return err(StatusCode::BAD_REQUEST, format!("invalid steps: {e}")),
    };
    if steps.len() > 100 {
        return err(StatusCode::BAD_REQUEST, "too many steps (max 100)");
    }
    match store.save_draft(&id, steps).await {
        Ok(d) => Json(serde_json::json!({"saved": true, "video": d.meta, "steps": d.steps}))
            .into_response(),
        Err(e) => err(StatusCode::NOT_FOUND, e),
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

async fn api_run_grounded_frame(
    axum::extract::State(s): axum::extract::State<AppState>,
    axum::extract::Path((id, step)): axum::extract::Path<(String, u32)>,
) -> impl IntoResponse {
    match &s.traces {
        None => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"error": "trace store not configured"})),
        )
            .into_response(),
        Some(t) => match t.grounded_frame(&id, step).await {
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
                Json(serde_json::json!({"error": "no overlay for this step"})),
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
            lease: Lease::new(),
            control: None,
            config: None,
            videos: None,
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
        request_bytes(
            port,
            method,
            path,
            "application/json",
            body.map(str::as_bytes).unwrap_or(&[]),
        )
        .await
    }

    async fn request_bytes(
        port: u16,
        method: &str,
        path: &str,
        content_type: &str,
        payload: &[u8],
    ) -> (String, Vec<u8>) {
        let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .expect("connect");
        let req = format!(
            "{method} {path} HTTP/1.1\r\nHost: x\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            payload.len()
        );
        stream.write_all(req.as_bytes()).await.expect("write");
        stream.write_all(payload).await.expect("write body");
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
            _auto_approve: bool,
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
        // No overlay saved in the stub store: grounded frame is 404, but
        // the route exists (proves wiring without a fixture image).
        let (head, _) = get(port, "/api/runs/run-1/frames/2/grounded").await;
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

    struct StubControl {
        log: std::sync::Mutex<Vec<String>>,
    }
    #[async_trait]
    impl HumanInput for StubControl {
        async fn click_frac(&self, x: f64, y: f64, double: bool) -> Result<(), String> {
            self.log
                .lock()
                .expect("log")
                .push(format!("click {x:.2} {y:.2} {double}"));
            Ok(())
        }
        async fn type_text(&self, text: &str) -> Result<(), String> {
            self.log.lock().expect("log").push(format!("type {text}"));
            Ok(())
        }
        async fn press_keys(&self, keys: Vec<String>) -> Result<(), String> {
            self.log
                .lock()
                .expect("log")
                .push(format!("key {}", keys.join("+")));
            Ok(())
        }
        async fn scroll_frac(&self, x: f64, y: f64, dy_frac: f64) -> Result<(), String> {
            self.log
                .lock()
                .expect("log")
                .push(format!("scroll {x:.2} {y:.2} {dy_frac:.2}"));
            Ok(())
        }
    }

    /// Takeover flips the lease; input works only while human-held.
    #[tokio::test]
    async fn takeover_gates_input_and_handback_restores() {
        let control = Arc::new(StubControl {
            log: std::sync::Mutex::new(Vec::new()),
        });
        let mut st = state(None);
        st.control = Some(control.clone());
        let port = spawn(st).await;

        // Agent holds it: input is refused, takeover flips, handback restores.
        let (head, body) = get(port, "/api/status").await;
        assert!(head.contains("200 OK"), "{head}");
        let v: serde_json::Value = serde_json::from_slice(&body).expect("json");
        assert_eq!(v["lease"], "agent");

        let (head, _) = post(
            port,
            "/desktop/input",
            r#"{"kind":"click","x":0.5,"y":0.5}"#,
        )
        .await;
        assert!(head.contains("409"), "{head}");

        let (head, body) = post(port, "/desktop/takeover", "{}").await;
        assert!(head.contains("200 OK"), "{head}");
        let v: serde_json::Value = serde_json::from_slice(&body).expect("json");
        assert_eq!(v["holder"], "human");

        let (head, body) = get(port, "/api/status").await;
        assert!(head.contains("200 OK"), "{head}");
        let v: serde_json::Value = serde_json::from_slice(&body).expect("json");
        assert_eq!(v["lease"], "human");

        let (head, _) = post(
            port,
            "/desktop/input",
            r#"{"kind":"click","x":0.25,"y":0.75}"#,
        )
        .await;
        assert!(head.contains("200 OK"), "{head}");
        let (head, _) = post(
            port,
            "/desktop/input",
            r#"{"kind":"key","keys":["Escape"]}"#,
        )
        .await;
        assert!(head.contains("200 OK"), "{head}");
        // Bad shapes are 400, never dispatched.
        let (head, _) = post(port, "/desktop/input", r#"{"kind":"click"}"#).await;
        assert!(head.contains("400"), "{head}");
        let (head, _) = post(port, "/desktop/input", r#"{"kind":"nope"}"#).await;
        assert!(head.contains("400"), "{head}");
        assert_eq!(
            *control.log.lock().expect("log"),
            vec!["click 0.25 0.75 false", "key Escape"],
        );

        let (head, body) = post(port, "/desktop/handback", "{}").await;
        assert!(head.contains("200 OK"), "{head}");
        let v: serde_json::Value = serde_json::from_slice(&body).expect("json");
        assert_eq!(v["holder"], "agent");
        let (head, _) = post(
            port,
            "/desktop/input",
            r#"{"kind":"click","x":0.5,"y":0.5}"#,
        )
        .await;
        assert!(head.contains("409"), "{head}");
    }

    /// No control adapter: takeover reports 503 instead of flipping.
    #[tokio::test]
    async fn takeover_needs_a_control_adapter() {
        let port = spawn(state(None)).await;
        let (head, _) = post(port, "/desktop/takeover", "{}").await;
        assert!(head.contains("503"), "{head}");
        let (head, _) = post(port, "/desktop/handback", "{}").await;
        assert!(head.contains("200 OK"), "{head}");
    }

    struct StubConfig {
        current: std::sync::Mutex<rose_core::RoseConfig>,
    }
    #[async_trait]
    impl ConfigSource for StubConfig {
        async fn get(&self) -> rose_core::RoseConfig {
            self.current.lock().expect("cfg").clone()
        }
        async fn set(
            &self,
            cfg: rose_core::RoseConfig,
        ) -> Result<(rose_core::RoseConfig, bool), String> {
            let old = self.current.lock().expect("cfg").clone();
            let restart = config_restart_needed(&old, &cfg);
            *self.current.lock().expect("cfg") = cfg.clone();
            Ok((cfg, restart))
        }
    }

    async fn put(port: u16, path: &str, body: &str) -> (String, Vec<u8>) {
        request(port, "PUT", path, Some(body)).await
    }

    #[tokio::test]
    async fn config_get_put_roundtrip_with_validation() {
        // No source: 404 so the UI falls back to its local draft.
        let port = spawn(state(None)).await;
        let (head, _) = get(port, "/api/config").await;
        assert!(head.contains("404"), "{head}");

        let mut st = state(None);
        st.config = Some(Arc::new(StubConfig {
            current: std::sync::Mutex::new(rose_core::RoseConfig::default()),
        }));
        let port = spawn(st).await;
        let (_, body) = get(port, "/api/config").await;
        let v: serde_json::Value = serde_json::from_slice(&body).expect("json");
        assert_eq!(v["config"]["desktop"]["backend"], "x11");

        // Bad bind is 400, never applied.
        let (head, _) = put(
            port,
            "/api/config",
            r#"{"server":{"bind":"nope","allow_remote":false},"desktop":{"backend":"fake","display":":1","width":1600,"height":900},"models":{"brain":"fake","endpoint":"https://api.meta.ai/v1","model":"m"},"policy":{"max_steps":40,"max_minutes":15,"max_model_calls":60,"require_approval_for":[]},"data":{"dir":"/tmp/rose"},"cdp":{"endpoint":"http://127.0.0.1:9222"}}"#,
        )
        .await;
        assert!(head.contains("400"), "{head}");
        // Non-loopback bind without opt-in is refused.
        let (head, _) = put(
            port,
            "/api/config",
            r#"{"server":{"bind":"0.0.0.0:8787","allow_remote":false},"desktop":{"backend":"fake","display":":1","width":1600,"height":900},"models":{"brain":"fake","endpoint":"https://api.meta.ai/v1","model":"m"},"policy":{"max_steps":40,"max_minutes":15,"max_model_calls":60,"require_approval_for":[]},"data":{"dir":"/tmp/rose"},"cdp":{"endpoint":"http://127.0.0.1:9222"}}"#,
        )
        .await;
        assert!(head.contains("400"), "{head}");

        // Policy-only change: hot-reloads, no restart.
        let (head, body) = put(
            port,
            "/api/config",
            r#"{"server":{"bind":"127.0.0.1:8787","allow_remote":false},"desktop":{"backend":"x11","display":":1","width":1600,"height":900},"models":{"brain":"muse","endpoint":"https://api.meta.ai/v1","model":"muse-spark-1.3-contributor"},"policy":{"max_steps":12,"max_minutes":15,"max_model_calls":60,"require_approval_for":["pay"]},"data":{"dir":"~/.local/share/rose"},"cdp":{"endpoint":"http://127.0.0.1:9222"}}"#,
        )
        .await;
        assert!(head.contains("200 OK"), "{head}");
        let v: serde_json::Value = serde_json::from_slice(&body).expect("json");
        assert_eq!(v["restart_needed"], false);
        assert_eq!(v["config"]["policy"]["max_steps"], 12);

        // Backend switch: accepted but flags restart.
        let (head, body) = put(
            port,
            "/api/config",
            r#"{"server":{"bind":"127.0.0.1:8787","allow_remote":false},"desktop":{"backend":"fake","display":":1","width":1600,"height":900},"models":{"brain":"fake","endpoint":"https://api.meta.ai/v1","model":"muse-spark-1.3-contributor"},"policy":{"max_steps":12,"max_minutes":15,"max_model_calls":60,"require_approval_for":["pay"]},"data":{"dir":"~/.local/share/rose"},"cdp":{"endpoint":"http://127.0.0.1:9222"}}"#,
        )
        .await;
        assert!(head.contains("200 OK"), "{head}");
        let v: serde_json::Value = serde_json::from_slice(&body).expect("json");
        assert_eq!(v["restart_needed"], true);
    }

    #[test]
    fn config_validation_rejects_bad_values() {
        let mut cfg = rose_core::RoseConfig::default();
        assert!(validate_config(&cfg).is_ok());
        cfg.policy.max_steps = 0;
        assert!(validate_config(&cfg).is_err());
        cfg.policy.max_steps = 40;
        cfg.desktop.backend = "nope".to_string();
        assert!(validate_config(&cfg).is_err());
        cfg.desktop.backend = "fake".to_string();
        cfg.server.bind = "0.0.0.0:8787".to_string();
        assert!(validate_config(&cfg).is_err());
        cfg.server.allow_remote = true;
        assert!(validate_config(&cfg).is_ok());
    }

    type StubVideoRow = (VideoMeta, Vec<u8>, Vec<VideoStep>);
    struct StubVideos {
        items: std::sync::Mutex<std::collections::HashMap<String, StubVideoRow>>,
    }
    impl StubVideos {
        fn new() -> Self {
            Self {
                items: std::sync::Mutex::new(std::collections::HashMap::new()),
            }
        }
    }
    #[async_trait]
    impl VideoSource for StubVideos {
        async fn list(&self) -> Vec<VideoMeta> {
            let mut v: Vec<VideoMeta> = self
                .items
                .lock()
                .expect("v")
                .values()
                .map(|(m, _, _)| m.clone())
                .collect();
            v.sort_by(|a, b| b.created_at.cmp(&a.created_at));
            v
        }
        async fn upload(
            &self,
            name: &str,
            mime: &str,
            bytes: Vec<u8>,
        ) -> Result<VideoMeta, String> {
            let id = format!("v{}", self.items.lock().expect("v").len() + 1);
            let meta = VideoMeta {
                id: id.clone(),
                name: name.to_string(),
                size: bytes.len() as u64,
                mime: mime.to_string(),
                created_at: "2026-01-01T00:00:00Z".to_string(),
                status: "ready".to_string(),
                steps: 0,
            };
            self.items
                .lock()
                .expect("v")
                .insert(id, (meta.clone(), bytes, Vec::new()));
            Ok(meta)
        }
        async fn file(&self, id: &str) -> Option<(Vec<u8>, String)> {
            self.items
                .lock()
                .expect("v")
                .get(id)
                .map(|(m, b, _)| (b.clone(), m.mime.clone()))
        }
        async fn detail(&self, id: &str) -> Option<VideoDetail> {
            self.items
                .lock()
                .expect("v")
                .get(id)
                .map(|(m, _, s)| VideoDetail {
                    meta: m.clone(),
                    steps: s.clone(),
                })
        }
        async fn generate(&self, id: &str) -> Result<VideoDetail, String> {
            // Test double for the endpoint plumbing: returns one fixed
            // step so tests never depend on a model. Production
            // (apps/rosed) always calls Spark and errors without a key.
            let mut lock = self.items.lock().expect("v");
            let (meta, _, steps) = lock.get_mut(id).ok_or("unknown video".to_string())?;
            *steps = vec![VideoStep {
                ts: 0,
                say: "stub step (tests only)".to_string(),
                intent: "stub step (tests only)".to_string(),
                params: String::new(),
                approval: false,
                conf: 1.0,
                quote: String::new(),
            }];
            meta.steps = steps.len();
            meta.status = "ready".to_string();
            Ok(VideoDetail {
                meta: meta.clone(),
                steps: steps.clone(),
            })
        }
        async fn save_draft(&self, id: &str, steps: Vec<VideoStep>) -> Result<VideoDetail, String> {
            let mut lock = self.items.lock().expect("v");
            let (meta, _, saved) = lock.get_mut(id).ok_or("unknown video".to_string())?;
            *saved = steps;
            meta.steps = saved.len();
            Ok(VideoDetail {
                meta: meta.clone(),
                steps: saved.clone(),
            })
        }
        async fn remove(&self, id: &str) -> bool {
            self.items.lock().expect("v").remove(id).is_some()
        }
    }

    #[tokio::test]
    async fn videos_list_upload_instructions_draft_roundtrip() {
        // No source: 404 so the UI stays in local-only mode.
        let port = spawn(state(None)).await;
        let (head, _) = get(port, "/api/videos").await;
        assert!(head.contains("404"), "{head}");

        let mut st = state(None);
        st.videos = Some(Arc::new(StubVideos::new()));
        let port = spawn(st).await;
        let (_, body) = get(port, "/api/videos").await;
        let v: serde_json::Value = serde_json::from_slice(&body).expect("json");
        assert_eq!(v["videos"], serde_json::json!([]));

        // Upload raw bytes with ?filename=.
        let (head, body) = request(
            port,
            "POST",
            "/api/videos?filename=demo.mp4",
            Some("0123456789"),
        )
        .await;
        assert!(head.contains("200 OK"), "{head}");
        let v: serde_json::Value = serde_json::from_slice(&body).expect("json");
        let id = v["video"]["id"].as_str().expect("id").to_string();
        assert_eq!(v["video"]["name"], "demo.mp4");
        assert_eq!(v["video"]["size"], 10);

        // File serves the same bytes; detail shows no steps yet.
        let (head, body) = get(port, &format!("/api/videos/{id}/file")).await;
        assert!(head.contains("200 OK"), "{head}");
        assert_eq!(body, b"0123456789");
        let (_, body) = get(port, &format!("/api/videos/{id}")).await;
        let v: serde_json::Value = serde_json::from_slice(&body).expect("json");
        assert_eq!(v["steps"], serde_json::json!([]));

        // Bodies over axum's 2 MB default must still upload: every real
        // screen recording exceeds it, and the torn-down connection used
        // to surface in the browser as a bare network error.
        let big = vec![0u8; 3 * 1024 * 1024];
        let (head, body) = request_bytes(
            port,
            "POST",
            "/api/videos?filename=big.mp4",
            "video/mp4",
            &big,
        )
        .await;
        assert!(head.contains("200 OK"), "{head}");
        let v: serde_json::Value = serde_json::from_slice(&body).expect("json");
        assert_eq!(v["video"]["size"], 3 * 1024 * 1024);

        // Generate drafts server-side; unknown ids are 404, not a mock.
        let (head, body) = post(port, &format!("/api/videos/{id}/instructions"), "{}").await;
        assert!(head.contains("200 OK"), "{head}");
        let v: serde_json::Value = serde_json::from_slice(&body).expect("json");
        assert_eq!(v["steps"].as_array().expect("steps").len(), 1);
        let (head, _) = post(port, "/api/videos/v-nonexistent/instructions", "{}").await;
        assert!(head.contains("404"), "{head}");
        let (head, _) = put(port, &format!("/api/videos/{id}/draft"), r#"{"steps": []}"#).await;
        assert!(head.contains("200 OK"), "{head}");
        let (head, _) = request(port, "DELETE", &format!("/api/videos/{id}"), None).await;
        assert!(head.contains("200 OK"), "{head}");
        let (head, _) = get(port, &format!("/api/videos/{id}")).await;
        assert!(head.contains("404"), "{head}");
    }
}
