//! `rosed` composition root: config -> factories -> run.
//!
//! IMPLEMENTATION.md T0.3. CLI owns config loading (file + env for secrets)
//! and selects implementations via factory fns keyed on config. Real
//! backends land in later tasks; unknown selectors fail loudly here.

mod shell;

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use clap::{Parser, Subcommand};
use rose_core::{Action, Actor, DesktopBackend, Element, Policy, RoseConfig, StepSummary, Target};
use rose_desktop_fake::FakeDesktop;
use rose_desktop_x11::X11Desktop;
use rose_models::{resolve_element, FakeBrain, GroundingClient, ModelReply, MuseClient};
use rose_store::TraceStore;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Systemd units are the single source of truth in `deploy/systemd/` and are
/// embedded into the binary, so the VM needs only this one file.
const XVNC_SERVICE: &str = include_str!("../../../deploy/systemd/xvnc.service");
const ROSED_SERVICE: &str = include_str!("../../../deploy/systemd/rosed.service");
const EXAMPLE_CONFIG: &str = include_str!("../../../config/rose.example.toml");

#[derive(Debug, Parser)]
#[command(name = "rosed", about = "Rose agent daemon (one Rust binary)")]
struct Cli {
    /// Path to config TOML. Defaults to ~/.config/rose/config.toml.
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    /// Print the resolved config (secrets redacted) and exit.
    #[arg(long)]
    print_config: bool,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Install on this VM from this one binary: write user units + example
    /// config (never overwrites), enable lingering, (re)start services.
    /// The desktop platform itself (apt, Chrome, VNC server) still comes
    /// from deploy/setup.sh; run that once first on a fresh box.
    Install {
        /// Print what would be done without changing anything.
        #[arg(long)]
        dry_run: bool,
    },
    /// Inspect persisted run traces: list recent runs, or print one
    /// run's full event trace (`rosed runs <run-id>`).
    Runs {
        /// Run id to print the full trace for (omit to list recent runs).
        id: Option<String>,
        /// How many recent runs to list.
        #[arg(long, default_value_t = 20)]
        limit: u32,
    },
    /// Print persisted chat history (user goals + assistant replies).
    Chat {
        /// How many recent messages to print.
        #[arg(long, default_value_t = 50)]
        limit: u32,
    },
}

/// Secrets come from env or the web Setup (server memory only), never from
/// files. Kept out of `RoseConfig` (and out of `Debug` output) so they can't
/// leak into logs or traces.
#[derive(Debug, Default)]
struct ApiKeys {
    muse_api_key: Option<String>,
}

impl ApiKeys {
    fn from_env() -> Self {
        Self {
            muse_api_key: std::env::var("ROSE_MUSE_API_KEY")
                .ok()
                .filter(|s| !s.is_empty()),
        }
    }

    /// `<set>` / `<missing>` — the value itself is never printed.
    fn muse_status(&self) -> &'static str {
        if self.muse_api_key.is_some() {
            "<set>"
        } else {
            "<missing>"
        }
    }
}

/// Which planner answers. Muse needs a key at step time (env or web Setup);
/// fake is the scripted offline double (tests / no-key demos).
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BrainSvc {
    Muse,
    Fake,
}

impl From<&str> for BrainSvc {
    fn from(s: &str) -> Self {
        match s {
            "fake" => BrainSvc::Fake,
            _ => BrainSvc::Muse,
        }
    }
}

/// Composition-root adapter: persisted traces behind the web seam.
struct TraceAdapter {
    store: Arc<TraceStore>,
}

#[async_trait]
impl rose_web::TraceSource for TraceAdapter {
    async fn runs(&self, limit: u32) -> Vec<serde_json::Value> {
        self.store
            .list(limit)
            .into_iter()
            .filter_map(|s| serde_json::to_value(s).ok())
            .collect()
    }

    async fn run(&self, run_id: &str) -> Option<serde_json::Value> {
        let summary = self.store.summary(run_id)?;
        let events = self.store.events(run_id)?;
        Some(serde_json::json!({
            "summary": serde_json::to_value(summary).ok()?,
            "events": events,
        }))
    }

    async fn frame(&self, run_id: &str, step: u32) -> Option<Vec<u8>> {
        std::fs::read(self.store.frame_path(run_id, step)?).ok()
    }

    async fn chat(&self, limit: u32) -> Vec<serde_json::Value> {
        self.store
            .chat(limit)
            .into_iter()
            .filter_map(|m| serde_json::to_value(m).ok())
            .collect()
    }

    async fn chats(&self, limit: u32) -> Vec<serde_json::Value> {
        self.store
            .chats(limit)
            .into_iter()
            .filter_map(|c| serde_json::to_value(c).ok())
            .collect()
    }

    async fn chat_runs(&self, chat_id: &str) -> Option<Vec<serde_json::Value>> {
        let runs = self.store.runs_for_chat(chat_id);
        if runs.is_empty() {
            return None;
        }
        Some(
            runs.into_iter()
                .filter_map(|r| serde_json::to_value(r).ok())
                .collect(),
        )
    }

    async fn rename_chat(&self, chat_id: &str, title: &str) -> bool {
        self.store.rename_chat(chat_id, title)
    }

    async fn delete_chat(&self, chat_id: &str) -> bool {
        self.store.delete_chat(chat_id)
    }
}

/// Narrow adapter: composition root (the one place allowed to touch
/// backends) exposing snapshots to the web layer. Input injection stays
/// out until the T1.4 takeover lease gates it.
struct BackendSource {
    backend: Arc<dyn DesktopBackend>,
    display: String,
    width: u32,
    height: u32,
}

#[async_trait]
impl rose_web::FrameSource for BackendSource {
    async fn jpeg(&self) -> std::result::Result<Vec<u8>, String> {
        self.backend
            .capture()
            .await
            .map(|f| f.jpeg)
            .map_err(|e| e.to_string())
    }

    fn display(&self) -> String {
        self.display.clone()
    }

    fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }
}

/// Everything the daemon needs, built once at startup.
#[allow(dead_code)] // brain/store selectors are read via config today
struct Services {
    config: RoseConfig,
    api_keys: ApiKeys,
    desktop: Arc<dyn DesktopBackend>,
    brain: BrainSvc,
    traces: Arc<TraceStore>,
}

fn default_config_path() -> Option<PathBuf> {
    std::env::var("HOME")
        .ok()
        .map(|h| PathBuf::from(h).join(".config/rose/config.toml"))
}

/// Load TOML file (if present) + env secrets. Missing file = built-in default.
fn load_config(path_override: Option<PathBuf>) -> Result<(RoseConfig, ApiKeys)> {
    let path = path_override.or_else(default_config_path);
    let config = match path {
        Some(p) if p.exists() => {
            let text =
                std::fs::read_to_string(&p).with_context(|| format!("reading {}", p.display()))?;
            toml::from_str(&text).with_context(|| format!("parsing {}", p.display()))?
        }
        _ => RoseConfig::default(),
    };
    Ok((config, ApiKeys::from_env()))
}

fn expand_tilde(dir: &str) -> PathBuf {
    if let Some(rest) = dir.strip_prefix("~/") {
        match std::env::var("HOME") {
            Ok(h) => PathBuf::from(h).join(rest),
            Err(_) => PathBuf::from(dir),
        }
    } else {
        PathBuf::from(dir)
    }
}

fn make_desktop(config: &RoseConfig) -> Result<Arc<dyn DesktopBackend>> {
    match config.desktop.backend.as_str() {
        "fake" => Ok(Arc::new(FakeDesktop::new())),
        "x11" => Ok(Arc::new(X11Desktop::new(config.desktop.display.clone()))),
        other => bail!("unknown desktop backend: {other} (want \"x11\" | \"fake\")"),
    }
}

fn make_brain(config: &RoseConfig) -> Result<BrainSvc> {
    match config.models.brain.as_str() {
        "fake" => Ok(BrainSvc::Fake),
        // Muse builds without a key: the key is resolved per step from the
        // web Setup (server memory) or ROSE_MUSE_API_KEY, so the daemon can
        // start first and receive the key later via the UI.
        "muse" => Ok(BrainSvc::Muse),
        other => bail!("unknown brain: {other} (want \"muse\" | \"fake\")"),
    }
}

fn make_store(config: &RoseConfig) -> Arc<TraceStore> {
    Arc::new(TraceStore::new(expand_tilde(&config.data.dir)))
}

fn build_services(config: RoseConfig, api_keys: ApiKeys) -> Result<Services> {
    // Guard: never serve remotely without an explicit opt-in.
    if config.server.allow_remote {
        eprintln!("WARNING: allow_remote = true: UI/API is reachable beyond localhost");
    }
    Ok(Services {
        desktop: make_desktop(&config)?,
        brain: make_brain(&config)?,
        traces: make_store(&config),
        config,
        api_keys,
    })
}

