//! Persisted run traces (observability): every event the agent loop
//! emits to the chat is also appended, verbatim, to disk so a run can
//! be inspected after the browser is closed or `rosed` restarts.
//!
//! Layout under `<data dir>/traces/`:
//! - `<run-id>.jsonl` — one JSON event per line. First line is
//!   `run_meta` (goal, start time), last line is `final`.
//! - `<run-id>/step-<idx>.jpg` — the screenshot the brain decided on
//!   at that step (what it actually saw when it planned the action).
//!
//! Plain files on purpose: `cat`/`jq` on the VM beats a database for
//! debugging "why did it take 17 steps", and frames are files per the
//! spec (IMPLEMENTATION.md §4: DB stores paths, never blobs). Writes
//! are best-effort from the loop's perspective — a full disk must not
//! kill a run — but every failure is logged.

use rose_core::RunId;
use serde::Serialize;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Storage split, on purpose. **Database** (`<data dir>/rose.db`,
/// SQLite) holds structured history: `runs` (one row per run),
/// `events` (every trace event, queryable), `messages` (the chat:
/// user goals + assistant replies). **Files** hold screenshots
/// (`traces/<run-id>/step-NNN.jpg`) and a human-readable
/// `traces/<run-id>.jsonl` mirror of the events, so the VM can be
/// debugged with `cat`/`jq` even without the daemon. The database is
/// written alongside the files in `TraceRun::record`; if it cannot
/// be opened the files still work and reads fall back.
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS runs (
    run_id TEXT PRIMARY KEY,
    goal TEXT NOT NULL,
    state TEXT NOT NULL DEFAULT 'running',
    steps INTEGER NOT NULL DEFAULT 0,
    decisions INTEGER NOT NULL DEFAULT 0,
    say TEXT,
    started_at TEXT NOT NULL,
    ended_at TEXT
);
CREATE TABLE IF NOT EXISTS events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    run_id TEXT NOT NULL,
    ts TEXT NOT NULL,
    type TEXT NOT NULL,
    json TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_events_run ON events(run_id, id);
CREATE TABLE IF NOT EXISTS messages (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    run_id TEXT,
    role TEXT NOT NULL,
    text TEXT NOT NULL,
    ts TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS chats (
    chat_id TEXT PRIMARY KEY,
    title TEXT NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
";

/// One chat message (user goal or assistant reply), persisted.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ChatMessage {
    pub id: i64,
    pub run_id: Option<String>,
    pub role: String,
    pub text: String,
    pub ts: String,
}

type Db = Arc<Mutex<rusqlite::Connection>>;

/// Add `chat_id` to pre-chat databases and give every legacy run
/// its own chat (titled by its goal), so old history shows up as
/// separate sidebar chats instead of vanishing.
fn migrate_chats(conn: &rusqlite::Connection) {
    for table in ["runs", "messages"] {
        let has_column = conn
            .prepare(&format!("PRAGMA table_info({table})"))
            .and_then(|mut stmt| {
                let names = stmt.query_map([], |row| row.get::<_, String>(1))?;
                Ok(names.filter_map(|n| n.ok()).collect::<Vec<_>>())
            })
            .map(|names| names.iter().any(|n| n == "chat_id"))
            .unwrap_or(false);
        if !has_column {
            let _ = conn.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN chat_id TEXT"));
        }
    }
    let legacy: Vec<(String, String, String)> = conn
        .prepare("SELECT run_id, goal, started_at FROM runs WHERE chat_id IS NULL")
        .and_then(|mut stmt| {
            let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?;
            Ok(rows.filter_map(|r| r.ok()).collect())
        })
        .unwrap_or_default();
    for (run_id, goal, started_at) in legacy {
        let _ = conn.execute(
            "INSERT OR IGNORE INTO chats (chat_id, title, created_at, updated_at) VALUES (?1, ?2, ?3, ?3)",
            rusqlite::params![run_id, goal, started_at],
        );
        let _ = conn.execute(
            "UPDATE runs SET chat_id = ?1 WHERE run_id = ?1",
            rusqlite::params![run_id],
        );
        let _ = conn.execute(
            "UPDATE messages SET chat_id = ?1 WHERE run_id = ?1 AND chat_id IS NULL",
            rusqlite::params![run_id],
        );
    }
}

fn open_db(data_dir: &Path) -> Option<Db> {
    let path = data_dir.join("rose.db");
    match rusqlite::Connection::open(&path) {
        Ok(conn) => match conn.execute_batch(SCHEMA) {
            Ok(()) => {
                migrate_chats(&conn);
                Some(Arc::new(Mutex::new(conn)))
            }
            Err(e) => {
                tracing::warn!(error = %e, path = %path.display(), "could not init trace database");
                None
            }
        },
        Err(e) => {
            tracing::warn!(error = %e, path = %path.display(), "could not open trace database");
            None
        }
    }
}

/// One past run, summarised from its trace file for listings.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TraceSummary {
    pub run_id: String,
    pub chat_id: Option<String>,
    pub goal: String,
    /// done | failed | waiting_approval | finished | running (no final yet)
    pub state: String,
    /// Actions actually dispatched (count of `acted` events).
    pub steps: u32,
    /// Brain decisions (count of `planned` + `decision` events).
    pub decisions: u32,
    pub started_at: String,
    pub ended_at: Option<String>,
    /// The final reply text (or error), when the run has finished.
    pub say: Option<String>,
}

/// One conversation in the sidebar: a chat groups every run/turn
/// the user sent inside it, like ChatGPT/Open WebUI sessions.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ChatSummary {
    pub chat_id: String,
    pub title: String,
    /// State of the most recent run in the chat.
    pub state: String,
    pub turns: u32,
    pub steps: u32,
    pub started_at: String,
    pub updated_at: String,
    /// Most recent assistant reply, when any run has finished.
    pub say: Option<String>,
}

