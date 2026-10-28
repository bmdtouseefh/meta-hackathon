# Rose — Implementation Plan for Coding Agents

> **Read first:** `architecture.html` (v5) is the product/architecture source of
> truth. This file is the build plan: module boundaries, contracts, task order,
> and acceptance tests. If the two disagree, stop and flag it — do not silently
> pick one.

**Product:** Rose, an AI agent on an Ubuntu VM (local or EC2) that sees the XFCE
desktop (TigerVNC Xvnc, `DISPLAY=:1`) and acts on it with mouse/keyboard.
**Service:** `rosed` — one Rust binary. **Models:** Meta only. Phase 1 uses
exactly one of them: **Muse Spark 1.3 (Contributor tier)** — low cost, one
call per step. Muse Voice and SAM 3.1 arrive in Phase 2+. **Phase 1 goal:** a user types an instruction in a Web UI
and Rose performs it on the desktop, step-verified, with approvals and takeover.

---

## 0. Rules for coding agents (non-negotiable)

1. **Work one task ID at a time** (§6). Each task lists the crates you may
   touch, what to build, and the test that proves it. Do not start a later
   task's work early "while you're there".
2. **Respect the dependency rules (§2).** They are what keep the system
   modular. A PR that imports `x11rb` outside `rose-desktop-x11`, or calls a
   model from `rose-web`, is wrong even if it works.
3. **No third-party agent/automation frameworks.** No browser-use, Playwright,
   Cua, OpenAdapt, LangGraph, etc. — as dependencies or copied code. Build on
   X11/CDP protocols and Meta APIs directly (see `architecture.html` §1).
4. **Every external dependency has a `Fake`.** No task may require an X server,
   an API key, or network access to pass its tests. Real integrations are
   proven by the manual end-to-end (E2E) checklists, not by unit tests.
5. **One atomic action per `act()` call.** No batching multiple clicks/keys
   into one tool call. This is load-bearing for verification and replay.
6. **Models never act directly.** Model output → agent loop → Governor →
   desktop backend. There must be no other path to input injection.
7. **Secrets come from env/config or the web Setup (server memory only)**,
   never from source, fixtures, or committed files. Typed secrets are never written to traces (parameter
   slots only). Web-set keys live in process memory, are never persisted or
   logged, and are cleared on restart; the value is never returned by any API.
8. **When a task is done:** run its acceptance command, tick its checkbox in
   this file, and append one line to `CHANGELOG.md` (`date task-id summary`).
   If you discover a spec gap, write it under §10 (Open questions) instead of
   inventing behaviour.

---

## 1. Modularity strategy

Three mechanisms, in order of preference. **Do not build a dynamic plugin
system** (no `.so` loading, no Wasm runtime in Phase 1) — Rust has no stable
plugin ABI and Rose doesn't need third-party plugins; it needs clean internal
seams so *we* can swap, disable, or delete parts.

| Mechanism | Use for | Example |
|---|---|---|
| **Trait + config-selected implementation** (runtime) | Anything we may swap, fake, or disable per deployment | `backend = "x11" \| "fake"`, `brain = "muse" \| "fake"` (Phase 2+: which segmenter/transcriber) |
| **Cargo features** (compile-time) | Removing whole capabilities from the binary | `--no-default-features` drops the Web UI or the X11 backend from a build |
| **Separate process with a file/JSON contract** | Exploratory, iteration-heavy work that shouldn't destabilise the daemon | Phase 2 Python video worker, invoked by `rosed` with a job file (§8) |

Consequences that must hold:

- The **agent loop depends only on traits** in `rose-core`. It must compile
  and run end-to-end against `FakeDesktop` + `FakePlanner` with zero I/O.
- **Disabling = one config line or one feature flag**, never a code edit:
  brain off (`brain = "fake"`) → scripted runs with no API key; voice off → UI hides
  the mic (Phase 2+); Web UI off → `rosed` runs headless on its API.
