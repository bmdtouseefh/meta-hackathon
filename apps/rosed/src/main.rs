//! `rosed` composition root: config -> factories -> run.
//!
//! IMPLEMENTATION.md T0.3. CLI owns config loading (file + env for secrets)
//! and selects implementations via factory fns keyed on config. Real
//! backends land in later tasks; unknown selectors fail loudly here.

mod shell;

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use clap::{Parser, Subcommand};
use rose_agent::{Driver, Lease};
use rose_core::{Action, Actor, DesktopBackend, Element, Policy, RoseConfig, StepSummary, Target};
use rose_desktop_auto::AutoDesktop;
use rose_desktop_cdp::CdpDesktop;
use rose_desktop_fake::FakeDesktop;
use rose_desktop_x11::X11Desktop;
use rose_models::{resolve_element, FakeBrain, ModelReply, MuseClient};
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

    async fn grounded_frame(&self, run_id: &str, step: u32) -> Option<Vec<u8>> {
        std::fs::read(self.store.grounded_frame_path(run_id, step)?).ok()
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
/// backends) exposing snapshots to the web layer.
struct BackendSource {
    backend: Arc<dyn DesktopBackend>,
    display: String,
    width: u32,
    height: u32,
}

#[async_trait]
impl rose_web::FrameSource for BackendSource {
    /// Live view shows the same numbered grid the brain sees, never a
    /// raw frame (a frame that cannot be gridded is an error, not a
    /// raw fallback).
    async fn jpeg(&self) -> std::result::Result<Vec<u8>, String> {
        let frame = self.backend.capture().await.map_err(|e| e.to_string())?;
        rose_grid::annotate_grid_jpeg(&frame.jpeg)
    }

    fn display(&self) -> String {
        self.display.clone()
    }

    fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }
}

/// Human takeover input: maps live-view fractions onto a fresh frame and
/// dispatches through the same `act()` path as the agent, audited as
/// `Actor::Human`. The endpoints check the lease first; this re-checks,
/// so input landing between hand-back and dispatch is refused, not acted.
struct ControlSource {
    backend: Arc<dyn DesktopBackend>,
    lease: Lease,
}

impl ControlSource {
    fn require_human(&self) -> std::result::Result<(), String> {
        if self.lease.holder() == Driver::Human {
            Ok(())
        } else {
            Err("agent holds the desktop - take control first".to_string())
        }
    }

    /// Fresh frame so fractions map to the backend's *current* geometry
    /// (X11 pixels vs CDP viewport vs auto's per-window switch).
    async fn frame(&self) -> std::result::Result<rose_core::Frame, String> {
        self.require_human()?;
        self.backend.capture().await.map_err(|e| e.to_string())
    }

    fn point(frame: &rose_core::Frame, x: f64, y: f64) -> Target {
        let px = (x.clamp(0.0, 1.0) * frame.width as f64).round() as u32;
        let py = (y.clamp(0.0, 1.0) * frame.height as f64).round() as u32;
        Target::Point { x: px, y: py }
    }

    async fn dispatch(&self, action: Action) -> std::result::Result<(), String> {
        self.require_human()?;
        tracing::info!(?action, actor = "human", "takeover input");
        self.backend
            .act(&action, Actor::Human)
            .await
            .map_err(|e| e.to_string())
    }
}

#[async_trait]
impl rose_web::HumanInput for ControlSource {
    async fn click_frac(&self, x: f64, y: f64, double: bool) -> std::result::Result<(), String> {
        let frame = self.frame().await?;
        let target = Self::point(&frame, x, y);
        let action = if double {
            Action::DoubleClick { target }
        } else {
            Action::Click {
                target,
                button: rose_core::MouseButton::Left,
            }
        };
        self.dispatch(action).await
    }

    async fn type_text(&self, text: &str) -> std::result::Result<(), String> {
        self.dispatch(Action::Type {
            text: text.to_string(),
        })
        .await
    }

    async fn press_keys(&self, keys: Vec<String>) -> std::result::Result<(), String> {
        self.dispatch(Action::PressKey { keys }).await
    }