/// Run ids are uuids we minted; anything else is rejected so a run id
/// from the network can never become a path traversal.
fn valid_run_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

#[derive(Debug, Clone)]
pub struct TraceStore {
    root: PathBuf,
    db: Option<Db>,
}

impl TraceStore {
    /// Traces live in `<data dir>/traces`, the database at
    /// `<data dir>/rose.db`.
    pub fn new(data_dir: impl AsRef<Path>) -> Self {
        let data_dir = data_dir.as_ref();
        let db = std::fs::create_dir_all(data_dir)
            .ok()
            .and_then(|_| open_db(data_dir));
        Self {
            root: data_dir.join("traces"),
            db,
        }
    }

    /// Chat history (user goals + assistant replies), oldest first.
    /// From the database; falls back to deriving it from persisted
    /// run files for runs that predate the database.
    pub fn chat(&self, limit: u32) -> Vec<ChatMessage> {
        if let Some(db) = &self.db {
            if let Ok(conn) = db.lock() {
                if let Ok(mut stmt) = conn.prepare(
                    "SELECT id, run_id, role, text, ts FROM messages ORDER BY id DESC LIMIT ?1",
                ) {
                    if let Ok(rows) = stmt.query_map([limit], |row| {
                        Ok(ChatMessage {
                            id: row.get(0)?,
                            run_id: row.get(1)?,
                            role: row.get(2)?,
                            text: row.get(3)?,
                            ts: row.get(4)?,
                        })
                    }) {
                        let mut messages: Vec<ChatMessage> = rows.filter_map(|r| r.ok()).collect();
                        if !messages.is_empty() {
                            messages.reverse();
                            return messages;
                        }
                    }
                }
            }
        }
        // Fallback: derive from the run files (oldest first).
        let mut runs = self.list(limit);
        runs.reverse();
        let mut messages = Vec::new();
        for run in runs {
            messages.push(ChatMessage {
                id: 0,
                run_id: Some(run.run_id.clone()),
                role: "user".to_string(),
                text: run.goal,
                ts: run.started_at.clone(),
            });
            if let Some(say) = run.say {
                messages.push(ChatMessage {
                    id: 0,
                    run_id: Some(run.run_id.clone()),
                    role: "assistant".to_string(),
                    text: say,
                    ts: run.ended_at.unwrap_or(run.started_at),
                });
            }
        }
        messages
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn events_path(&self, run_id: &str) -> PathBuf {
        self.root.join(format!("{run_id}.jsonl"))
    }

    fn frames_dir(&self, run_id: &str) -> PathBuf {
        self.root.join(run_id)
    }

    /// Start a new trace. The returned handle appends events; dropping
    /// it never loses lines (each record is flushed). `chat_id`
    /// continues an existing chat; `None` starts a new one (titled by
    /// this first goal, like ChatGPT/Open WebUI).
    pub fn begin(&self, goal: &str, chat_id: Option<&str>) -> std::io::Result<TraceRun> {
        std::fs::create_dir_all(&self.root)?;
        let run_id = RunId::new().to_string();
        let chat_id = chat_id
            .filter(|id| valid_run_id(id))
            .map(str::to_string)
            .unwrap_or_else(|| RunId::new().to_string());
        std::fs::create_dir_all(self.frames_dir(&run_id))?;
        let file = std::fs::File::create(self.events_path(&run_id))?;
        let mut run = TraceRun {
            run_id: run_id.clone(),
            chat_id: chat_id.clone(),
            root: self.root.clone(),
            writer: std::io::BufWriter::new(file),
            db: self.db.clone(),
        };
        if let Some(db) = &run.db {
            if let Ok(conn) = db.lock() {
                let ts = chrono::Utc::now().to_rfc3339();
                let _ = conn.execute(
                    "INSERT OR IGNORE INTO chats (chat_id, title, created_at, updated_at) VALUES (?1, ?2, ?3, ?3)",
                    rusqlite::params![chat_id, goal, ts],
                );
                let _ = conn.execute(
                    "UPDATE chats SET updated_at = ?2 WHERE chat_id = ?1",
                    rusqlite::params![chat_id, ts],
                );
                let _ = conn.execute(
                    "INSERT OR REPLACE INTO runs (run_id, chat_id, goal, state, started_at) VALUES (?1, ?2, ?3, 'running', ?4)",
                    rusqlite::params![run_id, chat_id, goal, ts],
                );
                let _ = conn.execute(
                    "INSERT INTO messages (run_id, chat_id, role, text, ts) VALUES (?1, ?2, 'user', ?3, ?4)",
                    rusqlite::params![run_id, chat_id, goal, ts],
                );
            }
        }
        run.record(serde_json::json!({
            "type": "run_meta",
            "goal": goal,
            "chat_id": chat_id,
        }));
        Ok(run)
    }

    /// Persist the screenshot for one step; returns the file name to
    /// reference from the step's `observed` event.
    pub fn save_frame(&self, run_id: &str, step: u32, jpeg: &[u8]) -> std::io::Result<String> {
        if !valid_run_id(run_id) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "bad run id",
            ));
        }
        let name = format!("step-{step:03}.jpg");
        std::fs::create_dir_all(self.frames_dir(run_id))?;
        std::fs::write(self.frames_dir(run_id).join(&name), jpeg)?;
        Ok(name)
    }

    pub fn frame_path(&self, run_id: &str, step: u32) -> Option<PathBuf> {
        if !valid_run_id(run_id) {
            return None;
        }
        let path = self.frames_dir(run_id).join(format!("step-{step:03}.jpg"));
        path.is_file().then_some(path)
    }

    /// All events of one run, in order. `None` for unknown/bad ids.
    pub fn events(&self, run_id: &str) -> Option<Vec<serde_json::Value>> {
        if !valid_run_id(run_id) {
            return None;
        }
        let text = std::fs::read_to_string(self.events_path(run_id)).ok()?;
        Some(
            text.lines()
                .filter(|l| !l.trim().is_empty())
                .filter_map(|l| serde_json::from_str(l).ok())
                .collect(),
        )
    }

    pub fn summary(&self, run_id: &str) -> Option<TraceSummary> {
        let events = self.events(run_id)?;
        Some(summarize(run_id, &events))
    }

    /// Recent runs, newest first.
    pub fn list(&self, limit: u32) -> Vec<TraceSummary> {
        let mut summaries: Vec<TraceSummary> = match std::fs::read_dir(&self.root) {
            Ok(entries) => entries
                .filter_map(|e| e.ok())
                .filter_map(|e| {
                    let name = e.file_name().to_string_lossy().into_owned();
                    name.strip_suffix(".jsonl").map(str::to_string)
                })
                .filter_map(|id| self.summary(&id))
                .collect(),
            Err(_) => Vec::new(),
        };
        summaries.sort_by(|a, b| b.started_at.cmp(&a.started_at));
        summaries.truncate(limit as usize);
        summaries
    }

    /// All runs belonging to one chat, oldest first (its turns).
    pub fn runs_for_chat(&self, chat_id: &str) -> Vec<TraceSummary> {
        if !valid_run_id(chat_id) {
            return Vec::new();
        }
        let mut runs: Vec<TraceSummary> = self
            .list(500)
            .into_iter()
            .filter(|r| r.chat_id.as_deref() == Some(chat_id) || r.run_id == chat_id)
            .collect();
        runs.sort_by(|a, b| a.started_at.cmp(&b.started_at));
        runs
    }

    /// Recent (goal, reply) turns of a chat, oldest first — the
    /// conversation context handed to the brain on the next turn.
    pub fn chat_turns(&self, chat_id: &str, limit: usize) -> Vec<(String, Option<String>)> {
        let runs = self.runs_for_chat(chat_id);
        runs.iter()
            .rev()
            .take(limit)
            .rev()
            .map(|r| (r.goal.clone(), r.say.clone()))
            .collect()
    }

    /// Sidebar chats, most recently updated first.
    pub fn chats(&self, limit: u32) -> Vec<ChatSummary> {
        let mut db_chats: Vec<ChatSummary> = Vec::new();
        if let Some(db) = &self.db {
            if let Ok(conn) = db.lock() {
                if let Ok(mut stmt) = conn.prepare(
                    "SELECT c.chat_id, c.title, c.created_at, c.updated_at,
                            COUNT(r.run_id),
                            COALESCE((SELECT state FROM runs WHERE chat_id = c.chat_id ORDER BY started_at DESC LIMIT 1), 'running'),
                            COALESCE((SELECT say FROM runs WHERE chat_id = c.chat_id AND say IS NOT NULL ORDER BY started_at DESC LIMIT 1), NULL),
                            COALESCE(SUM(r.steps), 0)
                     FROM chats c LEFT JOIN runs r ON r.chat_id = c.chat_id
                     GROUP BY c.chat_id ORDER BY c.updated_at DESC LIMIT ?1",
                ) {
                    if let Ok(rows) = stmt.query_map([limit], |row| {
                        Ok(ChatSummary {
                            chat_id: row.get(0)?,
                            title: row.get(1)?,
                            started_at: row.get(2)?,
                            updated_at: row.get(3)?,
                            turns: row.get(4)?,
                            state: row.get(5)?,
                            say: row.get(6)?,
                            steps: row.get(7)?,
                        })
                    }) {
                        // Merged below with file-era chats that
                        // predate the database (no DB rows for them).
                        db_chats = rows.filter_map(|r| r.ok()).collect();
                    }
                }
            }
        }
        // Group the run files by chat (covers pre-database history).
        let mut by_chat: Vec<ChatSummary> = db_chats;
        for run in self.list(500) {
            let chat_id = run.chat_id.clone().unwrap_or_else(|| run.run_id.clone());
            if by_chat.iter().any(|c| c.chat_id == chat_id) {
                continue;
            }
            if let Some(chat) = by_chat.iter_mut().find(|c| c.chat_id == chat_id) {
                chat.turns += 1;
                chat.steps += run.steps;
            } else {
                by_chat.push(ChatSummary {
                    chat_id,
                    title: run.goal.clone(),
                    state: run.state.clone(),
                    turns: 1,
                    steps: run.steps,
                    started_at: run.started_at.clone(),
                    updated_at: run.started_at.clone(),
                    say: run.say.clone(),
                });
            }
        }
        by_chat.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        by_chat.truncate(limit as usize);
        by_chat
    }

    pub fn rename_chat(&self, chat_id: &str, title: &str) -> bool {
        if !valid_run_id(chat_id) || title.trim().is_empty() {
            return false;
        }
        if let Some(db) = &self.db {
            if let Ok(conn) = db.lock() {
                return conn
                    .execute(
                        "UPDATE chats SET title = ?2 WHERE chat_id = ?1",
                        rusqlite::params![chat_id, title.trim()],
                    )
                    .map(|n| n > 0)
                    .unwrap_or(false);
            }
        }
        false
    }

    /// Delete a chat and everything under it: DB rows, trace files,
    /// and step screenshots.
    pub fn delete_chat(&self, chat_id: &str) -> bool {
        if !valid_run_id(chat_id) {
            return false;
        }
        let runs = self.runs_for_chat(chat_id);
        if runs.is_empty() {
            return false;
        }
        for run in &runs {
            let _ = std::fs::remove_file(self.events_path(&run.run_id));
            let _ = std::fs::remove_dir_all(self.frames_dir(&run.run_id));
        }
        if let Some(db) = &self.db {
            if let Ok(conn) = db.lock() {
                let _ = conn.execute(
                    "DELETE FROM events WHERE run_id IN (SELECT run_id FROM runs WHERE chat_id = ?1)",
                    rusqlite::params![chat_id],
                );
                let _ = conn.execute(
                    "DELETE FROM messages WHERE chat_id = ?1",
                    rusqlite::params![chat_id],
                );
                let _ = conn.execute(
                    "DELETE FROM runs WHERE chat_id = ?1",
                    rusqlite::params![chat_id],
                );
                let _ = conn.execute(
                    "DELETE FROM chats WHERE chat_id = ?1",
                    rusqlite::params![chat_id],
                );
            }
        }
        true
    }
}

