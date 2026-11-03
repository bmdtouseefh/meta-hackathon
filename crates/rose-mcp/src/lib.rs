//! MCP tools for the Rose desktop app testing tool.
//!
//! The AI under test (or the AI doing the testing) talks to `rosed`
//! through these tools — over HTTP `POST /mcp` (JSON-RPC 2.0) — instead
//! of a built-in brain. Same desktop, same `act()` path, same audit
//! log; the only difference is the planner lives outside this binary
//! (a coding agent, a test harness, a human driving the Web UI).
//!
//! Dependency rule: `rose-core` + `rose-grid` + std JSON only. No I/O
//! except through the `DesktopBackend` / `TestSession` traits handed in
//! by the composition root (`apps/rosed`).

use async_trait::async_trait;
use rose_core::{Action, Actor, DesktopBackend, MouseButton, Target};
use std::sync::Arc;

// ---------- policy ----------

/// Safety gate for MCP-driven input. Desktop actions are allowed outright
/// (the agent under test must be free to click around); shell commands are
/// keyword-gated exactly like the old Governor rule so `rm -rf` and friends
/// come back as `approval_required` instead of executing.
#[derive(Debug, Clone)]
pub struct McpPolicy {
    pub require_approval_for: Vec<String>,
    pub allow_shell: bool,
}

impl Default for McpPolicy {
    fn default() -> Self {
        Self {
            require_approval_for: vec![
                "submit".to_string(),
                "send".to_string(),
                "pay".to_string(),
                "delete".to_string(),
                "publish".to_string(),
            ],
            allow_shell: true,
        }
    }
}

impl McpPolicy {
    /// Keyword gate over free text (shell command or narration).
    pub fn shell_gate(&self, command: &str) -> Result<(), String> {
        if !self.allow_shell {
            return Err("shell tool is disabled by policy".to_string());
        }
        let lower = command.to_lowercase();
        if self
            .require_approval_for
            .iter()
            .any(|k| !k.is_empty() && lower.contains(&k.to_lowercase()))
        {
            return Err(format!(
                "approval_required: this command needs a human to confirm it first: {command}"
            ));
        }
        if looks_dangerous(command) {
            return Err(format!(
                "approval_required: destructive-looking command, confirm with the user first: {command}"
            ));
        }
        Ok(())
    }
}

/// Conservative dangerous-command match (same list the old agent shell used).
pub fn looks_dangerous(command: &str) -> bool {
    let lower = format!(" {} ", command.to_lowercase());
    const NEEDLES: &[&str] = &[
        " rm ",
        " sudo ",
        " shutdown",
        " reboot",
        " kill ",
        " pkill",
        " apt ",
        " apt-get",
        " dpkg",
        " systemctl",
        " mkfs",
        " dd ",
        " chmod",
        " chown",
        " mv ",
        " shred",
        " passwd",
        " useradd",
        " userdel",
        " mount",
        " umount",
        " fdisk",
        " wipefs",
        "> /dev/",
    ];
    NEEDLES.iter().any(|n| lower.contains(n))
}

// ---------- test sessions (implemented in apps/rosed over TraceStore) ----------

/// Groups MCP actions into a named test run for the Web UI history.
/// The MCP crate never touches the store directly.
#[async_trait]
pub trait TestSession: Send + Sync {
    /// Begin a test run; returns its id.
    async fn start(&self, goal: &str) -> String;
    /// Record one executed action (best-effort; never fails the tool call).
    async fn record_act(&self, run_id: &str, action: &Action, actor: Actor);
    /// Finish a test run with a verdict.
    async fn finish(&self, run_id: &str, passed: bool, summary: &str) -> bool;
}

// ---------- video -> skill.md (implemented in apps/rosed) ----------

/// Video library + skill store behind the `video.*` / `skill.*` tools.
/// The MCP crate never touches files, the model, or the DB directly:
/// `video_to_skill` in the composition root uploads the recording to
/// Spark, drafts generalized steps (retaining the user's narration
/// verbatim in each step's quote), saves a named skill, writes its
/// `skill.md`, and returns `{skill_id, name, instructions, skill_md, steps}`.
#[async_trait]
pub trait SkillService: Send + Sync {
    /// Uploaded videos, newest first (JSON values).
    async fn list_videos(&self) -> Vec<serde_json::Value>;
    /// Draft generalized instructions from one video and save them as
    /// a named skill + `skill.md`. Returns the created skill payload.
    async fn video_to_skill(
        &self,
        video_id: &str,
        name: Option<&str>,
    ) -> Result<serde_json::Value, String>;
    /// Saved skills, newest first (JSON values).
    async fn list_skills(&self) -> Vec<serde_json::Value>;
    /// One saved skill (JSON value), if it exists.
    async fn skill(&self, skill_id: &str) -> Option<serde_json::Value>;
    /// The skill rendered as markdown (`skill.md` text), if it exists.
    async fn skill_doc(&self, skill_id: &str) -> Option<String>;
    /// Edit a saved skill: any of name/instructions/steps may be given
    /// (omitted fields keep their current values). Returns the updated
    /// skill, or an error for an unknown id or invalid values.
    async fn update_skill(
        &self,
        skill_id: &str,
        name: Option<&str>,
        instructions: Option<&str>,
        steps: Option<Vec<serde_json::Value>>,
    ) -> Result<serde_json::Value, String>;
}

// ---------- shell (for launching / driving the app under test) ----------

pub const SHELL_TIMEOUT_SECS: u64 = 30;

/// Settle delay after every screen-mutating desktop action, before the
/// tool returns: the agent was screenshotting faster than the UI could
/// update, so it kept deciding on stale pixels. Sleeping here,
/// server-side, means the agent's very next `desktop.screenshot` sees
/// the settled screen. `desktop.wait` is exempt (it already slept).
pub const POST_ACTION_SETTLE_SECS: u64 = 1;

