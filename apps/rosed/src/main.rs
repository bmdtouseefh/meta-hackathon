//! `rosed` composition root: config -> factories -> run.
//!
//! IMPLEMENTATION.md T0.3. CLI owns config loading (file + env for secrets)
//! and selects implementations via factory fns keyed on config. Real
//! backends land in later tasks; unknown selectors fail loudly here.

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use clap::{Parser, Subcommand};
use rose_agent::{Driver, Lease};
use rose_core::{Action, Actor, DesktopBackend, Element, Policy, RoseConfig, StepSummary, Target};
use rose_desktop_fake::FakeDesktop;
use rose_desktop_x11::X11Desktop;
use rose_models::{resolve_element, FakeBrain, ModelReply, MuseClient};
use rose_store::TraceStore;
use std::collections::HashMap;
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
    /// Optional separate key for video → instructions. When unset, video
    /// calls fall back to `muse_api_key` (old behaviour: one key for all).
    video_api_key: Option<String>,
}

impl ApiKeys {
    /// First non-empty value of the given env vars, if any.
    fn env_first(vars: &[&str]) -> Option<String> {
        vars.iter()
            .filter_map(|k| std::env::var(k).ok())
            .map(|v| v.trim().to_string())
            .find(|v| !v.is_empty())
    }

    fn from_env() -> Self {
        Self {
            muse_api_key: Self::env_first(&["ROSE_MUSE_API_KEY"]),
            video_api_key: Self::env_first(&[
                "ROSE_VIDEO_API_KEY",
                "ROSE_MUSE_VIDEO_API_KEY",
                "OPENAI_VIDEO_API_KEY",
            ]),
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

    /// `<set>` / `<missing>` / `<follows muse key>` — value never printed.
    fn video_status(&self) -> &'static str {
        if self.video_api_key.is_some() {
            "<set>"
        } else if self.muse_api_key.is_some() {
            "<follows muse key>"
        } else {
            "<missing>"
        }
    }
}

/// Effective video key: explicit video key (web Setup, then env) wins;
/// otherwise the brain key (web Setup, then env). `None` = none configured.
fn resolve_video_key(
    video_keys: &rose_web::KeySlot,
    video_env_key: &Option<String>,
    brain_keys: &rose_web::KeySlot,
    brain_env_key: &Option<String>,
) -> Option<String> {
    video_keys
        .get()
        .or_else(|| video_env_key.clone())
        .or_else(|| brain_keys.get())
        .or_else(|| brain_env_key.clone())
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

/// Generalized text instructions for a skill drafted from a video:
/// the task stated once, for any new {{parameter}} values, with the
/// user's own wording retained in every step and verbatim quote.
fn generalized_instructions(video_name: &str, steps: &[rose_web::VideoStep]) -> String {
    let mut out = format!(
        "Generalized from the user's narrated recording \"{video_name}\". \
         Do this task for any new values of its {{{{parameters}}}} — keep the user's \
         own wording below, swapping only the {{{{slots}}}} for the new run's values."
    );
    if steps.is_empty() {
        out.push_str("\n\n(no steps drafted yet)");
        return out;
    }
    for (i, s) in steps.iter().enumerate() {
        let intent = if s.intent.is_empty() {
            &s.say
        } else {
            &s.intent
        };
        out.push_str(&format!("\n{}. {intent}", i + 1));
        if !s.params.is_empty() {
            out.push_str(&format!(" [{}]", s.params));
        }
        if s.approval {
            out.push_str(" (needs approval)");
        }
        if !s.quote.is_empty() {
            out.push_str(&format!("\n   User said: \"{}\"", s.quote));
        }
    }
    out
}

/// MCP `video.*` / `skill.*` service: video -> generalized instructions
/// -> saved skill + `skill.md`, exposed to any MCP client (and the chat
/// UI's skill panel) through the same tools as desktop/shell/test.
struct McpSkills {
    videos: Arc<FileVideos>,
    skills: Arc<SkillAdapter>,
}

#[async_trait]
impl rose_mcp::SkillService for McpSkills {
    async fn list_videos(&self) -> Vec<serde_json::Value> {
        use rose_web::VideoSource;
        self.videos
            .list()
            .await
            .into_iter()
            .filter_map(|v| serde_json::to_value(v).ok())
            .collect()
    }

    async fn video_to_skill(
        &self,
        video_id: &str,
        name: Option<&str>,
    ) -> Result<serde_json::Value, String> {
        use rose_web::{SkillSource, VideoSource};
        // Drafts (or re-drafts) steps with Spark and persists steps.json
        // + instructions.md next to the upload.
        let detail = self.videos.generate(video_id).await?;
        let name = name
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| {
                detail
                    .meta
                    .name
                    .rsplit('.')
                    .nth(1)
                    .filter(|s| !s.is_empty())
                    .unwrap_or(&detail.meta.name)
                    .to_string()
            });
        let instructions = generalized_instructions(&detail.meta.name, &detail.steps);
        let steps: Vec<serde_json::Value> = detail
            .steps
            .iter()
            .filter_map(|s| serde_json::to_value(s).ok())
            .collect();
        let skill = self
            .skills
            .create(&name, &instructions, steps, &detail.meta.id)
            .await?;
        let skill_md = rose_web::skill_markdown(&skill);
        Ok(serde_json::json!({
            "skill_id": skill.get("id"),
            "name": name,
            "video_id": detail.meta.id,
            "instructions": instructions,
            "skill_md": skill_md,
            "steps": detail.steps,
        }))
    }

    async fn list_skills(&self) -> Vec<serde_json::Value> {
        use rose_web::SkillSource;
        self.skills.list().await
    }

    async fn skill(&self, skill_id: &str) -> Option<serde_json::Value> {
        use rose_web::SkillSource;
        self.skills.get(skill_id).await
    }

    async fn skill_doc(&self, skill_id: &str) -> Option<String> {
        use rose_web::SkillSource;
        let skill = self.skills.get(skill_id).await?;
        Some(rose_web::skill_markdown(&skill))
    }

    async fn update_skill(
        &self,
        skill_id: &str,
        name: Option<&str>,
        instructions: Option<&str>,
        steps: Option<Vec<serde_json::Value>>,
    ) -> Result<serde_json::Value, String> {
        use rose_web::SkillSource;
        let current = self
            .skills
            .get(skill_id)
            .await
            .ok_or_else(|| format!("unknown skill: {skill_id}"))?;
        let name = name
            .filter(|n| !n.trim().is_empty())
            .map(str::to_string)
            .or_else(|| {
                current
                    .get("name")
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
            })
            .unwrap_or_default();
        let instructions = instructions
            .map(str::to_string)
            .or_else(|| {
                current
                    .get("instructions")
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
            })
            .unwrap_or_default();
        let steps = steps.or_else(|| {
            current
                .get("steps")
                .and_then(|v| v.as_array())
                .cloned()
        });
        let steps = steps.unwrap_or_default();
        self.skills.update(skill_id, &name, &instructions, steps).await
    }
}

/// Skill library adapter: the composition root exposing the trace
/// store's skills table to the web layer.
struct SkillAdapter {
    store: Arc<TraceStore>,
    data: PathBuf,
}

#[async_trait]
impl rose_web::SkillSource for SkillAdapter {
    async fn list(&self) -> Vec<serde_json::Value> {
        self.store
            .list_skills()
            .into_iter()
            .filter_map(|s| serde_json::to_value(s).ok())
            .collect()
    }

    async fn create(
        &self,
        name: &str,
        instructions: &str,
        steps: Vec<serde_json::Value>,
        source: &str,
    ) -> Result<serde_json::Value, String> {
        let skill = self
            .store
            .create_skill(name, instructions, &steps, source)?;
        let value =
            serde_json::to_value(&skill).map_err(|e| format!("could not serialize skill: {e}"))?;
        self.write_doc(&skill.id, &value);
        Ok(value)
    }

    async fn get(&self, id: &str) -> Option<serde_json::Value> {
        let skill = self.store.get_skill(id)?;
        serde_json::to_value(skill).ok()
    }

    async fn update(
        &self,
        id: &str,
        name: &str,
        instructions: &str,
        steps: Vec<serde_json::Value>,
    ) -> Result<serde_json::Value, String> {
        let skill = self.store.update_skill(id, name, instructions, &steps)?;
        let value =
            serde_json::to_value(&skill).map_err(|e| format!("could not serialize skill: {e}"))?;
        self.write_doc(&skill.id, &value);
        Ok(value)
    }

    async fn remove(&self, id: &str) -> bool {
        if !self.store.delete_skill(id) {
            return false;
        }
        // Skills own on-disk artifacts (skill.md, cached step frames);
        // drop the whole dir so deletes don't litter the data dir.
        // Guard the join: skill ids are uuids, never paths.
        if !id.is_empty()
            && id.len() <= 64
            && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        {
            let _ = std::fs::remove_dir_all(self.data.join("skills").join(id));
        }
        true
    }

    async fn frame(&self, id: &str, n: usize) -> Option<Vec<u8>> {
        let skill = self.store.get_skill(id)?;
        let step = skill.steps.get(n)?;
        let ts = step
            .get("ts")
            .and_then(|v| v.as_u64())
            .unwrap_or(0)
            .min(6 * 3600);
        let dir = self.data.join("skills").join(&skill.id);
        let cached = dir.join(format!("step-{n:03}.jpg"));
        if let Ok(bytes) = std::fs::read(&cached) {
            if bytes.len() > 100 {
                return Some(bytes);
            }
        }
        let video = self.find_source_video(&skill.source)?;
        if std::fs::create_dir_all(&dir).is_err() {
            return None;
        }
        // Seek fast (before -i), one frame, web-friendly quality.
        let out = tokio::time::timeout(
            std::time::Duration::from_secs(90),
            tokio::process::Command::new("ffmpeg")
                .args(["-y", "-v", "error", "-ss", &ts.to_string(), "-i"])
                .arg(&video)
                .args(["-frames:v", "1", "-q:v", "4"])
                .arg(&cached)
                .output(),
        )
        .await
        .ok()?
        .ok()?;
        if !out.status.success() {
            return None;
        }
        match std::fs::read(&cached) {
            Ok(bytes) if bytes.len() > 100 => Some(bytes),
            _ => None,
        }
    }
}

impl SkillAdapter {
    /// Persist the skill's markdown doc next to its cached frames:
    /// `<data>/skills/<id>/skill.md`. Same builder as the /doc
    /// endpoint, so file and API can never disagree. Best-effort —
    /// a write failure must not fail the save itself.
    fn write_doc(&self, id: &str, skill: &serde_json::Value) {
        let dir = self.data.join("skills").join(id);
        if std::fs::create_dir_all(&dir).is_err() {
            return;
        }
        let doc = rose_web::skill_markdown(skill);
        if let Err(e) = std::fs::write(dir.join("skill.md"), doc) {
            tracing::warn!(skill_id = %id, error = %e, "could not write skill.md");
        }
    }