- **Deleting a module = deleting a crate + its factory line** in
  `apps/rosed/src/main.rs`. If deletion requires surgery elsewhere, the
  boundary was violated — fix the boundary, don't widen it.

---

## 2. Workspace layout & dependency rules

```
meta-hackathon/
├── architecture.html          # product source of truth (v5)
├── IMPLEMENTATION.md          # this file
├── CHANGELOG.md               # append one line per completed task
├── REFERENCES.md              # provenance log: what was consulted, per module
├── Cargo.toml                 # workspace
├── apps/
│   └── rosed/
│       └── src/main.rs        # composition root ONLY: config -> factories -> run
├── crates/
│   ├── rose-core/             # types, traits, errors, events, config schema
│   ├── rose-desktop-x11/      # DesktopBackend impl: X11 capture + XTEST input
│   ├── rose-desktop-fake/     # DesktopBackend impl: in-memory, deterministic
│   ├── rose-models/           # model traits' HTTP impls (Meta) + fakes
│   ├── rose-governor/         # pure policy: actions -> Allow/Deny/Approve
│   ├── rose-agent/            # the loop: observe->decide->ground->gate->act->verify
│   ├── rose-store/            # Store impl: SQLite + artifact files
│   └── rose-web/              # Axum HTTP/WS API + embedded static UI
├── ui/                        # index.html, app.js, style.css (embedded by rose-web)
├── prompts/                   # planner.md, verifier.md, elements.md (reloadable)
├── config/
│   └── rose.example.toml
├── deploy/
│   ├── install.sh             # idempotent setup for fresh Ubuntu Server
│   └── systemd/               # xvnc.service, rosed.service (user units)
├── fixtures/                  # screenshots, model response samples, test videos
└── video-worker/              # Phase 2 ONLY (Python); not in the Cargo workspace
```

### Allowed dependencies (add others only with a note in `CHANGELOG.md`)

| Crate | May depend on |
|---|---|
| `rose-core` | `serde`, `serde_json`, `thiserror`, `chrono`, `uuid`. **No Tokio, no I/O, no other rose crate.** |
| `rose-desktop-x11` | `rose-core`, `x11rb`, `image`, `tokio`, `tracing` |
| `rose-desktop-fake` | `rose-core`, `image`, `tokio` |
| `rose-models` | `rose-core`, `reqwest` (rustls), `serde_json`, `tokio`, `tracing`, `base64` |
| `rose-governor` | `rose-core` only. **Pure & synchronous: no I/O, no async, no models.** |
| `rose-agent` | `rose-core`, `rose-governor`, `tokio`, `tracing`. **Never** `rose-desktop-*`, `rose-models`, `rose-web`, `rusqlite`. |
| `rose-store` | `rose-core`, `rusqlite` (bundled), `tokio`, `tracing` |
| `rose-web` | `rose-core`, `rose-agent`, `rose-store`, `axum`, `tokio`, `tracing`. Calls agent/store traits only — never the desktop or models directly. |
| `apps/rosed` | Everything (it is the composition root), `anyhow`, `clap`, `toml`, `tracing-subscriber` |

### Cargo features (on `apps/rosed`, wired to crate features where needed)

| Feature | Default | Off means |
|---|---|---|
| `desktop-x11` | ✅ | No X11 backend in the binary (fake only) — for laptop dev/CI |
| `web-ui` | ✅ | No HTTP/WS server; `rosed` exposes only its internal API surface |
| `models-meta` | ✅ | Only fake models compile in (offline dev) |
| `video` | ❌ (Phase 2) | No recording upload/job endpoints |
| `skills` | ❌ (Phase 3) | No skill endpoints or skill tool |

---

## 3. Core contracts (`rose-core`) — freeze these first

These types are the seams everything plugs into. Changing one after Task C1
requires updating every implementation + this file in the same commit.