/// Parent environment minus anything secret-looking, plus DISPLAY.
fn scrubbed_env(display: &str) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = std::env::vars()
        .filter(|(key, _)| {
            let upper = key.to_uppercase();
            !["API_KEY", "TOKEN", "SECRET", "PASSWORD", "PASSWD"]
                .iter()
                .any(|needle| upper.contains(needle))
        })
        .collect();
    env.push(("DISPLAY".to_string(), display.to_string()));
    env
}

/// Run `bash -lc <command>` (30s timeout, killed on drop). Used to launch
/// the app under test and to inspect the VM around it.
pub async fn shell_exec(display: &str, command: &str) -> Result<serde_json::Value, String> {
    let started = std::time::Instant::now();
    let mut cmd = tokio::process::Command::new("bash");
    cmd.arg("-lc")
        .arg(command)
        .env_clear()
        .envs(scrubbed_env(display))
        .kill_on_drop(true);
    match tokio::time::timeout(
        std::time::Duration::from_secs(SHELL_TIMEOUT_SECS),
        cmd.output(),
    )
    .await
    {
        Ok(Ok(out)) => Ok(serde_json::json!({
            "code": out.status.code(),
            "stdout": String::from_utf8_lossy(&out.stdout),
            "stderr": String::from_utf8_lossy(&out.stderr),
            "timed_out": false,
            "ms": started.elapsed().as_millis() as u64,
        })),
        Ok(Err(e)) => Err(format!("could not run shell command: {e}")),
        Err(_) => Ok(serde_json::json!({
            "code": serde_json::Value::Null,
            "stdout": "",
            "stderr": format!("timed out after {SHELL_TIMEOUT_SECS}s and was killed"),
            "timed_out": true,
            "ms": started.elapsed().as_millis() as u64,
        })),
    }
}

// ---------- tool catalog ----------

const SERVER_NAME: &str = "rose-desktop-testing";
const PROTOCOL_VERSION: &str = "2024-11-05";

fn schema(props: serde_json::Value, required: &[&str]) -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": props,
        "required": required,
        "additionalProperties": false,
    })
}

/// Aim properties for one screen point: a grid cell (preferred — resolved
/// server-side against live geometry, so the agent never guesses pixels)
/// or raw x/y pixels (escape hatch for points a cell cannot express).
/// With an empty prefix the keys are `cell`/`x`/`y`; otherwise they are
/// prefixed (`from_cell`, `to_x`, …).
fn aim_props(
    cell_key: &str,
    x_key: &str,
    y_key: &str,
) -> serde_json::Map<String, serde_json::Value> {
    serde_json::json!({
        cell_key: { "type": "string", "description": "grid cell read off a gridded screenshot, e.g. g14,3 or g3,7@0.5,0.5 for sub-cell aim (preferred)" },
        x_key: { "type": "integer", "minimum": 0, "description": "screenshot pixel, escape hatch when a cell cannot express the point" },
        y_key: { "type": "integer", "minimum": 0, "description": "screenshot pixel, escape hatch when a cell cannot express the point" },
    })
    .as_object()
    .cloned()
    .unwrap_or_default()
}