    /// Locate the recording a skill was taught from: a video dir id
    /// matches directly, otherwise fall back to matching meta.json's
    /// stored upload name (skills record the name at save time).
    fn find_source_video(&self, source: &str) -> Option<PathBuf> {
        let videos = self.data.join("videos");
        let direct = videos.join(source).join("meta.json");
        if std::fs::metadata(&direct).is_ok() {
            return Self::original_in(&videos.join(source));
        }
        let entries = std::fs::read_dir(&videos).ok()?;
        for entry in entries.filter_map(|e| e.ok()) {
            let meta_path = entry.path().join("meta.json");
            let meta = std::fs::read_to_string(&meta_path).ok()?;
            let name: String = serde_json::from_str::<serde_json::Value>(&meta)
                .ok()?
                .get("name")?
                .as_str()?
                .to_string();
            if name == source {
                return Self::original_in(&entry.path());
            }
        }
        None
    }

    /// The stored upload, preferring the exact `original.*` file.
    fn original_in(dir: &std::path::Path) -> Option<PathBuf> {
        let entries = std::fs::read_dir(dir).ok()?;
        let mut fallback = None;
        for entry in entries.filter_map(|e| e.ok()) {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with("original.") {
                return Some(entry.path());
            }
            if fallback.is_none() && name.ends_with(".mp4") {
                fallback = Some(entry.path());
            }
        }
        fallback
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
    /// Live view is the raw desktop (no grid: prettier to watch, and
    /// the brain never reads this path — the Stepper captures from the
    /// backend directly and annotates its own copy, so grid grounding
    /// is unaffected. Timeline grounded frames still show exactly what
    /// the brain saw on any step.
    async fn jpeg(&self) -> std::result::Result<Vec<u8>, String> {
        let frame = self.backend.capture().await.map_err(|e| e.to_string())?;
        Ok(frame.jpeg)
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

    /// Fresh frame so fractions map to the backend's *current* geometry.
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
    config_path: Option<PathBuf>,
    api_keys: ApiKeys,
    desktop: Arc<dyn DesktopBackend>,
    brain: BrainSvc,
    traces: Arc<TraceStore>,
    lease: Lease,
    mcp_policy: rose_mcp::McpPolicy,
}

/// One live MCP test run: the `TraceRun` handle owns the append-only JSONL
/// writer, so it stays in this map from `test.start` to `test.finish`.
struct TestSessionImpl {
    store: Arc<TraceStore>,
    active: std::sync::Mutex<HashMap<String, rose_store::TraceRun>>,
}

impl TestSessionImpl {
    fn new(store: Arc<TraceStore>) -> Self {
        Self {
            store,
            active: std::sync::Mutex::new(HashMap::new()),
        }
    }
}

#[async_trait]
impl rose_mcp::TestSession for TestSessionImpl {
    async fn start(&self, goal: &str) -> String {
        match self.store.begin(goal, None) {
            Ok(run) => {
                let id = run.run_id().to_string();
                tracing::info!(run_id = %id, goal = %goal, "mcp test started");
                self.active
                    .lock()
                    .expect("test sessions")
                    .insert(id.clone(), run);
                id
            }
            Err(e) => {
                tracing::warn!(error = %e, "could not start test trace; using ephemeral id");
                format!("ephemeral-{}", uuid::Uuid::new_v4())
            }
        }
    }

    async fn record_act(&self, run_id: &str, action: &Action, actor: Actor) {
        let event = serde_json::json!({
            "type": "acted",
            "action": describe_action(action),
            "actor": format!("{actor:?}").to_lowercase(),
        });
        match self.active.lock().expect("test sessions").get_mut(run_id) {
            Some(run) => {
                run.record(event);
            }
            None => {
                tracing::debug!(run_id = %run_id, "no live test run for action");
            }
        }
    }

    async fn finish(&self, run_id: &str, passed: bool, summary: &str) -> bool {
        let mut active = self.active.lock().expect("test sessions");
        match active.remove(run_id) {
            Some(mut run) => {
                let event = if passed {
                    serde_json::json!({
                        "type": "final", "say": summary,
                        "acted": true, "approval_needed": false, "done": true,
                    })
                } else {
                    serde_json::json!({
                        "type": "final", "say": summary, "error": summary,
                        "acted": true, "approval_needed": false, "done": false,
                    })
                };
                run.record(event);
                tracing::info!(run_id = %run_id, passed, "mcp test finished");
                true
            }
            None => {
                tracing::debug!(run_id = %run_id, "finish for unknown test run");
                false
            }
        }
    }
}

/// Live config shared between the web layer (GET/PUT /api/config) and the
/// running loop. Policy + brain hot-reload on next run; backend/display/
/// bind/data report `restart_needed` until `rosed` restarts.
struct FileConfig {
    path: Option<PathBuf>,
    current: Arc<tokio::sync::RwLock<RoseConfig>>,
}

#[async_trait]
impl rose_web::ConfigSource for FileConfig {
    async fn get(&self) -> RoseConfig {
        self.current.read().await.clone()
    }

    async fn set(&self, cfg: RoseConfig) -> Result<(RoseConfig, bool), String> {
        rose_web::validate_config(&cfg)?;
        let old = self.current.read().await.clone();
        let restart = rose_web::config_restart_needed(&old, &cfg);
        *self.current.write().await = cfg.clone();
        if let Some(path) = &self.path {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("could not create {}: {e}", parent.display()))?;
            }
            let text = toml::to_string_pretty(&cfg)
                .map_err(|e| format!("could not serialize config: {e}"))?;
            std::fs::write(path, text)
                .map_err(|e| format!("could not write {}: {e}", path.display()))?;
        }
        tracing::info!(restart_needed = restart, "config updated via UI");
        Ok((cfg, restart))
    }

    fn source_name(&self) -> String {
        match &self.path {
            Some(p) => format!("rosed (/api/config, {})", p.display()),
            None => "rosed (/api/config, defaults)".to_string(),
        }
    }
}

/// File-backed video store: `<data>/videos/<id>/` holds the original
/// upload, `meta.json`, `steps.json`, and `job.json` (`{video_path,
/// out_dir}`). Instructions come from Muse Spark native video
/// understanding (Files API + Responses API, one call reads frames and
/// narration together) — no SAM stage, no separate transcription stage.
struct FileVideos {
    root: PathBuf,
    keys: rose_web::KeySlot,
    env_key: Option<String>,
    video_keys: rose_web::KeySlot,
    video_env_key: Option<String>,
    live: Arc<tokio::sync::RwLock<RoseConfig>>,
}

impl FileVideos {
    fn new(
        data_dir: PathBuf,
        keys: rose_web::KeySlot,
        env_key: Option<String>,
        video_keys: rose_web::KeySlot,
        video_env_key: Option<String>,
        live: Arc<tokio::sync::RwLock<RoseConfig>>,
    ) -> Self {
        Self {
            root: data_dir.join("videos"),
            keys,
            env_key,
            video_keys,
            video_env_key,
            live,
        }
    }

    fn dir(&self, id: &str) -> Result<PathBuf, String> {
        if !rose_web::valid_video_id(id) {
            return Err("unknown video".to_string());
        }
        Ok(self.root.join(id))
    }

    fn read_detail(&self, id: &str) -> Option<rose_web::VideoDetail> {
        let dir = self.dir(id).ok()?;
        let meta_text = std::fs::read_to_string(dir.join("meta.json")).ok()?;
        let meta: rose_web::VideoMeta = serde_json::from_str(&meta_text).ok()?;
        let steps: Vec<rose_web::VideoStep> = std::fs::read_to_string(dir.join("steps.json"))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        Some(rose_web::VideoDetail { meta, steps })
    }

    fn write_meta(&self, dir: &std::path::Path, meta: &rose_web::VideoMeta) -> Result<(), String> {
        let text = serde_json::to_string_pretty(meta).map_err(|e| e.to_string())?;
        std::fs::write(dir.join("meta.json"), text).map_err(|e| e.to_string())?;
        Ok(())
    }

    /// The video's instructions as a markdown file next to steps.json:
    /// simple numbered lines a human or model can read without parsing
    /// JSON. Written on every generate/save so the file never stales.
    fn write_instructions_md(
        &self,
        dir: &std::path::Path,
        meta: &rose_web::VideoMeta,
        steps: &[rose_web::VideoStep],
    ) {
        let stem = meta
            .name
            .rsplit('.')
            .nth(1)
            .filter(|s| !s.is_empty())
            .unwrap_or(&meta.name);
        let mut doc = format!(
            "# {stem}\n\nGeneralized instructions from the user's narrated recording \"{}\". \
             Do this task for any new values of its {{{{parameters}}}} — the user's own wording \
             is retained in each step and in their verbatim quotes.\n",
            meta.name
        );
        if steps.is_empty() {
            doc.push_str("\n_(no steps yet)_\n");
        }
        for (i, s) in steps.iter().enumerate() {
            let intent = if s.intent.is_empty() {
                &s.say
            } else {
                &s.intent
            };
            doc.push_str(&format!("\n{}. {intent}", i + 1));
            if !s.params.is_empty() {
                doc.push_str(&format!(" [{}]", s.params));
            }
            if s.approval {
                doc.push_str(" (needs approval)");
            }
            doc.push('\n');
            if !s.quote.is_empty() {
                doc.push_str(&format!("   > User said: \"{}\"\n", s.quote));
            }
        }
        doc.push_str(&format!(
            "\n_{} step{} · drafted {} — full evidence in steps.json_\n",
            steps.len(),
            if steps.len() == 1 { "" } else { "s" },
            meta.created_at,
        ));
        if let Err(e) = std::fs::write(dir.join("instructions.md"), doc) {
            tracing::warn!(video = %meta.id, error = %e, "could not write instructions.md");
        }
    }