/// Prototype Governor-lite (T2.2 builds the real one): irreversible-sounding
/// intent needs approval before anything moves.
fn needs_approval(policy: &Policy, intent: &str) -> bool {
    let lower = intent.to_lowercase();
    policy
        .require_approval_for
        .iter()
        .any(|k| !k.is_empty() && lower.contains(&k.to_lowercase()))
}

/// The Phase-1 loop (§6): capture -> brain -> gate -> act -> wait -> repeat
/// until the brain replies done/ask/fail, an action needs approval, the
/// same action repeats (stuck), or the step budget runs out.
///
/// This used to be a single step with an always-empty history, so a goal
/// like "open the browser" got exactly one click (which only *selects* a
/// desktop icon) and then stopped while narrating success. Looping with
/// real history is what lets the model see that nothing opened and try
/// the next route (double-click, menu search, wait for launch).
struct Stepper {
    desktop: Arc<dyn DesktopBackend>,
    brain: BrainSvc,
    fake: FakeBrain,
    keys: rose_web::KeySlot,
    env_key: Option<String>,
    endpoint: String,
    model: String,
    grounding_backend: String,
    grounding_endpoint: String,
    grounding_min_conf: u32,
    policy: Policy,
    traces: Arc<TraceStore>,
    shell: shell::ShellRunner,
    /// Set from the web UI (POST /api/stop); polled at every step
    /// checkpoint and while waiting on the brain or a shell command.
    stop_flag: Arc<std::sync::atomic::AtomicBool>,
    /// True while a run is in flight (also serialises runs: the stop
    /// flag's meaning is only well-defined for one run at a time).
    active: Arc<std::sync::atomic::AtomicBool>,
}

/// Shell commands need approval when they look destructive OR when
/// their narration trips the policy keywords — same rule as UI
/// actions, but the command text counts too, not just the `say`.
fn shell_needs_approval(policy: &Policy, say: &str, command: &str) -> bool {
    needs_approval(policy, say)
        || needs_approval(policy, command)
        || shell::looks_dangerous(command)
}

/// RAII: mark the stepper idle however the run exits.
struct ActiveGuard(Arc<std::sync::atomic::AtomicBool>);
impl Drop for ActiveGuard {
    fn drop(&mut self) {
        self.0.store(false, std::sync::atomic::Ordering::SeqCst);
    }
}

/// Await `fut`, but abandon it (returning `None`) as soon as the stop
/// flag is set. Used around the brain call and shell commands — the
/// two places a run can sit waiting for many seconds — so Stop feels
/// instant instead of "after the current 30s timeout".
async fn unless_stopped<T>(
    flag: &std::sync::atomic::AtomicBool,
    fut: impl std::future::Future<Output = T>,
) -> Option<T> {
    tokio::pin!(fut);
    loop {
        tokio::select! {
            value = &mut fut => return Some(value),
            _ = tokio::time::sleep(std::time::Duration::from_millis(200)) => {
                if flag.load(std::sync::atomic::Ordering::SeqCst) {
                    return None;
                }
            }
        }
    }
}

impl Stepper {
    /// After an action, wait for the desktop to settle and return the
    /// settled frame for the next brain call: poll until two captures
    /// agree (max ~3s). This replaces both the old fixed 1s sleep and
    /// standalone "wait" replies — an app launch is simply finished
    /// by the time the brain next looks, so it can go straight to the
    /// next real action instead of spending a step on waiting.
    async fn settled_frame(&self) -> Option<rose_core::Frame> {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let mut prev = self.desktop.capture().await.ok()?;
        for _ in 0..8 {
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            let frame = match self.desktop.capture().await {
                Ok(f) => f,
                Err(_) => return Some(prev),
            };
            if frame.jpeg == prev.jpeg {
                return Some(frame);
            }
            prev = frame;
        }
        Some(prev)
    }
}

fn step_view_steps(
    say: &str,
    acted: bool,
    approval_needed: bool,
    done: bool,
    steps: u32,
) -> serde_json::Value {
    let mut view = rose_web::step_view(say, acted, approval_needed, done);
    view["steps"] = serde_json::json!(steps);
    view
}

fn target_str(target: &Target) -> String {
    match target {
        Target::Point { x, y } => format!("({x}, {y})"),
        Target::Element(id) => format!("element {id}"),
    }
}

/// One-line, trace-safe description of an action for the chat feed.
/// Typed text is shown as a length only — secrets typed into fields
/// must never land in traces (IMPLEMENTATION.md rule 7).
pub fn describe_action(action: &Action) -> String {
    match action {
        Action::Click { target, button } => {
            format!("click {} [{button:?}]", target_str(target))
        }
        Action::DoubleClick { target } => format!("double-click {}", target_str(target)),
        Action::Type { text } => format!("type {} chars (text hidden)", text.chars().count()),
        Action::PressKey { keys } => format!("press {}", keys.join("+")),
        Action::Scroll { target, dy } => {
            format!("scroll dy={dy} at {}", target_str(target))
        }
        Action::Drag { from, to } => {
            format!("drag {} -> {}", target_str(from), target_str(to))
        }
        Action::Wait { ms } => format!("wait {ms}ms"),
    }
}