/// (name, description, inputSchema) for every tool. Single source of truth
/// for `tools/list` and for the Web UI help panel.
pub fn tool_catalog() -> Vec<(String, String, serde_json::Value)> {
    vec![
        (
            "desktop.info".to_string(),
            "Display size and the currently focused window. Call first to learn the screen geometry."
                .to_string(),
            schema(serde_json::json!({}), &[]),
        ),
        (
            "desktop.screenshot".to_string(),
            "One JPEG screenshot of the whole desktop, returned as an MCP image block. Gridded with reference lines and numbered axes — pass a cell (g<col>,<row>) straight to the action tools; never guess pixels."
                .to_string(),
            schema(
                serde_json::json!({
                    "grid": { "type": "boolean", "default": true, "description": "overlay thin red grid reference lines (default true)" },
                }),
                &[],
            ),
        ),
        (
            "desktop.click".to_string(),
            "Click a grid cell read off the screenshot (e.g. g14,3, or g3,7@0.5,0.5 for sub-cell aim) — resolved server-side to exact pixels, never guessed. Raw x/y are an escape hatch only. Waits ~1s for the UI to settle before returning, so screenshot next."
                .to_string(),
            {
                let mut props = aim_props("cell", "x", "y");
                props.insert(
                    "button".to_string(),
                    serde_json::json!({ "type": "string", "enum": ["left", "middle", "right"], "default": "left" }),
                );
                schema(serde_json::Value::Object(props), &[])
            },
        ),
        (
            "desktop.double_click".to_string(),
            "Double-click a grid cell (e.g. desktop icons need this — single click only selects). Same cell-or-pixels aiming as desktop.click. Waits ~1s for the UI to settle before returning."
                .to_string(),
            schema(serde_json::Value::Object(aim_props("cell", "x", "y")), &[]),
        ),
        (
            "desktop.type".to_string(),
            "Type text into the currently focused control. Focus the field with a click first."
                .to_string(),
            schema(
                serde_json::json!({
                    "text": { "type": "string", "minLength": 1 },
                }),
                &["text"],
            ),
        ),
        (
            "desktop.key".to_string(),
            "Press keys: named keys (Return, Tab, Escape, ctrl, alt, shift, super, F1..F12, arrows) or single chars. Chords as a list, e.g. [\"ctrl\", \"l\"]."
                .to_string(),
            schema(
                serde_json::json!({
                    "keys": { "type": "array", "items": { "type": "string" }, "minItems": 1 },
                }),
                &["keys"],
            ),
        ),
        (
            "desktop.scroll".to_string(),
            "Scroll wheel over a grid cell. dy<0 scrolls down (e.g. -600), dy>0 up.".to_string(),
            {
                let mut props = aim_props("cell", "x", "y");
                props.insert(
                    "dy".to_string(),
                    serde_json::json!({ "type": "integer", "description": "negative = down, positive = up" }),
                );
                schema(serde_json::Value::Object(props), &["dy"])
            },
        ),
        (
            "desktop.drag".to_string(),
            "Drag from one grid cell to another (selection, sliders, window moves)."
                .to_string(),
            {
                let mut props = aim_props("from_cell", "from_x", "from_y");
                props.extend(aim_props("to_cell", "to_x", "to_y"));
                schema(serde_json::Value::Object(props), &[])
            },
        ),
        (
            "desktop.wait".to_string(),
            "Wait for the desktop to settle (app launch, page load). Prefer this over guessing sleeps between steps."
                .to_string(),
            schema(
                serde_json::json!({
                    "ms": { "type": "integer", "minimum": 0 },
                }),
                &["ms"],
            ),
        ),
        (
            "shell.exec".to_string(),
            "Run a bash command on the VM (DISPLAY is set): launch the app under test, list files, inspect state. 30s timeout. Destructive commands are refused — confirm with the user instead."
                .to_string(),
            schema(
                serde_json::json!({
                    "command": { "type": "string", "minLength": 1 },
                }),
                &["command"],
            ),
        ),
        (
            "test.start".to_string(),
            "Begin a named test run so the Web UI history shows what the agent did, step by step."
                .to_string(),
            schema(
                serde_json::json!({
                    "goal": { "type": "string", "minLength": 1, "description": "what is being tested, e.g. 'export March invoices to CSV'" },
                }),
                &["goal"],
            ),
        ),
        (
            "test.finish".to_string(),
            "Finish a test run with a pass/fail verdict and a one-line summary.".to_string(),
            schema(
                serde_json::json!({
                    "run_id": { "type": "string" },
                    "passed": { "type": "boolean" },
                    "summary": { "type": "string" },
                }),
                &["run_id", "passed", "summary"],
            ),
        ),
        (
            "video.list".to_string(),
            "List uploaded narrated recordings available to turn into skills.".to_string(),
            schema(serde_json::json!({}), &[]),
        ),
        (
            "video.to_skill".to_string(),
            "Turn one uploaded video into a saved skill: Spark drafts generalized instructions from the recording (the user's narration is retained verbatim in each step), the skill is saved and its skill.md written. Returns {skill_id, name, instructions, skill_md, steps}.".to_string(),
            schema(
                serde_json::json!({
                    "video_id": { "type": "string", "minLength": 1 },
                    "name": { "type": "string", "description": "skill name (default: from the video file name)" },
                }),
                &["video_id"],
            ),
        ),
        (
            "skill.list".to_string(),
            "List saved skills, newest first.".to_string(),
            schema(serde_json::json!({}), &[]),
        ),
        (
            "skill.get".to_string(),
            "Get one saved skill with its steps.".to_string(),
            schema(
                serde_json::json!({
                    "skill_id": { "type": "string", "minLength": 1 },
                }),
                &["skill_id"],
            ),
        ),
        (
            "skill.doc".to_string(),
            "Get one saved skill rendered as markdown (its skill.md): generalized instructions plus every step with the user's original narration retained.".to_string(),
            schema(
                serde_json::json!({
                    "skill_id": { "type": "string", "minLength": 1 },
                }),
                &["skill_id"],
            ),
        ),
        (
            "skill.update".to_string(),
            "Edit a saved skill: update its name, instructions, and/or steps (omitted fields keep their current values). Use this to correct a skill doc instead of recreating the skill.".to_string(),
            schema(
                serde_json::json!({
                    "skill_id": { "type": "string", "minLength": 1 },
                    "name": { "type": "string", "description": "new skill name (omitted = keep)" },
                    "instructions": { "type": "string", "description": "new instructions text (omitted = keep)" },
                    "steps": { "type": "array", "items": {}, "description": "replacement step list (omitted = keep)" },
                }),
                &["skill_id"],
            ),
        ),
    ]
}

// ---------- dispatch ----------

fn u32_arg(args: &serde_json::Value, key: &str) -> Result<u32, String> {
    args.get(key)
        .and_then(|v| v.as_u64())
        .and_then(|n| u32::try_from(n).ok())
        .ok_or_else(|| format!("{key} must be a non-negative integer"))
}

/// Resolve one aim point from tool arguments: a grid `cell` reference is
/// resolved server-side against a fresh capture (exact live geometry — the
/// agent never converts grid to pixels itself, so hallucinations and pixel
/// shifts can't creep in). Raw `x`/`y` pixels are the escape hatch for
/// points a cell cannot express.
async fn resolve_point(
    backend: &Arc<dyn DesktopBackend>,
    args: &serde_json::Value,
    cell_key: &str,
    x_key: &str,
    y_key: &str,
) -> Result<(u32, u32), String> {
    if let Some(cell) = args.get(cell_key).and_then(|v| v.as_str()) {
        if cell.trim().is_empty() {
            return Err(format!("{cell_key} must not be empty"));
        }
        let frame = backend.capture().await.map_err(|e| e.to_string())?;
        return rose_grid::resolve_grid_cell(cell, frame.width, frame.height).ok_or_else(|| {
            format!(
                "cannot resolve grid cell {cell:?} — use g<col>,<row>[@fx,fy] read off a gridded screenshot"
            )
        });
    }
    let need = format!("give {cell_key} (preferred) or {x_key}+{y_key} pixels");
    let x = args
        .get(x_key)
        .and_then(|v| v.as_u64())
        .and_then(|n| u32::try_from(n).ok())
        .ok_or_else(|| need.clone())?;
    let y = args
        .get(y_key)
        .and_then(|v| v.as_u64())
        .and_then(|n| u32::try_from(n).ok())
        .ok_or_else(|| need.clone())?;
    Ok((x, y))
}

fn text_content(text: String) -> serde_json::Value {
    serde_json::json!({ "content": [{ "type": "text", "text": text }] })
}

/// Server-side settle after a screen-mutating action: give the UI a
/// second to catch up before the agent's next screenshot. Ignored by
/// `desktop.wait` (it already slept) and skipped on failure paths.
async fn settle_after_action() {
    tokio::time::sleep(std::time::Duration::from_secs(
        POST_ACTION_SETTLE_SECS,
    ))
    .await;
}