fn summarize(run_id: &str, events: &[serde_json::Value]) -> TraceSummary {
    let meta = events.first();
    let final_event = events.iter().rev().find(|e| e["type"] == "final");
    let (state, say, ended_at) = match final_event {
        Some(f) if f["stopped"] == true => (
            "stopped".to_string(),
            f["say"].as_str().map(str::to_string),
            f["ts"].as_str().map(str::to_string),
        ),
        Some(f) if f.get("error").is_some() => (
            "failed".to_string(),
            f["error"].as_str().map(str::to_string),
            f["ts"].as_str().map(str::to_string),
        ),
        Some(f) if f["approval_needed"] == true => (
            "waiting_approval".to_string(),
            f["say"].as_str().map(str::to_string),
            f["ts"].as_str().map(str::to_string),
        ),
        Some(f) if f["done"] == true => (
            "done".to_string(),
            f["say"].as_str().map(str::to_string),
            f["ts"].as_str().map(str::to_string),
        ),
        Some(f) => (
            "finished".to_string(),
            f["say"].as_str().map(str::to_string),
            f["ts"].as_str().map(str::to_string),
        ),
        None => ("running".to_string(), None, None),
    };
    TraceSummary {
        run_id: run_id.to_string(),
        chat_id: meta.and_then(|m| m["chat_id"].as_str().map(str::to_string)),
        goal: meta
            .map(|m| m["goal"].as_str().unwrap_or_default().to_string())
            .unwrap_or_default(),
        state,
        steps: events.iter().filter(|e| e["type"] == "acted").count() as u32,
        decisions: events
            .iter()
            .filter(|e| e["type"] == "planned" || e["type"] == "decision")
            .count() as u32,
        started_at: meta
            .and_then(|m| m["ts"].as_str())
            .unwrap_or_default()
            .to_string(),
        ended_at,
        say,
    }
}

