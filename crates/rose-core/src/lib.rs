//! Rose core contracts: types, traits, errors, events, config schema.
//!
//! Source of truth: `IMPLEMENTATION.md` section 3. Changing anything here
//! requires updating every implementation + the spec in the same commit.
//!
//! Dependency rule: serde/serde_json/thiserror/chrono/uuid (+ async-trait,
//! see CHANGELOG) only. No Tokio, no I/O, no other rose crates.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fmt;

// ---------- errors ----------

/// All errors in the system funnel through here; the agent loop matches on
/// variants, never on strings.
#[derive(Debug, thiserror::Error)]
pub enum RoseError {
    #[error("desktop error: {0}")]
    Desktop(String),
    #[error("model error: {0}")]
    Model(String),
    #[error("store error: {0}")]
    Store(String),
    #[error("config error: {0}")]
    Config(String),
    #[error("budget exhausted: {0}")]
    BudgetExceeded(String),
    #[error("run cancelled")]
    Cancelled,
    #[error("internal: {0}")]
    Internal(String),
}

pub type Result<T> = std::result::Result<T, RoseError>;

// ---------- ids ----------

macro_rules! id {
    ($name:ident) => {
        #[doc = concat!("Strongly-typed id for `", stringify!($name), "` (uuid v4, serde as string).")]
        #[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub String);

        impl $name {
            pub fn new() -> Self {
                Self(uuid::Uuid::new_v4().to_string())
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl From<String> for $name {
            fn from(s: String) -> Self {
                Self(s)
            }
        }
    };
}

id!(TaskId);
id!(RunId);
id!(StepId);
id!(FrameId);
id!(ElementId);
id!(ArtifactId);
id!(ApprovalId);

// ---------- desktop ----------

/// Point in screenshot pixels. Screen is fixed 1600x900, so 1:1 mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Point {
    pub x: u32,
    pub y: u32,
}

/// Rectangle in screenshot pixels.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
}

/// Where an action lands: raw pixels (Phase 1) or a grounded element.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Target {
    Point { x: u32, y: u32 },
    Element(ElementId),
}

/// Exactly ONE atomic action per `act()` call. No batching.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Action {
    Click {
        target: Target,
        button: MouseButton,
    },
    DoubleClick {
        target: Target,
    },
    /// Type into the currently focused control.
    Type {
        text: String,
    },
    /// e.g. ["ctrl", "l"], ["Return"].
    PressKey {
        keys: Vec<String>,
    },
    Scroll {
        target: Target,
        dy: i32,
    },
    Drag {
        from: Target,
        to: Target,
    },
    Wait {
        ms: u32,
    },
}