/// Execute one tool. Returns the MCP `result` object (`{content, isError?}`).
/// Every desktop/shell action is audited as `Actor::Agent` (external agent);
/// human Web UI input keeps riding `Actor::Human` through its own path.
pub async fn call_tool(
    backend: &Arc<dyn DesktopBackend>,
    session: &Option<Arc<dyn TestSession>>,
    policy: &McpPolicy,
    display: &str,
    run_id: Option<&str>,
    name: &str,
    args: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let args = if args.is_null() {
        &serde_json::json!({})
    } else {
        args
    };
    if !args.is_object() {
        return Err("arguments must be an object".to_string());
    }
    let record = |action: &Action| {
        let session = session.clone();
        let run_id = run_id.map(str::to_string);
        let action = action.clone();
        async move {
            if let (Some(s), Some(id)) = (session, run_id) {
                s.record_act(&id, &action, Actor::Agent).await;
            }
        }
    };
    match name {
        "desktop.info" => {
            let info = backend.info().await.map_err(|e| e.to_string())?;
            Ok(text_content(
                serde_json::to_string(&serde_json::json!({
                    "display": info.display,
                    "width": info.width,
                    "height": info.height,
                    "active_window": info.active_window,
                }))
                .unwrap_or_default(),
            ))
        }
        "desktop.screenshot" => {
            let grid = args.get("grid").and_then(|v| v.as_bool()).unwrap_or(true);
            let frame = backend.capture().await.map_err(|e| e.to_string())?;
            let jpeg = if grid {
                rose_grid::annotate_grid_jpeg(&frame.jpeg).map_err(|e| e.to_string())?
            } else {
                frame.jpeg.clone()
            };
            Ok(serde_json::json!({
                "content": [
                    { "type": "image", "data": base64::Engine::encode(
                        &base64::engine::general_purpose::STANDARD, &jpeg),
                      "mimeType": "image/jpeg" },
                    { "type": "text", "text": serde_json::to_string(&serde_json::json!({
                        "width": frame.width, "height": frame.height,
                        "active_window": frame.active_window, "grid": grid,
                    })).unwrap_or_default() },
                ]
            }))
        }
        "desktop.click" => {
            let (x, y) = resolve_point(backend, args, "cell", "x", "y").await?;
            let button = match args
                .get("button")
                .and_then(|v| v.as_str())
                .unwrap_or("left")
            {
                "left" => MouseButton::Left,
                "middle" => MouseButton::Middle,
                "right" => MouseButton::Right,
                other => return Err(format!("unknown button: {other}")),
            };
            let action = Action::Click {
                target: Target::Point { x, y },
                button,
            };
            backend
                .act(&action, Actor::Agent)
                .await
                .map_err(|e| e.to_string())?;
            record(&action).await;
            settle_after_action().await;
            Ok(text_content(format!("clicked ({x}, {y})")))
        }
        "desktop.double_click" => {
            let (x, y) = resolve_point(backend, args, "cell", "x", "y").await?;
            let action = Action::DoubleClick {
                target: Target::Point { x, y },
            };
            backend
                .act(&action, Actor::Agent)
                .await
                .map_err(|e| e.to_string())?;
            record(&action).await;
            settle_after_action().await;
            Ok(text_content(format!("double-clicked ({x}, {y})")))
        }
        "desktop.type" => {
            let text = args
                .get("text")
                .and_then(|v| v.as_str())
                .ok_or("text is required")?;
            if text.is_empty() {
                return Err("text must not be empty".to_string());
            }
            let action = Action::Type {
                text: text.to_string(),
            };
            backend
                .act(&action, Actor::Agent)
                .await
                .map_err(|e| e.to_string())?;
            record(&action).await;
            settle_after_action().await;
            Ok(text_content(format!(
                "typed {} chars (content hidden from logs)",
                text.chars().count()
            )))
        }
        "desktop.key" => {
            let keys: Vec<String> = args
                .get("keys")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|k| k.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            if keys.is_empty() {
                return Err("keys must list at least one key name".to_string());
            }
            let action = Action::PressKey { keys: keys.clone() };
            backend
                .act(&action, Actor::Agent)
                .await
                .map_err(|e| e.to_string())?;
            record(&action).await;
            settle_after_action().await;
            Ok(text_content(format!("pressed {}", keys.join("+"))))
        }
        "desktop.scroll" => {
            let (x, y) = resolve_point(backend, args, "cell", "x", "y").await?;
            let dy = args
                .get("dy")
                .and_then(|v| v.as_i64())
                .and_then(|n| i32::try_from(n).ok())
                .ok_or("dy must be an integer (negative = down)")?;
            let action = Action::Scroll {
                target: Target::Point { x, y },
                dy,
            };
            backend
                .act(&action, Actor::Agent)
                .await
                .map_err(|e| e.to_string())?;
            record(&action).await;
            settle_after_action().await;
            Ok(text_content(format!("scrolled dy={dy} at ({x}, {y})")))
        }
        "desktop.drag" => {
            let (x0, y0) = resolve_point(backend, args, "from_cell", "from_x", "from_y").await?;
            let (x1, y1) = resolve_point(backend, args, "to_cell", "to_x", "to_y").await?;
            let from = Target::Point { x: x0, y: y0 };
            let to = Target::Point { x: x1, y: y1 };
            let action = Action::Drag { from, to };
            backend
                .act(&action, Actor::Agent)
                .await
                .map_err(|e| e.to_string())?;
            record(&action).await;
            settle_after_action().await;
            Ok(text_content("dragged".to_string()))
        }
        "desktop.wait" => {
            let ms = u32_arg(args, "ms")?;
            let action = Action::Wait { ms };
            backend
                .act(&action, Actor::Agent)
                .await
                .map_err(|e| e.to_string())?;
            Ok(text_content(format!("waited {ms}ms")))
        }
        "shell.exec" => {
            let command = args
                .get("command")
                .and_then(|v| v.as_str())
                .ok_or("command is required")?;
            if command.is_empty() {
                return Err("command must not be empty".to_string());
            }
            policy.shell_gate(command)?;
            let out = shell_exec(display, command).await?;
            tracing::info!(command_len = command.len(), "mcp shell.exec");
            Ok(text_content(
                serde_json::to_string(&out).unwrap_or_default(),
            ))
        }
        "test.start" => {
            let session = session.as_ref().ok_or("test sessions are not configured")?;
            let goal = args
                .get("goal")
                .and_then(|v| v.as_str())
                .ok_or("goal is required")?;
            if goal.trim().is_empty() {
                return Err("goal must not be empty".to_string());
            }
            let run_id = session.start(goal).await;
            Ok(text_content(
                serde_json::to_string(&serde_json::json!({ "run_id": run_id })).unwrap_or_default(),
            ))
        }
        "test.finish" => {
            let session = session.as_ref().ok_or("test sessions are not configured")?;
            let run_id = args
                .get("run_id")
                .and_then(|v| v.as_str())
                .ok_or("run_id is required")?;
            let passed = args
                .get("passed")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let summary = args.get("summary").and_then(|v| v.as_str()).unwrap_or("");
            let ok = session.finish(run_id, passed, summary).await;
            Ok(text_content(
                serde_json::to_string(&serde_json::json!({ "ok": ok })).unwrap_or_default(),
            ))
        }
        other => Err(format!("unknown tool: {other}")),
    }
}