```rust
// ---------- Desktop ----------
#[async_trait]
pub trait DesktopBackend: Send + Sync {
    async fn info(&self) -> Result<DesktopInfo>;                 // size, display, active window
    async fn capture(&self) -> Result<Frame>;                    // one screenshot
    async fn act(&self, action: &Action, actor: Actor) -> Result<()>;  // ONE atomic action
}

pub struct Frame { pub id: FrameId, pub captured_at: DateTime<Utc>,
                   pub width: u32, pub height: u32, pub jpeg: Vec<u8>,
                   pub active_window: Option<String> }

pub enum Action {
    Click       { target: Target, button: MouseButton },
    DoubleClick { target: Target },
    Type        { text: String },              // into current focus
    PressKey    { keys: Vec<Key> },            // e.g. ["ctrl","l"], ["Return"]
    Scroll      { target: Target, dy: i32 },
    Drag        { from: Target, to: Target },
    Wait        { ms: u32 },
}
pub enum Target { Point { x: u32, y: u32 }, Element(ElementId) }
pub enum Actor  { Agent, Human }               // both ride the same path; audited

// ---------- Perception / grounding ----------
pub struct Element { pub id: ElementId, pub label: String, pub role: Option<String>,
                     pub bounds: Rect, pub confidence: f32, pub source: ElementSource }
pub enum ElementSource { Vision, Sam }         // Phase 4 may add: Ax, Atspi

#[async_trait]
pub trait Perception: Send + Sync {          // frame -> candidate elements
    async fn elements(&self, frame: &Frame, hint: Option<&str>) -> Result<Vec<Element>>;
}
#[async_trait]
pub trait Segmenter: Send + Sync {           // "the Export button" -> exact mask/point
    async fn locate(&self, frame: &Frame, description: &str) -> Result<Option<Point>>;
}

// ---------- Models ----------
pub struct StepContext { pub goal: String, pub history: Vec<StepRecord>,
                         pub observation: Observation, pub remaining_budget: Budget }
pub enum Decision {
    Act   { intent: String, action: Action, target_hint: String,
            postcondition: String, risk: Risk },
    AskUser { question: String },
    Done  { summary: String },
    Fail  { reason: String },
}
pub enum Risk { Normal, Irreversible }         // Governor has final say

#[async_trait]
pub trait Planner: Send + Sync {
    async fn next(&self, ctx: &StepContext) -> Result<Decision>;
}
pub enum Verdict { Pass, Fail { reason: String }, Unsure }
#[async_trait]
pub trait Verifier: Send + Sync {
    async fn verify(&self, intent: &str, postcondition: &str,
                    before: &Frame, after: &Frame) -> Result<Verdict>;
}
// Phase 2+: Transcriber (Muse Voice) joins rose-models behind the same pattern.

// ---------- Observation the planner sees ----------
pub struct Observation { pub frame: Frame, pub elements: Vec<Element> }

// ---------- Store ----------
#[async_trait]
pub trait Store: Send + Sync {
    async fn create_task(&self, goal: &str, source: TaskSource) -> Result<TaskId>;
    async fn start_run(&self, task: TaskId) -> Result<RunId>;
    async fn record_step(&self, run: RunId, step: &StepRecord) -> Result<()>;
    async fn set_run_state(&self, run: RunId, state: RunState) -> Result<()>;
    async fn recent_runs(&self, limit: u32) -> Result<Vec<RunSummary>>;
    async fn save_artifact(&self, run: RunId, kind: ArtifactKind, bytes: &[u8]) -> Result<ArtifactId>;
    // Phase 3: skill CRUD behind feature "skills"
}

// ---------- Events (agent -> web, tokio broadcast) ----------
pub enum Event {
    RunStateChanged { run: RunId, state: RunState },
    StepCompleted   { run: RunId, step: StepRecord },
    ApprovalNeeded  { run: RunId, intent: String, frame: FrameId },
    AgentStatus     { status: AgentStatus },   // Idle | Running | WaitingApproval | WaitingHuman
}
```