    fn original_path(&self, dir: &std::path::Path, name: &str) -> PathBuf {
        let ext = name
            .rsplit('.')
            .next()
            .filter(|e| e.len() <= 5 && e.chars().all(|c| c.is_ascii_alphanumeric()))
            .map(|e| format!(".{}", e.to_ascii_lowercase()))
            .unwrap_or_else(|| ".mp4".to_string());
        dir.join(format!("original{ext}"))
    }

    fn original_bytes(&self, dir: &std::path::Path, name: &str) -> Result<Vec<u8>, String> {
        // Prefer the exact stored path, fall back to any original.* file.
        let direct = self.original_path(dir, name);
        if let Ok(bytes) = std::fs::read(&direct) {
            return Ok(bytes);
        }
        let entries = std::fs::read_dir(dir).map_err(|e| e.to_string())?;
        for entry in entries.filter_map(|e| e.ok()) {
            if entry.file_name().to_string_lossy().starts_with("original.") {
                return std::fs::read(entry.path()).map_err(|e| e.to_string());
            }
        }
        Err("uploaded video file is missing".to_string())
    }
}

/// Spark only reads `video/mp4`: mp4 uploads pass through, anything else
/// is normalized with ffmpeg (ships on the VM per deploy/deb.sh).
/// Returns the mp4 bytes plus the filename to report to the API.
async fn ensure_mp4(name: &str, bytes: Vec<u8>) -> Result<(Vec<u8>, String), String> {
    if name.to_ascii_lowercase().ends_with(".mp4") {
        return Ok((bytes, rose_web::sanitize_video_name(name)));
    }
    let probe = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        tokio::process::Command::new("ffmpeg")
            .arg("-version")
            .output(),
    )
    .await;
    match probe {
        Ok(Ok(o)) if o.status.success() => {}
        _ => {
            return Err(format!(
                "video is {} — Muse Spark only reads mp4; install ffmpeg on the VM or upload an mp4",
                name.rsplit('.').next().unwrap_or("unknown")
            ));
        }
    }
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let tmp = std::env::temp_dir().join(format!("rose-vidconv-{}-{n}", std::process::id()));
    let stem = name
        .rsplit('/')
        .next()
        .unwrap_or(name)
        .rsplit('.')
        .nth(1)
        .unwrap_or("video");
    let stem = rose_web::sanitize_video_name(stem);
    let stem = stem.trim_end_matches(".mp4").to_string();
    let out_name = format!("{stem}.mp4");
    let convert = async {
        std::fs::create_dir_all(&tmp).map_err(|e| e.to_string())?;
        let inp = tmp.join(format!("in-{stem}"));
        let outp = tmp.join("out.mp4");
        std::fs::write(&inp, &bytes).map_err(|e| e.to_string())?;
        let res = tokio::time::timeout(
            std::time::Duration::from_secs(180),
            tokio::process::Command::new("ffmpeg")
                .args([
                    "-y",
                    "-i",
                    &inp.to_string_lossy(),
                    "-c:v",
                    "libx264",
                    "-preset",
                    "veryfast",
                    "-crf",
                    "23",
                    "-c:a",
                    "aac",
                    &outp.to_string_lossy(),
                ])
                .output(),
        )
        .await
        .map_err(|_| "ffmpeg conversion timed out".to_string())?;
        let out = res.map_err(|e| e.to_string())?;
        if !out.status.success() {
            return Err(format!(
                "ffmpeg could not convert this video: {}",
                String::from_utf8_lossy(&out.stderr)
                    .chars()
                    .take(300)
                    .collect::<String>()
            ));
        }
        std::fs::read(&outp).map_err(|e| e.to_string())
    }
    .await;
    let _ = std::fs::remove_dir_all(&tmp);
    convert.map(|mp4| (mp4, out_name))
}

#[async_trait]
impl rose_web::VideoSource for FileVideos {
    async fn list(&self) -> Vec<rose_web::VideoMeta> {
        let mut out = Vec::new();
        let entries =
            std::fs::read_dir(&self.root).map(|r| r.filter_map(|e| e.ok()).collect::<Vec<_>>());
        for entry in entries.unwrap_or_default() {
            let meta_path = entry.path().join("meta.json");
            if let Ok(text) = std::fs::read_to_string(&meta_path) {
                if let Ok(meta) = serde_json::from_str::<rose_web::VideoMeta>(&text) {
                    out.push(meta);
                }
            }
        }
        out.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        out
    }

    async fn upload(
        &self,
        name: &str,
        mime: &str,
        bytes: Vec<u8>,
    ) -> Result<rose_web::VideoMeta, String> {
        const MAX: usize = 200 * 1024 * 1024;
        if bytes.is_empty() {
            return Err("empty upload".to_string());
        }
        if bytes.len() > MAX {
            return Err("video too large (max 200 MB)".to_string());
        }
        let name = rose_web::sanitize_video_name(name);
        let lower = name.to_ascii_lowercase();
        if ![".mp4", ".webm", ".mov", ".mkv"]
            .iter()
            .any(|ext| lower.ends_with(ext))
        {
            return Err("only mp4 / webm / mov / mkv uploads".to_string());
        }
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let id = format!("v{now:x}-{n}");
        let dir = self.root.join(&id);
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let dest = self.original_path(&dir, &name);
        std::fs::write(&dest, &bytes).map_err(|e| e.to_string())?;
        // V2.1 job contract for the future Python worker.
        let job = serde_json::json!({
            "video_path": dest.to_string_lossy(),
            "out_dir": dir.to_string_lossy(),
        });
        let _ = std::fs::write(
            dir.join("job.json"),
            serde_json::to_string_pretty(&job).unwrap_or_default(),
        );
        let meta = rose_web::VideoMeta {
            id: id.clone(),
            name,
            size: bytes.len() as u64,
            mime: mime.to_string(),
            created_at: chrono::Utc::now().to_rfc3339(),
            status: "queued".to_string(),
            steps: 0,
        };
        self.write_meta(&dir, &meta)?;
        let _ = std::fs::write(dir.join("steps.json"), "[]");
        tracing::info!(video_id = %id, bytes = bytes.len(), "video uploaded");
        Ok(meta)
    }

    async fn file(&self, id: &str) -> Option<(Vec<u8>, String)> {
        let dir = self.dir(id).ok()?;
        let detail = self.read_detail(id)?;
        // Original keeps its uploaded extension.
        let entries = std::fs::read_dir(&dir).ok()?;
        for entry in entries.filter_map(|e| e.ok()) {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with("original.") {
                let bytes = std::fs::read(entry.path()).ok()?;
                return Some((bytes, detail.meta.mime));
            }
        }
        None
    }

    async fn detail(&self, id: &str) -> Option<rose_web::VideoDetail> {
        self.read_detail(id)
    }