/// Execute one tool, including the `video.*` / `skill.*` tools backed by
/// `skills`. All desktop/shell/test tools delegate to [`call_tool`], so
/// there is exactly one dispatch path for them.
#[allow(clippy::too_many_arguments)]
pub async fn call_tool_ext(
    backend: &Arc<dyn DesktopBackend>,
    session: &Option<Arc<dyn TestSession>>,
    skills: &Option<Arc<dyn SkillService>>,
    policy: &McpPolicy,
    display: &str,
    run_id: Option<&str>,
    name: &str,
    args: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let args = if args.is_null() {
        &serde_json::json!({})
    } else {
        args
    };
    let svc = || {
        skills
            .as_ref()
            .ok_or_else(|| "video/skill service is not configured".to_string())
    };
    match name {
        "video.list" => {
            let videos = svc()?.list_videos().await;
            Ok(text_content(
                serde_json::to_string(&serde_json::json!({ "videos": videos })).unwrap_or_default(),
            ))
        }
        "video.to_skill" => {
            let video_id = args
                .get("video_id")
                .and_then(|v| v.as_str())
                .filter(|s| !s.trim().is_empty())
                .ok_or("video_id is required")?;
            let name = args
                .get("name")
                .and_then(|v| v.as_str())
                .filter(|s| !s.trim().is_empty());
            let skill = svc()?.video_to_skill(video_id, name).await?;
            Ok(text_content(
                serde_json::to_string(&skill).unwrap_or_default(),
            ))
        }
        "skill.list" => {
            let skills = svc()?.list_skills().await;
            Ok(text_content(
                serde_json::to_string(&serde_json::json!({ "skills": skills })).unwrap_or_default(),
            ))
        }
        "skill.get" => {
            let skill_id = args
                .get("skill_id")
                .and_then(|v| v.as_str())
                .filter(|s| !s.trim().is_empty())
                .ok_or("skill_id is required")?;
            match svc()?.skill(skill_id).await {
                Some(skill) => Ok(text_content(
                    serde_json::to_string(&skill).unwrap_or_default(),
                )),
                None => Err(format!("unknown skill: {skill_id}")),
            }
        }
        "skill.doc" => {
            let skill_id = args
                .get("skill_id")
                .and_then(|v| v.as_str())
                .filter(|s| !s.trim().is_empty())
                .ok_or("skill_id is required")?;
            match svc()?.skill_doc(skill_id).await {
                Some(doc) => Ok(text_content(doc)),
                None => Err(format!("unknown skill: {skill_id}")),
            }
        }
        "skill.update" => {
            let skill_id = args
                .get("skill_id")
                .and_then(|v| v.as_str())
                .filter(|s| !s.trim().is_empty())
                .ok_or("skill_id is required")?;
            let name = args.get("name").and_then(|v| v.as_str());
            let instructions = args.get("instructions").and_then(|v| v.as_str());
            let steps: Option<Vec<serde_json::Value>> = match args.get("steps") {
                None | Some(serde_json::Value::Null) => None,
                Some(v) => Some(
                    serde_json::from_value(v.clone())
                        .map_err(|e| format!("invalid steps: {e}"))?,
                ),
            };
            if name.is_none() && instructions.is_none() && steps.is_none() {
                return Err(
                    "nothing to update: give name, instructions, and/or steps".to_string(),
                );
            }
            let skill = svc()?
                .update_skill(skill_id, name, instructions, steps)
                .await?;
            Ok(text_content(
                serde_json::to_string(&skill).unwrap_or_default(),
            ))
        }
        _ => call_tool(backend, session, policy, display, run_id, name, args).await,
    }
}

// ---------- JSON-RPC envelope ----------