/// Append-only handle for one live run's trace.
pub struct TraceRun {
    run_id: String,
    chat_id: String,
    root: PathBuf,
    writer: std::io::BufWriter<std::fs::File>,
    db: Option<Db>,
}

impl TraceRun {
    pub fn chat_id(&self) -> &str {
        &self.chat_id
    }

    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    /// Enrich an event with run id + timestamp, append it (flushed),
    /// and return the enriched event so the same object can be shown
    /// live. Persistence failure is logged, never propagated: the run
    /// must go on, but the gap is observable in the server log.
    pub fn record(&mut self, mut event: serde_json::Value) -> serde_json::Value {
        if let Some(obj) = event.as_object_mut() {
            obj.insert("run_id".to_string(), serde_json::json!(self.run_id));
            obj.insert("chat_id".to_string(), serde_json::json!(self.chat_id));
            obj.insert(
                "ts".to_string(),
                serde_json::json!(chrono::Utc::now().to_rfc3339()),
            );
        }
        match serde_json::to_string(&event) {
            Ok(line) => {
                if let Err(e) = writeln!(self.writer, "{line}").and_then(|_| self.writer.flush()) {
                    tracing::warn!(run_id = %self.run_id, error = %e, "trace write failed");
                }
                self.record_db(&event, &line);
            }
            Err(e) => {
                tracing::warn!(run_id = %self.run_id, error = %e, "trace serialize failed")
            }
        }
        event
    }