#[async_trait]
impl rose_web::StepRunner for Stepper {
    async fn run_step(
        &self,
        goal: &str,
        force: bool,
        chat_id: Option<&str>,
        tx: &rose_web::StepEventSender,
    ) -> Result<serde_json::Value, String> {
        use std::sync::atomic::Ordering;
        if self.active.swap(true, Ordering::SeqCst) {
            return Err(
                "a run is already in progress — stop it before starting another".to_string(),
            );
        }
        let _run_guard = ActiveGuard(Arc::clone(&self.active));
        self.stop_flag.store(false, Ordering::SeqCst);
        // Every phase is emitted as it happens so the chat can narrate
        // the run live — and the identical event is appended to the
        // persisted trace (`rose_store`), so the run stays observable
        // after the browser closes or rosed restarts: what was
        // observed, what the brain planned, what was done, what the
        // next screenshot showed, and exactly where anything failed.
        // Prior turns of this chat (if continuing one) become the
        // brain's opening context, so "now do the same for March"
        // works across messages instead of starting amnesiac.
        let prior_turns: Vec<StepSummary> = match chat_id {
            Some(id) => self
                .traces
                .chat_turns(id, 3)
                .into_iter()
                .enumerate()
                .map(|(i, (past_goal, reply))| StepSummary {
                    idx: i as u32,
                    intent: format!(
                        "earlier in this chat the user asked {past_goal:?} and Rose replied {:?}",
                        reply.unwrap_or_else(|| "(no reply recorded)".to_string())
                    ),
                    say: format!("Earlier: {past_goal}"),
                })
                .collect(),
            None => Vec::new(),
        };
        let mut trace = match self.traces.begin(goal, chat_id) {
            Ok(run) => Some(run),
            Err(e) => {
                tracing::warn!(error = %e, "could not start trace; run will not be persisted");
                None
            }
        };
        let run_id = trace
            .as_ref()
            .map(|t| t.run_id().to_string())
            .unwrap_or_default();
        let chat_id = trace
            .as_ref()
            .map(|t| t.chat_id().to_string())
            .unwrap_or_default();
        tracing::info!(run_id = %run_id, goal = %goal, "run started");
        macro_rules! emit {
            ($event:expr) => {{
                let event = $event;
                let event = match &mut trace {
                    Some(run) => run.record(event),
                    None => event,
                };
                rose_web::emit(tx, event);
            }};
        }
        // The terminal `final` is recorded to the trace here; the web
        // layer emits its own copy to the stream.
        macro_rules! record_final {
            ($view:expr) => {{
                if let Some(run) = &mut trace {
                    let mut final_view = $view;
                    final_view["type"] = serde_json::json!("final");
                    run.record(final_view);
                }
            }};
        }
        macro_rules! finish_ok {
            ($view:expr) => {{
                let mut view = $view;
                view["chat_id"] = serde_json::json!(chat_id);
                record_final!(view.clone());
                tracing::info!(run_id = %run_id, steps = %view["steps"], done = %view["done"], "run finished");
                return Ok(view);
            }};
        }
        macro_rules! finish_stopped {
            ($step:expr, $acted:expr) => {{
                let step: u32 = $step;
                emit!(serde_json::json!({
                    "type": "stopped", "step": step,
                    "say": "Stopped by user — no further actions will be taken",
                }));
                let mut view = step_view_steps(
                    "Stopped by user",
                    $acted,
                    false,
                    false,
                    step,
                );
                view["stopped"] = serde_json::json!(true);
                view["chat_id"] = serde_json::json!(chat_id);
                record_final!(view.clone());
                tracing::info!(run_id = %run_id, step, "run stopped by user");
                return Ok(view);
            }};
        }
        macro_rules! check_stop {
            ($step:expr, $acted:expr) => {{
                if self.stop_flag.load(Ordering::SeqCst) {
                    finish_stopped!($step, $acted);
                }
            }};
        }
        macro_rules! finish_err {
            ($error:expr) => {{
                let error: String = $error;
                record_final!(serde_json::json!({
                    "say": error, "acted": false, "approval_needed": false,
                    "done": false, "error": error,
                }));
                tracing::warn!(run_id = %run_id, error = %error, "run failed");
                return Err(error);
            }};
        }
        emit!(serde_json::json!({
            "type": "run_started", "goal": goal, "max_steps": self.policy.max_steps,
        }));
        let mut history: Vec<StepSummary> = prior_turns;
        let mut acted = false;
        let mut last_action: Option<Action> = None;
        let mut last_outcome: Option<bool> = None;
        let mut repeats = 0u32;
        let mut replans = 0u32;
        // Frame captured while settling after the last action; consumed
        // as the next step's observation instead of re-capturing.
        let mut prefetched: Option<rose_core::Frame> = None;
        // The last dispatched action + the frame it was decided on, so
        // the next observation can report (and feed back) its result.
        let mut pending: Option<(u32, String, Vec<u8>, Option<String>, u64)> = None;
        // Text-first loop: a step starts as a text-only brain call
        // (no screenshot captured, nothing sent to vision). Shell
        // work — launching apps, files, processes — is decided and
        // verified from command output alone. The brain must
        // explicitly ask to `look` (or a GUI action must be pending
        // verification) before any screenshot is taken or sent.
        let mut vision = false;
        let mut mode_switches = 0u32;
        let mut idx = 0u32;
        while idx < self.policy.max_steps {
            check_stop!(idx, acted);
            let mut seen_frame: Option<rose_core::Frame> = None;
            let mut seen_elements: Option<Vec<Element>> = None;
            let brain_ms: u64;
            let reply = if !vision {
                emit!(serde_json::json!({
                    "type": "thinking", "step": idx, "mode": "text",
                    "note": "text/shell step — no screenshot taken or sent",
                }));
                let brain_started = std::time::Instant::now();
                let reply = match unless_stopped(&self.stop_flag, async {
                    match self.brain {
                        BrainSvc::Fake => self
                            .fake
                            .next_action_text(goal, &history)
                            .await
                            .map_err(|e| e.to_string()),
                        BrainSvc::Muse => match self
                            .keys
                            .get()
                            .or_else(|| self.env_key.clone())
                            .ok_or_else(|| {
                                "no Muse API key — set it in Setup or ROSE_MUSE_API_KEY".to_string()
                            })
                            .and_then(|key| {
                                MuseClient::new(&self.endpoint, &self.model, &key)
                                    .map_err(|e| e.to_string())
                            }) {
                            Ok(client) => client
                                .next_action_text(goal, &history)
                                .await
                                .map_err(|e| e.to_string()),
                            Err(e) => Err(e),
                        },
                    }
                })
                .await
                {
                    Some(reply) => reply,
                    None => finish_stopped!(idx, acted),
                };
                let reply = match reply {
                    Ok(reply) => reply,
                    Err(error) => {
                        emit!(serde_json::json!({
                            "type": "failed", "step": idx, "stage": "brain", "mode": "text",
                            "ms": brain_started.elapsed().as_millis() as u64, "error": error,
                        }));
                        finish_err!(error);
                    }
                };
                brain_ms = brain_started.elapsed().as_millis() as u64;
                // Blind pixel actions are never executed: without a
                // screenshot there is nothing to aim at. A look/act
                // request switches the next pass to vision instead.
                let vision_request: Option<String> = match &reply {
                    ModelReply::Look { say } => Some(say.clone()),
                    ModelReply::Act { say, .. } => Some(format!(
                        "{say} (pixel action requested without a screenshot)"
                    )),
                    _ => None,
                };
                if let Some(say) = vision_request {
                    mode_switches += 1;
                    if mode_switches > 4 {
                        let msg = "Stopped: the brain kept asking to see the desktop without making progress.".to_string();
                        emit!(serde_json::json!({"type": "stuck", "step": idx, "say": msg}));
                        finish_ok!(step_view_steps(&msg, acted, false, false, idx));
                    }
                    emit!(serde_json::json!({
                        "type": "vision_requested", "step": idx, "say": say,
                    }));
                    vision = true;
                    continue;
                }
                reply
            } else {
                emit!(serde_json::json!({"type": "observing", "step": idx}));
                let frame = match prefetched.take() {
                    Some(frame) => frame,
                    None => match self.desktop.capture().await {
                        Ok(frame) => frame,
                        Err(e) => {
                            let error = e.to_string();
                            emit!(serde_json::json!({
                                "type": "failed", "step": idx, "stage": "capture", "error": error,
                            }));
                            finish_err!(format!("screenshot failed at step {idx}: {error}"));
                        }
                    },
                };
                if let Some((prev_step, action, before_jpeg, before_window, settle_ms)) =
                    pending.take()
                {
                    let screen_changed = frame.jpeg != before_jpeg;
                    last_outcome = Some(screen_changed);
                    // Feed the outcome back to the brain, in text, in the
                    // history it will re-read — not only to the chat. A
                    // missed click now says so where the brain looks.
                    if let Some(entry) = history.iter_mut().rev().find(|s| s.idx == prev_step) {
                        entry.intent = format!(
                            "{action} -> {}",
                            if screen_changed {
                                match &frame.active_window {
                                    Some(w) => format!("screen changed (active window: {w})"),
                                    None => "screen changed".to_string(),
                                }
                            } else {
                                "NO visible change".to_string()
                            }
                        );
                    }
                    emit!(serde_json::json!({
                        "type": "result", "step": prev_step, "observed_at_step": idx,
                        "action": action, "screen_changed": screen_changed,
                        "active_window_before": before_window,
                        "active_window_after": frame.active_window,
                        "settle_ms": settle_ms,
                        "feedback": if screen_changed {
                            "screen changed — new screenshot + history below are fed back to the brain"
                        } else {
                            "screen did NOT change — new screenshot + history below are fed back to the brain"
                        },
                    }));
                }
                // Persist what the brain is about to see, then report it.
                let frame_file =
                    trace.as_ref().and_then(|run| {
                        run.save_frame(idx, &frame.jpeg)
                    .map_err(|e| tracing::warn!(run_id = %run_id, error = %e, "frame save failed"))
                    .ok()
                    });
                emit!(serde_json::json!({
                    "type": "observed", "step": idx,
                    "width": frame.width, "height": frame.height,
                    "frame_bytes": frame.jpeg.len(),
                    "active_window": frame.active_window,
                    "frame": frame_file,
                    "history": history.iter().map(|s| s.say.clone()).collect::<Vec<_>>(),
                }));
                emit!(serde_json::json!({
                    "type": "thinking", "step": idx, "mode": "vision",
                }));
                // SoM grounding (same idx, never a separate step): parse the
                // raw frame into numbered elements, send the *annotated* JPEG
                // to the brain so it picks `element: N` instead of guessing
                // x,y. A dead sidecar falls back to the raw screenshot.
                let mut brain_jpeg = frame.jpeg.clone();
                let mut grounded: Option<Vec<Element>> = None;
                if self.grounding_backend == "omniparser" {
                    match GroundingClient::new(&self.grounding_endpoint, self.grounding_min_conf) {
                        Ok(client) => {
                            let g_started = std::time::Instant::now();
                            let overlay_opt =
                                unless_stopped(&self.stop_flag, client.parse(&frame.jpeg))
                                    .await
                                    .flatten();
                            if let Some(overlay) = overlay_opt {
                                let ms = g_started.elapsed().as_millis() as u64;
                                // Persist what the brain is about to see so a
                                // trace replay shows the badges, not just the
                                // raw frame (`step-NNN.jpg` stays raw).
                                let grounded_file =
                                    trace.as_ref().and_then(|run| {
                                        run.save_grounded_frame(idx, &overlay.annotated_jpeg)
                                        .map_err(|e| tracing::warn!(run_id = %run_id, error = %e, "grounded frame save failed"))
                                        .ok()
                                    });
                                // First labels help judge overlay quality from
                                // the trace alone (cap: full list is in the
                                // sidecar response, not the chat feed).
                                let labels: Vec<String> = overlay
                                    .elements
                                    .iter()
                                    .take(8)
                                    .map(|e| format!("{}:{}", e.id.0, e.label))
                                    .collect();
                                emit!(serde_json::json!({
                                    "type": "grounded", "step": idx,
                                    "elements": overlay.elements.len(),
                                    "ms": ms, "parse_ms": overlay.parse_ms,
                                    "frame": grounded_file, "labels": labels,
                                }));
                                if !overlay.elements.is_empty() {
                                    brain_jpeg = overlay.annotated_jpeg.clone();
                                    grounded = Some(overlay.elements);
                                }
                            }
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "bad grounding config, using raw screenshot");
                        }
                    }
                }
                let brain_started = std::time::Instant::now();
                // Awaited through unless_stopped: pressing Stop while the
                // brain thinks abandons the call instead of acting on a
                // reply the user no longer wants executed.
                let vision_reply = match unless_stopped(&self.stop_flag, async {
                    match self.brain {
                        BrainSvc::Fake => self
                            .fake
                            .next_action(goal, &history, &brain_jpeg)
                            .await
                            .map_err(|e| e.to_string()),
                        BrainSvc::Muse => match self
                            .keys
                            .get()
                            .or_else(|| self.env_key.clone())
                            .ok_or_else(|| {
                                "no Muse API key — set it in Setup or ROSE_MUSE_API_KEY".to_string()
                            })
                            .and_then(|key| {
                                MuseClient::new(&self.endpoint, &self.model, &key)
                                    .map_err(|e| e.to_string())
                            }) {
                            Ok(client) => client
                                .next_action(goal, &history, &brain_jpeg)
                                .await
                                .map_err(|e| e.to_string()),
                            Err(e) => Err(e),
                        },
                    }
                })
                .await
                {
                    Some(reply) => reply,
                    None => finish_stopped!(idx, acted),
                };
                let vision_reply = match vision_reply {
                    Ok(reply) => reply,
                    Err(error) => {
                        emit!(serde_json::json!({
                            "type": "failed", "step": idx, "stage": "brain", "mode": "vision",
                            "ms": brain_started.elapsed().as_millis() as u64, "error": error,
                        }));
                        finish_err!(error);
                    }
                };
                brain_ms = brain_started.elapsed().as_millis() as u64;
                seen_frame = Some(frame);
                seen_elements = grounded;
                vision_reply
            };
            match reply {
                ModelReply::Look { say } => {
                    // Already looking (vision step) or a spurious look:
                    // drop back to text and let the switch guard stop
                    // any ping-pong between modes.
                    mode_switches += 1;
                    if mode_switches > 4 {
                        let msg = "Stopped: the brain kept asking to see the desktop without making progress.".to_string();
                        emit!(serde_json::json!({"type": "stuck", "step": idx, "say": msg}));
                        finish_ok!(step_view_steps(&msg, acted, false, false, idx));
                    }
                    emit!(serde_json::json!({
                        "type": "vision_requested", "step": idx, "say": say,
                    }));
                    vision = false;
                    continue;
                }
                ModelReply::Act {
                    action,
                    say,
                    enter_after,
                } => {
                    let frame = seen_frame
                        .take()
                        .expect("GUI actions are only handled after a vision step");
                    // Same-idx SoM resolution: the brain picked `element: N`
                    // from this step's overlay. Map it to a click point now.
                    // Unknown ids replan (never blind-click); pixel replies
                    // pass through untouched for fallback runs.
                    let mut action = action;
                    if let Some(elements) = seen_elements.take() {
                        let resolve = |t: &Target| -> Option<Target> {
                            match t {
                                Target::Element(id) => resolve_element(&elements, &id.0)
                                    .map(|(x, y)| Target::Point { x, y }),
                                p => Some(p.clone()),
                            }
                        };
                        let resolved = match &action {
                            Action::Click { target, button } => {
                                resolve(target).map(|target| Action::Click {
                                    target,
                                    button: *button,
                                })
                            }
                            Action::DoubleClick { target } => {
                                resolve(target).map(|target| Action::DoubleClick { target })
                            }
                            Action::Scroll { target, dy } => {
                                resolve(target).map(|target| Action::Scroll { target, dy: *dy })
                            }
                            Action::Drag { from, to } => match (resolve(from), resolve(to)) {
                                (Some(from), Some(to)) => Some(Action::Drag { from, to }),
                                _ => None,
                            },
                            _ => Some(action.clone()),
                        };
                        match resolved {
                            Some(a) => action = a,
                            None => {
                                let msg = "I picked an element number that is not on screen; choosing a different target".to_string();
                                emit!(serde_json::json!({
                                    "type": "rejected", "step": idx,
                                    "action": describe_action(&action), "say": msg,
                                }));
                                history.push(StepSummary {
                                    idx,
                                    intent: format!(
                                        "REJECTED unknown element in {}",
                                        describe_action(&action)
                                    ),
                                    say: msg,
                                });
                                idx += 1;
                                continue;
                            }
                        }
                    } else {
                        // Grounding was off (or the sidecar was down) and the
                        // brain still replied `element: N`: nothing to map it
                        // through, so replan with pixels instead of acting.
                        let has_element = match &action {
                            Action::Click { target, .. }
                            | Action::DoubleClick { target }
                            | Action::Scroll { target, .. } => {
                                matches!(target, Target::Element(_))
                            }
                            Action::Drag { from, to } => {
                                matches!(from, Target::Element(_))
                                    || matches!(to, Target::Element(_))
                            }
                            _ => false,
                        };
                        if has_element {
                            let msg = "I picked an element number but grounding is off; reply with x,y pixels instead".to_string();
                            emit!(serde_json::json!({
                                "type": "rejected", "step": idx,
                                "action": describe_action(&action), "say": msg,
                            }));
                            history.push(StepSummary {
                                idx,
                                intent: format!(
                                    "REJECTED element without overlay in {}",
                                    describe_action(&action)
                                ),
                                say: msg,
                            });
                            idx += 1;
                            continue;
                        }
                    }
                    mode_switches = 0;
                    let mut action_text = describe_action(&action);
                    if enter_after {
                        action_text.push_str(" + press Return");
                    }
                    emit!(serde_json::json!({
                        "type": "planned", "step": idx, "say": say,
                        "action": action_text, "brain_ms": brain_ms,
                    }));
                    if !force && needs_approval(&self.policy, &say) {
                        tracing::warn!(%say, "step gated: needs approval");
                        emit!(serde_json::json!({
                            "type": "approval", "step": idx, "say": say, "action": action_text,
                        }));
                        finish_ok!(step_view_steps(&say, acted, true, false, idx));
                    }
                    // No-effect repeat: the previous identical action
                    // provably changed nothing on screen, so executing
                    // it again is a wasted step. Reject it, tell the
                    // brain (via history) to pick a different route,
                    // and re-plan instead. Twice is patience enough.
                    if last_outcome == Some(false) && last_action.as_ref() == Some(&action) {
                        replans += 1;
                        if replans > 2 {
                            let msg = format!(
                                "Stopped: I kept repeating an action that had no visible effect ({action_text}). \
                                 The target is probably not where I am aiming."
                            );
                            emit!(serde_json::json!({
                                "type": "stuck", "step": idx, "say": msg, "action": action_text,
                            }));
                            finish_ok!(step_view_steps(&msg, acted, false, false, idx));
                        }
                        emit!(serde_json::json!({
                            "type": "rejected", "step": idx, "action": action_text,
                            "say": "That exact action had no visible effect last time — not repeating it; pick a different route",
                        }));
                        history.push(StepSummary {
                            idx,
                            intent: format!("REJECTED repeat of {action_text} (no visible effect)"),
                            say: "My repeated action had no visible effect; choosing a different route".to_string(),
                        });
                        idx += 1;
                        continue;
                    }
                    // Stuck guard: a third identical action in a row means
                    // the clicks are landing nowhere useful. Stop and say
                    // so instead of burning the whole budget repeating it.
                    if last_action.as_ref() == Some(&action) {
                        repeats += 1;
                        if repeats >= 2 {
                            let msg = format!(
                                "Stopped: I repeated the same action 3 times without it working ({say}). \
                                 The target may not be where I am clicking."
                            );
                            tracing::warn!(%say, "step loop: repeated action, stopping");
                            emit!(serde_json::json!({
                                "type": "stuck", "step": idx, "say": msg, "action": action_text,
                            }));
                            finish_ok!(step_view_steps(&msg, acted, false, false, idx));
                        }
                    } else {
                        last_action = Some(action.clone());
                        repeats = 0;
                    }
                    check_stop!(idx, acted);
                    tracing::info!(idx, %say, ?action, "step act");
                    emit!(serde_json::json!({
                        "type": "acting", "step": idx, "action": action_text,
                    }));
                    let waited = matches!(action, Action::Wait { .. });
                    let act_started = std::time::Instant::now();
                    let mut dispatch = self.desktop.act(&action, Actor::Agent).await;
                    if dispatch.is_ok() && enter_after {
                        // Second atomic act() of the same decision
                        // (type the text, then submit it).
                        dispatch = self
                            .desktop
                            .act(
                                &Action::PressKey {
                                    keys: vec!["Return".to_string()],
                                },
                                Actor::Agent,
                            )
                            .await;
                    }
                    if let Err(e) = dispatch {
                        let error = e.to_string();
                        emit!(serde_json::json!({
                            "type": "failed", "step": idx, "stage": "action",
                            "action": action_text, "error": error,
                        }));
                        finish_err!(format!(
                            "action failed at step {idx} ({action_text}): {error}"
                        ));
                    }
                    emit!(serde_json::json!({
                        "type": "acted", "step": idx, "action": action_text,
                        "ms": act_started.elapsed().as_millis() as u64,
                        "note": "dispatched OK — result is checked on the next screenshot",
                    }));
                    acted = true;
                    last_outcome = None;
                    history.push(StepSummary {
                        idx,
                        intent: action_text.clone(),
                        say: say.clone(),
                    });
                    // Wait for the desktop to settle and keep that
                    // frame as the next observation (apps launching
                    // mid-flight no longer burn a "wait" decision).
                    // A Wait action already slept inside act().
                    let settle_started = std::time::Instant::now();
                    if !waited {
                        prefetched = self.settled_frame().await;
                    }
                    pending = Some((
                        idx,
                        action_text,
                        frame.jpeg.clone(),
                        frame.active_window.clone(),
                        settle_started.elapsed().as_millis() as u64,
                    ));
                }
                ModelReply::Shell { command, say } => {
                    let action_text = format!("shell: {}", shell::truncate_output(&command, 160));
                    emit!(serde_json::json!({
                        "type": "planned", "step": idx, "say": say,
                        "action": action_text, "command": command, "brain_ms": brain_ms,
                    }));
                    if !force && shell_needs_approval(&self.policy, &say, &command) {
                        tracing::warn!(%say, %command, "shell command gated: needs approval");
                        emit!(serde_json::json!({
                            "type": "approval", "step": idx, "say": say, "action": action_text,
                        }));
                        finish_ok!(step_view_steps(&say, acted, true, false, idx));
                    }
                    check_stop!(idx, acted);
                    tracing::info!(idx, %say, %command, "shell exec");
                    emit!(serde_json::json!({
                        "type": "acting", "step": idx, "action": action_text,
                    }));
                    let output =
                        match unless_stopped(&self.stop_flag, self.shell.run(&command)).await {
                            Some(Ok(output)) => output,
                            Some(Err(error)) => {
                                emit!(serde_json::json!({
                                    "type": "failed", "step": idx, "stage": "shell",
                                    "action": action_text, "error": error,
                                }));
                                finish_err!(format!("shell failed at step {idx}: {error}"));
                            }
                            None => finish_stopped!(idx, acted),
                        };
                    emit!(serde_json::json!({
                        "type": "shell_result", "step": idx, "action": action_text,
                        "command": command, "exit_code": output.code,
                        "timed_out": output.timed_out, "ms": output.ms,
                        "stdout": output.stdout, "stderr": output.stderr,
                    }));
                    emit!(serde_json::json!({
                        "type": "acted", "step": idx, "action": action_text,
                        "ms": output.ms,
                        "note": "shell output above is fed back to the brain in its history",
                    }));
                    acted = true;
                    last_action = None;
                    last_outcome = None;
                    history.push(StepSummary {
                        idx,
                        intent: format!(
                            "shell `{}` -> {}: {}",
                            shell::truncate_output(&command, 80),
                            output
                                .code
                                .map(|c| format!("exit {c}"))
                                .unwrap_or_else(|| "killed/timed out".to_string()),
                            // 600 chars: on the VM a `df -h` answer
                            // (incl. the total line) was cut at 240 and
                            // the brain kept re-running variants of
                            // the same command instead of answering.
                            shell::truncate_output(&output.stdout.replace('\n', " "), 600),
                        ),
                        say: say.clone(),
                    });
                    // Next step is text again: shell output is the
                    // feedback, no screenshot is needed to judge it.
                    // (Only a vision-decided shell command settles and
                    // reports its desktop result on the next look.)
                    vision = false;
                    mode_switches = 0;
                    if let Some(frame) = seen_frame.take() {
                        let settle_started = std::time::Instant::now();
                        prefetched = self.settled_frame().await;
                        pending = Some((
                            idx,
                            action_text,
                            frame.jpeg.clone(),
                            frame.active_window.clone(),
                            settle_started.elapsed().as_millis() as u64,
                        ));
                    } else {
                        // No screenshot was taken for this step; any
                        // prefetched frame would be stale by now.
                        prefetched = None;
                    }
                }
                ModelReply::Done { say } => {
                    emit!(serde_json::json!({
                        "type": "decision", "step": idx, "status": "done", "say": say,
                        "brain_ms": brain_ms,
                    }));
                    finish_ok!(step_view_steps(&say, acted, false, true, idx));
                }
                ModelReply::Ask { say } => {
                    emit!(serde_json::json!({
                        "type": "decision", "step": idx, "status": "ask", "say": say,
                        "brain_ms": brain_ms,
                    }));
                    finish_ok!(step_view_steps(&say, acted, false, false, idx));
                }
                ModelReply::Fail { say } => {
                    emit!(serde_json::json!({
                        "type": "decision", "step": idx, "status": "fail", "say": say,
                        "brain_ms": brain_ms,
                    }));
                    // If we already acted, surface the narration instead of
                    // replacing the whole run with a bare error.
                    if acted {
                        finish_ok!(step_view_steps(&say, true, false, false, idx));
                    }
                    finish_err!(say);
                }
            }
            idx += 1;
        }
        let msg = format!(
            "Stopped after {} steps (budget) without finishing.",
            self.policy.max_steps
        );
        emit!(serde_json::json!({"type": "stuck", "say": msg}));
        let mut view = step_view_steps(&msg, acted, false, false, self.policy.max_steps);
        view["chat_id"] = serde_json::json!(chat_id);
        record_final!(view.clone());
        Ok(view)
    }

    fn request_stop(&self) -> bool {
        use std::sync::atomic::Ordering;
        if self.active.load(Ordering::SeqCst) {
            self.stop_flag.store(true, Ordering::SeqCst);
            true
        } else {
            false
        }
    }
}