/// Handle one JSON-RPC 2.0 message for `POST /mcp`. Returns `None` for
/// notifications (no response). Protocol: `initialize`, `tools/list`,
/// `tools/call`, `ping`.
pub async fn handle_request(
    backend: &Arc<dyn DesktopBackend>,
    session: &Option<Arc<dyn TestSession>>,
    policy: &McpPolicy,
    display: &str,
    run_id: Option<&str>,
    body: &serde_json::Value,
) -> Option<serde_json::Value> {
    let id = body.get("id").cloned().unwrap_or(serde_json::Value::Null);
    let method = body.get("method").and_then(|m| m.as_str()).unwrap_or("");
    let params = body.get("params").cloned().unwrap_or(serde_json::json!({}));
    let ok = |result: serde_json::Value| {
        Some(serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": result }))
    };
    let err = |code: i64, message: String| {
        Some(
            serde_json::json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } }),
        )
    };
    match method {
        // Notifications get no response.
        "notifications/initialized" | "notifications/cancelled" => None,
        "initialize" => ok(serde_json::json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": { "tools": {} },
            "serverInfo": { "name": SERVER_NAME, "version": env!("CARGO_PKG_VERSION") },
        })),
        "ping" => ok(serde_json::json!({})),
        "tools/list" => ok(serde_json::json!({
            "tools": tool_catalog().into_iter().map(|(name, description, input_schema)| {
                serde_json::json!({ "name": name, "description": description, "inputSchema": input_schema })
            }).collect::<Vec<_>>(),
        })),
        "tools/call" => {
            let name = params
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            // Per-call run id lets one agent drive several test runs.
            let call_run = params
                .get("_meta")
                .and_then(|m| m.get("run_id"))
                .and_then(|v| v.as_str())
                .map(str::to_string);
            let call_run = call_run.as_deref().or(run_id);
            let args = params
                .get("arguments")
                .cloned()
                .unwrap_or(serde_json::json!({}));
            if name.is_empty() {
                return err(-32602, "tools/call needs params.name".to_string());
            }
            match call_tool(backend, session, policy, display, call_run, &name, &args).await {
                Ok(result) => ok(result),
                Err(message) => ok(serde_json::json!({
                    "content": [{ "type": "text", "text": format!("error: {message}") }],
                    "isError": true,
                })),
            }
        }
        "" => err(-32600, "invalid request: missing method".to_string()),
        _ => err(-32601, format!("method not found: {method}")),
    }
}