    async fn generate(&self, id: &str) -> Result<rose_web::VideoDetail, String> {
        let dir = self.dir(id)?;
        let mut detail = self.read_detail(id).ok_or("unknown video".to_string())?;
        // Video has its own endpoint/model/key, each falling back to the
        // brain's when unset — a fully separate provider works, and the old
        // single-key setup keeps working untouched.
        let key = resolve_video_key(
            &self.video_keys,
            &self.video_env_key,
            &self.keys,
            &self.env_key,
        )
        .ok_or_else(|| {
            "no video API key — set it in Setup → Keys (Video key) or ROSE_VIDEO_API_KEY; \
                 empty follows the Muse key (Setup → Keys or ROSE_MUSE_API_KEY)"
                .to_string()
        })?;
        let snap = self.live.read().await.clone();
        let bytes = self.original_bytes(&dir, &detail.meta.name)?;
        let (mp4, upload_name) = ensure_mp4(&detail.meta.name, bytes).await?;
        let draft = rose_models::video::extract_instructions(
            &snap.models.resolved_video_endpoint(),
            &snap.models.resolved_video_model(),
            &key,
            &upload_name,
            &mp4,
        )
        .await
        .map_err(|e| e.to_string())?;
        if draft.is_empty() {
            return Err(
                "Spark found no task steps in this video — try a narrated recording".to_string(),
            );
        }
        let steps: Vec<rose_web::VideoStep> = draft
            .into_iter()
            .map(|s| rose_web::VideoStep {
                ts: s.ts,
                say: s.say,
                intent: s.intent,
                params: s.params,
                approval: s.approval,
                conf: s.conf,
                quote: s.quote,
            })
            .collect();
        std::fs::write(
            dir.join("steps.json"),
            serde_json::to_string_pretty(&steps).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
        detail.meta.status = "ready".to_string();
        detail.meta.steps = steps.len();
        detail.steps = steps;
        self.write_instructions_md(&dir, &detail.meta, &detail.steps);
        self.write_meta(&dir, &detail.meta)?;
        tracing::info!(video_id = %id, steps = detail.steps.len(), "video instructions drafted by spark");
        Ok(detail)
    }

    async fn save_draft(
        &self,
        id: &str,
        steps: Vec<rose_web::VideoStep>,
    ) -> Result<rose_web::VideoDetail, String> {
        let dir = self.dir(id)?;
        let mut detail = self.read_detail(id).ok_or("unknown video".to_string())?;
        std::fs::write(
            dir.join("steps.json"),
            serde_json::to_string_pretty(&steps).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
        self.write_instructions_md(&dir, &detail.meta, &steps);
        detail.meta.steps = steps.len();
        if detail.meta.status == "queued" && !steps.is_empty() {
            detail.meta.status = "ready".to_string();
        }
        detail.steps = steps;
        self.write_meta(&dir, &detail.meta)?;
        Ok(detail)
    }

    async fn remove(&self, id: &str) -> bool {
        match self.dir(id) {
            Ok(dir) => std::fs::remove_dir_all(&dir).is_ok(),
            Err(_) => false,
        }
    }
}

fn default_config_path() -> Option<PathBuf> {
    std::env::var("HOME")
        .ok()
        .map(|h| PathBuf::from(h).join(".config/rose/config.toml"))
}

/// Load TOML file (if present) + env overrides. Missing file = built-in default.
/// `models.endpoint` / `models.model` accept env overrides
/// (`ROSE_MODELS_ENDPOINT` / `ROSE_MODELS_BASE_URL` / `OPENAI_BASE_URL`, …;
/// see `ModelsConfig`) so a different OpenAI-compatible base URL works without
/// editing the file. Meta stays the default when nothing is set.
/// Returns the resolved file path too, so PUT /api/config knows where to persist.
fn load_config(path_override: Option<PathBuf>) -> Result<(RoseConfig, ApiKeys, Option<PathBuf>)> {
    let path = path_override.or_else(default_config_path);
    let mut config: RoseConfig = match &path {
        Some(p) if p.exists() => {
            let text =
                std::fs::read_to_string(p).with_context(|| format!("reading {}", p.display()))?;
            toml::from_str(&text).with_context(|| format!("parsing {}", p.display()))?
        }
        _ => RoseConfig::default(),
    };
    config.models.apply_env_overrides();
    Ok((config, ApiKeys::from_env(), path))
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
        other => {
            bail!("unknown desktop backend: {other} (want \"x11\" | \"fake\")")
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

fn build_services(
    config: RoseConfig,
    api_keys: ApiKeys,
    config_path: Option<PathBuf>,
) -> Result<Services> {
    // Guard: never serve remotely without an explicit opt-in.
    if config.server.allow_remote {
        eprintln!("WARNING: allow_remote = true: UI/API is reachable beyond localhost");
    }
    let mcp_policy = rose_mcp::McpPolicy {
        require_approval_for: config.policy.require_approval_for.clone(),
        allow_shell: true,
    };
    Ok(Services {
        desktop: make_desktop(&config)?,
        brain: make_brain(&config)?,
        traces: make_store(&config),
        lease: Lease::new(),
        mcp_policy,
        config,
        config_path,
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

/// One shell result, parsed from MCP `shell.exec` output.
struct McpShellOutput {
    code: Option<i32>,
    stdout: String,
    stderr: String,
    timed_out: bool,
    ms: u64,
}

/// Dispatch one resolved action through the MCP tool catalog
/// (`desktop.click`, `desktop.type`, …): the chat loop and external
/// MCP agents share exactly one action path, policy, and audit shape.
/// The Stepper has already resolved grid/element aims to points and
/// already applied its approval gate, so MCP policy here is permissive.
async fn act_via_mcp(
    backend: &Arc<dyn DesktopBackend>,
    action: &Action,
) -> std::result::Result<(), String> {
    fn point(target: &Target) -> std::result::Result<(u32, u32), String> {
        match target {
            Target::Point { x, y } => Ok((*x, *y)),
            Target::Element(id) => Err(format!("unresolved element target: {id}")),
        }
    }
    let (name, args) = match action {
        Action::Click { target, button } => {
            let (x, y) = point(target)?;
            let button = match button {
                rose_core::MouseButton::Left => "left",
                rose_core::MouseButton::Middle => "middle",
                rose_core::MouseButton::Right => "right",
            };
            (
                "desktop.click",
                serde_json::json!({"x": x, "y": y, "button": button}),
            )
        }
        Action::DoubleClick { target } => {
            let (x, y) = point(target)?;
            ("desktop.double_click", serde_json::json!({"x": x, "y": y}))
        }
        Action::Type { text } => ("desktop.type", serde_json::json!({"text": text})),
        Action::PressKey { keys } => ("desktop.key", serde_json::json!({"keys": keys})),
        Action::Scroll { target, dy } => {
            let (x, y) = point(target)?;
            (
                "desktop.scroll",
                serde_json::json!({"x": x, "y": y, "dy": dy}),
            )
        }
        Action::Drag { from, to } => {
            let (x0, y0) = point(from)?;
            let (x1, y1) = point(to)?;
            (
                "desktop.drag",
                serde_json::json!({"from_x": x0, "from_y": y0, "to_x": x1, "to_y": y1}),
            )
        }
        Action::Wait { ms } => ("desktop.wait", serde_json::json!({"ms": ms})),
    };
    rose_mcp::call_tool(
        backend,
        &None,
        &rose_mcp::McpPolicy::default(),
        "",
        None,
        name,
        &args,
    )
    .await
    .map(|_| ())
}

/// Run one shell command through the MCP shell executor (the same
/// `shell_exec` the MCP `shell.exec` tool wraps). The Stepper's approval
/// gate has already run (including approve/force answers), so the MCP
/// tool's keyword gate is not re-applied here — it would block commands
/// a human just approved.
async fn shell_via_mcp(
    display: &str,
    command: &str,
) -> std::result::Result<McpShellOutput, String> {
    let out = rose_mcp::shell_exec(display, command).await?;
    Ok(McpShellOutput {
        code: out["code"].as_i64().map(|c| c as i32),
        stdout: out["stdout"].as_str().unwrap_or("").trim().to_string(),
        stderr: out["stderr"].as_str().unwrap_or("").trim().to_string(),
        timed_out: out["timed_out"].as_bool().unwrap_or(false),
        ms: out["ms"].as_u64().unwrap_or(0),
    })
}

/// The Phase-1 loop (§6): capture -> brain -> gate -> act -> wait -> repeat
/// until the brain replies done/ask/fail, an action needs approval,
/// or the step budget runs out. Repeats are allowed — every step's
/// outcome is fed back on the next observation.
///
/// This used to be a single step with an always-empty history, so a goal
/// like "open the browser" got exactly one click (which only *selects* a
/// desktop icon) and then stopped while narrating success. Looping with
/// real history is what lets the model see that nothing opened and try
/// the next route (double-click, menu search, wait for launch).
struct Stepper {
    desktop: Arc<dyn DesktopBackend>,
    fake: FakeBrain,
    keys: rose_web::KeySlot,
    env_key: Option<String>,
    /// Live config: policy + brain hot-reload from PUT /api/config.
    /// Read once per run so a run sees a consistent snapshot.
    live: Arc<tokio::sync::RwLock<RoseConfig>>,
    traces: Arc<TraceStore>,
    /// Display for MCP `shell.exec` (the chat loop acts via MCP tools).
    display: String,
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
    /// Paused-at-approval state, answered by the very next run in the
    /// same chat (approve = resume where it paused, anything else drops
    /// it). Single-use by design: at most one paused run exists, because
    /// runs are serialised.
    resume: std::sync::Mutex<Option<ResumeState>>,
}

/// A run that paused for approval, waiting for the answer. The next
/// forced run in the same chat resumes with the full step history, so
/// Approve continues where it stopped instead of restarting the task
/// (which would redo — and possibly duplicate — already-done actions).
struct ResumeState {
    chat_id: String,
    goal: String,
    history: Vec<StepSummary>,
    acted: bool,
    steps_done: u32,
}

/// Shell commands need approval when they look destructive OR when
/// their narration trips the policy keywords — same rule as UI
/// actions, but the command text counts too, not just the `say`.
fn shell_needs_approval(policy: &Policy, say: &str, command: &str) -> bool {
    needs_approval(policy, say)
        || needs_approval(policy, command)
        || rose_mcp::looks_dangerous(command)
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

/// Chat/trace text that keeps the brain's grid aim visible:
/// `g36,21 → click (847, 472) [Left]` — the cell is what the brain
/// chose, the point is what actually actuated (audit truth).
/// Non-grid aims (raw pixels, viewport-centre scrolls) fall back to
/// the plain description.
fn describe_grid_act(requested: &Action, resolved: &Action) -> String {
    fn cell_of(target: &Target) -> Option<&str> {
        match target {
            Target::Element(id) if id.0 != rose_core::VIEWPORT_CENTRE_CELL => Some(id.0.as_str()),
            _ => None,
        }
    }
    let cell = match requested {
        Action::Click { target, .. }
        | Action::DoubleClick { target, .. }
        | Action::Scroll { target, .. } => cell_of(target),
        Action::Drag { from, .. } => cell_of(from),
        _ => None,
    };
    let resolved_text = describe_action(resolved);
    match cell {
        Some(c) => format!("{c} → {resolved_text}"),
        None => resolved_text,
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
        auto_approve: bool,
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
        // Answer a paused-for-approval run, or drop the pause: the
        // approval question is answered by the very next forced run in
        // the same chat for the same goal — anything else dismisses it.
        // (Single-use: the slot is taken whether or not it matches.)
        let resumed = self
            .resume
            .lock()
            .ok()
            .and_then(|mut slot| slot.take())
            .filter(|p| {
                force
                    && !p.chat_id.is_empty()
                    && chat_id.is_some_and(|c| c == p.chat_id)
                    && p.goal == goal
            });
        // `force` answers one pending gate; `auto_approve` ("don't ask
        // again for this run") answers every gate this run. Both are
        // per-run: a fresh run gates normally.
        let mut force = force;
        // Live config snapshot: PUT /api/config hot-reloads policy + brain
        // on the next run; a run sees one consistent snapshot throughout.
        let snapshot = self.live.read().await.clone();
        let policy = snapshot.policy.clone();
        let brain: BrainSvc = snapshot.models.brain.as_str().into();
        // Env wins over the live snapshot so ROSE_MODELS_ENDPOINT /
        // OPENAI_BASE_URL (and ..._MODEL) take effect without a restart;
        // otherwise the configured base URL (Meta by default) is used.
        let endpoint = snapshot.models.resolved_endpoint();
        let model = snapshot.models.resolved_model();
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
                .chat_turns(id, usize::MAX)
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
            "type": "run_started", "goal": goal, "max_steps": policy.max_steps,
        }));
        let mut history: Vec<StepSummary> = prior_turns;
        let mut acted = false;
        let mut base_steps = 0u32;
        let mut did_resume = false;
        // Resume: seed the full step history of the paused run so the
        // brain continues where it stopped instead of redoing (and
        // possibly duplicating) already-done actions.
        if let Some(p) = resumed {
            base_steps = p.steps_done;
            history.extend(p.history);
            acted = p.acted;
            did_resume = true;
        }
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
        let mut idx = base_steps;
        if did_resume {
            emit!(serde_json::json!({
                "type": "resumed", "step": idx,
                "say": format!("Approved — resuming at step {idx}, not restarting"),
            }));
        }
        // Takeover pauses the loop (never kills it): announce once per
        // hold, wait for hand-back (Stop still works while waiting), then
        // resume with a fresh observation — the human may have changed the
        // desktop under us, so nothing cached survives.
        let mut paused = false;
        while idx < policy.max_steps {
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
                    match brain {
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
                                MuseClient::new(&endpoint, &model, &key).map_err(|e| e.to_string())
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
                // constant numbered grid — drawn over every pixel (canvas,
                // native apps, browser alike) and the brain aims by grid
                // cell `g<col>,<row>` or pixels read off the labelled axes.
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
                    match brain {
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
                                MuseClient::new(&endpoint, &model, &key).map_err(|e| e.to_string())
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
                ModelReply::Chat { text } => {
                    // General chat answer: no desktop action taken.
                    // Finish as a normal chat reply (done = answered).
                    emit!(serde_json::json!({
                        "type": "decision", "step": idx, "status": "chat", "say": text,
                        "brain_ms": brain_ms,
                    }));
                    finish_ok!(step_view_steps(&text, acted, false, true, idx));
                }
                ModelReply::Look { say } => {
                    // Already looking (vision step) or a spurious look:
                    // drop back to text. A look changes nothing.
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
                    // Unknown ids replan (never blind-click).
                    let mut action = action;
                    // The brain's original aim, for chat/trace display:
                    // resolution below rewrites Element picks to pixels,
                    // and the UI must show the grid cell it chose.
                    let requested = action.clone();
                    if let Some(elements) = seen_elements.take() {
                        let resolve = |t: &Target| -> Option<Target> {
                            match t {
                                // Viewport centre for bare scrolls, resolved
                                // against live geometry (never guessed).
                                Target::Element(id) if id.0 == rose_core::VIEWPORT_CENTRE_CELL => {
                                    Some(Target::Point {
                                        x: frame.width / 2,
                                        y: frame.height / 2,
                                    })
                                }
                                // Element list first (DOM numbers),
                                // then a grid-cell pick `g<col>,<row>`
                                // resolved against this frame's geometry.
                                Target::Element(id) => resolve_element(&elements, &id.0)
                                    .or_else(|| {
                                        rose_grid::resolve_grid_cell(
                                            &id.0,
                                            frame.width,
                                            frame.height,
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
                    // Element picks resolved to points above are legitimate;
                    // raw pixel aims execute as given.
                    // Show the brain's grid aim, not just resolved pixels:
                    // `g36,21 → click (847, 472) [Left]`.
                    let mut action_text = describe_grid_act(&requested, &action);
                    if enter_after {
                        action_text.push_str(" + press Return");
                    }
                    emit!(serde_json::json!({
                        "type": "planned", "step": idx, "say": say,
                        "action": action_text, "brain_ms": brain_ms,
                    }));
                    if needs_approval(&policy, &say) {
                        if auto_approve {
                            // "Don't ask again" for this run: act, but leave
                            // the bypass in the trace so it stays auditable.
                            emit!(serde_json::json!({
                                "type": "auto_approved", "step": idx,
                                "say": say, "action": action_text,
                            }));
                        } else if force {
                            // Answering this one gate; further gates still ask.
                            force = false;
                        } else {
                            tracing::warn!(%say, "step gated: needs approval");
                            emit!(serde_json::json!({
                                "type": "approval", "step": idx, "say": say, "action": action_text,
                            }));
                            // Pause here instead of acting: the answer
                            // arrives as the next run in this chat (approve
                            // resumes with full history, anything else
                            // drops the pause and starts fresh).
                            if !chat_id.is_empty() {
                                let pause = ResumeState {
                                    chat_id: chat_id.clone(),
                                    goal: goal.to_string(),
                                    history: history.clone(),
                                    acted,
                                    steps_done: idx,
                                };
                                if let Ok(mut slot) = self.resume.lock() {
                                    *slot = Some(pause);
                                }
                            }
                            finish_ok!(step_view_steps(&say, acted, true, false, idx));
                        }
                    }
                    // Repeats are allowed: the run is bounded by max_steps,
                    // and the next observation still reports whether the
                    // action changed anything.
                    check_stop!(idx, acted);
                    tracing::info!(idx, %say, ?action, "step act");
                    emit!(serde_json::json!({
                        "type": "acting", "step": idx, "action": action_text,
                    }));
                    let waited = matches!(action, Action::Wait { .. });
                    let act_started = std::time::Instant::now();
                    let mut dispatch = act_via_mcp(&self.desktop, &action)
                        .await
                        .map_err(rose_core::RoseError::Desktop);
                    if dispatch.is_ok() && enter_after {
                        // Second atomic act() of the same decision
                        // (type the text, then submit it).
                        dispatch = act_via_mcp(
                            &self.desktop,
                            &Action::PressKey {
                                keys: vec!["Return".to_string()],
                            },
                        )
                        .await
                        .map_err(rose_core::RoseError::Desktop);
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
                    // observation reports whether the screen changed.
                    pending = Some((
                        idx,
                        action_text,
                        frame.jpeg.clone(),
                        frame.active_window.clone(),
                        settle_started.elapsed().as_millis() as u64,
                    ));
                }
                ModelReply::Shell { command, say } => {
                    let action_text = format!("shell: {command}");
                    emit!(serde_json::json!({
                        "type": "planned", "step": idx, "say": say,
                        "action": action_text, "command": command, "brain_ms": brain_ms,
                    }));
                    if shell_needs_approval(&policy, &say, &command) {
                        if auto_approve {
                            emit!(serde_json::json!({
                                "type": "auto_approved", "step": idx,
                                "say": say, "action": action_text,
                            }));
                        } else if force {
                            force = false;
                        } else {
                            tracing::warn!(%say, %command, "shell command gated: needs approval");
                            emit!(serde_json::json!({
                                "type": "approval", "step": idx, "say": say, "action": action_text,
                            }));
                            if !chat_id.is_empty() {
                                let pause = ResumeState {
                                    chat_id: chat_id.clone(),
                                    goal: goal.to_string(),
                                    history: history.clone(),
                                    acted,
                                    steps_done: idx,
                                };
                                if let Ok(mut slot) = self.resume.lock() {
                                    *slot = Some(pause);
                                }
                            }
                            finish_ok!(step_view_steps(&say, acted, true, false, idx));
                        }
                    }
                    check_stop!(idx, acted);
                    tracing::info!(idx, %say, %command, "shell exec");
                    emit!(serde_json::json!({
                        "type": "acting", "step": idx, "action": action_text,
                    }));
                    let output = match unless_stopped(
                        &self.stop_flag,
                        shell_via_mcp(&self.display, &command),
                    )
                    .await
                    {
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
                    history.push(StepSummary {
                        idx,
                        intent: format!(
                            "shell `{}` -> {}: {}",
                            command,
                            output
                                .code
                                .map(|c| format!("exit {c}"))
                                .unwrap_or_else(|| "killed/timed out".to_string()),
                            // Full output, no arbitrary character cap:
                            // truncating hid the end of outputs and the
                            // brain re-ran ever-new slicings to see it.
                            output.stdout.replace('\n', " "),
                        ),
                        say: say.clone(),
                    });
                    // Next step is text again: shell output is the
                    // feedback, no screenshot is needed to judge it.
                    // (Only a vision-decided shell command settles and
                    // reports its desktop result on the next look.)
                    vision = false;
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
            policy.max_steps
        );
        emit!(serde_json::json!({"type": "stuck", "say": msg}));
        let mut view = step_view_steps(&msg, acted, false, false, policy.max_steps);
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
    println!(
        "# base URL default is Meta ({}); override with",
        rose_core::DEFAULT_BASE_URL
    );
    println!(
        "# [models] base_url = \"...\" (alias: endpoint) or ROSE_MODELS_ENDPOINT / OPENAI_BASE_URL"
    );
    println!("# model override: [models] model = \"...\" or ROSE_MODELS_MODEL");
    println!("# video model: [models] video_model = \"...\" (empty = same as model) or ROSE_MODELS_VIDEO_MODEL");
    println!("# video base URL: [models] video_endpoint = \"...\" (empty = same as endpoint) or ROSE_MODELS_VIDEO_ENDPOINT");
    println!(
        "# video key: Setup → Keys (Video key) or ROSE_VIDEO_API_KEY (empty = same as Muse key)"
    );
    println!("# ROSE_VIDEO_API_KEY = {}", keys.video_status());
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
    // enable --now only starts a stopped unit: a replaced binary underneath
    // a running rosed would keep serving the old version forever (seen on
    // the 0.1.54 deploy). enable once, then always restart.
    run_as_user("systemctl", &["--user", "enable", "rosed.service"], dry_run)?;
    // rosed itself may bail until its backends land (T1.2/T3.1): warn, don't fail.
    if run_as_user(
        "systemctl",
        &["--user", "restart", "rosed.service"],
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

    let (config, keys, config_path) = load_config(cli.config)?;

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

    let services = build_services(config, keys, config_path)?;

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
        endpoint = %services.config.models.resolved_endpoint(),
        model = %services.config.models.resolved_model(),
        video_endpoint = %services.config.models.resolved_video_endpoint(),
        video_model = %services.config.models.resolved_video_model(),
        display = %info.display,
        size = %format!("{}x{}", info.width, info.height),
        "rosed starting"
    );
    // Web-set keys live in server memory (KeySlot); env key is the fallback.
    // Video has its own slot; when unset it follows the brain key.
    let keys = rose_web::KeySlot::default();
    let video_keys = rose_web::KeySlot::default();
    let live: Arc<tokio::sync::RwLock<RoseConfig>> =
        Arc::new(tokio::sync::RwLock::new(services.config.clone()));
    let data_dir = expand_tilde(&services.config.data.dir);
    let stepper = Stepper {
        desktop: Arc::clone(&services.desktop),
        fake: FakeBrain::demo(),
        keys: keys.clone(),
        env_key: services.api_keys.muse_api_key.clone(),
        live: Arc::clone(&live),
        traces: Arc::clone(&services.traces),
        display: services.config.desktop.display.clone(),
        lease: services.lease.clone(),
        stop_flag: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        active: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        resume: std::sync::Mutex::new(None),
    };
    let mcp_session: Arc<dyn rose_mcp::TestSession> =
        Arc::new(TestSessionImpl::new(Arc::clone(&services.traces)));
    let video_store = Arc::new(FileVideos::new(
        data_dir.clone(),
        keys.clone(),
        services.api_keys.muse_api_key.clone(),
        video_keys.clone(),
        services.api_keys.video_api_key.clone(),
        Arc::clone(&live),
    ));
    let skill_adapter = Arc::new(SkillAdapter {
        store: Arc::clone(&services.traces),
        data: data_dir,
    });
    let mcp_skills: Arc<dyn rose_mcp::SkillService> = Arc::new(McpSkills {
        videos: Arc::clone(&video_store),
        skills: Arc::clone(&skill_adapter),
    });
    let state = rose_web::AppState {
        source: Some(Arc::new(BackendSource {
            backend: Arc::clone(&services.desktop),
            display: info.display.clone(),
            width: info.width,
            height: info.height,
        })),
        mcp_backend: Some(Arc::clone(&services.desktop)),
        mcp_display: services.config.desktop.display.clone(),
        mcp_session: Some(mcp_session),
        mcp_skills: Some(mcp_skills),
        mcp_policy: services.mcp_policy.clone(),
        stepper: Some(Arc::new(stepper)),
        traces: Some(Arc::new(TraceAdapter {
            store: Arc::clone(&services.traces),
        })),
        keys: keys.clone(),
        video_keys: video_keys.clone(),
        lease: services.lease.clone(),
        control: Some(Arc::new(ControlSource {
            backend: Arc::clone(&services.desktop),
            lease: services.lease.clone(),
        })),
        config: Some(Arc::new(FileConfig {
            path: services.config_path.clone(),
            current: Arc::clone(&live),
        })),
        videos: Some(video_store),
        skills: Some(skill_adapter),
        env_key_present: services.api_keys.muse_api_key.is_some(),
        video_env_key_present: services.api_keys.video_api_key.is_some(),
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
        let (cfg, _, _) =
            load_config(Some(PathBuf::from("/nonexistent/rose-test.toml"))).expect("load");
        assert_eq!(cfg, RoseConfig::default());
    }

    #[tokio::test]
    async fn fake_selectors_build_and_capture() {
        let mut cfg = RoseConfig::default();
        cfg.desktop.backend = "fake".to_string();
        cfg.models.brain = "fake".to_string();
        let svc = build_services(cfg, ApiKeys::default(), None).expect("fake services build");
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
        build_services(cfg, ApiKeys::default(), None).expect("muse selector builds");
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
        let mut cfg = RoseConfig::default();
        cfg.desktop.backend = "fake".to_string();
        cfg.models.brain = match brain {
            BrainSvc::Fake => "fake".to_string(),
            BrainSvc::Muse => "muse".to_string(),
        };
        Stepper {
            desktop: Arc::new(FakeDesktop::new()),
            fake: FakeBrain::demo(),
            keys: rose_web::KeySlot::default(),
            env_key: None,
            live: Arc::new(tokio::sync::RwLock::new(cfg)),
            traces: test_traces(),
            display: ":fake".to_string(),
            lease: Lease::new(),
            stop_flag: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            active: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            resume: std::sync::Mutex::new(None),
        }
    }

    #[tokio::test]
    async fn skill_frames_resolve_video_and_serve_cache() {
        use rose_web::SkillSource;
        let dir = std::env::temp_dir().join(format!("rose-skillframe-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        // Fake video dir with a meta name match (no real video needed:
        // the cache-hit path never shells out to ffmpeg).
        let vids = dir.join("data").join("videos").join("v1");
        std::fs::create_dir_all(&vids).expect("tmp");
        std::fs::write(vids.join("meta.json"), r#"{"id":"v1","name":"demo.mp4"}"#).expect("meta");
        std::fs::write(vids.join("original.mp4"), vec![0u8; 2048]).expect("vid");
        let store = Arc::new(TraceStore::new(dir.join("data")));
        let skill = store
            .create_skill(
                "demo",
                "Do it",
                &[serde_json::json!({"ts": 8, "say": "Hi", "intent": "open it"})],
                "demo.mp4",
            )
            .expect("skill");
        let adapter = SkillAdapter {
            store: Arc::clone(&store),
            data: dir.join("data"),
        };
        // Unknown skill / out-of-range step: None, no ffmpeg involved.
        assert!(adapter.frame("nope", 0).await.is_none());
        assert!(adapter.frame(&skill.id, 5).await.is_none());
        // Cache hit serves without touching ffmpeg.
        let cache = dir.join("data").join("skills").join(&skill.id);
        std::fs::create_dir_all(&cache).expect("tmp");
        std::fs::write(cache.join("step-000.jpg"), vec![0xffu8; 200]).expect("frame");
        let got = adapter.frame(&skill.id, 0).await.expect("cached frame");
        assert_eq!(got.len(), 200);
        assert!(adapter.frame(&skill.id, 1).await.is_none(), "no step 1");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn skill_create_writes_markdown_doc() {
        use rose_web::SkillSource;
        let dir = std::env::temp_dir().join(format!("rose-skillmd-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Arc::new(TraceStore::new(dir.join("data")));
        let adapter = SkillAdapter {
            store,
            data: dir.join("data"),
        };
        let skill = adapter
            .create(
                "morning screen",
                "Do the screening",
                vec![serde_json::json!({"ts": 8, "say": "Open it", "intent": "open the app", "params": "", "approval": false})],
                "demo.mp4",
            )
            .await
            .expect("create");
        let id = skill.get("id").and_then(|v| v.as_str()).expect("id");
        let md = std::fs::read_to_string(dir.join("data").join("skills").join(id).join("skill.md"))
            .expect("skill.md written");
        assert!(md.starts_with("# morning screen\n"), "{md}");
        assert!(md.contains("### 1. Open it — open the app"), "{md}");
        assert!(!md.contains("\"intent\""), "no raw JSON in the doc: {md}");
        // Re-saving refreshes the doc.
        adapter
            .update(id, "evening screen", "Do it again", vec![])
            .await
            .expect("update");
        let md2 =
            std::fs::read_to_string(dir.join("data").join("skills").join(id).join("skill.md"))
                .expect("skill.md rewritten");
        assert!(md2.starts_with("# evening screen\n"), "{md2}");
        // Delete removes the DB row and the on-disk artifacts.
        assert!(adapter.remove(id).await);
        assert!(
            !dir.join("data").join("skills").join(id).exists(),
            "skill dir cleaned up"
        );
        assert!(adapter.get(id).await.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn file_config_set_persists_and_flags_restart() {
        use rose_web::ConfigSource;
        let dir = std::env::temp_dir().join(format!("rose-cfg-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("tmp");
        let path = dir.join("config.toml");
        let live = Arc::new(tokio::sync::RwLock::new(RoseConfig::default()));
        let store = FileConfig {
            path: Some(path.clone()),
            current: Arc::clone(&live),
        };
        let mut next = RoseConfig::default();
        next.policy.max_steps = 12;
        let (applied, restart) = store.set(next).await.expect("set");
        assert_eq!(applied.policy.max_steps, 12);
        assert!(!restart, "policy-only change hot-reloads");
        assert!(path.exists(), "config persisted to file");
        let mut back = RoseConfig::default();
        back.desktop.backend = "fake".to_string();
        let (_, restart) = store.set(back).await.expect("set");
        assert!(restart, "backend switch needs restart");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn test_videos(data_dir: PathBuf) -> FileVideos {
        FileVideos::new(
            data_dir,
            rose_web::KeySlot::default(),
            None,
            rose_web::KeySlot::default(),
            None,
            Arc::new(tokio::sync::RwLock::new(RoseConfig::default())),
        )
    }

    #[tokio::test]
    async fn file_videos_upload_save_remove_roundtrip() {
        use rose_web::VideoSource;
        let dir = std::env::temp_dir().join(format!("rose-vid-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("tmp");
        let store = test_videos(dir.clone());
        let meta = store
            .upload("demo.mp4", "video/mp4", b"0123456789".to_vec())
            .await
            .expect("upload");
        assert_eq!(meta.name, "demo.mp4");
        let (bytes, mime) = store.file(&meta.id).await.expect("file");
        assert_eq!(bytes, b"0123456789");
        assert_eq!(mime, "video/mp4");
        let saved = store.save_draft(&meta.id, Vec::new()).await.expect("save");
        assert!(saved.steps.is_empty());
        assert!(store.remove(&meta.id).await);
        assert!(store.detail(&meta.id).await.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn video_draft_saves_markdown_instructions() {
        use rose_web::VideoSource;
        let dir = std::env::temp_dir().join(format!("rose-vidmd-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("tmp");
        let store = test_videos(dir.clone());
        let meta = store
            .upload("demo.mp4", "video/mp4", b"0123456789".to_vec())
            .await
            .expect("upload");
        let steps = vec![
            rose_web::VideoStep {
                ts: 8,
                say: "Open it".to_string(),
                intent: "open the app".to_string(),
                params: String::new(),
                approval: false,
                conf: 0.9,
                quote: "open now".to_string(),
            },
            rose_web::VideoStep {
                ts: 20,
                say: "Pick month".to_string(),
                intent: "pick month".to_string(),
                params: "month = {{month}}".to_string(),
                approval: true,
                conf: 0.8,
                quote: String::new(),
            },
        ];
        store.save_draft(&meta.id, steps).await.expect("save");
        let md = std::fs::read_to_string(dir.join("videos").join(&meta.id).join("instructions.md"))
            .expect("instructions.md written");
        assert!(md.starts_with("# demo\n"), "{md}");
        assert!(md.contains("Generalized instructions"), "{md}");
        assert!(md.contains("1. open the app"), "{md}");
        assert!(
            md.contains("User said: \"open now\""),
            "user input retained: {md}"
        );
        assert!(
            md.contains("2. pick month [month = {{month}}] (needs approval)"),
            "{md}"
        );
        assert!(!md.contains("\"intent\""), "no raw JSON in the doc: {md}");
        // The generalized-instructions builder keeps wording + quotes too.
        let steps = vec![rose_web::VideoStep {
            ts: 8,
            say: "Open it".to_string(),
            intent: "open the app for {{month}}".to_string(),
            params: "month = {{month}}".to_string(),
            approval: false,
            conf: 0.9,
            quote: "open it for March".to_string(),
        }];
        let gen = generalized_instructions("demo.mp4", &steps);
        assert!(gen.contains("any new values"), "{gen}");
        assert!(gen.contains("open the app for {{month}}"), "{gen}");
        assert!(gen.contains("User said: \"open it for March\""), "{gen}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn video_generate_needs_an_api_key_not_a_mock() {
        use rose_web::VideoSource;
        // No key anywhere: generate must fail loudly instead of
        // returning stock steps. No network is touched.
        let dir = std::env::temp_dir().join(format!("rose-vid-key-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("tmp");
        let store = test_videos(dir.clone());
        let meta = store
            .upload("demo.mp4", "video/mp4", b"0123456789".to_vec())
            .await
            .expect("upload");
        let err = store.generate(&meta.id).await.unwrap_err();
        assert!(err.contains("API key"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn ensure_mp4_passes_mp4_through_untouched() {
        let bytes = b"fake-mp4-bytes".to_vec();
        let (out, name) = ensure_mp4("clip.mp4", bytes.clone()).await.expect("mp4");
        assert_eq!(out, bytes);
        assert_eq!(name, "clip.mp4");
    }

    #[tokio::test]
    async fn live_config_hot_reloads_policy_into_next_run() {
        use rose_web::StepRunner;
        // Script needs 2 vision steps; live max_steps=1 must stop it early.
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
        let stepper = Stepper {
            fake: FakeBrain::scripted(vec![
                rose_models::ModelReply::Look {
                    say: "Need to see the desktop".to_string(),
                },
                click,
                rose_models::ModelReply::Done {
                    say: "Done".to_string(),
                },
            ]),
            ..test_stepper(BrainSvc::Fake)
        };
        {
            let mut cfg = stepper.live.write().await;
            cfg.policy.max_steps = 1;
        }
        let (tx, _rx) = event_channel();
        let view = stepper
            .run_step("hot reload test", false, false, None, &tx)
            .await
            .expect("run");
        assert_eq!(view["steps"], 1, "live policy capped the run");
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
    async fn chat_reply_answers_without_desktop_action() {
        use rose_web::StepRunner;
        let stepper = Stepper {
            fake: FakeBrain::scripted(vec![rose_models::ModelReply::Chat {
                text: "Paris is the capital of France.".to_string(),
            }]),
            ..test_stepper(BrainSvc::Fake)
        };
        let (tx, _rx) = event_channel();
        let view = stepper
            .run_step("what is the capital of France?", false, false, None, &tx)
            .await
            .expect("chat run");
        assert_eq!(view["say"], "Paris is the capital of France.");
        assert_eq!(view["acted"], false);
        assert_eq!(view["done"], true);
        assert_eq!(
            stepper.fake.call_count(),
            1,
            "one text call, no screenshot/vision step"
        );
    }

    #[tokio::test]
    async fn fake_brain_step_acts_on_fake_desktop() {
        use rose_web::StepRunner;
        let (tx, _rx) = event_channel();
        let view = test_stepper(BrainSvc::Fake)
            .run_step("click the first cell", false, false, None, &tx)
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
            .run_step("do a thing", false, false, None, &tx)
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
            .run_step("pay", false, false, None, &tx)
            .await
            .expect("gated");
        assert_eq!(gated["acted"], false);
        assert_eq!(gated["approval_needed"], true);
        let forced = stepper
            .run_step("pay", true, false, None, &tx)
            .await
            .expect("forced");
        assert_eq!(forced["acted"], true);
    }

    fn risky_click(say: &str) -> rose_models::ModelReply {
        rose_models::ModelReply::Act {
            action: rose_core::Action::Click {
                target: rose_core::Target::Element(rose_core::ElementId(
                    "g1,1@0.42,0.42".to_string(),
                )),
                button: rose_core::MouseButton::Left,
            },
            say: say.to_string(),
            enter_after: false,
        }
    }

    #[tokio::test]
    async fn approve_resumes_where_it_paused_instead_of_restarting() {
        use rose_web::StepRunner;
        // Run 1 gates on the risky click; run 2 (forced, same chat+goal)
        // resumes with the paused history — a `resumed` event, no replay
        // of the discovery steps, and the gated action finally acts.
        let stepper = Stepper {
            fake: FakeBrain::scripted(vec![
                rose_models::ModelReply::Look {
                    say: "Need to see the desktop".to_string(),
                },
                risky_click("Clicking Pay now"),
                // Resumed runs restart text-first: look, then act.
                rose_models::ModelReply::Look {
                    say: "Need to see the desktop".to_string(),
                },
                risky_click("Clicking Pay now"),
                rose_models::ModelReply::Done {
                    say: "Paid".to_string(),
                },
            ]),
            ..test_stepper(BrainSvc::Fake)
        };
        let chat = "chat-resume-1";
        let (tx, mut rx) = event_channel();
        let gated = stepper
            .run_step("pay the bill", false, false, Some(chat), &tx)
            .await
            .expect("gated");
        assert_eq!(gated["approval_needed"], true);
        assert_eq!(gated["acted"], false);
        let resumed = stepper
            .run_step("pay the bill", true, false, Some(chat), &tx)
            .await
            .expect("resumed");
        assert_eq!(resumed["done"], true);
        assert_eq!(resumed["acted"], true);
        let events = drain(&mut rx);
        assert!(
            events.iter().any(|e| e["type"] == "resumed"),
            "continuation is announced, not a silent restart: {events:?}"
        );
        // Single-use: a further forced run finds no pause to answer.
        let (tx, mut rx) = event_channel();
        let _ = stepper
            .run_step("pay the bill", true, false, Some(chat), &tx)
            .await;
        let events = drain(&mut rx);
        assert!(
            !events.iter().any(|e| e["type"] == "resumed"),
            "pause is consumed by its answer: {events:?}"
        );
    }

    #[tokio::test]
    async fn approve_all_bypasses_further_gates_with_audit() {
        use rose_web::StepRunner;
        let stepper = Stepper {
            fake: FakeBrain::scripted(vec![
                rose_models::ModelReply::Look {
                    say: "Need to see the desktop".to_string(),
                },
                risky_click("Clicking Pay now"),
                risky_click("Clicking Send receipt"),
                rose_models::ModelReply::Done {
                    say: "Paid and sent".to_string(),
                },
            ]),
            ..test_stepper(BrainSvc::Fake)
        };
        let (tx, mut rx) = event_channel();
        let view = stepper
            .run_step("pay and send", false, true, Some("chat-auto-1"), &tx)
            .await
            .expect("auto-approved run");
        assert_eq!(view["done"], true);
        assert_eq!(view["approval_needed"], false);
        let events = drain(&mut rx);
        assert_eq!(
            events
                .iter()
                .filter(|e| e["type"] == "auto_approved")
                .count(),
            2,
            "every bypassed gate stays in the trace: {events:?}"
        );
        assert!(
            !events.iter().any(|e| e["type"] == "approval"),
            "no pause was raised: {events:?}"
        );
    }

    #[tokio::test]
    async fn force_answers_only_the_first_gate() {
        use rose_web::StepRunner;
        let stepper = Stepper {
            fake: FakeBrain::scripted(vec![
                rose_models::ModelReply::Look {
                    say: "Need to see the desktop".to_string(),
                },
                risky_click("Clicking Pay now"),
                risky_click("Clicking Send receipt"),
                rose_models::ModelReply::Done {
                    say: "Paid and sent".to_string(),
                },
            ]),
            ..test_stepper(BrainSvc::Fake)
        };
        let (tx, _rx) = event_channel();
        let view = stepper
            .run_step("pay and send", true, false, Some("chat-once-1"), &tx)
            .await
            .expect("forced run");
        // First gate passed via force; the second one still asked.
        assert_eq!(view["approval_needed"], true);
        assert_eq!(view["acted"], true);
    }

    #[tokio::test]
    async fn loop_runs_until_done_not_just_one_step() {
        use rose_web::StepRunner;
        // Regression: "open the browser" needs more than one action.
        // The old stepper stopped after the first Act, so the run
        // narrated but never finished the job.
        // Grid picks resolve under the current grid geometry: derive the
        // cell + fraction from the target pixels via cell_bounds.
        let click = |x: u32| {
            let col = x * rose_grid::GRID_COLS / 1600;
            let cell = rose_grid::cell_bounds(col, 0, 1600, 900).expect("cell");
            let fx = (x - cell.x) as f32 / cell.width.saturating_sub(1).max(1) as f32;
            rose_models::ModelReply::Act {
                action: rose_core::Action::Click {
                    target: rose_core::Target::Element(rose_core::ElementId(format!(
                        "g{},1@{:.2},0.42",
                        col + 1,
                        fx
                    ))),
                    button: rose_core::MouseButton::Left,
                },
                say: format!("Clicking {x}"),
                enter_after: false,
            }
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
            .run_step("open the browser", false, false, None, &tx)
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
        assert_eq!(
            result["action"], "g1,1@0.45,0.42 → click (10, 9) [Left]",
            "chat shows the grid cell the brain chose, then the actuated point"
        );
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
            async move {
                stepper
                    .run_step("takeover test", false, false, None, &tx)
                    .await
            }
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
    async fn removal_goal_finishes_on_first_done() {
        use rose_web::StepRunner;
        // Removal read-back was removed: Done finishes immediately,
        // even for removal-worded goals.
        let stepper = Stepper {
            fake: FakeBrain::scripted(vec![rose_models::ModelReply::Done {
                say: "Volume gone".to_string(),
            }]),
            ..test_stepper(BrainSvc::Fake)
        };
        let (tx, _rx) = event_channel();
        let view = stepper
            .run_step("remove the volume pane", false, false, None, &tx)
            .await
            .expect("run");
        assert_eq!(view["done"], true);
        assert_eq!(stepper.fake.call_count(), 1, "no verification round-trip");
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
            .run_step("open the GOOGL chart", false, false, None, &tx)
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
            id: rose_core::ElementId("grid-1".to_string()),
            label: "button: Cell".to_string(),
            role: Some("button".to_string()),
            bounds: rose_core::Rect {
                x: 40,
                y: 40,
                width: 320,
                height: 370,
            },
            confidence: 1.0,
            source: rose_core::ElementSource::Grid,
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
                            "grid-1".to_string(),
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
            .run_step("click the first cell", false, false, None, &tx)
            .await
            .expect("run");
        assert_eq!(view["done"], true);
        assert_eq!(view["acted"], true);
        assert_eq!(
            desktop.highlighted(),
            Some(0),
            "element: grid-1 resolved to cell 0's pixels"
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
            .run_step("click the first cell", false, false, None, &tx)
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
    async fn pixel_click_acts_directly_without_grid() {
        use rose_web::StepRunner;
        // Guessed pixels are no longer refused: the click dispatches
        // as-is, then the run continues.
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
                rose_models::ModelReply::Done {
                    say: "Clicked".to_string(),
                },
            ]),
            ..test_stepper(BrainSvc::Fake)
        };
        let (tx, mut rx) = event_channel();
        let view = stepper
            .run_step("click the first cell", false, false, None, &tx)
            .await
            .expect("run");
        assert_eq!(view["done"], true);
        assert_eq!(view["acted"], true);
        assert_eq!(desktop.highlighted(), Some(0), "pixel click acted");
        let events = drain(&mut rx);
        assert!(
            !events.iter().any(|e| e["type"] == "rejected"),
            "no refusal anymore: {events:?}"
        );
        assert!(
            events.iter().any(|e| e["type"] == "acted"),
            "pixel dispatch recorded: {events:?}"
        );
    }

    #[tokio::test]
    async fn repeated_pixel_clicks_keep_acting() {
        use rose_web::StepRunner;
        // Repeats are no longer refused or stopped: every identical
        // action dispatches (the run is bounded by max_steps), then the
        // run finishes on Done.
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
        let (tx, mut rx) = event_channel();
        let view = stepper
            .run_step("click the first cell", false, false, None, &tx)
            .await
            .expect("run");
        assert_eq!(view["done"], true);
        assert_eq!(view["acted"], true);
        assert_eq!(desktop.highlighted(), Some(0), "pixel clicks acted");
        let events = drain(&mut rx);
        assert_eq!(
            events.iter().filter(|e| e["type"] == "acted").count(),
            3,
            "every repeat dispatched: {events:?}"
        );
    }

    #[tokio::test]
    async fn grid_cell_pick_resolves_to_cell_centre() {
        use rose_web::StepRunner;
        // The brain names a cell from the numbered axes ("g5,5"); the
        // Stepper clicks that cell's centre (107,103 on the 1600x900
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
            .run_step("click cell 5,5", false, false, None, &tx)
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
            .run_step("look at the desktop", false, false, None, &tx)
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
            .run_step("click the first cell", false, false, None, &tx)
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

    #[tokio::test]
    async fn loop_keeps_dispatching_repeats_until_the_script_ends() {
        use rose_web::StepRunner;
        // Repeats are no longer refused or stopped: every identical
        // action dispatches (bounded by max_steps), then the run ends
        // when the brain replies Done.
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
                rose_models::ModelReply::Done {
                    say: "Browser is open".to_string(),
                },
            ]),
            ..test_stepper(BrainSvc::Fake)
        };
        let (tx, mut rx) = event_channel();
        let view = stepper
            .run_step("open the browser", false, false, None, &tx)
            .await
            .expect("run");
        assert_eq!(view["acted"], true);
        assert_eq!(view["done"], true);
        // All three identical clicks dispatched, then Done.
        assert_eq!(stepper.fake.call_count(), 5);
        let events = drain(&mut rx);
        assert_eq!(
            events.iter().filter(|e| e["type"] == "acted").count(),
            3,
            "every repeat dispatched: {events:?}"
        );
    }

    #[tokio::test]
    async fn identical_repeat_is_dispatched_not_rejected() {
        use rose_web::StepRunner;
        // Grid picks resolve under the current grid geometry: derive the
        // cell + fraction from the target pixels via cell_bounds, so the
        // helper keeps resolving to the same pixels if the grid changes.
        let click = |x: u32, y: u32| {
            let col = (x * rose_grid::GRID_COLS / 1600).min(rose_grid::GRID_COLS - 1);
            let row = (y * rose_grid::GRID_ROWS / 900).min(rose_grid::GRID_ROWS - 1);
            let cell = rose_grid::cell_bounds(col, row, 1600, 900).expect("cell");
            let fx = (x - cell.x) as f32 / cell.width.saturating_sub(1).max(1) as f32;
            let fy = (y - cell.y) as f32 / cell.height.saturating_sub(1).max(1) as f32;
            rose_models::ModelReply::Act {
                action: rose_core::Action::Click {
                    target: rose_core::Target::Element(rose_core::ElementId(format!(
                        "g{},{}@{:.2},{:.2}",
                        col + 1,
                        row + 1,
                        fx,
                        fy
                    ))),
                    button: rose_core::MouseButton::Left,
                },
                say: format!("Clicking {x},{y}"),
                enter_after: false,
            }
        };
        let stepper = Stepper {
            fake: FakeBrain::scripted(vec![
                rose_models::ModelReply::Look {
                    say: "Need to see the desktop".to_string(),
                },
                click(10, 10),   // misses every cell: no visible change
                click(10, 10),   // identical repeat: dispatched again now
                click(200, 225), // inside cell 0: highlights, screen changes
                rose_models::ModelReply::Done {
                    say: "Done".to_string(),
                },
            ]),
            ..test_stepper(BrainSvc::Fake)
        };
        let (tx, mut rx) = event_channel();
        let view = stepper
            .run_step("click a cell", false, false, None, &tx)
            .await
            .expect("run");
        assert_eq!(view["done"], true, "{view}");
        assert_eq!(stepper.fake.call_count(), 5);
        let events = drain(&mut rx);
        assert_eq!(
            events.iter().filter(|e| e["type"] == "acted").count(),
            3,
            "every click dispatched, including the repeat: {events:?}"
        );
        assert!(
            !events.iter().any(|e| e["type"] == "rejected"),
            "no rejection anymore: {events:?}"
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
        // Grid picks resolve under the current grid geometry: derive the
        // cell + fraction from the target pixels via cell_bounds.
        let click = |x: u32| {
            let col = (x * rose_grid::GRID_COLS / 1600).min(rose_grid::GRID_COLS - 1);
            let cell = rose_grid::cell_bounds(col, 9, 1600, 900).expect("cell");
            let fx = (x - cell.x) as f32 / cell.width.saturating_sub(1).max(1) as f32;
            rose_models::ModelReply::Act {
                action: rose_core::Action::Click {
                    target: rose_core::Target::Element(rose_core::ElementId(format!(
                        "g{},10@{:.2},0",
                        col + 1,
                        fx
                    ))),
                    button: rose_core::MouseButton::Left,
                },
                say: format!("Clicking {x}"),
                enter_after: false,
            }
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
            let run = stepper_ref.run_step("stop me", false, false, None, &tx);
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
            .run_step("echo test", false, false, None, &tx)
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
            .run_step("clean", false, false, None, &tx)
            .await
            .expect("gated");
        assert_eq!(gated["approval_needed"], true, "{gated}");
        assert_eq!(gated["acted"], false);
        let forced = stepper
            .run_step("clean", true, false, None, &tx)
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
            .run_step("first goal", false, false, None, &tx)
            .await
            .expect("turn 1");
        let chat_id = first["chat_id"].as_str().expect("chat id").to_string();
        let second = stepper
            .run_step("second goal", false, false, Some(&chat_id), &tx)
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
            .run_step("persist this run", false, false, None, &tx)
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
        build_services(cfg, ApiKeys::default(), None).expect("x11 selector builds");
    }

    #[test]
    fn unknown_selectors_rejected() {
        let mut cfg = RoseConfig::default();
        cfg.desktop.backend = "wayland".to_string();
        cfg.models.brain = "fake".to_string();
        let err = build_services(cfg, ApiKeys::default(), None)
            .map(|_| ())
            .unwrap_err();
        assert!(err.to_string().contains("unknown desktop backend"), "{err}");
    }
}