**Governor contract** (pure, in `rose-governor`):

```rust
pub enum Gate { Allow, Deny { reason: String }, NeedsApproval { reason: String } }
pub fn evaluate(action: &Action, ctx: &GateContext, policy: &Policy) -> Gate;
```

`Policy` is data (config + per-run counters): irreversible action patterns,
URL/app allowlist, max steps / wall-time / model calls, redaction on/off.
The Governor never performs I/O; approvals are the agent loop's state
machine reacting to `Gate::NeedsApproval`.

---

## 4. Data model (SQLite, `rose-store`)

```sql
CREATE TABLE tasks   (id TEXT PK, goal TEXT, source TEXT, created_at TEXT);
CREATE TABLE runs    (id TEXT PK, task_id TEXT FK, state TEXT, started_at TEXT,
                      ended_at TEXT, summary TEXT, model_calls INTEGER DEFAULT 0);
CREATE TABLE steps   (id TEXT PK, run_id TEXT FK, idx INTEGER, intent TEXT,
                      action_json TEXT, locator TEXT, verdict TEXT,
                      before_frame TEXT, after_frame TEXT, created_at TEXT);
CREATE TABLE artifacts(id TEXT PK, run_id TEXT FK, kind TEXT, path TEXT, created_at TEXT);
CREATE TABLE approvals(id TEXT PK, run_id TEXT FK, intent TEXT, decision TEXT, created_at TEXT);
-- Phase 3 (feature "skills"): skills(id, name, version, yaml, status, created_at)
```

Frames/artifacts are files: `~/.local/share/rose/artifacts/<run-id>/…`.
DB stores paths, never blobs. No migrations framework in Phase 1 — one
`schema.sql` + `PRAGMA user_version`; write a migration only when the schema
actually changes.

---

## 5. HTTP / WebSocket API (`rose-web`)

| Route | Body / behaviour |
|---|---|
| `GET /` | Embedded UI (single page, §7 of architecture) |
| `POST /tasks` | `{ "goal": "..." }` → `{ task_id, run_id }`; starts the loop |
| `GET /runs?limit=20` | Recent `RunSummary[]` |
| `GET /runs/:id` | Run + step records (timeline) |
| `POST /runs/:id/stop` | Kill switch: cancel loop, release lease |
| `POST /runs/:id/approve` / `/deny` | Resolve a pending `ApprovalNeeded` |
| `POST /api/keys` | `{ "muse_api_key": "..." }` → memory-only key (Setup modal); `GET` shows `{set, source}`, `DELETE` clears |
| `POST /api/step` | Prototype single step: `{ "goal": "..." }` → `{say, acted, approval_needed, done}`; `force: true` bypasses the keyword gate (full loop in M2) |
| `POST /desktop/takeover` / `/handback` | Lease to human / back to Rose (handback forces re-observe) |
| `WS /events` | `Event` stream (JSON) |
| `WS /desktop/frames` | Server→client: JPEG frames (~5–10 fps, only while page open or run active). Client→server: `{type:"click",x,y}` / `{type:"key",keys}` / `{type:"type",text}` — executed **only** while the human holds the lease |

Auth: single session token from config, `Authorization: Bearer …`; server
binds `127.0.0.1` by default (Tailscale/SSH tunnel is the perimeter — never
bind `0.0.0.0` without a config flag named `allow_remote = true`).

---

## 6. Phase 1 — the simple version (read this plainly)

Phase 1 is only this loop. Nothing more:

```text
1. Take a screenshot of the desktop (JPEG).
2. Send the screenshot + the user's prompt + a short history
   ("what I already did") to Muse Spark 1.3 Contributor tier.
3. The model replies with ONE small JSON action (see below).
4. If the action is risky (send / submit / pay / delete), ask the user first.
5. Do the action with XTEST (click / type / key / scroll).
6. Wait about 1 second. Save the screenshot + action in the run history.
7. Go back to 1, until the model says "done", the user presses Stop,
   or 40 steps have passed.
```