    /// Mirror one event into SQLite (structured history). Best-effort
    /// like the file write: failures are logged, never fatal.
    fn record_db(&self, event: &serde_json::Value, line: &str) {
        let Some(db) = &self.db else { return };
        let Ok(conn) = db.lock() else { return };
        let ts = event["ts"].as_str().unwrap_or_default();
        let kind = event["type"].as_str().unwrap_or_default();
        if let Err(e) = conn.execute(
            "INSERT INTO events (run_id, ts, type, json) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![self.run_id, ts, kind, line],
        ) {
            tracing::warn!(run_id = %self.run_id, error = %e, "trace db event write failed");
            return;
        }
        match kind {
            "acted" => {
                let _ = conn.execute(
                    "UPDATE runs SET steps = steps + 1 WHERE run_id = ?1",
                    rusqlite::params![self.run_id],
                );
            }
            "planned" | "decision" => {
                let _ = conn.execute(
                    "UPDATE runs SET decisions = decisions + 1 WHERE run_id = ?1",
                    rusqlite::params![self.run_id],
                );
            }
            "final" => {
                let state = if event["stopped"] == true {
                    "stopped"
                } else if event.get("error").is_some() {
                    "failed"
                } else if event["approval_needed"] == true {
                    "waiting_approval"
                } else if event["done"] == true {
                    "done"
                } else {
                    "finished"
                };
                let say = event["say"]
                    .as_str()
                    .or_else(|| event["error"].as_str())
                    .unwrap_or_default();
                let _ = conn.execute(
                    "UPDATE runs SET state = ?2, say = ?3, ended_at = ?4 WHERE run_id = ?1",
                    rusqlite::params![self.run_id, state, say, ts],
                );
                let _ = conn.execute(
                    "INSERT INTO messages (run_id, chat_id, role, text, ts) VALUES (?1, ?2, 'assistant', ?3, ?4)",
                    rusqlite::params![self.run_id, self.chat_id, say, ts],
                );
                let _ = conn.execute(
                    "UPDATE chats SET updated_at = ?2 WHERE chat_id = ?1",
                    rusqlite::params![self.chat_id, ts],
                );
            }
            _ => {}
        }
    }