/// Resolved config with secrets redacted. Acceptance command for T0.3.
fn print_config(config: &RoseConfig, keys: &ApiKeys) -> Result<()> {
    let toml = toml::to_string_pretty(config)?;
    println!("{toml}");
    println!("# secrets (env only, values never printed)");
    println!("# ROSE_MUSE_API_KEY = {}", keys.muse_status());
    Ok(())
}

fn home_dir() -> Result<PathBuf> {
    std::env::var("HOME")
        .map(PathBuf::from)
        .context("HOME is not set")
}

fn run_as_user(program: &str, args: &[&str], dry_run: bool) -> Result<()> {
    if dry_run {
        println!("  [dry-run] $ {program} {}", args.join(" "));
        return Ok(());
    }
    let status = std::process::Command::new(program)
        .args(args)
        .status()
        .with_context(|| format!("spawning {program}"))?;
    if !status.success() {
        bail!("{program} {} exited with {status}", args.join(" "));
    }
    Ok(())
}

fn write_file(path: &Path, contents: &str, dry_run: bool) -> Result<()> {
    if dry_run {
        println!(
            "  [dry-run] write {} ({} bytes)",
            path.display(),
            contents.len()
        );
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(path, contents).with_context(|| format!("writing {}", path.display()))?;
    println!("  wrote {}", path.display());
    Ok(())
}

/// Rewrite the unit's ExecStart to the given binary path. Pure: unit-tested.
/// Note: when exe already equals the default the output is unchanged input —
/// that is a valid no-op, not an error (checked via marker, not equality).
fn render_rosed_unit(template: &str, exe: &str) -> Result<String> {
    const MARKER: &str = "ExecStart=/usr/bin/rosed";
    if !template.contains(MARKER) {
        bail!("embedded rosed.service template has no {MARKER} line to rewrite");
    }
    Ok(template.replace(MARKER, &format!("ExecStart={exe}")))
}

/// App-style install from the single binary. Idempotent; never overwrites
/// an existing config.toml.
fn cmd_install(dry_run: bool) -> Result<()> {
    let home = home_dir()?;
    let user = std::env::var("USER").unwrap_or_else(|_| "rose".to_string());
    let units_dir = home.join(".config/systemd/user");
    let config_path = home.join(".config/rose/config.toml");

    println!("rosed install (dry_run={dry_run})");

    println!("units -> {}", units_dir.display());
    write_file(&units_dir.join("xvnc.service"), XVNC_SERVICE, dry_run)?;
    // Point the unit at THIS binary (deb -> /usr/bin/rosed, source builds ->
    // ~/.local/bin/rosed), so the service always runs what you installed.
    // (When exe already equals the default the content is unchanged — fine.
    // An earlier guard compared output equality and mistook that no-op for
    // a broken template; the marker check in render_rosed_unit is correct.)
    let exe = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "/usr/bin/rosed".to_string());
    let rosed_unit = render_rosed_unit(ROSED_SERVICE, &exe)?;
    if dry_run {
        println!(
            "  [dry-run] write {} (ExecStart={exe})",
            units_dir.join("rosed.service").display()
        );
    } else {
        write_file(&units_dir.join("rosed.service"), &rosed_unit, false)?;
    }

    println!("config -> {}", config_path.display());
    if config_path.exists() {
        println!("  exists, leaving it alone");
    } else {
        write_file(&config_path, EXAMPLE_CONFIG, dry_run)?;
    }

    // Lingering lets user services run without an active login. Needs sudo
    // once; failure is non-fatal (setup.sh or a manual invocation covers it).
    if dry_run {
        println!("  [dry-run] $ sudo loginctl enable-linger {user}");
    } else if let Err(e) = run_as_user("sudo", &["loginctl", "enable-linger", &user], false) {
        eprintln!("WARNING: lingering not enabled ({e}); run: sudo loginctl enable-linger {user}");
    }

    run_as_user("systemctl", &["--user", "daemon-reload"], dry_run)?;
    // Stale locks from a killed run make vncserver exit 1; clear first.
    if dry_run {
        println!("  [dry-run] clear stale :1 locks");
    } else {
        let _ = std::process::Command::new("vncserver")
            .args(["-kill", ":1"])
            .output();
        let _ = std::fs::remove_file("/tmp/.X1-lock");
        let _ = std::fs::remove_file("/tmp/.X11-unix/X1");
    }
    run_as_user(
        "systemctl",
        &["--user", "enable", "--now", "xvnc.service"],
        dry_run,
    )?;
    // rosed itself may bail until its backends land (T1.2/T3.1): warn, don't fail.
    if run_as_user(
        "systemctl",
        &["--user", "enable", "--now", "rosed.service"],
        dry_run,
    )
    .is_err()
        && !dry_run
    {
        eprintln!("WARNING: rosed.service did not start (expected until M1/M2 backends land).");
        eprintln!("  See: journalctl --user -u rosed -n 30");
    }

    println!("done. Next: ssh -L 5901:127.0.0.1:5901 <vm>, VNC to localhost:5901");
    Ok(())
}