That is the whole agent. The model call does three jobs at once — it looks at
the new screenshot, judges whether the last action worked, and picks the next
action. There is **no** separate vision/element system, **no** separate
verifier call, and **no SAM in Phase 1**. SAM 3.1 comes later (Phase 2/3)
when we need precise grounding for learned skills. If clicks are sometimes
imprecise in Phase 1, that is expected and accepted for the prototype.

The only reply format the model may use (one JSON object, nothing else):

```json
{ "status": "act",  "action": "click", "x": 812, "y": 240,
  "text": null, "say": "Clicking Export" }
{ "status": "act",  "action": "type",  "text": "invoice march",
  "say": "Typing the search text" }
{ "status": "act",  "action": "key",   "text": "Return", "say": "Pressing Enter" }
{ "status": "act",  "action": "scroll","dy": -600, "say": "Scrolling down" }
{ "status": "done", "say": "The CSV is downloaded" }
{ "status": "ask",  "say": "Which month should I export?" }
{ "status": "fail", "say": "I cannot find the invoices page" }
```

Coordinates are screenshot pixels (screen is fixed at 1600×900, so this is
stable). `say` is the one-line explanation shown in the UI timeline.
If the reply is not valid JSON, ask the model once more; then stop the run.

So in Phase 1 there are only **two things to build well**:

| Piece | What it does | Rust module |
|---|---|---|
| **Hands** | `screenshot() -> JPEG` and `act(action)` via X11/XTEST | `rose-desktop-x11` (fake version for tests) |
| **Brain call** | screenshot + prompt + history → one JSON action (above) | `rose-models`: `next_action(...)` only |

Everything else (web page, SQLite, Governor keyword check, Stop) is plain
support code around that loop. The richer traits in §3 (`Perception`,
`Segmenter`, separate `Verifier`) are **Phase 2+ seams — do not build them in
Phase 1.** In Phase 1 the agent loop calls exactly:
`desktop.capture()`, `model.next_action(...)`, `desktop.act(...)`.

### M0 — Scaffold
- [x] **T0.1 Workspace skeleton.** Workspace `Cargo.toml`, all §2 crates as
  empty libs that compile, `apps/rosed` hello-world. `rustfmt` + `clippy -D warnings` clean.
  *Accept:* `cargo build && cargo test` green on a machine with no X server.
- [x] **T0.2 `rose-core` contracts.** Implemented §3 types/traits exactly;
  serde derives on all wire types; unit tests for (de)serialisation round-trips.
  *Touch:* `rose-core` only. *Accept:* `cargo test -p rose-core`.
- [x] **T0.3 Config.** `config/rose.example.toml` + loader in `apps/rosed`
  (TOML file + env overrides for secrets: `ROSE_MUSE_API_KEY`, etc.).
  Composition root builds a `Services` struct via factory fns keyed on config
  (`backend`, `brain`, `store` selectors).
  *Accept:* `rosed --print-config` shows resolved config with secrets redacted.