    /// Save a step screenshot into this run's frame directory.
    pub fn save_frame(&self, step: u32, jpeg: &[u8]) -> std::io::Result<String> {
        let name = format!("step-{step:03}.jpg");
        std::fs::write(self.root.join(&self.run_id).join(&name), jpeg)?;
        Ok(name)
    }

    /// Save the SoM-annotated screenshot the brain actually decided on
    /// (red boxes + number badges). Raw `step-NNN.jpg` stays untouched so
    /// replays can show both; the `grounded` event references this file.
    pub fn save_grounded_frame(&self, step: u32, jpeg: &[u8]) -> std::io::Result<String> {
        let name = format!("step-{step:03}-grounded.jpg");
        std::fs::write(self.root.join(&self.run_id).join(&name), jpeg)?;
        Ok(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store(tag: &str) -> (TraceStore, PathBuf) {
        let dir = std::env::temp_dir().join(format!("rose-store-test-{tag}-{}", RunId::new()));
        let store = TraceStore::new(&dir);
        (store, dir)
    }

    #[test]
    fn traces_survive_a_fresh_store_instance() {
        let (store, dir) = temp_store("persist");
        let run_id = {
            let mut run = store.begin("open the browser", None).expect("begin");
            assert!(
                run.record(serde_json::json!({"type": "planned", "step": 0}))["ts"].is_string()
            );
            run.record(serde_json::json!({"type": "acted", "step": 0, "action": "click (1, 2)"}));
            run.record(serde_json::json!({
                "type": "final", "say": "Browser is open", "done": true,
                "acted": true, "approval_needed": false,
            }));
            run.run_id().to_string()
        };
        // A new instance (i.e. after a rosed restart) sees the same run.
        let reopened = TraceStore::new(&dir);
        let events = reopened.events(&run_id).expect("events persisted");
        assert_eq!(events.len(), 4); // meta + planned + acted + final
        assert_eq!(events[0]["type"], "run_meta");
        let summary = reopened.summary(&run_id).expect("summary");
        assert_eq!(summary.goal, "open the browser");
        assert_eq!(summary.state, "done");
        assert_eq!(summary.steps, 1);
        assert_eq!(summary.decisions, 1);
        assert_eq!(summary.say.as_deref(), Some("Browser is open"));
        let listed = reopened.list(10);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].run_id, run_id);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn chat_history_and_run_rows_land_in_sqlite() {
        let (store, dir) = temp_store("sqlite");
        {
            let mut run = store.begin("how much storage?", None).expect("begin");
            run.record(serde_json::json!({"type": "acted", "step": 0}));
            run.record(serde_json::json!({
                "type": "final", "say": "42 GB free", "done": true,
                "acted": true, "approval_needed": false,
            }));
        }
        // Reopen as a new process would: history must come from the db.
        let reopened = TraceStore::new(&dir);
        let chat = reopened.chat(10);
        assert_eq!(chat.len(), 2, "{chat:?}");
        assert_eq!(chat[0].role, "user");
        assert_eq!(chat[0].text, "how much storage?");
        assert_eq!(chat[1].role, "assistant");
        assert_eq!(chat[1].text, "42 GB free");
        assert!(chat[0].run_id.is_some());
        assert!(dir.join("rose.db").is_file());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn two_runs_in_one_chat_group_together() {
        let (store, dir) = temp_store("chats");
        let chat_id = {
            let mut first = store.begin("first goal", None).expect("begin 1");
            let chat_id = first.chat_id().to_string();
            first.record(serde_json::json!({
                "type": "final", "say": "first reply", "done": true,
                "acted": true, "approval_needed": false,
            }));
            chat_id
        };
        {
            let mut second = store.begin("follow up", Some(&chat_id)).expect("begin 2");
            assert_eq!(second.chat_id(), chat_id);
            second.record(serde_json::json!({
                "type": "final", "say": "second reply", "done": true,
                "acted": true, "approval_needed": false,
            }));
        }
        let chats = store.chats(10);
        assert_eq!(chats.len(), 1, "{chats:?}");
        assert_eq!(chats[0].title, "first goal");
        assert_eq!(chats[0].turns, 2);
        let turns = store.chat_turns(&chat_id, 5);
        assert_eq!(
            turns,
            vec![
                ("first goal".to_string(), Some("first reply".to_string())),
                ("follow up".to_string(), Some("second reply".to_string())),
            ]
        );
        assert_eq!(store.runs_for_chat(&chat_id).len(), 2);
        assert!(store.rename_chat(&chat_id, "Renamed chat"));
        assert_eq!(store.chats(10)[0].title, "Renamed chat");
        assert!(store.delete_chat(&chat_id));
        assert!(store.chats(10).is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn frames_roundtrip_and_bad_ids_are_rejected() {
        let (store, dir) = temp_store("frames");
        let run = store.begin("g", None).expect("begin");
        let name = run.save_frame(3, &[0xff, 0xd8, 0xff, 0xd9]).expect("frame");
        assert_eq!(name, "step-003.jpg");
        assert!(store.frame_path(run.run_id(), 3).is_some());
        assert!(store.frame_path(run.run_id(), 4).is_none());
        // Path traversal attempts never resolve.
        assert!(store.events("../etc/passwd").is_none());
        assert!(store.events("a/b").is_none());
        assert!(store.frame_path("../../etc", 0).is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn unfinished_run_lists_as_running() {
        let (store, dir) = temp_store("running");
        let mut run = store.begin("long task", None).expect("begin");
        run.record(serde_json::json!({"type": "planned", "step": 0}));
        let summary = store.summary(run.run_id()).expect("summary");
        assert_eq!(summary.state, "running");
        assert_eq!(summary.steps, 0);
        std::fs::remove_dir_all(&dir).ok();
    }
}