/// `rosed runs [run-id]`: observability without the browser — list
/// recent persisted runs, or dump one run's full event trace.
fn cmd_runs(config: &RoseConfig, id: Option<&str>, limit: u32) -> Result<()> {
    use std::io::Write;
    // Locked stdout, stopping quietly on EPIPE (`rosed runs <id> | head`
    // must not panic).
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let store = TraceStore::new(expand_tilde(&config.data.dir));
    match id {
        None => {
            let runs = store.list(limit);
            if runs.is_empty() {
                let _ = writeln!(out, "no persisted runs in {}", store.root().display());
            }
            for run in runs {
                let _ = writeln!(
                    out,
                    "{}  {:<16} steps={:<3} decisions={:<3} {}  {}",
                    run.run_id, run.state, run.steps, run.decisions, run.started_at, run.goal,
                );
                if let Some(say) = &run.say {
                    let _ = writeln!(out, "    -> {say}");
                }
            }
            Ok(())
        }
        Some(id) => {
            let events = store
                .events(id)
                .ok_or_else(|| anyhow::anyhow!("unknown run id: {id}"))?;
            for event in events {
                if writeln!(out, "{}", serde_json::to_string_pretty(&event)?).is_err() {
                    break;
                }
            }
            Ok(())
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();

    if let Some(Command::Install { dry_run }) = cli.command {
        return cmd_install(dry_run);
    }

    let (config, keys) = load_config(cli.config)?;

    if cli.print_config {
        return print_config(&config, &keys);
    }

    if let Some(Command::Runs { id, limit }) = cli.command {
        return cmd_runs(&config, id.as_deref(), limit);
    }

    if let Some(Command::Chat { limit }) = cli.command {
        let store = TraceStore::new(expand_tilde(&config.data.dir));
        for message in store.chat(limit) {
            println!(
                "{} {:<9} ({}) {}",
                message.ts,
                message.role,
                message.run_id.as_deref().unwrap_or("-"),
                message.text
            );
        }
        return Ok(());
    }

    let services = build_services(config, keys)?;

    // Bind guard: loopback always OK; anything else needs explicit opt-in.
    let addr: std::net::SocketAddr = services
        .config
        .server
        .bind
        .parse()
        .context("invalid server.bind (want 127.0.0.1:8787)")?;
    if !addr.ip().is_loopback() && !services.config.server.allow_remote {
        bail!("refusing to bind {addr} without allow_remote = true");
    }

    let info = services.desktop.info().await?;
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        bind = %addr,
        backend = %services.config.desktop.backend,
        brain = %services.config.models.brain,
        display = %info.display,
        size = %format!("{}x{}", info.width, info.height),
        "rosed starting"
    );
    // Web-set keys live in server memory (KeySlot); env key is the fallback.
    let keys = rose_web::KeySlot::default();
    let stepper = Stepper {
        desktop: Arc::clone(&services.desktop),
        brain: services.config.models.brain.as_str().into(),
        fake: FakeBrain::demo(),
        keys: keys.clone(),
        env_key: services.api_keys.muse_api_key.clone(),
        endpoint: services.config.models.endpoint.clone(),
        model: services.config.models.model.clone(),
        grounding_backend: services.config.grounding.backend.clone(),
        grounding_endpoint: services.config.grounding.endpoint.clone(),
        grounding_min_conf: services.config.grounding.min_conf,
        policy: services.config.policy.clone(),
        traces: Arc::clone(&services.traces),
        shell: shell::ShellRunner::new(services.config.desktop.display.clone()),
        stop_flag: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        active: Arc::new(std::sync::atomic::AtomicBool::new(false)),
    };
    let state = rose_web::AppState {
        source: Some(Arc::new(BackendSource {
            backend: Arc::clone(&services.desktop),
            display: info.display.clone(),
            width: info.width,
            height: info.height,
        })),
        stepper: Some(Arc::new(stepper)),
        traces: Some(Arc::new(TraceAdapter {
            store: Arc::clone(&services.traces),
        })),
        keys,
        env_key_present: services.api_keys.muse_api_key.is_some(),
        backend: services.config.desktop.backend.clone(),
        brain: services.config.models.brain.clone(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        started: std::time::SystemTime::now(),
    };
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding {addr}"))?;
    println!("rosed: UI on http://{addr} (localhost + tunnel only)");
    rose_web::serve(listener, state)
        .await
        .context("web server")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn example_toml_parses() {
        let text = include_str!("../../../config/rose.example.toml");
        let cfg: RoseConfig = toml::from_str(text).expect("example must parse");
        assert_eq!(cfg.server.bind, "127.0.0.1:8787");
        assert!(!cfg.server.allow_remote);
        assert_eq!(cfg.desktop.backend, "x11");
        assert_eq!(cfg.policy.max_steps, 40);
    }

    #[test]
    fn missing_file_falls_back_to_default() {
        let (cfg, _) =
            load_config(Some(PathBuf::from("/nonexistent/rose-test.toml"))).expect("load");
        assert_eq!(cfg, RoseConfig::default());
    }

    #[tokio::test]
    async fn fake_selectors_build_and_capture() {
        let mut cfg = RoseConfig::default();
        cfg.desktop.backend = "fake".to_string();
        cfg.models.brain = "fake".to_string();
        let svc = build_services(cfg, ApiKeys::default()).expect("fake services build");
        let info = svc.desktop.info().await.expect("info");
        assert_eq!((info.width, info.height), (1600, 900));
        let frame = svc.desktop.capture().await.expect("capture");
        assert!(frame.jpeg.starts_with(&[0xff, 0xd8]), "is jpeg");
    }

    #[test]
    fn muse_builds_without_key_and_gates_risky_intents() {
        // The daemon starts keyless; the key arrives later via Setup/env and
        // is resolved per step.
        let mut cfg = RoseConfig::default(); // backend x11 (lazy), brain muse
        cfg.desktop.backend = "fake".to_string(); // keep this test display-free
        build_services(cfg, ApiKeys::default()).expect("muse selector builds");
        assert!(needs_approval(&Policy::default(), "Click Pay now"));
        assert!(!needs_approval(&Policy::default(), "Clicking Export"));
    }

    fn event_channel() -> (
        rose_web::StepEventSender,
        tokio::sync::mpsc::UnboundedReceiver<serde_json::Value>,
    ) {
        tokio::sync::mpsc::unbounded_channel()
    }

    fn drain(
        rx: &mut tokio::sync::mpsc::UnboundedReceiver<serde_json::Value>,
    ) -> Vec<serde_json::Value> {
        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }
        events
    }

    fn test_traces() -> Arc<TraceStore> {
        Arc::new(TraceStore::new(
            std::env::temp_dir().join(format!("rose-stepper-test-{}", std::process::id())),
        ))
    }

    fn test_stepper(brain: BrainSvc) -> Stepper {
        Stepper {
            desktop: Arc::new(FakeDesktop::new()),
            brain,
            fake: FakeBrain::demo(),
            keys: rose_web::KeySlot::default(),
            env_key: None,
            endpoint: "https://api.meta.ai/v1".to_string(),
            model: "muse-spark-1.3-contributor".to_string(),
            grounding_backend: "off".to_string(),
            grounding_endpoint: "http://127.0.0.1:8791".to_string(),
            grounding_min_conf: 5,
            policy: Policy::default(),
            traces: test_traces(),
            shell: shell::ShellRunner::new(":fake"),
            stop_flag: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            active: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    #[test]
    fn render_unit_rewrites_or_keeps() {
        // The exact VM case: exe already the default -> unchanged, no error.
        let same = render_rosed_unit(ROSED_SERVICE, "/usr/bin/rosed").expect("default exe");
        assert!(same.contains("ExecStart=/usr/bin/rosed"));
        let other =
            render_rosed_unit(ROSED_SERVICE, "/home/rose/.local/bin/rosed").expect("other exe");
        assert!(other.contains("ExecStart=/home/rose/.local/bin/rosed"));
        assert!(render_rosed_unit("[Service]\n", "/usr/bin/rosed").is_err());
    }

    #[tokio::test]
    async fn fake_brain_step_acts_on_fake_desktop() {
        use rose_web::StepRunner;
        let (tx, _rx) = event_channel();
        let view = test_stepper(BrainSvc::Fake)
            .run_step("click the first cell", false, None, &tx)
            .await
            .expect("step");
        assert_eq!(view["acted"], true);
        assert_eq!(view["approval_needed"], false);
    }

    #[tokio::test]
    async fn muse_step_without_key_errors_clearly() {
        use rose_web::StepRunner;
        let (tx, mut rx) = event_channel();
        let err = test_stepper(BrainSvc::Muse)
            .run_step("do a thing", false, None, &tx)
            .await
            .unwrap_err();
        assert!(err.contains("API key"), "{err}");
        // The failure is also streamed, with its stage, before returning.
        let events = drain(&mut rx);
        let failed = events
            .iter()
            .find(|e| e["type"] == "failed")
            .expect("failed event streamed");
        assert_eq!(failed["stage"], "brain");
    }

    #[tokio::test]
    async fn risky_step_needs_approval_until_forced() {
        use rose_web::StepRunner;
        let pay_click = || rose_models::ModelReply::Act {
            action: rose_core::Action::Click {
                target: rose_core::Target::Point { x: 10, y: 10 },
                button: rose_core::MouseButton::Left,
            },
            say: "Clicking Pay now".to_string(),
            enter_after: false,
        };
        let stepper = Stepper {
            // One reply per step call: first gated (no act), second forced.
            fake: FakeBrain::scripted(vec![
                pay_click(),
                pay_click(),
                rose_models::ModelReply::Look {
                    say: "Need to see the desktop".to_string(),
                },
                pay_click(),
                rose_models::ModelReply::Done {
                    say: "Paid".to_string(),
                },
            ]),
            ..test_stepper(BrainSvc::Fake)
        };
        let (tx, _rx) = event_channel();
        let gated = stepper
            .run_step("pay", false, None, &tx)
            .await
            .expect("gated");
        assert_eq!(gated["acted"], false);
        assert_eq!(gated["approval_needed"], true);
        let forced = stepper
            .run_step("pay", true, None, &tx)
            .await
            .expect("forced");
        assert_eq!(forced["acted"], true);
    }

    #[tokio::test]
    async fn loop_runs_until_done_not_just_one_step() {
        use rose_web::StepRunner;
        // Regression: "open the browser" needs more than one action.
        // The old stepper stopped after the first Act, so the run
        // narrated but never finished the job.
        let click = |x| rose_models::ModelReply::Act {
            action: rose_core::Action::Click {
                target: rose_core::Target::Point { x, y: 10 },
                button: rose_core::MouseButton::Left,
            },
            say: format!("Clicking {x}"),
            enter_after: false,
        };
        let stepper = Stepper {
            fake: FakeBrain::scripted(vec![
                rose_models::ModelReply::Look {
                    say: "Need to see the desktop".to_string(),
                },
                click(10),
                click(20),
                rose_models::ModelReply::Done {
                    say: "Browser is open".to_string(),
                },
            ]),
            ..test_stepper(BrainSvc::Fake)
        };
        let (tx, mut rx) = event_channel();
        let view = stepper
            .run_step("open the browser", false, None, &tx)
            .await
            .expect("run");
        assert_eq!(view["acted"], true);
        assert_eq!(view["done"], true);
        assert_eq!(view["say"], "Browser is open");
        assert_eq!(stepper.fake.call_count(), 4, "loop kept stepping");
        // The whole loop was streamed live, in order, including what
        // was fed back to the brain after each action.
        let events = drain(&mut rx);
        let types: Vec<&str> = events
            .iter()
            .map(|e| e["type"].as_str().expect("type"))
            .collect();
        assert_eq!(
            types,
            vec![
                "run_started",
                "thinking",
                "vision_requested",
                "observing",
                "observed",
                "thinking",
                "planned",
                "acting",
                "acted",
                "observing",
                "result",
                "observed",
                "thinking",
                "planned",
                "acting",
                "acted",
                "observing",
                "result",
                "observed",
                "thinking",
                "decision",
            ],
            "{types:?}"
        );
        // Step 1's observation carries step 0's narration as history —
        // exactly the feedback that goes back into the brain.
        let second_observed = events
            .iter()
            .find(|e| e["type"] == "observed" && e["step"] == 1)
            .expect("second observation");
        assert_eq!(
            second_observed["history"],
            serde_json::json!(["Clicking 10"])
        );
        assert_eq!(second_observed["width"], 1600);
        let result = events
            .iter()
            .find(|e| e["type"] == "result")
            .expect("action result");
        assert_eq!(result["action"], "click (10, 10) [Left]");
    }

    #[tokio::test]
    async fn loop_stops_when_the_same_click_repeats() {
        use rose_web::StepRunner;
        let click = || rose_models::ModelReply::Act {
            action: rose_core::Action::Click {
                target: rose_core::Target::Point { x: 10, y: 10 },
                button: rose_core::MouseButton::Left,
            },
            say: "Clicking the browser icon".to_string(),
            enter_after: false,
        };
        let stepper = Stepper {
            fake: FakeBrain::scripted(vec![
                rose_models::ModelReply::Look {
                    say: "Need to see the desktop".to_string(),
                },
                click(),
                click(),
                click(),
                click(),
                click(),
            ]),
            ..test_stepper(BrainSvc::Fake)
        };
        let (tx, _rx) = event_channel();
        let view = stepper
            .run_step("open the browser", false, None, &tx)
            .await
            .expect("run");
        assert_eq!(view["acted"], true);
        assert_eq!(view["done"], false);
        assert!(
            view["say"]
                .as_str()
                .expect("say")
                .contains("no visible effect"),
            "{view}"
        );
        // Only the first click was dispatched; the repeats were
        // rejected without acting (one brain call each to re-plan).
        assert_eq!(stepper.fake.call_count(), 5);
    }

    #[tokio::test]
    async fn no_effect_repeat_is_rejected_and_a_new_route_recovers() {
        use rose_web::StepRunner;
        let click = |x, y| rose_models::ModelReply::Act {
            action: rose_core::Action::Click {
                target: rose_core::Target::Point { x, y },
                button: rose_core::MouseButton::Left,
            },
            say: format!("Clicking {x},{y}"),
            enter_after: false,
        };
        let stepper = Stepper {
            fake: FakeBrain::scripted(vec![
                rose_models::ModelReply::Look {
                    say: "Need to see the desktop".to_string(),
                },
                click(10, 10),   // misses every cell: no visible change
                click(10, 10),   // identical repeat: rejected, not acted
                click(200, 225), // inside cell 0: highlights, screen changes
                rose_models::ModelReply::Done {
                    say: "Done".to_string(),
                },
            ]),
            ..test_stepper(BrainSvc::Fake)
        };
        let (tx, mut rx) = event_channel();
        let view = stepper
            .run_step("click a cell", false, None, &tx)
            .await
            .expect("run");
        assert_eq!(view["done"], true, "{view}");
        assert_eq!(stepper.fake.call_count(), 5);
        let events = drain(&mut rx);
        assert_eq!(
            events.iter().filter(|e| e["type"] == "acted").count(),
            2,
            "the repeated miss was never dispatched: {events:?}"
        );
        assert!(
            events.iter().any(|e| e["type"] == "rejected"),
            "rejection is observable: {events:?}"
        );
        // The outcome was fed back to the brain in its history text.
        let last_observed = events
            .iter()
            .rev()
            .find(|e| e["type"] == "observed")
            .expect("observed");
        assert_eq!(last_observed["step"], 3);
    }

    #[tokio::test]
    async fn stop_flag_halts_the_loop_between_steps() {
        use rose_web::StepRunner;
        let click = |x| rose_models::ModelReply::Act {
            action: rose_core::Action::Click {
                target: rose_core::Target::Point { x, y: 225 },
                button: rose_core::MouseButton::Left,
            },
            say: format!("Clicking {x}"),
            enter_after: false,
        };
        let stop_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stepper = Stepper {
            fake: FakeBrain::scripted(vec![
                click(200),
                click(600),
                rose_models::ModelReply::Done {
                    say: "should not reach".to_string(),
                },
            ]),
            stop_flag: Arc::clone(&stop_flag),
            ..test_stepper(BrainSvc::Fake)
        };
        let (tx, mut rx) = event_channel();
        // Deterministic: set the flag (exactly what request_stop does)
        // as soon as the first `acted` event arrives, while the run
        // future is still being driven below.
        let stepper_ref = &stepper;
        let flag = Arc::clone(&stop_flag);
        let view = {
            let run = stepper_ref.run_step("stop me", false, None, &tx);
            tokio::pin!(run);
            loop {
                tokio::select! {
                    v = &mut run => break v.expect("view"),
                    _ = tokio::time::sleep(std::time::Duration::from_millis(50)) => {
                        while let Ok(ev) = rx.try_recv() {
                            if ev["type"] == "acted" {
                                flag.store(true, std::sync::atomic::Ordering::SeqCst);
                            }
                        }
                    }
                }
            }
        };
        assert_eq!(view["stopped"], true, "{view}");
        assert_eq!(view["done"], false);
        assert_eq!(
            stepper.fake.call_count(),
            2,
            "stopped before any further brain call after the first action"
        );
    }

    #[tokio::test]
    async fn shell_commands_run_natively_and_feed_output_back() {
        use rose_web::StepRunner;
        let stepper = Stepper {
            fake: FakeBrain::scripted(vec![
                rose_models::ModelReply::Shell {
                    command: "echo hello-rose".to_string(),
                    say: "Echoing from the shell".to_string(),
                },
                rose_models::ModelReply::Done {
                    say: "Saw the output".to_string(),
                },
            ]),
            ..test_stepper(BrainSvc::Fake)
        };
        let (tx, mut rx) = event_channel();
        let view = stepper
            .run_step("echo test", false, None, &tx)
            .await
            .expect("run");
        assert_eq!(view["done"], true, "{view}");
        let events = drain(&mut rx);
        let result = events
            .iter()
            .find(|e| e["type"] == "shell_result")
            .expect("shell result streamed");
        assert_eq!(result["exit_code"], 0);
        assert_eq!(result["stdout"], "hello-rose");
        // Regression: a shell-only run must never touch the desktop —
        // no screenshot captured, saved, or sent to the brain.
        assert!(
            !events
                .iter()
                .any(|e| e["type"] == "observed" || e["type"] == "observing"),
            "shell step took a screenshot: {events:?}"
        );
        let run = stepper
            .traces
            .list(50)
            .into_iter()
            .find(|r| r.goal == "echo test")
            .expect("shell run persisted");
        assert!(stepper.traces.frame_path(&run.run_id, 0).is_none());
    }

    #[tokio::test]
    async fn dangerous_shell_commands_need_approval() {
        use rose_web::StepRunner;
        let rm = || rose_models::ModelReply::Shell {
            command: "rm -rf /tmp/rose-definitely-missing-xyz".to_string(),
            say: "Cleaning up".to_string(),
        };
        let stepper = Stepper {
            fake: FakeBrain::scripted(vec![
                rm(),
                rm(),
                rose_models::ModelReply::Done {
                    say: "Cleaned".to_string(),
                },
            ]),
            ..test_stepper(BrainSvc::Fake)
        };
        let (tx, _rx) = event_channel();
        let gated = stepper
            .run_step("clean", false, None, &tx)
            .await
            .expect("gated");
        assert_eq!(gated["approval_needed"], true, "{gated}");
        assert_eq!(gated["acted"], false);
        let forced = stepper
            .run_step("clean", true, None, &tx)
            .await
            .expect("forced");
        assert_eq!(forced["done"], true, "{forced}");
    }

    #[tokio::test]
    async fn two_turns_share_one_chat() {
        use rose_web::StepRunner;
        let shell_turn = |out: &str| rose_models::ModelReply::Shell {
            command: format!("echo {out}"),
            say: format!("Echoing {out}"),
        };
        let stepper = Stepper {
            fake: FakeBrain::scripted(vec![
                shell_turn("one"),
                rose_models::ModelReply::Done {
                    say: "first reply".to_string(),
                },
                shell_turn("two"),
                rose_models::ModelReply::Done {
                    say: "second reply".to_string(),
                },
            ]),
            ..test_stepper(BrainSvc::Fake)
        };
        let (tx, _rx) = event_channel();
        let first = stepper
            .run_step("first goal", false, None, &tx)
            .await
            .expect("turn 1");
        let chat_id = first["chat_id"].as_str().expect("chat id").to_string();
        let second = stepper
            .run_step("second goal", false, Some(&chat_id), &tx)
            .await
            .expect("turn 2");
        assert_eq!(second["chat_id"], chat_id);
        let chats = stepper.traces.chats(10);
        let chat = chats
            .iter()
            .find(|c| c.chat_id == chat_id)
            .expect("chat listed");
        assert_eq!(chat.turns, 2, "{chat:?}");
        assert_eq!(chat.title, "first goal");
        assert_eq!(stepper.traces.runs_for_chat(&chat_id).len(), 2);
    }

    #[tokio::test]
    async fn run_trace_is_persisted_with_frames() {
        use rose_web::StepRunner;
        let stepper = test_stepper(BrainSvc::Fake);
        let (tx, _rx) = event_channel();
        let view = stepper
            .run_step("persist this run", false, None, &tx)
            .await
            .expect("run");
        assert_eq!(view["done"], true);
        // Same store, fresh listing: the run is all there.
        let runs = stepper.traces.list(50);
        let run = runs
            .iter()
            .find(|r| r.goal == "persist this run")
            .expect("run persisted");
        assert_eq!(run.state, "done");
        let events = stepper.traces.events(&run.run_id).expect("events");
        assert_eq!(events.first().expect("meta")["type"], "run_meta");
        assert_eq!(events.last().expect("final")["type"], "final");
        assert!(events.iter().all(|e| e["run_id"] == run.run_id));
        assert!(stepper.traces.frame_path(&run.run_id, 0).is_some());
    }

    #[test]
    fn describe_action_hides_typed_text() {
        assert_eq!(
            describe_action(&Action::Click {
                target: Target::Point { x: 812, y: 240 },
                button: rose_core::MouseButton::Left,
            }),
            "click (812, 240) [Left]"
        );
        let typed = describe_action(&Action::Type {
            text: "hunter2".to_string(),
        });
        assert_eq!(typed, "type 7 chars (text hidden)");
        assert!(!typed.contains("hunter2"), "secrets never enter traces");
        assert_eq!(
            describe_action(&Action::PressKey {
                keys: vec!["ctrl".to_string(), "l".to_string()],
            }),
            "press ctrl+l"
        );
    }

    #[test]
    fn x11_selector_constructs_without_a_display() {
        // Construction is lazy: no X connection until first capture/act.
        let mut cfg = RoseConfig::default();
        cfg.desktop.backend = "x11".to_string();
        cfg.models.brain = "fake".to_string();
        build_services(cfg, ApiKeys::default()).expect("x11 selector builds");
    }

    #[test]
    fn unknown_selectors_rejected() {
        let mut cfg = RoseConfig::default();
        cfg.desktop.backend = "wayland".to_string();
        cfg.models.brain = "fake".to_string();
        let err = build_services(cfg, ApiKeys::default())
            .map(|_| ())
            .unwrap_err();
        assert!(err.to_string().contains("unknown desktop backend"), "{err}");
    }
}