/// Who injected the input. Both ride the same path; both are audited.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Actor {
    Agent,
    Human,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DesktopInfo {
    pub display: String,
    pub width: u32,
    pub height: u32,
    pub active_window: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Frame {
    pub id: FrameId,
    pub captured_at: DateTime<Utc>,
    pub width: u32,
    pub height: u32,
    /// JPEG bytes. Stored as files on disk, never in SQLite, never in logs.
    pub jpeg: Vec<u8>,
    pub active_window: Option<String>,
}

#[async_trait]
pub trait DesktopBackend: Send + Sync {
    async fn info(&self) -> Result<DesktopInfo>;
    /// One screenshot of the root window.
    async fn capture(&self) -> Result<Frame>;
    /// Perform ONE atomic action.
    async fn act(&self, action: &Action, actor: Actor) -> Result<()>;
}

// ---------- perception / grounding (Phase 2+ seams; Phase 1 uses raw points) ----------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ElementSource {
    Vision,
    Sam,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Element {
    pub id: ElementId,
    pub label: String,
    pub role: Option<String>,
    pub bounds: Rect,
    pub confidence: f32,
    pub source: ElementSource,
}

#[async_trait]
pub trait Perception: Send + Sync {
    /// Frame -> candidate elements.
    async fn elements(&self, frame: &Frame, hint: Option<&str>) -> Result<Vec<Element>>;
}

#[async_trait]
pub trait Segmenter: Send + Sync {
    /// "The Export button" -> exact point.
    async fn locate(&self, frame: &Frame, description: &str) -> Result<Option<Point>>;
}

// ---------- models ----------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Risk {
    Normal,
    Irreversible,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Decision {
    Act {
        intent: String,
        action: Action,
        target_hint: String,
        postcondition: String,
        risk: Risk,
    },
    AskUser {
        question: String,
    },
    Done {
        summary: String,
    },
    Fail {
        reason: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Verdict {
    Pass,
    Fail { reason: String },
    Unsure,
}

/// One line of history given to the planner (never old screenshots).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepSummary {
    pub idx: u32,
    pub intent: String,
    /// Short human narration shown in the UI timeline ("Clicking Export").
    pub say: String,
}

/// Per-run budgets; the Governor enforces them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Budget {
    pub max_steps: u32,
    pub max_minutes: u64,
    pub max_model_calls: u32,
    pub steps_used: u32,
    pub model_calls_used: u32,
}

impl Budget {
    pub fn steps_left(&self) -> u32 {
        self.max_steps.saturating_sub(self.steps_used)
    }

    pub fn calls_left(&self) -> u32 {
        self.max_model_calls.saturating_sub(self.model_calls_used)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Observation {
    pub frame: Frame,
    pub elements: Vec<Element>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StepContext {
    pub goal: String,
    pub history: Vec<StepSummary>,
    pub observation: Observation,
    pub remaining_budget: Budget,
}

#[async_trait]
pub trait Planner: Send + Sync {
    async fn next(&self, ctx: &StepContext) -> Result<Decision>;
}

#[async_trait]
pub trait Verifier: Send + Sync {
    async fn verify(
        &self,
        intent: &str,
        postcondition: &str,
        before: &Frame,
        after: &Frame,
    ) -> Result<Verdict>;
}

// ---------- store ----------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TaskSource {
    User,
    Schedule,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RunState {
    Queued,
    Running,
    WaitingApproval,
    WaitingHuman,
    Recovering,
    Succeeded,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunSummary {
    pub run_id: RunId,
    pub task_id: TaskId,
    pub goal: String,
    pub state: RunState,
    pub steps: u32,
    pub started_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StepRecord {
    pub id: StepId,
    pub idx: u32,
    pub intent: String,
    pub action: Action,
    pub locator: Option<String>,
    pub verdict: Option<Verdict>,
    pub before_frame: FrameId,
    pub after_frame: FrameId,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ArtifactKind {
    Frame,
    Download,
    Recording,
    Log,
}

#[async_trait]
pub trait Store: Send + Sync {
    async fn create_task(&self, goal: &str, source: TaskSource) -> Result<TaskId>;
    async fn start_run(&self, task: TaskId) -> Result<RunId>;
    async fn record_step(&self, run: RunId, step: &StepRecord) -> Result<()>;
    async fn set_run_state(&self, run: RunId, state: RunState) -> Result<()>;
    async fn recent_runs(&self, limit: u32) -> Result<Vec<RunSummary>>;
    async fn save_artifact(
        &self,
        run: RunId,
        kind: ArtifactKind,
        bytes: &[u8],
    ) -> Result<ArtifactId>;
}

// ---------- events (agent -> web, tokio broadcast) ----------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentStatus {
    Idle,
    Running,
    WaitingApproval,
    WaitingHuman,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Event {
    RunStateChanged {
        run: RunId,
        state: RunState,
    },
    StepCompleted {
        run: RunId,
        step: StepRecord,
    },
    ApprovalNeeded {
        run: RunId,
        intent: String,
        frame: FrameId,
    },
    AgentStatus {
        status: AgentStatus,
    },
}

// ---------- governor (pure data; evaluation lives in rose-governor) ----------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Gate {
    Allow,
    Deny { reason: String },
    NeedsApproval { reason: String },
}

/// What the Governor sees before every action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GateContext {
    pub action: Action,
    /// The planner's intent text; keyword matching happens here.
    pub intent: String,
    pub remaining_budget: Budget,
}

/// Policy is data (config + per-run counters), never code.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Policy {
    pub max_steps: u32,
    pub max_minutes: u64,
    pub max_model_calls: u32,
    pub require_approval_for: Vec<String>,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            max_steps: 40,
            max_minutes: 15,
            max_model_calls: 60,
            require_approval_for: vec![
                "submit".to_string(),
                "send".to_string(),
                "pay".to_string(),
                "delete".to_string(),
                "publish".to_string(),
            ],
        }
    }
}

// ---------- config schema (loaded by apps/rosed in T0.3) ----------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerConfig {
    pub bind: String,
    #[serde(default)]
    pub allow_remote: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DesktopConfig {
    /// "x11" | "fake".
    pub backend: String,
    pub display: String,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelsConfig {
    /// "muse" | "fake".
    pub brain: String,
    /// Meta Model API base URL (OpenAI-compatible). Override for tests/proxies.
    #[serde(default = "default_muse_endpoint")]
    pub endpoint: String,
    /// e.g. "muse-spark-1.3-contributor" (Contributor tier, Phase 1).
    #[serde(default = "default_muse_model")]
    pub model: String,
}

fn default_muse_endpoint() -> String {
    "https://api.meta.ai/v1".to_string()
}

fn default_muse_model() -> String {
    "muse-spark-1.3-contributor".to_string()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DataConfig {
    pub dir: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoseConfig {
    pub server: ServerConfig,
    pub desktop: DesktopConfig,
    pub models: ModelsConfig,
    pub policy: Policy,
    pub data: DataConfig,
}

impl Default for RoseConfig {
    fn default() -> Self {
        Self {
            server: ServerConfig {
                bind: "127.0.0.1:8787".to_string(),
                allow_remote: false,
            },
            desktop: DesktopConfig {
                backend: "x11".to_string(),
                display: ":1".to_string(),
                width: 1600,
                height: 900,
            },
            models: ModelsConfig {
                brain: "muse".to_string(),
                endpoint: default_muse_endpoint(),
                model: default_muse_model(),
            },
            policy: Policy::default(),
            data: DataConfig {
                dir: "~/.local/share/rose".to_string(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip<T>(v: &T) -> T
    where
        T: Serialize + for<'de> Deserialize<'de>,
    {
        let json = serde_json::to_string(v).expect("serialize");
        serde_json::from_str(&json).expect("deserialize")
    }

    fn sample_frame() -> Frame {
        Frame {
            id: FrameId("frame-1".to_string()),
            captured_at: Utc::now(),
            width: 1600,
            height: 900,
            jpeg: vec![0xff, 0xd8, 0xff, 0xd9],
            active_window: Some("Terminal".to_string()),
        }
    }

    #[test]
    fn ids_display_and_roundtrip() {
        let id = TaskId::new();
        assert!(!id.to_string().is_empty());
        assert_eq!(roundtrip(&id), id);
        let run = RunId::new();
        assert_eq!(roundtrip(&run), run);
    }

    #[test]
    fn actions_roundtrip() {
        let actions = vec![
            Action::Click {
                target: Target::Point { x: 812, y: 240 },
                button: MouseButton::Left,
            },
            Action::DoubleClick {
                target: Target::Element(ElementId("e-7".to_string())),
            },
            Action::Type {
                text: "invoice march".to_string(),
            },
            Action::PressKey {
                keys: vec!["ctrl".to_string(), "l".to_string()],
            },
            Action::Scroll {
                target: Target::Point { x: 800, y: 450 },
                dy: -600,
            },
            Action::Drag {
                from: Target::Point { x: 100, y: 100 },
                to: Target::Point { x: 200, y: 200 },
            },
            Action::Wait { ms: 1000 },
        ];
        for a in &actions {
            assert_eq!(&roundtrip(a), a);
        }
    }

    #[test]
    fn decisions_roundtrip() {
        let decisions = vec![
            Decision::Act {
                intent: "Clicking Export".to_string(),
                action: Action::Click {
                    target: Target::Point { x: 812, y: 240 },
                    button: MouseButton::Left,
                },
                target_hint: "Export button, top right".to_string(),
                postcondition: "export dialog visible".to_string(),
                risk: Risk::Normal,
            },
            Decision::AskUser {
                question: "Which month should I export?".to_string(),
            },
            Decision::Done {
                summary: "The CSV is downloaded".to_string(),
            },
            Decision::Fail {
                reason: "cannot find the invoices page".to_string(),
            },
        ];
        for d in &decisions {
            assert_eq!(&roundtrip(d), d);
        }
    }

    #[test]
    fn frame_and_observation_roundtrip() {
        let obs = Observation {
            frame: sample_frame(),
            elements: vec![Element {
                id: ElementId("e-1".to_string()),
                label: "Export".to_string(),
                role: Some("button".to_string()),
                bounds: Rect {
                    x: 780,
                    y: 220,
                    width: 90,
                    height: 32,
                },
                confidence: 0.93,
                source: ElementSource::Vision,
            }],
        };
        assert_eq!(roundtrip(&obs), obs);
    }

    #[test]
    fn events_and_gate_roundtrip() {
        let run = RunId::new();
        let events = vec![
            Event::RunStateChanged {
                run: run.clone(),
                state: RunState::Running,
            },
            Event::ApprovalNeeded {
                run: run.clone(),
                intent: "Submit the form".to_string(),
                frame: FrameId::new(),
            },
            Event::AgentStatus {
                status: AgentStatus::WaitingApproval,
            },
        ];
        for e in &events {
            assert_eq!(&roundtrip(e), e);
        }
        let gates = vec![
            Gate::Allow,
            Gate::Deny {
                reason: "domain not allowlisted".to_string(),
            },
            Gate::NeedsApproval {
                reason: "intent contains 'pay'".to_string(),
            },
        ];
        for g in &gates {
            assert_eq!(&roundtrip(g), g);
        }
    }

    #[test]
    fn config_default_matches_spec_shape() {
        let cfg = RoseConfig::default();
        assert_eq!(cfg.server.bind, "127.0.0.1:8787");
        assert!(!cfg.server.allow_remote);
        assert_eq!(cfg.desktop.width, 1600);
        assert_eq!(cfg.desktop.height, 900);
        assert_eq!(cfg.policy.max_steps, 40);
        let back: RoseConfig = serde_json::from_str(&serde_json::to_string(&cfg).unwrap()).unwrap();
        assert_eq!(back, cfg);
    }

    #[test]
    fn budget_arithmetic_saturates() {
        let b = Budget {
            max_steps: 40,
            max_minutes: 15,
            max_model_calls: 60,
            steps_used: 40,
            model_calls_used: 61,
        };
        assert_eq!(b.steps_left(), 0);
        assert_eq!(b.calls_left(), 0);
    }
}