/// Like [`handle_request`], but `video.*` / `skill.*` tools are backed by
/// `skills` (video -> skill.md). Desktop/shell/test tools behave identically.
#[allow(clippy::too_many_arguments)]
pub async fn handle_request_ext(
    backend: &Arc<dyn DesktopBackend>,
    session: &Option<Arc<dyn TestSession>>,
    skills: &Option<Arc<dyn SkillService>>,
    policy: &McpPolicy,
    display: &str,
    run_id: Option<&str>,
    body: &serde_json::Value,
) -> Option<serde_json::Value> {
    // Only tools/call differs from the base handler; everything else
    // (initialize, tools/list, ping, notifications) delegates unchanged.
    let method = body.get("method").and_then(|m| m.as_str()).unwrap_or("");
    if method != "tools/call" {
        return handle_request(backend, session, policy, display, run_id, body).await;
    }
    let id = body.get("id").cloned().unwrap_or(serde_json::Value::Null);
    let params = body.get("params").cloned().unwrap_or(serde_json::json!({}));
    let name = params
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if name.is_empty() {
        return Some(
            serde_json::json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32602, "message": "tools/call needs params.name" } }),
        );
    }
    let call_run = params
        .get("_meta")
        .and_then(|m| m.get("run_id"))
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let call_run = call_run.as_deref().or(run_id);
    let args = params
        .get("arguments")
        .cloned()
        .unwrap_or(serde_json::json!({}));
    let result = call_tool_ext(
        backend, session, skills, policy, display, call_run, &name, &args,
    )
    .await;
    Some(match result {
        Ok(result) => serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": result }),
        Err(message) => serde_json::json!({
            "jsonrpc": "2.0", "id": id,
            "result": {
                "content": [{ "type": "text", "text": format!("error: {message}") }],
                "isError": true,
            }
        }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rose_core::{DesktopInfo, Frame, FrameId};

    struct Stub;
    #[async_trait]
    impl DesktopBackend for Stub {
        async fn info(&self) -> rose_core::Result<DesktopInfo> {
            Ok(DesktopInfo {
                display: ":stub".to_string(),
                width: 1600,
                height: 900,
                active_window: Some("Terminal".to_string()),
            })
        }
        async fn capture(&self) -> rose_core::Result<Frame> {
            // Minimal valid JPEG: SOI + EOI. Grid overlay needs a real
            // image, so screenshot tests use FakeDesktop-style bytes;
            // here we only test info + arg validation paths.
            Ok(Frame {
                id: FrameId::new(),
                captured_at: chrono::Utc::now(),
                width: 1600,
                height: 900,
                jpeg: vec![0xff, 0xd8, 0xff, 0xd9],
                active_window: Some("Terminal".to_string()),
            })
        }
        async fn act(&self, _action: &Action, _actor: Actor) -> rose_core::Result<()> {
            Ok(())
        }
    }

    struct NoSession;
    #[async_trait]
    impl TestSession for NoSession {
        async fn start(&self, _goal: &str) -> String {
            "run-stub".to_string()
        }
        async fn record_act(&self, _run: &str, _a: &Action, _actor: Actor) {}
        async fn finish(&self, _run: &str, _p: bool, _s: &str) -> bool {
            true
        }
    }

    fn backend() -> Arc<dyn DesktopBackend> {
        Arc::new(Stub)
    }

    #[tokio::test]
    async fn tools_list_has_desktop_and_shell_and_test() {
        let names: Vec<String> = tool_catalog().into_iter().map(|(n, _, _)| n).collect();
        for want in [
            "desktop.info",
            "desktop.screenshot",
            "desktop.click",
            "desktop.double_click",
            "desktop.type",
            "desktop.key",
            "desktop.scroll",
            "desktop.drag",
            "desktop.wait",
            "shell.exec",
            "test.start",
            "test.finish",
            "video.list",
            "video.to_skill",
            "skill.list",
            "skill.get",
            "skill.doc",
            "skill.update",
        ] {
            assert!(names.iter().any(|n| n == want), "missing {want}");
        }
        assert_eq!(names.len(), 18);
    }

    struct StubSkills;
    #[async_trait]
    impl SkillService for StubSkills {
        async fn list_videos(&self) -> Vec<serde_json::Value> {
            vec![serde_json::json!({"id": "v1", "name": "demo.mp4"})]
        }
        async fn video_to_skill(
            &self,
            video_id: &str,
            name: Option<&str>,
        ) -> Result<serde_json::Value, String> {
            Ok(serde_json::json!({
                "skill_id": "s1",
                "name": name.unwrap_or("demo"),
                "video_id": video_id,
                "skill_md": "# demo\n\nOpen it (user said: \"open it\")\n",
            }))
        }
        async fn list_skills(&self) -> Vec<serde_json::Value> {
            vec![serde_json::json!({"id": "s1", "name": "demo"})]
        }
        async fn skill(&self, skill_id: &str) -> Option<serde_json::Value> {
            (skill_id == "s1").then(|| serde_json::json!({"id": "s1", "name": "demo"}))
        }
        async fn skill_doc(&self, skill_id: &str) -> Option<String> {
            (skill_id == "s1").then(|| "# demo\n".to_string())
        }
        async fn update_skill(
            &self,
            skill_id: &str,
            name: Option<&str>,
            instructions: Option<&str>,
            steps: Option<Vec<serde_json::Value>>,
        ) -> Result<serde_json::Value, String> {
            if skill_id != "s1" {
                return Err(format!("unknown skill: {skill_id}"));
            }
            Ok(serde_json::json!({
                "id": "s1",
                "name": name.unwrap_or("demo"),
                "instructions": instructions.unwrap_or("do it"),
                "steps": steps.unwrap_or_default(),
            }))
        }
    }

    #[tokio::test]
    async fn video_to_skill_and_skill_doc_tools() {
        let skills: Option<Arc<dyn SkillService>> = Some(Arc::new(StubSkills));
        let none: Option<Arc<dyn TestSession>> = None;
        let out = call_tool_ext(
            &backend(),
            &none,
            &skills,
            &McpPolicy::default(),
            ":1",
            None,
            "video.to_skill",
            &serde_json::json!({"video_id": "v1"}),
        )
        .await
        .expect("to_skill");
        let text = out["content"][0]["text"].as_str().unwrap_or("");
        assert!(text.contains("skill_md"), "{text}");
        assert!(text.contains("user said"), "{text}");
        let doc = call_tool_ext(
            &backend(),
            &none,
            &skills,
            &McpPolicy::default(),
            ":1",
            None,
            "skill.doc",
            &serde_json::json!({"skill_id": "s1"}),
        )
        .await
        .expect("doc");
        assert_eq!(doc["content"][0]["text"], "# demo\n");
        // Unconfigured service errors clearly instead of panicking.
        let no_skills: Option<Arc<dyn SkillService>> = None;
        assert!(call_tool_ext(
            &backend(),
            &none,
            &no_skills,
            &McpPolicy::default(),
            ":1",
            None,
            "video.list",
            &serde_json::json!({}),
        )
        .await
        .is_err());
        // JSON-RPC path reaches the same tools.
        let resp = handle_request_ext(
            &backend(),
            &none,
            &skills,
            &McpPolicy::default(),
            ":1",
            None,
            &serde_json::json!({"jsonrpc": "2.0", "id": 9, "method": "tools/call",
                "params": {"name": "skill.list", "arguments": {}}}),
        )
        .await
        .expect("response");
        assert!(resp["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or("")
            .contains("s1"));
    }

    #[tokio::test]
    async fn skill_update_edits_instead_of_recreating() {
        let skills: Option<Arc<dyn SkillService>> = Some(Arc::new(StubSkills));
        let none: Option<Arc<dyn TestSession>> = None;
        let b = backend();
        // No fields: nothing to update.
        assert!(call_tool_ext(
            &b,
            &none,
            &skills,
            &McpPolicy::default(),
            ":1",
            None,
            "skill.update",
            &serde_json::json!({"skill_id": "s1"}),
        )
        .await
        .is_err());
        // Unknown id: clear error, not a silent no-op.
        assert!(call_tool_ext(
            &b,
            &none,
            &skills,
            &McpPolicy::default(),
            ":1",
            None,
            "skill.update",
            &serde_json::json!({"skill_id": "nope", "name": "x"}),
        )
        .await
        .is_err());
        // Rename keeps the rest; instructions edit sticks.
        let out = call_tool_ext(
            &b,
            &none,
            &skills,
            &McpPolicy::default(),
            ":1",
            None,
            "skill.update",
            &serde_json::json!({"skill_id": "s1", "name": "renamed"}),
        )
        .await
        .expect("update");
        let text = out["content"][0]["text"].as_str().unwrap_or("");
        assert!(text.contains("renamed"), "{text}");
        let out = call_tool_ext(
            &b,
            &none,
            &skills,
            &McpPolicy::default(),
            ":1",
            None,
            "skill.update",
            &serde_json::json!({"skill_id": "s1", "instructions": "new steps here"}),
        )
        .await
        .expect("update");
        let text = out["content"][0]["text"].as_str().unwrap_or("");
        assert!(text.contains("new steps here"), "{text}");
    }

    #[tokio::test]
    async fn click_settles_before_returning() {
        // The agent screenshotted faster than the UI updated: every
        // screen-mutating action waits ~1s server-side so the next
        // screenshot sees the settled screen.
        let started = std::time::Instant::now();
        call_tool(
            &backend(),
            &None,
            &McpPolicy::default(),
            ":1",
            None,
            "desktop.click",
            &serde_json::json!({"x": 100, "y": 200}),
        )
        .await
        .expect("click");
        assert!(
            started.elapsed() >= std::time::Duration::from_secs(POST_ACTION_SETTLE_SECS),
            "click must settle ~1s before returning"
        );
    }

    #[tokio::test]
    async fn desktop_info_reports_geometry() {
        let out = call_tool(
            &backend(),
            &None,
            &McpPolicy::default(),
            ":1",
            None,
            "desktop.info",
            &serde_json::json!({}),
        )
        .await
        .expect("info");
        let text = out["content"][0]["text"].as_str().unwrap_or("");
        assert!(text.contains("1600"), "{text}");
    }

    #[tokio::test]
    async fn click_validates_args() {
        let policy = McpPolicy::default();
        // Raw pixels still work as an escape hatch.
        assert!(call_tool(
            &backend(),
            &None,
            &policy,
            ":1",
            None,
            "desktop.click",
            &serde_json::json!({"x": 100, "y": 200}),
        )
        .await
        .is_ok());
        assert!(call_tool(
            &backend(),
            &None,
            &policy,
            ":1",
            None,
            "desktop.click",
            &serde_json::json!({"x": 100}),
        )
        .await
        .is_err());
        assert!(call_tool(
            &backend(),
            &None,
            &policy,
            ":1",
            None,
            "desktop.click",
            &serde_json::json!({"x": 100, "y": 200, "button": "sideways"}),
        )
        .await
        .is_err());
    }

    #[tokio::test]
    async fn click_resolves_grid_cells_server_side() {
        let policy = McpPolicy::default();
        // A grid cell aims without any pixel guessing (stub is 1600x900,
        // so g14,3 lands inside the frame).
        let out = call_tool(
            &backend(),
            &None,
            &policy,
            ":1",
            None,
            "desktop.click",
            &serde_json::json!({"cell": "g14,3"}),
        )
        .await
        .expect("cell click");
        let text = out["content"][0]["text"].as_str().unwrap_or("");
        assert!(text.contains("clicked (322, 57)"), "{text}");
        // Sub-cell fractions aim inside the cell.
        assert!(call_tool(
            &backend(),
            &None,
            &policy,
            ":1",
            None,
            "desktop.click",
            &serde_json::json!({"cell": "g3,7@0.5,0.5"}),
        )
        .await
        .is_ok());
        // Out-of-range cells and empty aims are refused, never guessed.
        assert!(call_tool(
            &backend(),
            &None,
            &policy,
            ":1",
            None,
            "desktop.click",
            &serde_json::json!({"cell": "g99,99"}),
        )
        .await
        .is_err());
        assert!(call_tool(
            &backend(),
            &None,
            &policy,
            ":1",
            None,
            "desktop.click",
            &serde_json::json!({}),
        )
        .await
        .is_err());
        // Drag endpoints take cells too.
        assert!(call_tool(
            &backend(),
            &None,
            &policy,
            ":1",
            None,
            "desktop.drag",
            &serde_json::json!({"from_cell": "g1,1", "to_cell": "g2,2"}),
        )
        .await
        .is_ok());
    }

    #[tokio::test]
    async fn shell_gates_dangerous_commands() {
        assert!(McpPolicy::default()
            .shell_gate("google-chrome https://example.com")
            .is_ok());
        assert!(McpPolicy::default().shell_gate("rm -rf /tmp/x").is_err());
        assert!(McpPolicy::default().shell_gate("submit the form").is_err());
        assert!(McpPolicy {
            allow_shell: false,
            ..McpPolicy::default()
        }
        .shell_gate("ls")
        .is_err());
    }

    #[tokio::test]
    async fn unknown_tool_errors() {
        assert!(call_tool(
            &backend(),
            &None,
            &McpPolicy::default(),
            ":1",
            None,
            "browser.goto",
            &serde_json::json!({}),
        )
        .await
        .is_err());
    }

    #[tokio::test]
    async fn jsonrpc_initialize_list_call_ping() {
        let b = backend();
        let none: Option<Arc<dyn TestSession>> = None;
        let init = handle_request(
            &b,
            &none,
            &McpPolicy::default(),
            ":1",
            None,
            &serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}}),
        )
        .await
        .expect("response");
        assert_eq!(init["result"]["serverInfo"]["name"], "rose-desktop-testing");

        let list = handle_request(
            &b,
            &none,
            &McpPolicy::default(),
            ":1",
            None,
            &serde_json::json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}}),
        )
        .await
        .expect("response");
        assert!(list["result"]["tools"].as_array().unwrap().len() >= 12);

        let call = handle_request(
            &b,
            &none,
            &McpPolicy::default(),
            ":1",
            None,
            &serde_json::json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call",
                "params": {"name": "desktop.info", "arguments": {}}}),
        )
        .await
        .expect("response");
        assert!(call["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or("")
            .contains("1600"));

        // Unknown tool surfaces as isError content, not a protocol error.
        let bad = handle_request(
            &b,
            &none,
            &McpPolicy::default(),
            ":1",
            None,
            &serde_json::json!({"jsonrpc": "2.0", "id": 4, "method": "tools/call",
                "params": {"name": "nope", "arguments": {}}}),
        )
        .await
        .expect("response");
        assert_eq!(bad["result"]["isError"], true);

        // Notifications get no response.
        assert!(handle_request(
            &b,
            &none,
            &McpPolicy::default(),
            ":1",
            None,
            &serde_json::json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        )
        .await
        .is_none());

        let ping = handle_request(
            &b,
            &none,
            &McpPolicy::default(),
            ":1",
            None,
            &serde_json::json!({"jsonrpc": "2.0", "id": 5, "method": "ping"}),
        )
        .await
        .expect("response");
        assert!(ping["result"].is_object());
    }

    #[tokio::test]
    async fn test_session_roundtrip() {
        let s: Option<Arc<dyn TestSession>> = Some(Arc::new(NoSession));
        let b = backend();
        let started = call_tool(
            &b,
            &s,
            &McpPolicy::default(),
            ":1",
            None,
            "test.start",
            &serde_json::json!({"goal": "export march invoices"}),
        )
        .await
        .expect("start");
        assert!(started["content"][0]["text"]
            .as_str()
            .unwrap_or("")
            .contains("run-stub"));
        // No session configured -> clear error.
        assert!(call_tool(
            &b,
            &None,
            &McpPolicy::default(),
            ":1",
            None,
            "test.start",
            &serde_json::json!({"goal": "x"}),
        )
        .await
        .is_err());
    }

    #[test]
    fn danger_list_matches_old_shell_rules() {
        assert!(looks_dangerous("sudo apt install foo"));
        assert!(!looks_dangerous("ls ~/Downloads"));
    }
}