Config shape (extend, don't reshape):

```toml
[server]   bind = "127.0.0.1:8787"        # allow_remote = false
[desktop]  backend = "x11"                # "x11" | "fake"; display = ":1"; width=1600; height=900
[models]   brain = "muse"                 # "muse" | "fake"  (Muse Spark 1.3, Contributor tier)
           # SAM 3.1 is NOT used in Phase 1. It arrives in Phase 2/3.
[policy]   max_steps = 40; max_minutes = 15; max_model_calls = 60
           require_approval_for = ["submit","send","pay","delete","publish"]
[data]     dir = "~/.local/share/rose"
```

### M1 — Hands before brains (desktop + live view, no models)
- [x] **T1.1 `FakeDesktop`.** Deterministic in-memory backend: serves a
  generated test-pattern frame (draw grid + labelled rectangles with `image`),
  records every `act()` into a log, supports "clicking" rectangle N by
  changing the pattern (so verify logic has something to observe).
  *Touch:* `rose-desktop-fake`, `rose-core`. *Accept:* `cargo test -p rose-desktop-fake`.
- [x] **T1.2 `X11Desktop`.** Connect `$DISPLAY`; `capture()` = root-window
  GetImage → JPEG; `act()` = XTEST pointer/key events; `info()` = root size +
  active window (EWMH `_NET_ACTIVE_WINDOW` / title). Coordinate mapping is
  1:1 at the fixed resolution — no scaling logic in v1.
  *Touch:* `rose-desktop-x11`. *Accept:* `cargo test -p rose-desktop-x11`
  (unit) **and** manual E2E-1 (§7).
- [ ] **T1.3 `rosed doctor` + desktop debug CLI.** `rosed doctor` checks
  DISPLAY connectivity, frame size, XTEST availability. `rosed shot out.jpg`,
  `rosed click X Y`, `rosed type "text"`, `rosed key Return` subcommands on
  the same backend (essential debugging tools, keep forever).
  *Touch:* `apps/rosed`, backend crates. *Accept:* E2E-1 items 1–3.
- [ ] **T1.4 Web shell + live view + takeover.** Embedded single-page UI:
  header (status, Take Control/Hand Back, Stop), live frame canvas
  (`WS /desktop/frames`), instruction box (disabled logic OK — tasks don't
  run yet), empty timeline. Lease logic in `rose-agent` (`Lease` type):
  human input via WS executes only under human lease; agent input only under
  agent lease; takeover takes effect between actions.
  *Touch:* `rose-web`, `ui/`, `rose-agent` (lease only). *Accept:* E2E-1 complete.

### M2 — State, Governor, loop skeleton (fake models)
- [ ] **T2.1 `SqliteStore`.** §4 schema, `Store` impl, artifact file layout.
  *Accept:* `cargo test -p rose-store` (temp-dir DB).
- [ ] **T2.2 Governor.** `evaluate()` + `Policy` from config; irreversible
  detection = action/target-text heuristics supplied by the agent (the loop
  passes the intent text; Governor matches `require_approval_for` +
  budget counters). Table-driven unit tests (≥12 cases: allow, deny by
  allowlist, approve by keyword, deny by exhausted budget…).
  *Accept:* `cargo test -p rose-governor`.
- [ ] **T2.3 The loop (with a fake brain).** Implement exactly the §6 loop:
  screenshot → `model.next_action(goal, history, jpeg)` → Governor keyword
  check → `desktop.act(...)` → wait 1s → save step → repeat. Run states are
  just: `running`, `waiting_approval`, `done`, `failed`, `stopped`.
  Approval = pause the loop until the web layer answers. Stop = a shared
  flag checked before every action. The fake brain returns a scripted list
  of the §6 JSON actions from a fixture file (e.g. click, type, done), so
  the whole loop is testable with no API key.
  *Touch:* `rose-agent`, fake brain in `rose-models` (`brain = "fake"`).
  *Accept:* integration test: scripted run on FakeDesktop completes,
  asserts step order, one approval round-trip, and Stop mid-run — all in
  `cargo test -p rose-agent`, no display, no network.
- [ ] **T2.4 Web wiring.** `POST /tasks` → loop; `WS /events` → timeline UI;
  approvals banner; recent-runs list from store; Stop button.
  *Accept:* with `backend="fake"`, drive a full scripted run from the browser UI.

### M3 — One real brain call
- [x] **T3.1 Muse Spark client (the only model call in Phase 1).**
  In `rose-models`, one function: `next_action` (above). Unit-tested with
  fixture replies (valid action, done, garbage) — no network in tests.
  *Accept:* `cargo test -p rose-models` green; live proof needs a real key
  on the VM (Setup modal or `ROSE_MUSE_API_KEY`).

  ```rust
  async fn next_action(goal: &str, history: &[StepSummary], jpeg: &[u8])
      -> Result<ModelReply>   // the §6 JSON: act / done / ask / fail
  ```

  Use **Muse Spark 1.3, Contributor tier** (low cost — this call happens
  once per step, so tier matters more than peak capability here). Send the
  goal, a *short* history (previous actions + their `say` lines, not old
  screenshots), and the current JPEG. The system prompt lives in
  `prompts/brain.md` (a file, so it can be tuned without recompiling) and
  must include: the §6 reply format, "reply with one JSON object only",
  "click the centre of the target", "if you cannot see the target, scroll
  or say fail — do not guess wildly", "when the goal is visibly complete,
  reply done".
  Endpoint URL + API key come from config/env (`ROSE_MUSE_API_KEY`); all
  request/response code sits in `rose-models/src/spark.rs` so API details
  touch exactly one file. Bad/non-JSON reply → retry once → stop the run
  with a clear error. Never panic on model output.
  *Accept:* `cargo test -p rose-models` with fixture replies (valid action,
  done, garbage) — no network in tests.
- [ ] **T3.2 That's it for models in Phase 1.** No SAM, no element
  extraction, no verifier call, no embeddings. If a later phase needs them,
  they are added as new functions/crates behind §3's traits — the §6 loop
  does not change shape, it just gets better eyes.

### M4 — Real end-to-end + packaging
- [ ] **T4.1 Real run.** `backend="x11"`, `brain="muse"`: the §10 exit
  tasks on the real XFCE/Xvnc desktop. Fix failures as controller/loop bugs
  or prompt changes (prompts are files — tune them, log prompt changes in
  `CHANGELOG.md` like code).
- [ ] **T4.2 Setup script + services.** `deploy/install.sh` (idempotent:
  apt packages, Chrome `.deb`, `~/.vnc/xstartup`, user units
  `xvnc.service`/`rosed.service`, config from example, `loginctl enable-linger`),
  tested on a fresh Ubuntu Server VM **and** an EC2 instance.
  *Accept:* fresh box → script → open UI → "Hands test" (E2E-1) passes.
- [ ] **T4.3 Hardening pass.** Bind-address guard, token auth on all routes,
  budget enforcement proven (runaway fake planner stopped by Governor),
  frames not streamed when nobody is watching (WS subscriber count),
  log hygiene (no secrets, no base64 frames in logs).

**Phase 1 exit:** `architecture.html` §10 Phase 1 criteria, run and recorded
in `CHANGELOG.md` (task list, success count, approvals used).

---

## 7. Manual E2E checklists (keep in repo, tick per release)

**E2E-1 — Hands (no models).** On the target VM: (1) `rosed doctor` all-green;
(2) `rosed shot` produces a correct 1600×900 JPEG of the XFCE desktop;
(3) `rosed click/type/key` visibly operate a text editor via XTEST;
(4) browser UI shows live frames; (5) Take Control → your clicks in the page
move the real cursor; Rose input is refused under human lease; (6) Hand Back
→ agent lease restored; (7) direct VNC connection still works with `rosed` stopped.

**E2E-2 — Brains (fake everything, browser only).** `backend="fake"`,
scripted planner: full run from UI incl. one approval, one denied approval
(run aborts, desktop untouched), Stop mid-run (<1 s to halt), run visible in
Recent runs after `rosed` restart (SQLite persistence).

**E2E-3 — Real tasks.** The architecture §10 Phase 1 task set on a real
logged-in profile. Record per-task: success / steps / approvals / failure mode.

---

## 8. Phase 2 — video → skill (start only after Phase 1 exit)

Separate Python worker; `rosed` stays Rust and dumb about CV details.

- [ ] **V2.1 Job contract.** `rosed` (feature `video`) accepts an upload,
  writes `jobs/<id>/job.json` `{video_path, out_dir}`, runs
  `video-worker run jobs/<id>` as a subprocess, streams the worker's
  `progress.jsonl` lines (`{stage, pct, note}`) to the UI, and imports the
  resulting `skill.draft.yaml`. Worker crash/timeout = job failed, daemon unaffected.
- [ ] **V2.2 Worker pipeline** (`video-worker/`, Python): ffmpeg extract →
  Muse Voice transcript (timestamps) → frame-diff/cursor event detection →
  narration↔event alignment → Muse Spark + SAM 3.1 grounding → Skill DSL
  draft (schema in `architecture.html` §8) with per-step evidence +
  confidence. Each stage independently runnable on fixtures
  (`video-worker run --stage transcribe fixtures/demo1/`).
- [ ] **V2.3 Skill Studio.** New UI tab: step list synced to a `<video>`
  element (click step → seek), transcript line, target crop, editable
  intent/params/approval flags, confidence highlighting, Save (→ SQLite
  `skills` table, status `draft`→`reviewed`).
- **Exit:** architecture §10 Phase 2 criteria on 3 real narrated recordings.

## 9. Phase 3 — skill execution (start only after Phase 2 exit)

New crate `rose-skills` (feature `skills`) implementing a `SkillRunner`
behind a trait in `rose-core`, so it plugs into the agent like a model:

- [ ] **S3.1 DSL parser + validator** (YAML per architecture §8; parameter
  binding; locator ordering; reject raw absolute coordinates with no anchor).
- [ ] **S3.2 Semantic execution:** planner sees `skill.<name>` as a tool;
  invoking it runs DSL steps through the *same* §4 loop (Governor + verify
  per step — no shortcut path).
- [ ] **S3.3 Deterministic replay:** from recorded step traces
  (action + target descriptor + parameter slots), replay with new values;
  halt on first locator/postcondition mismatch; uncertain writes end as
  `needs_review`, never auto-retried; read-back postconditions where defined.
- [ ] **S3.4 Repair:** halted step → semantic retry of the intent under
  approvals → proposed skill YAML diff (v+1) stored as `draft`, never auto-published.
- [ ] **S3.5 Scheduler:** cron-like entries in SQLite creating tasks from
  skills (a `tasks.source = "schedule"` row + a timer in `rosed` — no external cron).
- **Exit:** architecture §10 Phase 3 criteria.

**Phase 4 (only on evidence):** `rose-desktop-cdp` implementing the same
`DesktopBackend`/`Perception` traits for Chromium windows, selected per-window
by the composition root. Nothing else changes — if it does, the §2 boundaries
were broken; fix them instead.

---

## 10. Open questions (append here; do not improvise answers in code)

1. Muse Voice timestamp granularity (word vs segment) — sets Phase 2
   alignment design. *Owner: confirm from API before V2.2.*
2. Muse Spark / SAM 3.1 exact endpoint + auth shapes — isolated in
   `rose-models/src/{spark,sam}.rs`; fill from official docs at T3.1.
   *(Resolved for Spark 2026-10-28, dev.meta.ai docs: `POST https://api.meta.ai/v1/chat/completions`,
   OpenAI-compatible, `Authorization: Bearer <key>`, model `muse-spark-1.3-contributor`,
   `developer` message steers, stateless, retry 429/5xx never 400. SAM still open.)*
3. Password-field redaction beyond fixed regions (vision-based) — post-Phase 1.
4. Frame rate vs cost: default 5 fps UI stream; measure in T4.1.

---

## 11. Definition of done (every task)

- [ ] Code compiles with `cargo clippy --workspace -- -D warnings`, `cargo fmt` applied
- [ ] Task's acceptance command green (paste output summary in the PR/commit body)
- [ ] No new dependency outside §2's table without a `CHANGELOG.md` note
- [ ] No secret, API key, or real screenshot committed (`fixtures/` uses synthetic/sample data only)
- [ ] Checkbox ticked here + one-line `CHANGELOG.md` entry