    async fn scroll_frac(&self, x: f64, y: f64, dy_frac: f64) -> std::result::Result<(), String> {
        let frame = self.frame().await?;
        let target = Self::point(&frame, x, y);
        let dy = (dy_frac * frame.height as f64).round() as i32;
        self.dispatch(Action::Scroll { target, dy }).await
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
    lease: Lease,
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
        // CDP talks to the VM's existing Chrome (google-chrome-stable) over
        // its remote-debugging port — no Chromium install, session survives.
        // Lazy: handshake happens on first capture/act, so rosed starts
        // even when Chrome isn't up yet.
        "cdp" => Ok(Arc::new(CdpDesktop::new(
            &config.cdp.endpoint,
            config.cdp.launch_if_missing,
            &config.cdp.chrome_binary,
        ))),
        // Hybrid: X11 everywhere (shell-launched apps, dialogs, native
        // windows keep working), CDP automatically when a Chrome window is
        // frontmost. Same lazy/no-I/O construction as the others.
        "auto" => Ok(Arc::new(AutoDesktop::new(
            &config.desktop.display,
            &config.cdp.endpoint,
            config.cdp.launch_if_missing,
            &config.cdp.chrome_binary,
        ))),
        other => {
            bail!("unknown desktop backend: {other} (want \"x11\" | \"cdp\" | \"auto\" | \"fake\")")
        }
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
        lease: Lease::new(),
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
    policy: Policy,
    traces: Arc<TraceStore>,
    shell: shell::ShellRunner,
    /// Who may inject input. While a human holds it the loop pauses at
    /// each checkpoint (never killed) and resumes with a fresh observation
    /// on hand-back.
    lease: Lease,
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

/// Goals that claim something is gone need a read-back, not a narration:
/// volume panes, tooltips and dialogs all "look dismissed" until a fresh
/// screenshot proves it. The loop answers with a second consecutive Done
/// instead of finishing on the first claim.
fn goal_needs_removal_check(goal: &str) -> bool {
    let lower = goal.to_lowercase();
    [
        "remove",
        "delete",
        "dismiss",
        "close",
        "hide",
        "uninstall",
        "turn off",
        "disable",
        "get rid of",
        "clear",
        "quit ",
        "exit ",
    ]
    .iter()
    .any(|k| lower.contains(k))
}

/// What an action claims to dismiss, if anything: the words after a
/// dismissal verb in the brain's narration ("Dismissing the indicators
/// tooltip" -> "indicators tooltip"). Used to verify the *outcome* on the
/// next observation instead of trusting that pixels moved: if a
/// distinctive word is still listed as a control, the dismissal did not
/// take and the brain is told so outright.
fn dismissal_target(say: &str) -> Option<String> {
    let lower = say.to_lowercase();
    // Longest forms first: split_once("dismiss") would also match inside
    // "dismissing" and leave a stray "ing" in the target.
    let verbs = [
        "dismissing",
        "closing",
        "removing",
        "deleting",
        "hiding",
        "dismiss",
        "close",
        "remove",
        "delete",
        "hide",
    ];
    let mut rest = None;
    for verb in verbs {
        if let Some((before, aft)) = lower.split_once(verb) {
            // The verb must be a word on its own, not inside another
            // ("enclosing" contains "closing" — reject it).
            let ok_before = before.is_empty() || !before.ends_with(|c: char| c.is_alphanumeric());
            if ok_before {
                rest = Some(aft);
                break;
            }
        }
    }
    let rest = rest?;
    let words: Vec<&str> = rest
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| {
            !w.is_empty()
                && !matches!(
                    *w,
                    "the" | "a" | "an" | "this" | "that" | "my" | "its" | "to" | "it"
                )
        })
        .take(4)
        .collect();
    if words.is_empty() {
        return None;
    }
    Some(words.join(" "))
}

/// Words distinctive enough to search control labels for: length 4+,
/// so "tab" or "bar" never trigger a false still-present.
fn distinctive_words(target: &str) -> Vec<String> {
    target
        .split_whitespace()
        .filter(|w| w.chars().count() >= 4)
        .map(str::to_string)
        .collect()
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
        // Consecutive vision replies that aimed with guessed pixels
        // instead of grid coordinates (refused, see ModelReply::Act).
        let mut pixel_rejects = 0u32;
        // Frame captured while settling after the last action; consumed
        // as the next step's observation instead of re-capturing.
        let mut prefetched: Option<rose_core::Frame> = None;
        // The last dispatched action + the frame it was decided on, so
        // the next observation can report (and feed back) its result.
        let mut pending: Option<(u32, String, Vec<u8>, Option<String>, u64)> = None;
        // Dismissal claimed by the last action ("dismissing the tooltip"):
        // (step, target words). Consumed on the next observation — if the
        // screen changed but the target is still listed as a control, the
        // motion was not progress and the brain is told so outright.
        let mut pending_dismiss: Option<(u32, String)> = None;
        // Text-first loop: a step starts as a text-only brain call
        // (no screenshot captured, nothing sent to vision). Shell
        // work — launching apps, files, processes — is decided and
        // verified from command output alone. The brain must
        // explicitly ask to `look` (or a GUI action must be pending
        // verification) before any screenshot is taken or sent.
        let mut vision = false;
        let mut mode_switches = 0u32;
        let mut idx = 0u32;
        // Read-back arming for removal goals (see goal_needs_removal_check):
        // the first Done forces a fresh observation instead of finishing;
        // only a second consecutive Done — after seeing that frame — ends
        // the run. Any Act/Shell/Look in between disarms (world changed).
        let mut awaiting_second_done = false;
        // Takeover pauses the loop (never kills it): announce once per
        // hold, wait for hand-back (Stop still works while waiting), then
        // resume with a fresh observation — the human may have changed the
        // desktop under us, so nothing cached survives.
        let mut paused = false;
        while idx < self.policy.max_steps {
            check_stop!(idx, acted);
            if self.lease.holder() != Driver::Agent {
                if !paused {
                    emit!(serde_json::json!({
                        "type": "lease", "step": idx, "holder": "human",
                        "say": "Takeover — you hold the desktop, Rose is paused (hand back to resume)",
                    }));
                    tracing::info!(run_id = %run_id, step = idx, "lease held by human, pausing loop");
                    paused = true;
                }
                if unless_stopped(&self.stop_flag, self.lease.wait_for_agent())
                    .await
                    .is_none()
                {
                    finish_stopped!(idx, acted);
                }
                check_stop!(idx, acted);
                prefetched = None;
                pending = None;
                pending_dismiss = None;
                last_action = None;
                last_outcome = None;
                repeats = 0;
                replans = 0;
                pixel_rejects = 0;
                vision = true;
                continue;
            }
            paused = false;
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
                // Grounding (same idx, never a separate step): the
                // constant numbered grid FIRST — it is drawn over every
                // pixel (canvas, native apps, browser alike) and the
                // brain primarily aims by grid cell `g<col>,<row>` or
                // pixels read off the labelled axes. The backend's DOM/AX
                // list, when it has one (CDP/auto-on-Chrome), is only a
                // supplement: its elements travel as text alongside the
                // same gridded image, never instead of the grid.
                // There is no raw brain image in this loop: the only
                // JPEG handed to the brain is this annotated one, and a
                // frame that cannot be gridded fails the step instead of
                // reaching the model raw.
                let grounding_started = std::time::Instant::now();
                let brain_jpeg = match rose_grid::annotate_grid_jpeg(&frame.jpeg) {
                    Ok(annotated) => annotated,
                    Err(e) => {
                        let error = format!("grid overlay failed: {e}");
                        emit!(serde_json::json!({
                            "type": "failed", "step": idx, "stage": "grounding", "error": error,
                        }));
                        finish_err!(error);
                    }
                };
                // Supplementary DOM list (best-effort, never blocks the
                // grid): a dead/slow/empty DOM just means no list.
                let grounded: Option<Vec<Element>>;
                let grounded_dom: bool;
                let ms = grounding_started.elapsed().as_millis() as u64;
                let (elements, source) = match tokio::time::timeout(
                    std::time::Duration::from_secs(15),
                    self.desktop.dom_overlay(&frame.jpeg),
                )
                .await
                {
                    Ok(Ok(Some(o))) if !o.elements.is_empty() => (o.elements, "grid+dom"),
                    Ok(Ok(_)) => (Vec::new(), "grid"),
                    Ok(Err(e)) => {
                        tracing::warn!(error = %e, "dom overlay failed, grid only");
                        (Vec::new(), "grid")
                    }
                    Err(_) => {
                        tracing::warn!("dom overlay timed out, grid only");
                        (Vec::new(), "grid")
                    }
                };
                let overlay = (elements, brain_jpeg, ms, source);
                // The annotated JPEG is the ONLY image the brain ever
                // sees (`step-NNN.jpg` on disk stays the raw capture for
                // humans; the `-grounded` file is the exact image sent).
                let (elements, brain_jpeg, ms, source) = overlay;
                {
                    let grounded_file = trace.as_ref().and_then(|run| {
                        run.save_grounded_frame(idx, &brain_jpeg)
                            .map_err(|e| tracing::warn!(run_id = %run_id, error = %e, "grounded frame save failed"))
                            .ok()
                    });
                    // First labels help judge overlay quality from
                    // the trace alone (cap: full list is in the backend,
                    // not the chat feed).
                    let labels: Vec<String> = elements
                        .iter()
                        .take(8)
                        .map(|e| format!("{}:{}", e.id.0, e.label))
                        .collect();
                    emit!(serde_json::json!({
                        "type": "grounded", "step": idx,
                        "elements": elements.len(),
                        "ms": ms, "source": source,
                        "frame": grounded_file, "labels": labels,
                    }));
                    grounded = Some(elements);
                    grounded_dom = source == "grid+dom";
                }
                // Outcome read-back: the last action claimed a dismissal and
                // the screen did change — but motion is not progress. If a
                // distinctive word from the target is still listed as a
                // control, say so outright instead of letting the brain
                // narrate the flicker as success.
                if let Some((prev_step, target)) = pending_dismiss.take() {
                    if last_outcome == Some(true) {
                        if let Some(els) = grounded.as_deref() {
                            let still_there: Vec<String> = distinctive_words(&target)
                                .into_iter()
                                .filter(|w| els.iter().any(|e| e.label.to_lowercase().contains(w.as_str())))
                                .collect();
                            if !still_there.is_empty() {
                                let say = format!(
                                    "Note: {} still listed after my click — the screen moved but the dismissal did not take; try a different control (Escape first, then another route)",
                                    still_there.join(", ")
                                );
                                emit!(serde_json::json!({
                                    "type": "verify", "step": idx, "target": target,
                                    "present": true, "say": say,
                                }));
                                history.push(StepSummary {
                                    idx,
                                    intent: format!(
                                        "VERIFY step {prev_step} dismissal failed: {target} still present"
                                    ),
                                    say,
                                });
                            }
                        }
                    }
                }
                let brain_started = std::time::Instant::now();
                // The grounded element list travels as text with the
                // screenshot — but only the DOM list: short and semantic.
                // The grid is axis-numbered lines, so there is no list to
                // send; the brain picks a cell or reads pixels off it.
                let grounded_elements: &[Element] = match grounded.as_deref() {
                    Some(els) if grounded_dom => els,
                    _ => &[],
                };
                // Awaited through unless_stopped: pressing Stop while the
                // brain thinks abandons the call instead of acting on a
                // reply the user no longer wants executed.
                let vision_reply = match unless_stopped(&self.stop_flag, async {
                    match self.brain {
                        BrainSvc::Fake => self
                            .fake
                            .next_action(goal, &history, &brain_jpeg, grounded_elements)
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
                                .next_action(goal, &history, &brain_jpeg, grounded_elements)
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
                    // any ping-pong between modes. A look changes nothing,
                    // but it breaks a Done streak: re-arm the removal check.
                    awaiting_second_done = false;
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
                    // Unknown ids replan (never blind-click). Note whether
                    // the brain replied with raw pixels: those are refused
                    // below (grid coordinates only), while element picks
                    // are about to become points legitimately.
                    let replied_pixel = matches!(
                        &action,
                        Action::Click {
                            target: Target::Point { .. },
                            ..
                        } | Action::DoubleClick {
                            target: Target::Point { .. }
                        }
                    );
                    let mut action = action;
                    if let Some(elements) = seen_elements.take() {
                        let resolve = |t: &Target| -> Option<Target> {
                            match t {
                                // Element list first (DOM numbers),
                                // then a grid-cell pick `g<col>,<row>`
                                // resolved against this frame's geometry.
                                Target::Element(id) => resolve_element(&elements, &id.0)
                                    .or_else(|| {
                                        rose_grid::resolve_grid_cell(
                                            &id.0, frame.width, frame.height,
                                        )
                                    })
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
                        // No overlay was recorded for this step and the
                        // brain still replied `element: N`: nothing to map
                        // it through, so replan instead of acting.
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
                            let msg = "I picked an element number with no overlay to map it through; reply with a grid cell g<col>,<row>@fx,fy instead".to_string();
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
                    // Grid coordinates only: a pixel click target here
                    // means the brain guessed coordinates instead of
                    // reading the grid (the (1545,386) price-axis
                    // misclick wasted a whole call that way). Refuse
                    // and replan — element picks already resolved to
                    // points above are legitimate, raw pixels are not.
                    if replied_pixel {
                        pixel_rejects += 1;
                        if pixel_rejects > 2 {
                            let msg = format!(
                                "Stopped: I kept aiming with guessed pixels instead of grid coordinates ({say}). \
                                 Take Control from the live view to do it by hand, then Hand Back."
                            );
                            emit!(serde_json::json!({
                                "type": "stuck", "step": idx, "say": msg,
                                "action": describe_action(&action),
                            }));
                            finish_ok!(step_view_steps(&msg, acted, false, false, idx));
                        }
                        let msg = "Pixel coordinates are not accepted — aiming by grid instead".to_string();
                        emit!(serde_json::json!({
                            "type": "rejected", "step": idx,
                            "action": describe_action(&action), "say": msg,
                        }));
                        history.push(StepSummary {
                            idx,
                            intent: format!(
                                "REJECTED pixel target in {}",
                                describe_action(&action)
                            ),
                            say: "My pixel guess was refused; reply with a grid cell g<col>,<row>@fx,fy or an element number".to_string(),
                        });
                        idx += 1;
                        continue;
                    }
                    pixel_rejects = 0;
                    mode_switches = 0;
                    awaiting_second_done = false;
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
                                 The target is probably not where I am aiming. \
                                 Take Control from the live view to do it by hand, then Hand Back."
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
                                 The target may not be where I am clicking. \
                                 Take Control from the live view to do it by hand, then Hand Back."
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
                    // Outcome check travels with the action: the next
                    // observation verifies the named thing is actually gone.
                    pending_dismiss = dismissal_target(&say).map(|t| (idx, t));
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
                    awaiting_second_done = false;
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
                    if goal_needs_removal_check(goal) && !awaiting_second_done {
                        awaiting_second_done = true;
                        history.push(StepSummary {
                            idx,
                            intent: format!(
                                "claimed done ({say}) — verifying the removal on a fresh look"
                            ),
                            say: "Claimed done; verifying the removal is real before finishing"
                                .to_string(),
                        });
                        vision = true;
                        mode_switches = 0;
                        idx += 1;
                        continue;
                    }
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
        policy: services.config.policy.clone(),
        traces: Arc::clone(&services.traces),
        shell: shell::ShellRunner::new(services.config.desktop.display.clone()),
        lease: services.lease.clone(),
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
        lease: services.lease.clone(),
        control: Some(Arc::new(ControlSource {
            backend: Arc::clone(&services.desktop),
            lease: services.lease.clone(),
        })),
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
        assert_eq!(cfg.desktop.backend, "auto");
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

    #[test]
    fn removal_goals_need_read_back_others_do_not() {
        assert!(goal_needs_removal_check("remove the volume indicator"));
        assert!(goal_needs_removal_check("Delete downloaded CSVs"));
        assert!(goal_needs_removal_check("dismiss the popup"));
        assert!(goal_needs_removal_check("close the settings tab"));
        assert!(goal_needs_removal_check("turn off the alarm"));
        assert!(!goal_needs_removal_check("open the GOOGL chart"));
        assert!(!goal_needs_removal_check("export March invoices"));
        assert!(!goal_needs_removal_check("what time is it"));
    }

    #[test]
    fn dismissal_target_extracts_the_object() {
        assert_eq!(
            dismissal_target("Dismissing the indicators tooltip"),
            Some("indicators tooltip".to_string())
        );
        assert_eq!(
            dismissal_target("Clicking Vol delete icon"),
            Some("icon".to_string()),
            "the clicked control's own name still verifies its removal"
        );
        assert_eq!(
            dismissal_target("Remove the volume pane"),
            Some("volume pane".to_string())
        );
        assert_eq!(
            dismissal_target("enclosing the dialog"),
            None,
            "verb inside another word must not match"
        );
        assert_eq!(dismissal_target("Close"), None, "bare verb, no object");
        assert_eq!(
            distinctive_words("indicators tooltip"),
            vec!["indicators".to_string(), "tooltip".to_string()]
        );
        assert!(distinctive_words("the tab").is_empty());
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
            policy: Policy::default(),
            traces: test_traces(),
            shell: shell::ShellRunner::new(":fake"),
            lease: Lease::new(),
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
                target: rose_core::Target::Element(rose_core::ElementId(
                    "g1,1@0.42,0.42".to_string(),
                )),
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
                target: rose_core::Target::Element(rose_core::ElementId(format!(
                    "g1,1@{:.2},0.42",
                    (x % 25) as f32 / 24.0
                ))),
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
                // The grid floor grounds every vision step even with
                // the default test backend — no raw screenshot is sent.
                "grounded",
                "planned",
                "acting",
                "acted",
                "observing",
                "result",
                "observed",
                "thinking",
                "grounded",
                "planned",
                "acting",
                "acted",
                "observing",
                "result",
                "observed",
                "thinking",
                "grounded",
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
    async fn takeover_pauses_the_loop_without_killing_it() {
        use rose_web::StepRunner;
        // Takeover holds the loop at its next checkpoint; hand-back resumes
        // the same run, which then finishes normally. Nothing is killed and
        // no step is lost: the brain never even sees the pause.
        let lease = Lease::new();
        let click = rose_models::ModelReply::Act {
            action: rose_core::Action::Click {
                target: rose_core::Target::Element(rose_core::ElementId(
                    "g1,1@0.42,0.42".to_string(),
                )),
                button: rose_core::MouseButton::Left,
            },
            say: "Clicking".to_string(),
            enter_after: false,
        };
        let stepper = Arc::new(Stepper {
            // Hand-back forces a fresh vision step, so the script starts
            // there (no text-mode Look handshake).
            fake: FakeBrain::scripted(vec![
                click,
                rose_models::ModelReply::Done {
                    say: "Done".to_string(),
                },
            ]),
            lease: lease.clone(),
            ..test_stepper(BrainSvc::Fake)
        });
        lease.take_human();
        let (tx, mut rx) = event_channel();
        let run = tokio::spawn({
            let stepper = Arc::clone(&stepper);
            async move { stepper.run_step("takeover test", false, None, &tx).await }
        });
        // The loop announces the pause and dispatches nothing while held.
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert!(
            !run.is_finished(),
            "run must wait out the takeover, not exit"
        );
        assert_eq!(
            stepper.fake.call_count(),
            0,
            "no brain call while a human holds the desktop"
        );
        let events = drain(&mut rx);
        assert!(
            events.iter().any(|e| e["type"] == "lease"),
            "pause is narrated, {events:?}"
        );
        assert!(
            !events.iter().any(|e| e["type"] == "acted"),
            "nothing acted while paused"
        );
        // Hand back: the same run resumes and finishes.
        lease.hand_back();
        let view = run.await.expect("join").expect("run");
        assert_eq!(view["done"], true);
        assert_eq!(view["acted"], true);
        assert_eq!(stepper.fake.call_count(), 2, "loop resumed where it paused");
    }

    #[tokio::test]
    async fn removal_goal_verifies_on_a_second_look() {
        use rose_web::StepRunner;
        // "remove the volume pane" + Done, Done: the first claim arms the
        // read-back (fresh vision step), the second — after seeing that
        // frame — finishes. History records the verification.
        let done = || rose_models::ModelReply::Done {
            say: "Volume gone".to_string(),
        };
        let stepper = Stepper {
            fake: FakeBrain::scripted(vec![done(), done()]),
            ..test_stepper(BrainSvc::Fake)
        };
        let (tx, mut rx) = event_channel();
        let view = stepper
            .run_step("remove the volume pane", false, None, &tx)
            .await
            .expect("run");
        assert_eq!(view["done"], true);
        assert_eq!(stepper.fake.call_count(), 2, "claim + verified claim");
        let events = drain(&mut rx);
        assert_eq!(
            events.iter().filter(|e| e["type"] == "decision").count(),
            2,
            "two done decisions, {events:?}"
        );
        assert!(
            events
                .iter()
                .any(|e| e["type"] == "observed" && e["step"] == 1),
            "second claim saw a fresh frame, {events:?}"
        );
    }

    #[tokio::test]
    async fn plain_goal_finishes_on_first_done() {
        use rose_web::StepRunner;
        let stepper = Stepper {
            fake: FakeBrain::scripted(vec![rose_models::ModelReply::Done {
                say: "Chart is open".to_string(),
            }]),
            ..test_stepper(BrainSvc::Fake)
        };
        let (tx, _rx) = event_channel();
        let view = stepper
            .run_step("open the GOOGL chart", false, None, &tx)
            .await
            .expect("run");
        assert_eq!(view["done"], true);
        assert_eq!(stepper.fake.call_count(), 1, "no verification round-trip");
    }

    /// Backend DOM overlay: the Stepper grounds `element: N` from
    /// `dom_overlay`, sends the gridded frame plus the text list, and
    /// resolves the pick to a pixel the backend acts on. Proves the whole
    /// DOM-supplement path end to end.
    struct DomOverlayStub {
        inner: FakeDesktop,
        elements: Vec<rose_core::Element>,
    }

    fn cell_element() -> rose_core::Element {
        // FakeDesktop cell 0 spans (40,40,320,370): a resolved click lands
        // mid-cell and highlights it — pixel proof below.
        rose_core::Element {
            id: rose_core::ElementId("cdp-1".to_string()),
            label: "button: Cell".to_string(),
            role: Some("button".to_string()),
            bounds: rose_core::Rect {
                x: 40,
                y: 40,
                width: 320,
                height: 370,
            },
            confidence: 1.0,
            source: rose_core::ElementSource::Cdp,
        }
    }

    #[async_trait]
    impl DesktopBackend for DomOverlayStub {
        async fn info(&self) -> rose_core::Result<rose_core::DesktopInfo> {
            self.inner.info().await
        }
        async fn capture(&self) -> rose_core::Result<rose_core::Frame> {
            self.inner.capture().await
        }
        async fn act(
            &self,
            action: &rose_core::Action,
            actor: rose_core::Actor,
        ) -> rose_core::Result<()> {
            self.inner.act(action, actor).await
        }
        async fn dom_overlay(
            &self,
            _frame_jpeg: &[u8],
        ) -> rose_core::Result<Option<rose_core::DomOverlay>> {
            Ok(Some(rose_core::DomOverlay {
                elements: self.elements.clone(),
            }))
        }
    }

    #[tokio::test]
    async fn dom_overlay_supplements_the_grid_with_element_picks() {
        use rose_web::StepRunner;
        let desktop = FakeDesktop::new();
        let stepper = Stepper {
            desktop: Arc::new(DomOverlayStub {
                inner: desktop.clone(),
                elements: vec![cell_element()],
            }),
            fake: FakeBrain::scripted(vec![
                rose_models::ModelReply::Look {
                    say: "Need to see the desktop".to_string(),
                },
                rose_models::ModelReply::Act {
                    action: rose_core::Action::Click {
                        target: rose_core::Target::Element(rose_core::ElementId(
                            "cdp-1".to_string(),
                        )),
                        button: rose_core::MouseButton::Left,
                    },
                    say: "Clicking the badged cell".to_string(),
                    enter_after: false,
                },
                rose_models::ModelReply::Done {
                    say: "Clicked".to_string(),
                },
            ]),
            // The DOM source must carry element grounding alone.
            ..test_stepper(BrainSvc::Fake)
        };
        let (tx, mut rx) = event_channel();
        let view = stepper
            .run_step("click the first cell", false, None, &tx)
            .await
            .expect("run");
        assert_eq!(view["done"], true);
        assert_eq!(view["acted"], true);
        assert_eq!(
            desktop.highlighted(),
            Some(0),
            "element: cdp-1 resolved to cell 0's pixels"
        );
        let (jpeg_len, el_count) = stepper.fake.last_vision().expect("vision recorded");
        assert!(jpeg_len > 1000, "gridded frame, not empty");
        assert_eq!(el_count, 1, "element list travelled with the screenshot");
        let events = drain(&mut rx);
        let grounded = events
            .iter()
            .find(|e| e["type"] == "grounded")
            .expect("grounded event");
        assert_eq!(grounded["source"], "grid+dom");
        assert_eq!(grounded["elements"], 1);
    }

    #[tokio::test]
    async fn grid_frame_sends_no_element_text_and_cell_pick_acts() {
        use rose_web::StepRunner;
        // FakeDesktop has no DOM list: the brain sees a numbered-grid
        // frame with no element text and answers with a cell pick.
        let desktop = FakeDesktop::new();
        let stepper = Stepper {
            desktop: Arc::new(desktop.clone()),
            fake: FakeBrain::scripted(vec![
                rose_models::ModelReply::Look {
                    say: "Need to see the desktop".to_string(),
                },
                rose_models::ModelReply::Act {
                    action: rose_core::Action::Click {
                        target: rose_core::Target::Element(rose_core::ElementId(
                            "g9,10@0,0".to_string(),
                        )),
                        button: rose_core::MouseButton::Left,
                    },
                    say: "Clicking the cell I read off the grid".to_string(),
                    enter_after: false,
                },
                rose_models::ModelReply::Done {
                    say: "Clicked".to_string(),
                },
            ]),
            ..test_stepper(BrainSvc::Fake)
        };
        let (tx, mut rx) = event_channel();
        let view = stepper
            .run_step("click the first cell", false, None, &tx)
            .await
            .expect("run");
        assert_eq!(view["done"], true);
        assert_eq!(view["acted"], true);
        assert_eq!(desktop.highlighted(), Some(0), "cell pick lands in cell 0");
        let events = drain(&mut rx);
        let grounded = events
            .iter()
            .find(|e| e["type"] == "grounded")
            .expect("grounded event");
        assert_eq!(grounded["source"], "grid");
        let (_, el_count) = stepper.fake.last_vision().expect("vision recorded");
        assert_eq!(el_count, 0, "grid sends no element text");
    }

    #[tokio::test]
    async fn pixel_click_is_rejected_then_grid_pick_acts() {
        use rose_web::StepRunner;
        // A guessed-pixel reply is refused without acting (one replan);
        // the follow-up grid pick acts normally.
        let desktop = FakeDesktop::new();
        let stepper = Stepper {
            desktop: Arc::new(desktop.clone()),
            fake: FakeBrain::scripted(vec![
                rose_models::ModelReply::Look {
                    say: "Need to see the desktop".to_string(),
                },
                rose_models::ModelReply::Act {
                    action: rose_core::Action::Click {
                        target: rose_core::Target::Point { x: 200, y: 225 },
                        button: rose_core::MouseButton::Left,
                    },
                    say: "Clicking by guessed pixels".to_string(),
                    enter_after: false,
                },
                rose_models::ModelReply::Act {
                    action: rose_core::Action::Click {
                        target: rose_core::Target::Element(rose_core::ElementId(
                            "g9,10@0,0".to_string(),
                        )),
                        button: rose_core::MouseButton::Left,
                    },
                    say: "Clicking grid cell 9,10".to_string(),
                    enter_after: false,
                },
                rose_models::ModelReply::Done {
                    say: "Clicked".to_string(),
                },
            ]),
            ..test_stepper(BrainSvc::Fake)
        };
        let (tx, mut rx) = event_channel();
        let view = stepper
            .run_step("click the first cell", false, None, &tx)
            .await
            .expect("run");
        assert_eq!(view["done"], true);
        assert_eq!(view["acted"], true);
        assert_eq!(desktop.highlighted(), Some(0), "only the grid pick acted");
        let events = drain(&mut rx);
        let rejected = events
            .iter()
            .find(|e| e["type"] == "rejected")
            .expect("pixel reply rejected");
        assert_eq!(rejected["action"], "click (200, 225) [Left]");
        assert!(
            rejected["say"].as_str().unwrap_or("").contains("Pixel"),
            "{rejected}"
        );
    }

    #[tokio::test]
    async fn repeated_pixel_clicks_stop_the_run() {
        use rose_web::StepRunner;
        // Three pixel replies in a row: refused, refused, stuck-stop —
        // never acted on.
        let pixel = || rose_models::ModelReply::Act {
            action: rose_core::Action::Click {
                target: rose_core::Target::Point { x: 200, y: 225 },
                button: rose_core::MouseButton::Left,
            },
            say: "Clicking by guessed pixels".to_string(),
            enter_after: false,
        };
        let desktop = FakeDesktop::new();
        let stepper = Stepper {
            desktop: Arc::new(desktop.clone()),
            fake: FakeBrain::scripted(vec![
                rose_models::ModelReply::Look {
                    say: "Need to see the desktop".to_string(),
                },
                pixel(),
                pixel(),
                pixel(),
                rose_models::ModelReply::Done {
                    say: "Clicked".to_string(),
                },
            ]),
            ..test_stepper(BrainSvc::Fake)
        };
        let (tx, _rx) = event_channel();
        let view = stepper
            .run_step("click the first cell", false, None, &tx)
            .await
            .expect("run");
        assert_eq!(view["acted"], false);
        assert_eq!(desktop.highlighted(), None, "no pixel click ever acted");
        assert!(
            view["say"].as_str().unwrap_or("").contains("guessed pixels"),
            "{view}"
        );
    }

    #[tokio::test]
    async fn grid_cell_pick_resolves_to_cell_centre() {
        use rose_web::StepRunner;
        // The brain names a cell from the numbered axes ("g5,5"); the
        // Stepper clicks that cell's centre (112,112 on the 1600x900
        // fake frame), which lands inside FakeDesktop cell 0.
        let desktop = FakeDesktop::new();
        let stepper = Stepper {
            desktop: Arc::new(desktop.clone()),
            fake: FakeBrain::scripted(vec![
                rose_models::ModelReply::Look {
                    say: "Need to see the desktop".to_string(),
                },
                rose_models::ModelReply::Act {
                    action: rose_core::Action::Click {
                        target: rose_core::Target::Element(rose_core::ElementId(
                            "g5,5".to_string(),
                        )),
                        button: rose_core::MouseButton::Left,
                    },
                    say: "Clicking grid cell 5,5".to_string(),
                    enter_after: false,
                },
                rose_models::ModelReply::Done {
                    say: "Clicked".to_string(),
                },
            ]),
            ..test_stepper(BrainSvc::Fake)
        };
        let (tx, _rx) = event_channel();
        let view = stepper
            .run_step("click cell 5,5", false, None, &tx)
            .await
            .expect("run");
        assert_eq!(view["done"], true);
        assert_eq!(view["acted"], true);
        assert_eq!(
            desktop.highlighted(),
            Some(0),
            "grid cell g5,5 centre lands in cell 0"
        );
    }

    #[tokio::test]
    async fn vision_steps_always_send_a_gridded_screenshot() {
        use rose_web::StepRunner;
        // No DOM list on the fake desktop: the brain still only ever
        // receives the numbered-grid frame (source "grid"), never raw.
        let stepper = Stepper {
            fake: FakeBrain::scripted(vec![
                rose_models::ModelReply::Look {
                    say: "Need to see the desktop".to_string(),
                },
                rose_models::ModelReply::Done {
                    say: "Seen it".to_string(),
                },
            ]),
            ..test_stepper(BrainSvc::Fake)
        };
        let (tx, mut rx) = event_channel();
        let view = stepper
            .run_step("look at the desktop", false, None, &tx)
            .await
            .expect("run");
        assert_eq!(view["done"], true);
        let events = drain(&mut rx);
        let grounded = events
            .iter()
            .find(|e| e["type"] == "grounded")
            .expect("grounded event on every vision step");
        assert_eq!(grounded["source"], "grid");
        let (jpeg_len, _) = stepper.fake.last_vision().expect("vision recorded");
        assert!(jpeg_len > 1000, "gridded frame, not empty");
    }

    /// A frame the grid cannot annotate must fail the step loudly instead
    /// of burning a ~30s brain call on a raw screenshot with no grounding.
    struct CorruptFrameDesktop {
        inner: FakeDesktop,
    }

    #[async_trait]
    impl DesktopBackend for CorruptFrameDesktop {
        async fn info(&self) -> rose_core::Result<rose_core::DesktopInfo> {
            self.inner.info().await
        }
        async fn capture(&self) -> rose_core::Result<rose_core::Frame> {
            Ok(rose_core::Frame {
                id: rose_core::FrameId::new(),
                captured_at: chrono::Utc::now(),
                width: 1600,
                height: 900,
                jpeg: b"not a jpeg".to_vec(),
                active_window: None,
            })
        }
        async fn act(
            &self,
            action: &rose_core::Action,
            actor: rose_core::Actor,
        ) -> rose_core::Result<()> {
            self.inner.act(action, actor).await
        }
    }

    #[tokio::test]
    async fn grid_failure_fails_before_any_brain_call() {
        use rose_web::StepRunner;
        let stepper = Stepper {
            desktop: Arc::new(CorruptFrameDesktop {
                inner: FakeDesktop::new(),
            }),
            // Look first: text mode hands off to vision, where grounding
            // fails on the corrupt frame.
            fake: FakeBrain::scripted(vec![
                rose_models::ModelReply::Look {
                    say: "Need to see the desktop".to_string(),
                },
                rose_models::ModelReply::Done {
                    say: "Done".to_string(),
                },
            ]),
            ..test_stepper(BrainSvc::Fake)
        };
        let (tx, mut rx) = event_channel();
        let err = stepper
            .run_step("click the first cell", false, None, &tx)
            .await
            .expect_err("grid failure must fail the step, not fall back to raw");
        assert!(err.contains("grid overlay failed"), "{err}");
        assert_eq!(
            stepper.fake.last_vision(),
            Some((0, 0)),
            "only the empty text handshake ran; no vision call saw the raw frame"
        );
        let events = drain(&mut rx);
        assert!(
            events
                .iter()
                .any(|e| e["type"] == "failed" && e["stage"] == "grounding"),
            "failure is streamed with its stage, {events:?}"
        );
    }

    fn tooltip_element() -> rose_core::Element {
        rose_core::Element {
            id: rose_core::ElementId("cdp-2".to_string()),
            label: "tooltip: Help".to_string(),
            role: Some("tooltip".to_string()),
            bounds: rose_core::Rect {
                x: 460,
                y: 60,
                width: 200,
                height: 60,
            },
            confidence: 1.0,
            source: rose_core::ElementSource::Cdp,
        }
    }

    fn dismiss_then_done_script(say: &str) -> Vec<rose_models::ModelReply> {
        vec![
            rose_models::ModelReply::Look {
                say: "Need to see the desktop".to_string(),
            },
            rose_models::ModelReply::Act {
                action: rose_core::Action::Click {
                    target: rose_core::Target::Element(rose_core::ElementId(
                            "g9,10@0,0".to_string(),
                        )),
                    button: rose_core::MouseButton::Left,
                },
                say: say.to_string(),
                enter_after: false,
            },
            rose_models::ModelReply::Done {
                say: "Done".to_string(),
            },
        ]
    }

    #[tokio::test]
    async fn dismissal_still_present_is_flagged_not_trusted() {
        use rose_web::StepRunner;
        // The click lands (frame changes) but the tooltip is still listed:
        // motion without progress must be said outright.
        let desktop = FakeDesktop::new();
        let stepper = Stepper {
            desktop: Arc::new(DomOverlayStub {
                inner: desktop.clone(),
                elements: vec![cell_element(), tooltip_element()],
            }),
            fake: FakeBrain::scripted(dismiss_then_done_script("Dismissing the help tooltip")),
            ..test_stepper(BrainSvc::Fake)
        };
        let (tx, mut rx) = event_channel();
        let view = stepper
            .run_step("click the tooltip", false, None, &tx)
            .await
            .expect("run");
        assert_eq!(view["done"], true);
        let events = drain(&mut rx);
        let verify = events
            .iter()
            .find(|e| e["type"] == "verify")
            .expect("verify event");
        assert_eq!(verify["present"], true);
        assert!(
            verify["say"].as_str().unwrap_or("").contains("tooltip"),
            "{verify}"
        );
    }

    #[tokio::test]
    async fn dismissal_gone_passes_quietly() {
        use rose_web::StepRunner;
        // Same click, but the tooltip is gone from the next listing:
        // nothing to flag, no verify event.
        let desktop = FakeDesktop::new();
        let stepper = Stepper {
            desktop: Arc::new(DomOverlayStub {
                inner: desktop.clone(),
                elements: vec![cell_element()],
            }),
            fake: FakeBrain::scripted(dismiss_then_done_script("Dismissing the help tooltip")),
            ..test_stepper(BrainSvc::Fake)
        };
        let (tx, mut rx) = event_channel();
        let view = stepper
            .run_step("click the tooltip", false, None, &tx)
            .await
            .expect("run");
        assert_eq!(view["done"], true);
        let events = drain(&mut rx);
        assert!(
            !events.iter().any(|e| e["type"] == "verify"),
            "gone target needs no flag, {events:?}"
        );
    }

    #[tokio::test]
    async fn loop_stops_when_the_same_click_repeats() {
        use rose_web::StepRunner;
        let click = || rose_models::ModelReply::Act {
            action: rose_core::Action::Click {
                target: rose_core::Target::Element(rose_core::ElementId(
                    "g1,1@0.42,0.42".to_string(),
                )),
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
                target: rose_core::Target::Element(rose_core::ElementId(format!(
                    "g{},{}@{:.2},{:.2}",
                    x / 25 + 1,
                    y / 25 + 1,
                    (x % 25) as f32 / 24.0,
                    (y % 25) as f32 / 24.0
                ))),
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
                target: rose_core::Target::Element(rose_core::ElementId(format!(
                    "g{},10@{:.2},0",
                    x / 25 + 1,
                    (x % 25) as f32 / 24.0
                ))),
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
    fn cdp_selector_constructs_without_chrome() {
        // Construction is lazy: no CDP handshake until first capture/act,
        // so rosed starts even when Chrome isn't up yet.
        let mut cfg = RoseConfig::default();
        cfg.desktop.backend = "cdp".to_string();
        cfg.models.brain = "fake".to_string();
        build_services(cfg, ApiKeys::default()).expect("cdp selector builds");
    }

    #[test]
    fn auto_selector_constructs_without_display_or_chrome() {
        // Router is lazy on both sides: X per call, CDP on first Chrome capture.
        let mut cfg = RoseConfig::default();
        cfg.desktop.backend = "auto".to_string();
        cfg.models.brain = "fake".to_string();
        build_services(cfg, ApiKeys::default()).expect("auto selector builds");
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
