//! SQLite persistence: WAL mode, versioned migrations, append-only event
//! ledger with monotonic sequence, snapshots, and replay-from-sequence.
//!
//! The ledger is the audit source of truth. The daemon persists every event
//! here BEFORE broadcasting it to subscribers.

use std::path::Path;
use std::sync::Mutex;

use autoharness_protocol::Event;
use rusqlite::{
    Connection, OptionalExtension, TransactionBehavior, params, params_from_iter,
    types::Value as SqlValue,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("corruption: {0}")]
    Corruption(String),
    #[error("invalid operation: {0}")]
    InvalidOperation(String),
}

pub type Result<T> = std::result::Result<T, StoreError>;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Project {
    pub id: String,
    pub name: String,
    pub path: String,
    pub created_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Run {
    pub id: String,
    pub project_id: String,
    pub engine: String,
    /// Provider model pinned when the run was created. NULL means provider
    /// default, not an unknown or UI-only selection.
    pub model: Option<String>,
    /// Provider-native effort pinned with the model.
    pub reasoning_effort: Option<String>,
    pub objective: String,
    pub state: String,
    /// Verification command for direct runs (Phase 4+); NULL when unset.
    pub check_command: Option<String>,
    /// The run this one continues, if any. A thread is a parent chain.
    pub parent_run_id: Option<String>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    /// The competing-attempts group this run belongs to, if any.
    ///
    /// Several runs can answer one objective — different engines, different
    /// models — each in its own worktree and verified by the same check. This
    /// is what lets their results be compared rather than read as unrelated
    /// runs that happen to look alike.
    pub attempt_group: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ExternalHistoryAdoption {
    Created(Box<Run>),
    AlreadyAdopted { run_id: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatMessage {
    pub id: i64,
    pub run_id: String,
    pub role: String,
    pub content: String,
    pub created_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventRecord {
    pub seq: i64,
    pub run_id: Option<String>,
    pub run_seq: i64,
    pub timestamp_ms: i64,
    pub kind: String,
    pub payload: serde_json::Value,
}

impl EventRecord {
    /// Convert to the wire envelope.
    pub fn to_event(&self) -> Event {
        Event::new(
            self.seq as u64,
            self.run_id.clone(),
            self.run_seq as u64,
            self.timestamp_ms,
            self.kind.clone(),
            self.payload.clone(),
        )
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CheckpointRow {
    pub id: String,
    pub run_id: String,
    pub node_id: Option<String>,
    pub last_event_seq: i64,
    pub snapshot_json: serde_json::Value,
    pub created_at_ms: i64,
}

/// Health of the database and the ledger invariants built on it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IntegrityReport {
    pub healthy: bool,
    /// SQLite's own verdict; "ok" when the file is intact.
    pub sqlite: String,
    pub foreign_key_violations: i64,
    /// Events pointing at runs that no longer exist.
    pub orphan_events: i64,
    /// Run states the domain does not define.
    pub unknown_run_states: Vec<String>,
}

/// What a purge removed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PurgeReport {
    pub project_removed: bool,
    pub runs: usize,
    pub events: usize,
    pub memory_facts: usize,
}

/// Provider session/thread identity bound to a run, used to resume sessions
/// when safe (Phase 2+).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EngineSession {
    pub run_id: String,
    pub engine: String,
    pub session_id: String,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

/// Metadata AutoHarness discovers in external provider history. This is
/// provenance only: transcript bodies stay in the provider-owned files.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExternalHistoryEntry {
    pub provider: String,
    pub source_id: String,
    pub transcript_path: String,
    pub cwd: Option<String>,
    pub title: Option<String>,
    pub first_prompt: Option<String>,
    pub updated_at_ms: i64,
    pub last_seen_ms: i64,
    pub missing: bool,
    pub diagnostic: Option<String>,
    pub adopted_run_id: Option<String>,
}

/// Largest evidence excerpt an artifact may carry.
///
/// An artifact points at evidence and describes it. It is not a place to put
/// the evidence itself: a build log or a regenerated lockfile would grow the
/// ledger without bound, so the store truncates every summary here.
pub const ARTIFACT_SUMMARY_CAP: usize = 4096;

/// A bounded reference to something a run produced.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactRecord {
    pub id: String,
    pub run_id: String,
    pub node_id: Option<String>,
    /// "diff", "check_output", or "file".
    pub kind: String,
    pub name: String,
    /// Where the evidence lives, if it lives anywhere. Never copied in.
    pub path: Option<String>,
    pub byte_size: Option<i64>,
    /// Truncated excerpt or summary, at most [`ARTIFACT_SUMMARY_CAP`] bytes.
    pub summary: String,
    pub created_at_ms: i64,
}

/// One daemon-managed worktree, as recorded when it was created.
///
/// `path` is the canonical absolute path; reclaim compares the canonicalized
/// request against it so a symlink or `..` cannot name a different directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeRecord {
    pub path: String,
    /// "run" or "graph_node".
    pub kind: String,
    pub run_id: String,
    pub node_id: Option<String>,
    pub repo_path: String,
    pub branch: String,
    pub base_commit: String,
    pub created_at_ms: i64,
    pub removed_at_ms: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeAttemptRecord {
    pub node_id: String,
    pub attempt: i64,
    pub state: String,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExternalHistoryFilter {
    pub provider: Option<String>,
    pub project_path: Option<String>,
    pub query: Option<String>,
    pub cursor: Option<String>,
    pub limit: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ExternalHistoryPage {
    pub entries: Vec<ExternalHistoryEntry>,
    pub next_cursor: Option<String>,
}

const MIGRATIONS: &[&str] = &[
    // v1: Phase-1 schema.
    r#"
    CREATE TABLE projects (
        id TEXT PRIMARY KEY,
        name TEXT NOT NULL,
        path TEXT NOT NULL UNIQUE,
        created_at_ms INTEGER NOT NULL
    );
    CREATE TABLE runs (
        id TEXT PRIMARY KEY,
        project_id TEXT NOT NULL REFERENCES projects(id),
        engine TEXT NOT NULL,
        objective TEXT NOT NULL,
        state TEXT NOT NULL,
        created_at_ms INTEGER NOT NULL,
        updated_at_ms INTEGER NOT NULL
    );
    CREATE TABLE chat_messages (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        run_id TEXT NOT NULL,
        role TEXT NOT NULL,
        content TEXT NOT NULL,
        created_at_ms INTEGER NOT NULL
    );
    CREATE TABLE events (
        seq INTEGER PRIMARY KEY AUTOINCREMENT,
        run_id TEXT,
        run_seq INTEGER NOT NULL,
        timestamp_ms INTEGER NOT NULL,
        kind TEXT NOT NULL,
        payload TEXT NOT NULL
    );
    CREATE INDEX events_run ON events(run_id, run_seq);
    CREATE TABLE graph_versions (
        id TEXT PRIMARY KEY,
        run_id TEXT NOT NULL,
        version INTEGER NOT NULL,
        graph_json TEXT NOT NULL,
        created_at_ms INTEGER NOT NULL
    );
    CREATE TABLE node_attempts (
        id TEXT PRIMARY KEY,
        run_id TEXT NOT NULL,
        node_id TEXT NOT NULL,
        attempt INTEGER NOT NULL,
        state TEXT NOT NULL,
        started_at_ms INTEGER,
        finished_at_ms INTEGER,
        detail TEXT
    );
    CREATE TABLE artifacts (
        id TEXT PRIMARY KEY,
        run_id TEXT NOT NULL,
        node_id TEXT,
        kind TEXT NOT NULL,
        data_json TEXT NOT NULL,
        created_at_ms INTEGER NOT NULL
    );
    CREATE TABLE checkpoints (
        id TEXT PRIMARY KEY,
        run_id TEXT NOT NULL,
        node_id TEXT,
        last_event_seq INTEGER NOT NULL,
        snapshot_json TEXT NOT NULL,
        created_at_ms INTEGER NOT NULL
    );
    CREATE TABLE memory_facts (
        id TEXT PRIMARY KEY,
        project_id TEXT NOT NULL,
        kind TEXT NOT NULL,
        statement TEXT NOT NULL,
        evidence TEXT NOT NULL,
        confidence REAL NOT NULL,
        supersedes TEXT,
        verified INTEGER NOT NULL DEFAULT 0,
        created_at_ms INTEGER NOT NULL
    );
    CREATE TABLE policy_versions (
        id TEXT PRIMARY KEY,
        version INTEGER NOT NULL,
        data_json TEXT NOT NULL,
        promoted INTEGER NOT NULL DEFAULT 0,
        created_at_ms INTEGER NOT NULL
    );
    "#,
    // v2: provider session/thread identity persistence (Phase 2).
    r#"
    CREATE TABLE engine_sessions (
        run_id TEXT PRIMARY KEY REFERENCES runs(id),
        engine TEXT NOT NULL,
        session_id TEXT NOT NULL,
        created_at_ms INTEGER NOT NULL,
        updated_at_ms INTEGER NOT NULL
    );
    "#,
    // v3: direct-run verification command (Phase 4).
    r#"
    ALTER TABLE runs ADD COLUMN check_command TEXT;
    "#,
    // v4: lexical search over verified memory (Phase 8). FTS5 in V1;
    // embeddings are deferred until lexical retrieval proves insufficient.
    r#"
    CREATE VIRTUAL TABLE memory_fts USING fts5(
        statement,
        content='memory_facts',
        content_rowid='rowid'
    );
    CREATE TRIGGER memory_facts_ai AFTER INSERT ON memory_facts BEGIN
        INSERT INTO memory_fts(rowid, statement) VALUES (new.rowid, new.statement);
    END;
    CREATE TRIGGER memory_facts_ad AFTER DELETE ON memory_facts BEGIN
        INSERT INTO memory_fts(memory_fts, rowid, statement)
        VALUES ('delete', old.rowid, old.statement);
    END;
    CREATE TRIGGER memory_facts_au AFTER UPDATE ON memory_facts BEGIN
        INSERT INTO memory_fts(memory_fts, rowid, statement)
        VALUES ('delete', old.rowid, old.statement);
        INSERT INTO memory_fts(rowid, statement) VALUES (new.rowid, new.statement);
    END;
    "#,
    // v5: a follow-up continues a thread rather than starting a new one. The
    // parent's engine session is resumed, so the model keeps its context.
    r#"
    ALTER TABLE runs ADD COLUMN parent_run_id TEXT;
    "#,
    // v6: read-only external provider history index + adoption provenance.
    r#"
    CREATE TABLE external_history (
        provider TEXT NOT NULL,
        source_id TEXT NOT NULL,
        transcript_path TEXT NOT NULL,
        cwd TEXT,
        title TEXT,
        first_prompt TEXT,
        updated_at_ms INTEGER NOT NULL,
        last_seen_ms INTEGER NOT NULL,
        missing INTEGER NOT NULL DEFAULT 0,
        diagnostic TEXT,
        adopted_run_id TEXT REFERENCES runs(id),
        PRIMARY KEY(provider, source_id)
    );
    CREATE INDEX external_history_updated ON external_history(updated_at_ms DESC, provider, source_id);
    CREATE INDEX external_history_cwd ON external_history(cwd);
    "#,
    // v7: user settings. Singleton JSON row keeps the typed Rust contract as
    // the validation surface while the schema remains forward-compatible.
    r#"
    CREATE TABLE app_settings (
        id INTEGER PRIMARY KEY CHECK (id = 1),
        version INTEGER NOT NULL,
        settings_json TEXT NOT NULL,
        updated_at_ms INTEGER NOT NULL,
        -- Idempotency key of the last applied settings.update. Kept on the
        -- singleton row so check-and-apply is one transaction.
        last_request_id TEXT
    );
    "#,
    // v8: the authoritative index of daemon-managed worktrees. Reclaim is
    // fail-closed against THIS table: a directory with no row here is never
    // touched, whatever it looks like on disk.
    r#"
    CREATE TABLE worktrees (
        path TEXT PRIMARY KEY,
        kind TEXT NOT NULL,
        run_id TEXT NOT NULL,
        node_id TEXT,
        repo_path TEXT NOT NULL,
        branch TEXT NOT NULL,
        base_commit TEXT NOT NULL,
        created_at_ms INTEGER NOT NULL,
        removed_at_ms INTEGER
    );
    CREATE INDEX worktrees_run ON worktrees(run_id);
    CREATE INDEX worktrees_live ON worktrees(removed_at_ms, created_at_ms DESC);
    "#,
    // v9: durable user intent. Objective and steering work are persisted
    // before dispatch so a daemon or UI restart cannot silently drop them.
    r#"
    CREATE TABLE queue_items (
        id TEXT PRIMARY KEY,
        kind TEXT NOT NULL CHECK(kind IN ('objective', 'steering')),
        state TEXT NOT NULL CHECK(state IN ('pending', 'dispatching', 'completed', 'failed', 'cancelled')),
        run_id TEXT NOT NULL REFERENCES runs(id),
        project_id TEXT NOT NULL REFERENCES projects(id),
        content TEXT NOT NULL,
        position INTEGER NOT NULL,
        created_at_ms INTEGER NOT NULL,
        updated_at_ms INTEGER NOT NULL,
        error TEXT,
        start_json TEXT NOT NULL,
        enqueue_request_id TEXT NOT NULL UNIQUE,
        last_mutation_id TEXT
    );
    CREATE INDEX queue_pending ON queue_items(kind, state, position, created_at_ms);
    CREATE INDEX queue_run ON queue_items(run_id, kind, state, position);
    CREATE TABLE queue_mutations (
        request_id TEXT PRIMARY KEY,
        item_id TEXT NOT NULL REFERENCES queue_items(id),
        operation TEXT NOT NULL,
        created_at_ms INTEGER NOT NULL
    );
    "#,
    // v10: execution targeting. Model and reasoning belong to the run, not
    // the UI or queue item, so replay, resume, graphs, and export all agree.
    r#"
    ALTER TABLE runs ADD COLUMN model TEXT;
    ALTER TABLE runs ADD COLUMN reasoning_effort TEXT;
    "#,
    // v11: competing attempts. Several runs can answer ONE objective —
    // different engines, different models — each in its own worktree, each
    // verified by the same check. The group is what lets their results be
    // compared instead of read as unrelated runs that happen to look alike.
    r#"
    ALTER TABLE runs ADD COLUMN attempt_group TEXT;
    CREATE INDEX runs_attempt_group ON runs(attempt_group);
    "#,
];

const RUN_COLUMNS: &str = "id, project_id, engine, model, reasoning_effort, objective, state, \
                          check_command, parent_run_id, created_at_ms, updated_at_ms, \
                          attempt_group";

fn row_to_run(r: &rusqlite::Row<'_>) -> rusqlite::Result<Run> {
    Ok(Run {
        id: r.get(0)?,
        project_id: r.get(1)?,
        engine: r.get(2)?,
        model: r.get(3)?,
        reasoning_effort: r.get(4)?,
        objective: r.get(5)?,
        state: r.get(6)?,
        check_command: r.get(7)?,
        parent_run_id: r.get(8)?,
        created_at_ms: r.get(9)?,
        updated_at_ms: r.get(10)?,
        attempt_group: r.get(11)?,
    })
}

/// Turn free text into a safe FTS5 MATCH expression. FTS5 has its own query
/// syntax, so a user's words are quoted as literal terms rather than parsed —
/// otherwise a stray quote or `NEAR` becomes a syntax error, or worse, an
/// unintended query.
fn sanitize_fts_query(query: &str) -> String {
    query
        .split_whitespace()
        .map(|word| word.trim_matches(|c: char| !c.is_alphanumeric() && c != '_'))
        .filter(|word| !word.is_empty())
        .map(|word| format!("\"{word}\""))
        .collect::<Vec<_>>()
        .join(" OR ")
}

fn row_to_fact(r: &rusqlite::Row<'_>) -> rusqlite::Result<autoharness_core::MemoryFact> {
    let kind: String = r.get(2)?;
    let evidence: String = r.get(4)?;
    let verified: i64 = r.get(7)?;
    Ok(autoharness_core::MemoryFact {
        id: r.get(0)?,
        project_id: r.get(1)?,
        kind: serde_json::from_str(&format!("\"{kind}\""))
            .unwrap_or(autoharness_core::MemoryFactKind::Convention),
        statement: r.get(3)?,
        evidence: serde_json::from_str(&evidence).unwrap_or_default(),
        confidence: r.get(5)?,
        supersedes: r.get(6)?,
        verified: verified == 1,
        created_at_ms: r.get(8)?,
    })
}

fn row_to_external_history(r: &rusqlite::Row<'_>) -> rusqlite::Result<ExternalHistoryEntry> {
    let missing: i64 = r.get(8)?;
    Ok(ExternalHistoryEntry {
        provider: r.get(0)?,
        source_id: r.get(1)?,
        transcript_path: r.get(2)?,
        cwd: r.get(3)?,
        title: r.get(4)?,
        first_prompt: r.get(5)?,
        updated_at_ms: r.get(6)?,
        last_seen_ms: r.get(7)?,
        missing: missing != 0,
        diagnostic: r.get(9)?,
        adopted_run_id: r.get(10)?,
    })
}

fn row_to_worktree(r: &rusqlite::Row<'_>) -> rusqlite::Result<WorktreeRecord> {
    Ok(WorktreeRecord {
        path: r.get(0)?,
        kind: r.get(1)?,
        run_id: r.get(2)?,
        node_id: r.get(3)?,
        repo_path: r.get(4)?,
        branch: r.get(5)?,
        base_commit: r.get(6)?,
        created_at_ms: r.get(7)?,
        removed_at_ms: r.get(8)?,
    })
}

fn invalid_queue_value(column: usize, value: String) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        column,
        rusqlite::types::Type::Text,
        Box::new(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("invalid queue value {value}"),
        )),
    )
}

fn row_to_queue(
    r: &rusqlite::Row<'_>,
) -> rusqlite::Result<autoharness_protocol::params::QueueItem> {
    use autoharness_protocol::params::{QueueItem, QueueKind, QueueState};

    let kind_value: String = r.get(1)?;
    let kind = match kind_value.as_str() {
        "objective" => QueueKind::Objective,
        "steering" => QueueKind::Steering,
        _ => return Err(invalid_queue_value(1, kind_value)),
    };
    let state_value: String = r.get(2)?;
    let state = match state_value.as_str() {
        "pending" => QueueState::Pending,
        "dispatching" => QueueState::Dispatching,
        "completed" => QueueState::Completed,
        "failed" => QueueState::Failed,
        "cancelled" => QueueState::Cancelled,
        _ => return Err(invalid_queue_value(2, state_value)),
    };
    Ok(QueueItem {
        id: r.get(0)?,
        kind,
        state,
        run_id: r.get(3)?,
        project_id: r.get(4)?,
        content: r.get(5)?,
        position: r.get(6)?,
        created_at_ms: r.get(7)?,
        updated_at_ms: r.get(8)?,
        error: r.get(9)?,
    })
}

const QUEUE_COLUMNS: &str =
    "id, kind, state, run_id, project_id, content, position, created_at_ms, updated_at_ms, error";

fn queue_kind_str(kind: autoharness_protocol::params::QueueKind) -> &'static str {
    use autoharness_protocol::params::QueueKind;
    match kind {
        QueueKind::Objective => "objective",
        QueueKind::Steering => "steering",
    }
}

fn queue_state_str(state: autoharness_protocol::params::QueueState) -> &'static str {
    use autoharness_protocol::params::QueueState;
    match state {
        QueueState::Pending => "pending",
        QueueState::Dispatching => "dispatching",
        QueueState::Completed => "completed",
        QueueState::Failed => "failed",
        QueueState::Cancelled => "cancelled",
    }
}

fn history_cursor(entry: &ExternalHistoryEntry) -> String {
    format!(
        "{}\u{1f}{}\u{1f}{}",
        entry.updated_at_ms, entry.provider, entry.source_id
    )
}

fn parse_history_cursor(cursor: Option<&str>) -> Option<(i64, String, String)> {
    let cursor = cursor?;
    let mut parts = cursor.split('\u{1f}');
    let updated = parts.next()?.parse().ok()?;
    let provider = parts.next()?.to_string();
    let source = parts.next()?.to_string();
    Some((updated, provider, source))
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn civil_from_days(days: i64) -> (i32, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = mp + if mp < 10 { 3 } else { -9 };
    let year = y + if m <= 2 { 1 } else { 0 };
    (year as i32, m as u32, d as u32)
}

fn days_from_civil(year: i32, month: u32, day: u32) -> i64 {
    let mut y = year as i64;
    let m = month as i64;
    let d = day as i64;
    y -= (m <= 2) as i64;
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = m + if m > 2 { -3 } else { 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Keep the last `max` bytes, cut on a character boundary.
///
/// The tail is the useful end: a compiler puts its summary last, and a diff
/// stat ends with the totals.
fn truncate_on_char_boundary(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut start = text.len() - max;
    while start < text.len() && !text.is_char_boundary(start) {
        start += 1;
    }
    format!("…{}", &text[start..])
}

fn write_app_settings(
    conn: &rusqlite::Connection,
    settings: &autoharness_protocol::params::AppSettings,
    request_id: Option<&str>,
) -> Result<()> {
    conn.execute(
        "INSERT INTO app_settings(id, version, settings_json, updated_at_ms, last_request_id)
         VALUES (1, ?1, ?2, ?3, ?4)
         ON CONFLICT(id) DO UPDATE SET
            version = excluded.version,
            settings_json = excluded.settings_json,
            updated_at_ms = excluded.updated_at_ms,
            last_request_id = excluded.last_request_id",
        params![
            settings.version,
            serde_json::to_string(settings)?,
            now_ms(),
            request_id
        ],
    )?;
    Ok(())
}

fn day_start_ms(timestamp_ms: i64) -> i64 {
    timestamp_ms.div_euclid(86_400_000) * 86_400_000
}

fn month_start_ms(timestamp_ms: i64) -> i64 {
    let days = timestamp_ms.div_euclid(86_400_000);
    let (year, month, _) = civil_from_days(days);
    days_from_civil(year, month, 1) * 86_400_000
}

/// SQLite-backed store. `Connection` is `Send` but not `Sync`, so access is
/// serialized through a mutex; WAL mode still allows a second connection
/// (another `Store`) to read concurrently.
pub struct Store {
    conn: Mutex<Connection>,
}

impl Store {
    /// Open (creating if needed) a database file, applying WAL pragmas and
    /// any pending migrations.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        if let Some(parent) = path.as_ref().parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        Self::init(conn)
    }

    /// In-memory store for tests.
    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        let store = Self {
            conn: Mutex::new(conn),
        };
        store.migrate()?;
        Ok(store)
    }

    /// Apply pending migrations. Idempotent: the migrations table records
    /// which versions already ran.
    pub fn migrate(&self) -> Result<()> {
        let conn = self.conn.lock().expect("store mutex poisoned");
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS migrations (
                version INTEGER PRIMARY KEY,
                applied_at_ms INTEGER NOT NULL
            );",
        )?;
        let applied: i64 = conn.query_row(
            "SELECT COALESCE(MAX(version), 0) FROM migrations",
            [],
            |r| r.get(0),
        )?;
        for (idx, sql) in MIGRATIONS.iter().enumerate() {
            let version = (idx + 1) as i64;
            if version <= applied {
                continue;
            }
            let tx = conn.unchecked_transaction()?;
            tx.execute_batch(sql)?;
            tx.execute(
                "INSERT INTO migrations(version, applied_at_ms) VALUES (?1, ?2)",
                params![version, now_ms()],
            )?;
            tx.commit()?;
        }
        Ok(())
    }

    /// Integrity check used for corruption reporting.
    pub fn integrity_check(&self) -> Result<()> {
        let conn = self.conn.lock().expect("store mutex poisoned");
        let result: String = conn.query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
        if result == "ok" {
            Ok(())
        } else {
            Err(StoreError::Corruption(result))
        }
    }

    // ---- projects ----

    pub fn add_project(&self, name: &str, path: &str) -> Result<Project> {
        let conn = self.conn.lock().expect("store mutex poisoned");
        let existing = conn
            .query_row(
                "SELECT id, name, path, created_at_ms FROM projects WHERE path = ?1",
                params![path],
                |r| {
                    Ok(Project {
                        id: r.get(0)?,
                        name: r.get(1)?,
                        path: r.get(2)?,
                        created_at_ms: r.get(3)?,
                    })
                },
            )
            .optional()?;
        if let Some(project) = existing {
            return Ok(project);
        }
        let project = Project {
            id: autoharness_core::new_id(),
            name: name.to_string(),
            path: path.to_string(),
            created_at_ms: now_ms(),
        };
        conn.execute(
            "INSERT INTO projects(id, name, path, created_at_ms) VALUES (?1, ?2, ?3, ?4)",
            params![
                project.id,
                project.name,
                project.path,
                project.created_at_ms
            ],
        )?;
        Ok(project)
    }

    pub fn list_projects(&self) -> Result<Vec<Project>> {
        let conn = self.conn.lock().expect("store mutex poisoned");
        let mut stmt = conn
            .prepare("SELECT id, name, path, created_at_ms FROM projects ORDER BY created_at_ms")?;
        let rows = stmt.query_map([], |r| {
            Ok(Project {
                id: r.get(0)?,
                name: r.get(1)?,
                path: r.get(2)?,
                created_at_ms: r.get(3)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub fn get_project(&self, project_id: &str) -> Result<Project> {
        let conn = self.conn.lock().expect("store mutex poisoned");
        conn.query_row(
            "SELECT id, name, path, created_at_ms FROM projects WHERE id = ?1",
            params![project_id],
            |r| {
                Ok(Project {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    path: r.get(2)?,
                    created_at_ms: r.get(3)?,
                })
            },
        )
        .optional()?
        .ok_or_else(|| StoreError::NotFound(format!("project {project_id}")))
    }

    pub fn remove_project(&self, project_id: &str) -> Result<bool> {
        let conn = self.conn.lock().expect("store mutex poisoned");
        let n = conn.execute("DELETE FROM projects WHERE id = ?1", params![project_id])?;
        Ok(n > 0)
    }

    // ---- runs ----

    pub fn create_run(&self, project_id: &str, engine: &str, objective: &str) -> Result<Run> {
        self.create_run_with_check(project_id, engine, objective, None)
    }

    pub fn create_run_with_check(
        &self,
        project_id: &str,
        engine: &str,
        objective: &str,
        check_command: Option<&str>,
    ) -> Result<Run> {
        self.create_run_full(project_id, engine, objective, check_command, None)
    }

    /// Create a run, optionally continuing an earlier one.
    pub fn create_run_full(
        &self,
        project_id: &str,
        engine: &str,
        objective: &str,
        check_command: Option<&str>,
        parent_run_id: Option<&str>,
    ) -> Result<Run> {
        self.create_run_configured(
            project_id,
            engine,
            None,
            None,
            objective,
            check_command,
            parent_run_id,
        )
    }

    /// Create a run with an immutable provider selection.
    #[allow(clippy::too_many_arguments)]
    pub fn create_run_configured(
        &self,
        project_id: &str,
        engine: &str,
        model: Option<&str>,
        reasoning_effort: Option<&str>,
        objective: &str,
        check_command: Option<&str>,
        parent_run_id: Option<&str>,
    ) -> Result<Run> {
        let now = now_ms();
        let run = Run {
            id: autoharness_core::new_id(),
            project_id: project_id.to_string(),
            engine: engine.to_string(),
            model: model.map(str::to_string),
            reasoning_effort: reasoning_effort.map(str::to_string),
            objective: objective.to_string(),
            state: "draft".to_string(),
            check_command: check_command.map(str::to_string),
            parent_run_id: parent_run_id.map(str::to_string),
            created_at_ms: now,
            updated_at_ms: now,
            attempt_group: None,
        };
        let conn = self.conn.lock().expect("store mutex poisoned");
        conn.execute(
            "INSERT INTO runs(id, project_id, engine, model, reasoning_effort, objective, state,
                              check_command, parent_run_id, created_at_ms, updated_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                run.id,
                run.project_id,
                run.engine,
                run.model,
                run.reasoning_effort,
                run.objective,
                run.state,
                run.check_command,
                run.parent_run_id,
                run.created_at_ms,
                run.updated_at_ms
            ],
        )?;
        Ok(run)
    }

    pub fn get_run(&self, run_id: &str) -> Result<Run> {
        let conn = self.conn.lock().expect("store mutex poisoned");
        conn.query_row(
            &format!("SELECT {RUN_COLUMNS} FROM runs WHERE id = ?1"),
            params![run_id],
            row_to_run,
        )
        .optional()?
        .ok_or_else(|| StoreError::NotFound(format!("run {run_id}")))
    }

    /// Runs in any of the given states (e.g. reconciliation on restart).
    pub fn runs_in_states(&self, states: &[&str]) -> Result<Vec<Run>> {
        if states.is_empty() {
            return Ok(vec![]);
        }
        let conn = self.conn.lock().expect("store mutex poisoned");
        let placeholders = states.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let sql = format!(
            "SELECT {RUN_COLUMNS} FROM runs WHERE state IN ({placeholders}) ORDER BY created_at_ms"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(states.iter()), row_to_run)?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// Give a run its objective, and report whether a row was actually
    /// rewritten. The `state = 'draft'` predicate is in the statement rather
    /// than in a read-then-write, so two clients racing the same draft cannot
    /// both believe they set it.
    pub fn set_draft_objective(&self, run_id: &str, objective: &str) -> Result<bool> {
        let conn = self.conn.lock().expect("store mutex poisoned");
        let changed = conn.execute(
            "UPDATE runs SET objective = ?1, updated_at_ms = ?2
             WHERE id = ?3 AND state = 'draft'",
            params![objective, now_ms(), run_id],
        )?;
        Ok(changed > 0)
    }

    pub fn set_run_state(&self, run_id: &str, state: &str) -> Result<()> {
        let conn = self.conn.lock().expect("store mutex poisoned");
        conn.execute(
            "UPDATE runs SET state = ?1, updated_at_ms = ?2 WHERE id = ?3",
            params![state, now_ms(), run_id],
        )?;
        Ok(())
    }

    // ---- durable objective and steering queues ----

    /// Create a draft run and its objective queue item in one transaction.
    /// The request id is durable idempotency, not just a connection-local
    /// response cache: retrying after a daemon restart returns the same run.
    pub fn enqueue_run(
        &self,
        project_id: &str,
        engine: &str,
        objective: &str,
        check_command: Option<&str>,
        parent_run_id: Option<&str>,
        request_id: &str,
    ) -> Result<(Run, autoharness_protocol::params::QueueItem, bool)> {
        self.enqueue_run_with_options(
            project_id,
            engine,
            objective,
            check_command,
            parent_run_id,
            request_id,
            &serde_json::json!({}),
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn enqueue_run_with_options(
        &self,
        project_id: &str,
        engine: &str,
        objective: &str,
        check_command: Option<&str>,
        parent_run_id: Option<&str>,
        request_id: &str,
        start_options: &serde_json::Value,
    ) -> Result<(Run, autoharness_protocol::params::QueueItem, bool)> {
        self.enqueue_run_configured(
            project_id,
            engine,
            None,
            None,
            objective,
            check_command,
            parent_run_id,
            request_id,
            start_options,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn enqueue_run_configured(
        &self,
        project_id: &str,
        engine: &str,
        model: Option<&str>,
        reasoning_effort: Option<&str>,
        objective: &str,
        check_command: Option<&str>,
        parent_run_id: Option<&str>,
        request_id: &str,
        start_options: &serde_json::Value,
    ) -> Result<(Run, autoharness_protocol::params::QueueItem, bool)> {
        let mut conn = self.conn.lock().expect("store mutex poisoned");
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing = tx
            .query_row(
                &format!("SELECT {QUEUE_COLUMNS} FROM queue_items WHERE enqueue_request_id = ?1"),
                params![request_id],
                row_to_queue,
            )
            .optional()?;
        if let Some(item) = existing {
            let run = tx.query_row(
                &format!("SELECT {RUN_COLUMNS} FROM runs WHERE id = ?1"),
                params![item.run_id],
                row_to_run,
            )?;
            tx.commit()?;
            return Ok((run, item, false));
        }

        let now = now_ms();
        let run = Run {
            id: autoharness_core::new_id(),
            project_id: project_id.to_string(),
            engine: engine.to_string(),
            model: model.map(str::to_string),
            reasoning_effort: reasoning_effort.map(str::to_string),
            objective: objective.to_string(),
            state: "draft".into(),
            check_command: check_command.map(str::to_string),
            parent_run_id: parent_run_id.map(str::to_string),
            created_at_ms: now,
            updated_at_ms: now,
            attempt_group: None,
        };
        tx.execute(
            "INSERT INTO runs(id, project_id, engine, model, reasoning_effort, objective, state,
                              check_command, parent_run_id, created_at_ms, updated_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                run.id,
                run.project_id,
                run.engine,
                run.model,
                run.reasoning_effort,
                run.objective,
                run.state,
                run.check_command,
                run.parent_run_id,
                run.created_at_ms,
                run.updated_at_ms,
            ],
        )?;
        let position: i64 = tx.query_row(
            "SELECT COALESCE(MAX(position), 0) + 1024 FROM queue_items
             WHERE kind = 'objective' AND state = 'pending'",
            [],
            |r| r.get(0),
        )?;
        let item = autoharness_protocol::params::QueueItem {
            id: autoharness_core::new_id(),
            kind: autoharness_protocol::params::QueueKind::Objective,
            state: autoharness_protocol::params::QueueState::Pending,
            run_id: run.id.clone(),
            project_id: run.project_id.clone(),
            content: run.objective.clone(),
            position,
            created_at_ms: now,
            updated_at_ms: now,
            error: None,
        };
        tx.execute(
            "INSERT INTO queue_items(id, kind, state, run_id, project_id, content, position,
                                     created_at_ms, updated_at_ms, error, start_json,
                                     enqueue_request_id, last_mutation_id)
             VALUES (?1, 'objective', 'pending', ?2, ?3, ?4, ?5, ?6, ?7, NULL, ?8, ?9, NULL)",
            params![
                item.id,
                item.run_id,
                item.project_id,
                item.content,
                item.position,
                item.created_at_ms,
                item.updated_at_ms,
                serde_json::to_string(start_options)?,
                request_id,
            ],
        )?;
        tx.commit()?;
        Ok((run, item, true))
    }

    /// Queue a draft that already exists, giving it its objective in the same
    /// transaction.
    ///
    /// This is what the sidebar's `+` needs. It creates a real draft run so a
    /// row exists — and is dated — the moment it is clicked; the objective
    /// arrives later, when the user has typed one. Going through the queue
    /// rather than calling `run.start` directly is deliberate: admission
    /// control (`max_active_runs`) lives in the scheduler, and a second way to
    /// begin a run would be a second way to exceed it.
    ///
    /// Idempotent on `request_id`, and refuses anything that is no longer a
    /// draft — a started run's objective is what an engine was actually told.
    pub fn enqueue_existing_draft(
        &self,
        run_id: &str,
        objective: &str,
        request_id: &str,
        start_options: &serde_json::Value,
    ) -> Result<(Run, autoharness_protocol::params::QueueItem, bool)> {
        let mut conn = self.conn.lock().expect("store mutex poisoned");
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing = tx
            .query_row(
                &format!("SELECT {QUEUE_COLUMNS} FROM queue_items WHERE enqueue_request_id = ?1"),
                params![request_id],
                row_to_queue,
            )
            .optional()?;
        if let Some(item) = existing {
            let run = tx.query_row(
                &format!("SELECT {RUN_COLUMNS} FROM runs WHERE id = ?1"),
                params![item.run_id],
                row_to_run,
            )?;
            tx.commit()?;
            return Ok((run, item, false));
        }

        let now = now_ms();
        // The `state = 'draft'` predicate is in the statement, so a run that
        // left draft between the read and the write cannot be queued anyway.
        let updated = tx.execute(
            "UPDATE runs SET objective = ?1, updated_at_ms = ?2
             WHERE id = ?3 AND state = 'draft'",
            params![objective, now, run_id],
        )?;
        if updated == 0 {
            return Err(StoreError::NotFound(format!(
                "no draft run {run_id} to queue"
            )));
        }
        let run = tx.query_row(
            &format!("SELECT {RUN_COLUMNS} FROM runs WHERE id = ?1"),
            params![run_id],
            row_to_run,
        )?;
        let position: i64 = tx.query_row(
            "SELECT COALESCE(MAX(position), 0) + 1024 FROM queue_items
             WHERE kind = 'objective' AND state = 'pending'",
            [],
            |r| r.get(0),
        )?;
        let item = autoharness_protocol::params::QueueItem {
            id: autoharness_core::new_id(),
            kind: autoharness_protocol::params::QueueKind::Objective,
            state: autoharness_protocol::params::QueueState::Pending,
            run_id: run.id.clone(),
            project_id: run.project_id.clone(),
            content: run.objective.clone(),
            position,
            created_at_ms: now,
            updated_at_ms: now,
            error: None,
        };
        tx.execute(
            "INSERT INTO queue_items(id, kind, state, run_id, project_id, content, position,
                                     created_at_ms, updated_at_ms, error, start_json,
                                     enqueue_request_id, last_mutation_id)
             VALUES (?1, 'objective', 'pending', ?2, ?3, ?4, ?5, ?6, ?7, NULL, ?8, ?9, NULL)",
            params![
                item.id,
                item.run_id,
                item.project_id,
                item.content,
                item.position,
                item.created_at_ms,
                item.updated_at_ms,
                serde_json::to_string(start_options)?,
                request_id,
            ],
        )?;
        tx.commit()?;
        Ok((run, item, true))
    }

    /// Create one run per attempt for a single objective, and queue them all
    /// in one transaction.
    ///
    /// This is the thing a terminal full of agents cannot do: the same task,
    /// answered several ways, each in its own worktree on its own branch, each
    /// verified by the SAME check — so the results are comparable rather than
    /// several opinions with no adjudicator. Every attempt is an ordinary run,
    /// so the ledger, the diff, the commit and the reclaim rules all apply
    /// unchanged; the group is only what lets them be read together.
    ///
    /// Idempotent on `request_id`, like every other enqueue: a retry after a
    /// daemon restart returns the group that already exists rather than
    /// spending a second set of attempts.
    pub fn enqueue_attempts(
        &self,
        project_id: &str,
        objective: &str,
        check_command: Option<&str>,
        attempts: &[(String, Option<String>, Option<String>)],
        request_id: &str,
        start_options: &serde_json::Value,
    ) -> Result<(String, Vec<Run>, bool)> {
        if attempts.is_empty() {
            return Err(StoreError::NotFound(
                "an attempt set needs at least one attempt".into(),
            ));
        }
        let mut conn = self.conn.lock().expect("store mutex poisoned");
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;

        // A retry must not spend a second set of attempts.
        let existing: Option<String> = tx
            .query_row(
                "SELECT run_id FROM queue_items WHERE enqueue_request_id = ?1",
                params![request_id],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(run_id) = existing {
            let group: Option<String> = tx.query_row(
                "SELECT attempt_group FROM runs WHERE id = ?1",
                params![run_id],
                |r| r.get(0),
            )?;
            let group = group.unwrap_or(run_id);
            let mut stmt = tx.prepare(&format!(
                "SELECT {RUN_COLUMNS} FROM runs WHERE attempt_group = ?1 ORDER BY created_at_ms"
            ))?;
            let runs = stmt
                .query_map(params![group], row_to_run)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            drop(stmt);
            tx.commit()?;
            return Ok((group, runs, false));
        }

        let now = now_ms();
        let group = autoharness_core::new_id();
        let mut created = Vec::new();
        let mut position: i64 = tx.query_row(
            "SELECT COALESCE(MAX(position), 0) + 1024 FROM queue_items
             WHERE kind = \'objective\' AND state = \'pending\'",
            [],
            |r| r.get(0),
        )?;

        for (index, (engine, model, effort)) in attempts.iter().enumerate() {
            let run = Run {
                id: autoharness_core::new_id(),
                project_id: project_id.to_string(),
                engine: engine.clone(),
                model: model.clone(),
                reasoning_effort: effort.clone(),
                objective: objective.to_string(),
                state: "draft".into(),
                check_command: check_command.map(str::to_string),
                parent_run_id: None,
                created_at_ms: now,
                updated_at_ms: now,
                attempt_group: Some(group.clone()),
            };
            tx.execute(
                "INSERT INTO runs(id, project_id, engine, model, reasoning_effort, objective,
                                  state, check_command, parent_run_id, created_at_ms,
                                  updated_at_ms, attempt_group)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                params![
                    run.id,
                    run.project_id,
                    run.engine,
                    run.model,
                    run.reasoning_effort,
                    run.objective,
                    run.state,
                    run.check_command,
                    run.parent_run_id,
                    run.created_at_ms,
                    run.updated_at_ms,
                    run.attempt_group,
                ],
            )?;
            // Only the first item carries the caller's request id, which is
            // what makes the whole set idempotent as one unit.
            let enqueue_id = if index == 0 {
                request_id.to_string()
            } else {
                format!("{request_id}#{index}")
            };
            tx.execute(
                "INSERT INTO queue_items(id, kind, state, run_id, project_id, content, position,
                                         created_at_ms, updated_at_ms, error, start_json,
                                         enqueue_request_id, last_mutation_id)
                 VALUES (?1, \'objective\', \'pending\', ?2, ?3, ?4, ?5, ?6, ?7, NULL, ?8, ?9, NULL)",
                params![
                    autoharness_core::new_id(),
                    run.id,
                    run.project_id,
                    run.objective,
                    position,
                    now,
                    now,
                    serde_json::to_string(start_options)?,
                    enqueue_id,
                ],
            )?;
            position += 1024;
            created.push(run);
        }
        tx.commit()?;
        Ok((group, created, true))
    }

    /// Every run in an attempt group, oldest first.
    pub fn attempts_in_group(&self, group: &str) -> Result<Vec<Run>> {
        let conn = self.conn.lock().expect("store mutex poisoned");
        let mut stmt = conn.prepare(&format!(
            "SELECT {RUN_COLUMNS} FROM runs WHERE attempt_group = ?1 ORDER BY created_at_ms"
        ))?;
        let runs = stmt
            .query_map(params![group], row_to_run)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(runs)
    }

    /// Persist both the visible chat message and the delivery intent. A
    /// duplicate request creates neither a duplicate row nor duplicate chat.
    pub fn enqueue_steering(
        &self,
        run_id: &str,
        message: &str,
        request_id: &str,
    ) -> Result<(autoharness_protocol::params::QueueItem, bool)> {
        let mut conn = self.conn.lock().expect("store mutex poisoned");
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing = tx
            .query_row(
                &format!("SELECT {QUEUE_COLUMNS} FROM queue_items WHERE enqueue_request_id = ?1"),
                params![request_id],
                row_to_queue,
            )
            .optional()?;
        if let Some(item) = existing {
            tx.commit()?;
            return Ok((item, false));
        }
        let project_id: String = tx
            .query_row(
                "SELECT project_id FROM runs WHERE id = ?1",
                params![run_id],
                |r| r.get(0),
            )
            .optional()?
            .ok_or_else(|| StoreError::NotFound(format!("run {run_id}")))?;
        let now = now_ms();
        tx.execute(
            "INSERT INTO chat_messages(run_id, role, content, created_at_ms)
             VALUES (?1, 'user', ?2, ?3)",
            params![run_id, message, now],
        )?;
        let position: i64 = tx.query_row(
            "SELECT COALESCE(MAX(position), 0) + 1024 FROM queue_items
             WHERE kind = 'steering' AND run_id = ?1 AND state = 'pending'",
            params![run_id],
            |r| r.get(0),
        )?;
        let item = autoharness_protocol::params::QueueItem {
            id: autoharness_core::new_id(),
            kind: autoharness_protocol::params::QueueKind::Steering,
            state: autoharness_protocol::params::QueueState::Pending,
            run_id: run_id.to_string(),
            project_id,
            content: message.to_string(),
            position,
            created_at_ms: now,
            updated_at_ms: now,
            error: None,
        };
        tx.execute(
            "INSERT INTO queue_items(id, kind, state, run_id, project_id, content, position,
                                     created_at_ms, updated_at_ms, error, start_json,
                                     enqueue_request_id, last_mutation_id)
             VALUES (?1, 'steering', 'pending', ?2, ?3, ?4, ?5, ?6, ?7, NULL, '{}', ?8, NULL)",
            params![
                item.id,
                item.run_id,
                item.project_id,
                item.content,
                item.position,
                item.created_at_ms,
                item.updated_at_ms,
                request_id,
            ],
        )?;
        tx.commit()?;
        Ok((item, true))
    }

    pub fn get_queue_item(&self, item_id: &str) -> Result<autoharness_protocol::params::QueueItem> {
        let conn = self.conn.lock().expect("store mutex poisoned");
        conn.query_row(
            &format!("SELECT {QUEUE_COLUMNS} FROM queue_items WHERE id = ?1"),
            params![item_id],
            row_to_queue,
        )
        .optional()?
        .ok_or_else(|| StoreError::NotFound(format!("queue item {item_id}")))
    }

    pub fn queue_start_options(&self, item_id: &str) -> Result<serde_json::Value> {
        let conn = self.conn.lock().expect("store mutex poisoned");
        let json: String = conn
            .query_row(
                "SELECT start_json FROM queue_items WHERE id = ?1",
                params![item_id],
                |r| r.get(0),
            )
            .optional()?
            .ok_or_else(|| StoreError::NotFound(format!("queue item {item_id}")))?;
        serde_json::from_str(&json).map_err(Into::into)
    }

    pub fn dispatching_objective_count(&self) -> Result<usize> {
        let conn = self.conn.lock().expect("store mutex poisoned");
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM queue_items
             WHERE kind = 'objective' AND state = 'dispatching'",
            [],
            |r| r.get(0),
        )?;
        Ok(count.max(0) as usize)
    }

    pub fn run_has_dispatching_objective(&self, run_id: &str) -> Result<bool> {
        let conn = self.conn.lock().expect("store mutex poisoned");
        conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM queue_items
                           WHERE run_id = ?1 AND kind = 'objective' AND state = 'dispatching')",
            params![run_id],
            |r| r.get(0),
        )
        .map_err(Into::into)
    }

    pub fn list_queue(
        &self,
        filter: &autoharness_protocol::params::QueueList,
    ) -> Result<Vec<autoharness_protocol::params::QueueItem>> {
        let conn = self.conn.lock().expect("store mutex poisoned");
        let kind = filter.kind.map(queue_kind_str);
        let mut stmt = conn.prepare(&format!(
            "SELECT {QUEUE_COLUMNS} FROM queue_items
             WHERE (?1 IS NULL OR run_id = ?1)
               AND (?2 IS NULL OR kind = ?2)
               AND (?3 = 1 OR state IN ('pending', 'dispatching'))
             ORDER BY kind, position, created_at_ms, id"
        ))?;
        let rows = stmt.query_map(
            params![filter.run_id, kind, i64::from(filter.include_terminal)],
            row_to_queue,
        )?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    fn claim_next_queue(
        &self,
        kind: autoharness_protocol::params::QueueKind,
        run_id: Option<&str>,
    ) -> Result<Option<autoharness_protocol::params::QueueItem>> {
        let mut conn = self.conn.lock().expect("store mutex poisoned");
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let item = tx
            .query_row(
                &format!(
                    "SELECT {QUEUE_COLUMNS} FROM queue_items
                     WHERE kind = ?1 AND state = 'pending' AND (?2 IS NULL OR run_id = ?2)
                     ORDER BY position, created_at_ms, id LIMIT 1"
                ),
                params![queue_kind_str(kind), run_id],
                row_to_queue,
            )
            .optional()?;
        let Some(mut item) = item else {
            tx.commit()?;
            return Ok(None);
        };
        let now = now_ms();
        let changed = tx.execute(
            "UPDATE queue_items SET state = 'dispatching', updated_at_ms = ?2
             WHERE id = ?1 AND state = 'pending'",
            params![item.id, now],
        )?;
        if changed != 1 {
            return Err(StoreError::InvalidOperation(format!(
                "queue item {} was claimed concurrently",
                item.id
            )));
        }
        item.state = autoharness_protocol::params::QueueState::Dispatching;
        item.updated_at_ms = now;
        tx.commit()?;
        Ok(Some(item))
    }

    pub fn claim_next_objective(&self) -> Result<Option<autoharness_protocol::params::QueueItem>> {
        self.claim_next_queue(autoharness_protocol::params::QueueKind::Objective, None)
    }

    pub fn claim_next_steering(
        &self,
        run_id: &str,
    ) -> Result<Option<autoharness_protocol::params::QueueItem>> {
        self.claim_next_queue(
            autoharness_protocol::params::QueueKind::Steering,
            Some(run_id),
        )
    }

    pub fn move_queue_item(
        &self,
        item_id: &str,
        before_item_id: Option<&str>,
        request_id: &str,
    ) -> Result<(autoharness_protocol::params::QueueItem, bool)> {
        use autoharness_protocol::params::{QueueKind, QueueState};

        let mut conn = self.conn.lock().expect("store mutex poisoned");
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut item = tx
            .query_row(
                &format!("SELECT {QUEUE_COLUMNS} FROM queue_items WHERE id = ?1"),
                params![item_id],
                row_to_queue,
            )
            .optional()?
            .ok_or_else(|| StoreError::NotFound(format!("queue item {item_id}")))?;
        let duplicate: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM queue_mutations WHERE request_id = ?1)",
            params![request_id],
            |r| r.get(0),
        )?;
        if duplicate {
            tx.commit()?;
            return Ok((item, false));
        }
        if item.state != QueueState::Pending {
            return Err(StoreError::InvalidOperation(format!(
                "queue item {item_id} is not pending"
            )));
        }

        let run_filter = (item.kind == QueueKind::Steering).then_some(item.run_id.as_str());
        let mut stmt = tx.prepare(&format!(
            "SELECT {QUEUE_COLUMNS} FROM queue_items
             WHERE kind = ?1 AND state = 'pending' AND (?2 IS NULL OR run_id = ?2)
             ORDER BY position, created_at_ms, id"
        ))?;
        let mut ordered: Vec<autoharness_protocol::params::QueueItem> = stmt
            .query_map(params![queue_kind_str(item.kind), run_filter], row_to_queue)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(stmt);
        ordered.retain(|candidate| candidate.id != item_id);
        let insert_at = match before_item_id {
            Some(before) => ordered
                .iter()
                .position(|candidate| candidate.id == before)
                .ok_or_else(|| {
                    StoreError::InvalidOperation(format!(
                        "queue target {before} is not pending in the same queue"
                    ))
                })?,
            None => ordered.len(),
        };
        ordered.insert(insert_at, item.clone());
        let now = now_ms();
        for (index, candidate) in ordered.iter().enumerate() {
            tx.execute(
                "UPDATE queue_items SET position = ?2, updated_at_ms = ?3,
                                        last_mutation_id = CASE WHEN id = ?4 THEN ?5 ELSE last_mutation_id END
                 WHERE id = ?1",
                params![
                    candidate.id,
                    (index as i64 + 1) * 1024,
                    now,
                    item_id,
                    request_id,
                ],
            )?;
        }
        tx.execute(
            "INSERT INTO queue_mutations(request_id, item_id, operation, created_at_ms)
             VALUES (?1, ?2, 'move', ?3)",
            params![request_id, item_id, now],
        )?;
        item = tx.query_row(
            &format!("SELECT {QUEUE_COLUMNS} FROM queue_items WHERE id = ?1"),
            params![item_id],
            row_to_queue,
        )?;
        tx.commit()?;
        Ok((item, true))
    }

    pub fn cancel_queue_item(
        &self,
        item_id: &str,
        request_id: &str,
    ) -> Result<(autoharness_protocol::params::QueueItem, bool)> {
        use autoharness_protocol::params::{QueueKind, QueueState};

        let mut conn = self.conn.lock().expect("store mutex poisoned");
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut item = tx
            .query_row(
                &format!("SELECT {QUEUE_COLUMNS} FROM queue_items WHERE id = ?1"),
                params![item_id],
                row_to_queue,
            )
            .optional()?
            .ok_or_else(|| StoreError::NotFound(format!("queue item {item_id}")))?;
        let duplicate: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM queue_mutations WHERE request_id = ?1)",
            params![request_id],
            |r| r.get(0),
        )?;
        if duplicate || item.state == QueueState::Cancelled {
            tx.commit()?;
            return Ok((item, false));
        }
        if item.state != QueueState::Pending {
            return Err(StoreError::InvalidOperation(format!(
                "queue item {item_id} is already being dispatched"
            )));
        }
        let now = now_ms();
        tx.execute(
            "UPDATE queue_items SET state = 'cancelled', updated_at_ms = ?2,
                                    last_mutation_id = ?3 WHERE id = ?1",
            params![item_id, now, request_id],
        )?;
        if item.kind == QueueKind::Objective {
            tx.execute(
                "UPDATE runs SET state = 'cancelled', updated_at_ms = ?2
                 WHERE id = ?1 AND state = 'draft'",
                params![item.run_id, now],
            )?;
        }
        tx.execute(
            "INSERT INTO queue_mutations(request_id, item_id, operation, created_at_ms)
             VALUES (?1, ?2, 'cancel', ?3)",
            params![request_id, item_id, now],
        )?;
        item.state = QueueState::Cancelled;
        item.updated_at_ms = now;
        tx.commit()?;
        Ok((item, true))
    }

    pub fn finish_queue_item(
        &self,
        item_id: &str,
        state: autoharness_protocol::params::QueueState,
        error: Option<&str>,
    ) -> Result<autoharness_protocol::params::QueueItem> {
        use autoharness_protocol::params::QueueState;
        if !matches!(
            state,
            QueueState::Completed | QueueState::Failed | QueueState::Cancelled
        ) {
            return Err(StoreError::InvalidOperation(format!(
                "{state:?} is not a terminal queue state"
            )));
        }
        let conn = self.conn.lock().expect("store mutex poisoned");
        let changed = conn.execute(
            "UPDATE queue_items SET state = ?2, error = ?3, updated_at_ms = ?4
             WHERE id = ?1 AND state IN ('pending', 'dispatching')",
            params![item_id, queue_state_str(state), error, now_ms()],
        )?;
        if changed == 0 {
            let existing = conn
                .query_row(
                    &format!("SELECT {QUEUE_COLUMNS} FROM queue_items WHERE id = ?1"),
                    params![item_id],
                    row_to_queue,
                )
                .optional()?;
            return existing.ok_or_else(|| StoreError::NotFound(format!("queue item {item_id}")));
        }
        conn.query_row(
            &format!("SELECT {QUEUE_COLUMNS} FROM queue_items WHERE id = ?1"),
            params![item_id],
            row_to_queue,
        )
        .map_err(Into::into)
    }

    pub fn finish_objective_for_run(
        &self,
        run_id: &str,
        state: autoharness_protocol::params::QueueState,
        error: Option<&str>,
    ) -> Result<Option<autoharness_protocol::params::QueueItem>> {
        let item_id = {
            let conn = self.conn.lock().expect("store mutex poisoned");
            conn.query_row(
                "SELECT id FROM queue_items WHERE run_id = ?1 AND kind = 'objective'
                 ORDER BY created_at_ms LIMIT 1",
                params![run_id],
                |r| r.get::<_, String>(0),
            )
            .optional()?
        };
        item_id
            .map(|id| self.finish_queue_item(&id, state, error))
            .transpose()
    }

    /// A process that died after claiming but before acknowledgement leaves
    /// no authority in memory. Reset those rows so reconciliation can deliver
    /// them again instead of silently losing them.
    pub fn reset_dispatching_queue(&self) -> Result<usize> {
        let conn = self.conn.lock().expect("store mutex poisoned");
        conn.execute(
            "UPDATE queue_items SET state = 'pending', updated_at_ms = ?1
             WHERE state = 'dispatching'
               AND (kind = 'steering' OR run_id IN (SELECT id FROM runs WHERE state = 'draft'))",
            params![now_ms()],
        )
        .map_err(Into::into)
    }

    // ---- engine sessions ----

    /// Persist (or replace) the provider session/thread ID for a run.
    pub fn save_engine_session(&self, run_id: &str, engine: &str, session_id: &str) -> Result<()> {
        let now = now_ms();
        let conn = self.conn.lock().expect("store mutex poisoned");
        conn.execute(
            "INSERT INTO engine_sessions(run_id, engine, session_id, created_at_ms, updated_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(run_id) DO UPDATE SET
                engine = excluded.engine,
                session_id = excluded.session_id,
                updated_at_ms = excluded.updated_at_ms",
            params![run_id, engine, session_id, now, now],
        )?;
        Ok(())
    }

    pub fn get_engine_session(&self, run_id: &str) -> Result<Option<EngineSession>> {
        let conn = self.conn.lock().expect("store mutex poisoned");
        conn.query_row(
            "SELECT run_id, engine, session_id, created_at_ms, updated_at_ms
             FROM engine_sessions WHERE run_id = ?1",
            params![run_id],
            |r| {
                Ok(EngineSession {
                    run_id: r.get(0)?,
                    engine: r.get(1)?,
                    session_id: r.get(2)?,
                    created_at_ms: r.get(3)?,
                    updated_at_ms: r.get(4)?,
                })
            },
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn delete_engine_session(&self, run_id: &str) -> Result<bool> {
        let conn = self.conn.lock().expect("store mutex poisoned");
        let n = conn.execute(
            "DELETE FROM engine_sessions WHERE run_id = ?1",
            params![run_id],
        )?;
        Ok(n > 0)
    }

    // ---- app settings ----

    pub fn app_settings(&self) -> Result<autoharness_protocol::params::AppSettings> {
        let conn = self.conn.lock().expect("store mutex poisoned");
        let json: Option<String> = conn
            .query_row(
                "SELECT settings_json FROM app_settings WHERE id = 1",
                [],
                |r| r.get(0),
            )
            .optional()?;
        match json {
            Some(json) => Ok(
                serde_json::from_str::<autoharness_protocol::params::AppSettings>(&json)?.clamped(),
            ),
            None => Ok(autoharness_protocol::params::AppSettings::default()),
        }
    }

    pub fn save_app_settings(
        &self,
        settings: &autoharness_protocol::params::AppSettings,
    ) -> Result<autoharness_protocol::params::AppSettings> {
        let settings = settings.clone().clamped();
        let conn = self.conn.lock().expect("store mutex poisoned");
        write_app_settings(&conn, &settings, None)?;
        Ok(settings)
    }

    /// Apply a partial update and report whether it actually ran.
    ///
    /// A repeat of an already-applied `request_id` returns the stored settings
    /// with `false`, so the caller knows not to broadcast a second
    /// `settings.updated`. The check and the write share one transaction, so
    /// two racing retries of the same request cannot both apply.
    pub fn update_app_settings(
        &self,
        update: &autoharness_protocol::params::SettingsUpdate,
    ) -> Result<(autoharness_protocol::params::AppSettings, bool)> {
        let conn = self.conn.lock().expect("store mutex poisoned");
        let tx = conn.unchecked_transaction()?;
        let stored: Option<(String, Option<String>)> = tx
            .query_row(
                "SELECT settings_json, last_request_id FROM app_settings WHERE id = 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let mut settings = match &stored {
            Some((json, _)) => {
                serde_json::from_str::<autoharness_protocol::params::AppSettings>(json)?.clamped()
            }
            None => autoharness_protocol::params::AppSettings::default(),
        };
        if let Some(request_id) = update.request_id.as_deref()
            && stored
                .as_ref()
                .and_then(|(_, last)| last.as_deref())
                .is_some_and(|last| last == request_id)
        {
            tx.commit()?;
            return Ok((settings, false));
        }
        if let Some(value) = &update.default_engine {
            settings.default_engine = value.clone();
        }
        // Per engine, because a model id means nothing to another provider.
        if let Some((engine, model)) = &update.default_model {
            if model.is_empty() {
                settings.default_models.remove(engine);
            } else {
                settings
                    .default_models
                    .insert(engine.clone(), model.clone());
            }
        }
        if let Some((engine, effort)) = &update.default_reasoning_effort {
            if effort.is_empty() {
                settings.default_reasoning_efforts.remove(engine);
            } else {
                settings
                    .default_reasoning_efforts
                    .insert(engine.clone(), effort.clone());
            }
        }
        if let Some(value) = update.default_route_mode {
            settings.default_route_mode = value;
        }
        if let Some(value) = update.max_active_runs {
            settings.max_active_runs = value;
        }
        if let Some(value) = update.max_parallel_workers {
            settings.max_parallel_workers = value;
        }
        if let Some(value) = update.max_graph_nodes {
            settings.max_graph_nodes = value;
        }
        if let Some(value) = update.default_wall_time_minutes {
            settings.default_wall_time_minutes = value;
        }
        if let Some(value) = update.retention_days {
            settings.retention_days = value;
        }
        if let Some(value) = update.automatic_history_scan {
            settings.automatic_history_scan = value;
        }
        if let Some(value) = update.notifications_enabled {
            settings.notifications_enabled = value;
        }
        if let Some(value) = update.sounds_enabled {
            settings.sounds_enabled = value;
        }
        if let Some(value) = update.automatic_update_checks {
            settings.automatic_update_checks = value;
        }
        if let Some(value) = update.confirm_destructive_actions {
            settings.confirm_destructive_actions = value;
        }
        let settings = settings.clamped();
        write_app_settings(&tx, &settings, update.request_id.as_deref())?;
        tx.commit()?;
        Ok((settings, true))
    }

    // ---- artifacts ----

    /// Persist one artifact. The summary is truncated here, so no caller can
    /// put an unbounded blob in the ledger by accident.
    pub fn record_artifact(&self, artifact: &ArtifactRecord) -> Result<ArtifactRecord> {
        let mut artifact = artifact.clone();
        if artifact.id.is_empty() {
            artifact.id = autoharness_core::new_id();
        }
        if artifact.created_at_ms == 0 {
            artifact.created_at_ms = now_ms();
        }
        artifact.summary = truncate_on_char_boundary(&artifact.summary, ARTIFACT_SUMMARY_CAP);
        let data = serde_json::json!({
            "name": artifact.name,
            "path": artifact.path,
            "byte_size": artifact.byte_size,
            "summary": artifact.summary,
        });
        let conn = self.conn.lock().expect("store mutex poisoned");
        conn.execute(
            "INSERT INTO artifacts(id, run_id, node_id, kind, data_json, created_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                artifact.id,
                artifact.run_id,
                artifact.node_id,
                artifact.kind,
                serde_json::to_string(&data)?,
                artifact.created_at_ms,
            ],
        )?;
        Ok(artifact)
    }

    pub fn list_artifacts(&self, run_id: &str, limit: usize) -> Result<Vec<ArtifactRecord>> {
        let limit = limit.clamp(1, 500);
        let conn = self.conn.lock().expect("store mutex poisoned");
        let mut stmt = conn.prepare(
            "SELECT id, run_id, node_id, kind, data_json, created_at_ms
             FROM artifacts WHERE run_id = ?1
             ORDER BY created_at_ms DESC, id DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![run_id, limit as i64], |r| {
            let data: String = r.get(4)?;
            let data: serde_json::Value = serde_json::from_str(&data).unwrap_or_default();
            Ok(ArtifactRecord {
                id: r.get(0)?,
                run_id: r.get(1)?,
                node_id: r.get(2)?,
                kind: r.get(3)?,
                name: data
                    .get("name")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                path: data
                    .get("path")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string),
                byte_size: data.get("byte_size").and_then(serde_json::Value::as_i64),
                summary: data
                    .get("summary")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                created_at_ms: r.get(5)?,
            })
        })?;
        let mut artifacts = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        // Newest-first from SQL, oldest-first for the caller: the UI appends.
        artifacts.reverse();
        Ok(artifacts)
    }

    // ---- worktree index ----

    /// Record (or refresh) a daemon-managed worktree. This table is the
    /// authority for reclaim: a directory with no live row here is never
    /// removed, no matter what it looks like on disk.
    pub fn record_worktree(&self, entry: &WorktreeRecord) -> Result<()> {
        let conn = self.conn.lock().expect("store mutex poisoned");
        conn.execute(
            "INSERT INTO worktrees(path, kind, run_id, node_id, repo_path, branch,
                                   base_commit, created_at_ms, removed_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, NULL)
             ON CONFLICT(path) DO UPDATE SET
                kind = excluded.kind,
                run_id = excluded.run_id,
                node_id = excluded.node_id,
                repo_path = excluded.repo_path,
                branch = excluded.branch,
                base_commit = excluded.base_commit,
                created_at_ms = excluded.created_at_ms,
                removed_at_ms = NULL",
            params![
                entry.path,
                entry.kind,
                entry.run_id,
                entry.node_id,
                entry.repo_path,
                entry.branch,
                entry.base_commit,
                if entry.created_at_ms == 0 {
                    now_ms()
                } else {
                    entry.created_at_ms
                },
            ],
        )?;
        Ok(())
    }

    pub fn get_worktree(&self, path: &str) -> Result<Option<WorktreeRecord>> {
        let conn = self.conn.lock().expect("store mutex poisoned");
        conn.query_row(
            "SELECT path, kind, run_id, node_id, repo_path, branch, base_commit,
                    created_at_ms, removed_at_ms
             FROM worktrees WHERE path = ?1",
            params![path],
            row_to_worktree,
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn list_worktrees(&self, include_reclaimed: bool) -> Result<Vec<WorktreeRecord>> {
        let conn = self.conn.lock().expect("store mutex poisoned");
        let sql = if include_reclaimed {
            "SELECT path, kind, run_id, node_id, repo_path, branch, base_commit,
                    created_at_ms, removed_at_ms
             FROM worktrees ORDER BY created_at_ms DESC, path"
        } else {
            "SELECT path, kind, run_id, node_id, repo_path, branch, base_commit,
                    created_at_ms, removed_at_ms
             FROM worktrees WHERE removed_at_ms IS NULL ORDER BY created_at_ms DESC, path"
        };
        let mut stmt = conn.prepare(sql)?;
        let rows = stmt.query_map([], row_to_worktree)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// Mark a worktree reclaimed. Returns false when the row is missing or
    /// was already marked, which is what makes a repeated reclaim a no-op
    /// rather than a second removal attempt.
    pub fn mark_worktree_removed(&self, path: &str) -> Result<bool> {
        let conn = self.conn.lock().expect("store mutex poisoned");
        let n = conn.execute(
            "UPDATE worktrees SET removed_at_ms = ?2 WHERE path = ?1 AND removed_at_ms IS NULL",
            params![path, now_ms()],
        )?;
        Ok(n > 0)
    }

    // ---- external provider history index ----

    pub fn upsert_external_history(&self, entry: &ExternalHistoryEntry) -> Result<()> {
        let conn = self.conn.lock().expect("store mutex poisoned");
        conn.execute(
            "INSERT INTO external_history(
                provider, source_id, transcript_path, cwd, title, first_prompt,
                updated_at_ms, last_seen_ms, missing, diagnostic, adopted_run_id
             )
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
             ON CONFLICT(provider, source_id) DO UPDATE SET
                transcript_path = excluded.transcript_path,
                cwd = excluded.cwd,
                title = excluded.title,
                first_prompt = excluded.first_prompt,
                updated_at_ms = excluded.updated_at_ms,
                last_seen_ms = excluded.last_seen_ms,
                missing = excluded.missing,
                diagnostic = excluded.diagnostic,
                adopted_run_id = COALESCE(external_history.adopted_run_id, excluded.adopted_run_id)",
            params![
                entry.provider,
                entry.source_id,
                entry.transcript_path,
                entry.cwd,
                entry.title,
                entry.first_prompt,
                entry.updated_at_ms,
                entry.last_seen_ms,
                entry.missing as i64,
                entry.diagnostic,
                entry.adopted_run_id,
            ],
        )?;
        Ok(())
    }

    pub fn get_external_history(
        &self,
        provider: &str,
        source_id: &str,
    ) -> Result<Option<ExternalHistoryEntry>> {
        let conn = self.conn.lock().expect("store mutex poisoned");
        conn.query_row(
            "SELECT provider, source_id, transcript_path, cwd, title, first_prompt,
                    updated_at_ms, last_seen_ms, missing, diagnostic, adopted_run_id
             FROM external_history WHERE provider = ?1 AND source_id = ?2",
            params![provider, source_id],
            row_to_external_history,
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn list_external_history(
        &self,
        filter: &ExternalHistoryFilter,
    ) -> Result<ExternalHistoryPage> {
        let limit = filter.limit.clamp(1, 200);
        let conn = self.conn.lock().expect("store mutex poisoned");
        let mut clauses = vec!["missing = 0".to_string()];
        let mut values = Vec::<SqlValue>::new();
        if let Some(provider) = filter.provider.as_deref() {
            clauses.push("provider = ?".into());
            values.push(provider.to_string().into());
        }
        if let Some(project_path) = filter.project_path.as_deref() {
            clauses.push("cwd = ?".into());
            values.push(project_path.to_string().into());
        }
        if let Some(query) = filter
            .query
            .as_deref()
            .map(str::trim)
            .filter(|q| !q.is_empty())
        {
            clauses.push(
                "instr(lower(provider || char(31) || source_id || char(31) || transcript_path || \
                 char(31) || coalesce(cwd, '') || char(31) || coalesce(title, '') || char(31) || \
                 coalesce(first_prompt, '')), lower(?)) > 0"
                    .into(),
            );
            values.push(query.to_string().into());
        }
        if let Some((updated, provider, source)) = parse_history_cursor(filter.cursor.as_deref()) {
            clauses.push(
                "(updated_at_ms < ? OR \
                  (updated_at_ms = ? AND provider > ?) OR \
                  (updated_at_ms = ? AND provider = ? AND source_id > ?))"
                    .into(),
            );
            values.push(updated.into());
            values.push(updated.into());
            values.push(provider.clone().into());
            values.push(updated.into());
            values.push(provider.into());
            values.push(source.into());
        }
        let sql = format!(
            "SELECT provider, source_id, transcript_path, cwd, title, first_prompt,
                    updated_at_ms, last_seen_ms, missing, diagnostic, adopted_run_id
             FROM external_history
             WHERE {}
             ORDER BY updated_at_ms DESC, provider ASC, source_id ASC
             LIMIT ?",
            clauses.join(" AND ")
        );
        values.push(((limit + 1) as i64).into());
        let mut stmt = conn.prepare(&sql)?;
        let mapped = stmt.query_map(params_from_iter(values.iter()), row_to_external_history)?;
        let mut rows = mapped.collect::<std::result::Result<Vec<_>, _>>()?;
        let has_more = rows.len() > limit;
        rows.truncate(limit);
        let next_cursor = has_more.then(|| history_cursor(&rows[rows.len() - 1]));
        Ok(ExternalHistoryPage {
            entries: rows,
            next_cursor,
        })
    }

    pub fn mark_external_history_adopted(
        &self,
        provider: &str,
        source_id: &str,
        run_id: &str,
    ) -> Result<bool> {
        let conn = self.conn.lock().expect("store mutex poisoned");
        Ok(conn.execute(
            "UPDATE external_history SET adopted_run_id = ?3
             WHERE provider = ?1 AND source_id = ?2 AND adopted_run_id IS NULL",
            params![provider, source_id, run_id],
        )? > 0)
    }

    /// Atomically claim an indexed provider source and create its draft run.
    /// An immediate transaction ensures competing connections observe the
    /// winner's link before they can create another run.
    pub fn adopt_external_history(
        &self,
        provider: &str,
        source_id: &str,
        project_id: &str,
        engine: &str,
        objective: &str,
    ) -> Result<ExternalHistoryAdoption> {
        let mut conn = self.conn.lock().expect("store mutex poisoned");
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let adopted_run_id = tx
            .query_row(
                "SELECT adopted_run_id FROM external_history
                 WHERE provider = ?1 AND source_id = ?2",
                params![provider, source_id],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?
            .ok_or_else(|| {
                StoreError::NotFound(format!("external history {provider}/{source_id}"))
            })?;
        if let Some(run_id) = adopted_run_id {
            tx.commit()?;
            return Ok(ExternalHistoryAdoption::AlreadyAdopted { run_id });
        }

        let now = now_ms();
        let run = Run {
            id: autoharness_core::new_id(),
            project_id: project_id.to_string(),
            engine: engine.to_string(),
            model: None,
            reasoning_effort: None,
            objective: objective.to_string(),
            state: "draft".into(),
            check_command: None,
            parent_run_id: None,
            created_at_ms: now,
            updated_at_ms: now,
            attempt_group: None,
        };
        tx.execute(
            "INSERT INTO runs(id, project_id, engine, model, reasoning_effort, objective, state,
                              check_command, parent_run_id, created_at_ms, updated_at_ms)
             VALUES (?1, ?2, ?3, NULL, NULL, ?4, ?5, NULL, NULL, ?6, ?7)",
            params![
                run.id,
                run.project_id,
                run.engine,
                run.objective,
                run.state,
                run.created_at_ms,
                run.updated_at_ms,
            ],
        )?;
        let claimed = tx.execute(
            "UPDATE external_history SET adopted_run_id = ?3
             WHERE provider = ?1 AND source_id = ?2 AND adopted_run_id IS NULL",
            params![provider, source_id, run.id],
        )?;
        if claimed != 1 {
            return Err(StoreError::Corruption(format!(
                "external history claim changed during transaction: {provider}/{source_id}"
            )));
        }
        tx.commit()?;
        Ok(ExternalHistoryAdoption::Created(Box::new(run)))
    }

    pub fn mark_external_history_missing_except_seen(
        &self,
        provider: &str,
        scan_seen_ms: i64,
    ) -> Result<usize> {
        let conn = self.conn.lock().expect("store mutex poisoned");
        Ok(conn.execute(
            "UPDATE external_history SET missing = 1
             WHERE provider = ?1 AND last_seen_ms != ?2",
            params![provider, scan_seen_ms],
        )?)
    }

    // ---- chat ----

    pub fn append_chat(&self, run_id: &str, role: &str, content: &str) -> Result<ChatMessage> {
        let msg = ChatMessage {
            id: 0,
            run_id: run_id.to_string(),
            role: role.to_string(),
            content: content.to_string(),
            created_at_ms: now_ms(),
        };
        let conn = self.conn.lock().expect("store mutex poisoned");
        conn.execute(
            "INSERT INTO chat_messages(run_id, role, content, created_at_ms) VALUES (?1, ?2, ?3, ?4)",
            params![msg.run_id, msg.role, msg.content, msg.created_at_ms],
        )?;
        Ok(ChatMessage {
            id: conn.last_insert_rowid(),
            ..msg
        })
    }

    pub fn list_chat(&self, run_id: &str) -> Result<Vec<ChatMessage>> {
        let conn = self.conn.lock().expect("store mutex poisoned");
        let mut stmt = conn.prepare(
            "SELECT id, run_id, role, content, created_at_ms FROM chat_messages
             WHERE run_id = ?1 ORDER BY id",
        )?;
        let rows = stmt.query_map(params![run_id], |r| {
            Ok(ChatMessage {
                id: r.get(0)?,
                run_id: r.get(1)?,
                role: r.get(2)?,
                content: r.get(3)?,
                created_at_ms: r.get(4)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    // ---- event ledger (append-only) ----

    /// Append an event. Assigns the global monotonic `seq` and the per-run
    /// `run_seq` atomically. This is the ONLY writer of the events table.
    pub fn append_event(
        &self,
        run_id: Option<&str>,
        kind: &str,
        payload: serde_json::Value,
    ) -> Result<EventRecord> {
        let conn = self.conn.lock().expect("store mutex poisoned");
        let tx = conn.unchecked_transaction()?;
        let run_seq: i64 = tx
            .query_row(
                "SELECT COALESCE(MAX(run_seq), 0) + 1 FROM events WHERE run_id IS ?1",
                params![run_id],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(1);
        let record = EventRecord {
            seq: 0,
            run_id: run_id.map(str::to_string),
            run_seq,
            timestamp_ms: now_ms(),
            kind: kind.to_string(),
            payload,
        };
        tx.execute(
            "INSERT INTO events(run_id, run_seq, timestamp_ms, kind, payload) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                record.run_id,
                record.run_seq,
                record.timestamp_ms,
                record.kind,
                serde_json::to_string(&record.payload)?
            ],
        )?;
        let seq = tx.last_insert_rowid();
        tx.commit()?;
        Ok(EventRecord { seq, ..record })
    }

    #[cfg(test)]
    pub fn append_event_at_for_tests(
        &self,
        run_id: Option<&str>,
        kind: &str,
        payload: serde_json::Value,
        timestamp_ms: i64,
    ) -> Result<EventRecord> {
        let conn = self.conn.lock().expect("store mutex poisoned");
        let tx = conn.unchecked_transaction()?;
        let run_seq: i64 = tx
            .query_row(
                "SELECT COALESCE(MAX(run_seq), 0) + 1 FROM events WHERE run_id IS ?1",
                params![run_id],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(1);
        let record = EventRecord {
            seq: 0,
            run_id: run_id.map(str::to_string),
            run_seq,
            timestamp_ms,
            kind: kind.to_string(),
            payload,
        };
        tx.execute(
            "INSERT INTO events(run_id, run_seq, timestamp_ms, kind, payload) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                record.run_id,
                record.run_seq,
                record.timestamp_ms,
                record.kind,
                serde_json::to_string(&record.payload)?
            ],
        )?;
        let seq = tx.last_insert_rowid();
        tx.commit()?;
        Ok(EventRecord { seq, ..record })
    }

    /// Replay events with `seq > since_seq`, newest-last. Optional run filter.
    pub fn events_since(&self, since_seq: i64, run_id: Option<&str>) -> Result<Vec<EventRecord>> {
        let conn = self.conn.lock().expect("store mutex poisoned");
        let (sql, run_filter): (&str, Option<String>) = match run_id {
            Some(r) => (
                "SELECT seq, run_id, run_seq, timestamp_ms, kind, payload FROM events
                 WHERE seq > ?1 AND run_id = ?2 ORDER BY seq",
                Some(r.to_string()),
            ),
            None => (
                "SELECT seq, run_id, run_seq, timestamp_ms, kind, payload FROM events
                 WHERE seq > ?1 ORDER BY seq",
                None,
            ),
        };
        let mut stmt = conn.prepare(sql)?;
        let map_row = |r: &rusqlite::Row<'_>| -> rusqlite::Result<EventRecord> {
            let payload: String = r.get(5)?;
            Ok(EventRecord {
                seq: r.get(0)?,
                run_id: r.get(1)?,
                run_seq: r.get(2)?,
                timestamp_ms: r.get(3)?,
                kind: r.get(4)?,
                payload: serde_json::from_str(&payload).unwrap_or(serde_json::Value::Null),
            })
        };
        let rows = match run_filter {
            Some(r) => stmt.query_map(params![since_seq, r], map_row)?,
            None => stmt.query_map(params![since_seq], map_row)?,
        };
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// Current maximum sequence (0 when the ledger is empty).
    pub fn max_sequence(&self) -> Result<i64> {
        let conn = self.conn.lock().expect("store mutex poisoned");
        Ok(conn.query_row("SELECT COALESCE(MAX(seq), 0) FROM events", [], |r| r.get(0))?)
    }

    pub fn event_count(&self) -> Result<i64> {
        let conn = self.conn.lock().expect("store mutex poisoned");
        Ok(conn.query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))?)
    }

    /// Sequence of the newest event, or 0 when the ledger is empty.
    ///
    /// A subscriber uses this to tell history from news: everything at or
    /// below this number already happened.
    pub fn latest_event_seq(&self) -> Result<i64> {
        let conn = self.conn.lock().expect("store mutex poisoned");
        Ok(conn.query_row("SELECT COALESCE(MAX(seq), 0) FROM events", [], |r| r.get(0))?)
    }

    pub fn usage_summary(&self) -> Result<autoharness_protocol::params::UsageSummaryResult> {
        self.usage_summary_at(now_ms())
    }

    pub fn usage_summary_at(
        &self,
        generated_at_ms: i64,
    ) -> Result<autoharness_protocol::params::UsageSummaryResult> {
        use autoharness_protocol::params::{
            UsageBucket, UsageProviderSummary, UsageRunSummary, UsageSummaryResult,
        };
        use std::collections::{BTreeMap, BTreeSet};

        #[derive(Default)]
        struct Acc {
            provider: String,
            input: u64,
            output: u64,
            event_count: u64,
            latest_ts: i64,
        }

        let conn = self.conn.lock().expect("store mutex poisoned");
        let mut stmt = conn.prepare(
            "SELECT e.run_id, r.engine, e.timestamp_ms, e.payload
             FROM events e
             JOIN runs r ON r.id = e.run_id
             WHERE e.kind = 'engine.usage' AND e.run_id IS NOT NULL
             ORDER BY e.seq",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, String>(3)?,
            ))
        })?;

        let mut runs: BTreeMap<String, Acc> = BTreeMap::new();
        for row in rows {
            let (run_id, provider, timestamp_ms, payload) = row?;
            let payload: serde_json::Value =
                serde_json::from_str(&payload).unwrap_or(serde_json::Value::Null);
            let input = payload
                .get("input_tokens")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0);
            let output = payload
                .get("output_tokens")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0);
            let acc = runs.entry(run_id).or_insert_with(|| Acc {
                provider: provider.clone(),
                ..Acc::default()
            });
            acc.latest_ts = acc.latest_ts.max(timestamp_ms);
            acc.event_count += 1;
            match provider.as_str() {
                "codex" => {
                    acc.input = acc.input.max(input);
                    acc.output = acc.output.max(output);
                }
                "claude" => {
                    acc.input = acc.input.saturating_add(input);
                    acc.output = acc.output.saturating_add(output);
                }
                _ => {
                    acc.input = acc.input.saturating_add(input);
                    acc.output = acc.output.saturating_add(output);
                }
            }
        }

        let today_start_ms = day_start_ms(generated_at_ms);
        let month_start_ms = month_start_ms(generated_at_ms);
        let mut provider_buckets: BTreeMap<String, (UsageBucket, UsageBucket, UsageBucket)> =
            BTreeMap::new();
        let mut today_runs: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        let mut month_runs: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        let mut all_runs: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        let mut run_summaries = Vec::new();

        for (run_id, acc) in runs {
            let total = acc.input.saturating_add(acc.output);
            let (today, month, all) = provider_buckets.entry(acc.provider.clone()).or_default();
            all.input_tokens = all.input_tokens.saturating_add(acc.input);
            all.output_tokens = all.output_tokens.saturating_add(acc.output);
            all.total_tokens = all.total_tokens.saturating_add(total);
            all.event_count = all.event_count.saturating_add(acc.event_count);
            all_runs
                .entry(acc.provider.clone())
                .or_default()
                .insert(run_id.clone());
            if acc.latest_ts >= month_start_ms {
                month.input_tokens = month.input_tokens.saturating_add(acc.input);
                month.output_tokens = month.output_tokens.saturating_add(acc.output);
                month.total_tokens = month.total_tokens.saturating_add(total);
                month.event_count = month.event_count.saturating_add(acc.event_count);
                month_runs
                    .entry(acc.provider.clone())
                    .or_default()
                    .insert(run_id.clone());
            }
            if acc.latest_ts >= today_start_ms {
                today.input_tokens = today.input_tokens.saturating_add(acc.input);
                today.output_tokens = today.output_tokens.saturating_add(acc.output);
                today.total_tokens = today.total_tokens.saturating_add(total);
                today.event_count = today.event_count.saturating_add(acc.event_count);
                today_runs
                    .entry(acc.provider.clone())
                    .or_default()
                    .insert(run_id.clone());
            }
            run_summaries.push(UsageRunSummary {
                run_id,
                provider: acc.provider,
                input_tokens: acc.input,
                output_tokens: acc.output,
                total_tokens: total,
                event_count: acc.event_count,
            });
        }

        let providers = provider_buckets
            .into_iter()
            .map(|(provider, (mut today, mut month, mut all_time))| {
                today.run_count = today_runs
                    .get(&provider)
                    .map_or(0, |runs| runs.len() as u64);
                month.run_count = month_runs
                    .get(&provider)
                    .map_or(0, |runs| runs.len() as u64);
                all_time.run_count = all_runs.get(&provider).map_or(0, |runs| runs.len() as u64);
                UsageProviderSummary {
                    provider,
                    today,
                    month,
                    all_time,
                }
            })
            .collect();

        Ok(UsageSummaryResult {
            generated_at_ms,
            today_start_ms,
            month_start_ms,
            providers,
            runs: run_summaries,
        })
    }

    // ---- checkpoints / snapshots ----

    // Snapshot helpers continue below after graph/node attempt storage.
    // ---- graphs and node attempts (Phase 6) ----

    /// Persist a compiled graph for a run. Versions are append-only: a replan
    /// adds a version, it never rewrites the plan the user approved.
    pub fn save_graph(&self, run_id: &str, graph: &serde_json::Value) -> Result<i64> {
        let conn = self.conn.lock().expect("store mutex poisoned");
        let version: i64 = conn.query_row(
            "SELECT COALESCE(MAX(version), 0) + 1 FROM graph_versions WHERE run_id = ?1",
            params![run_id],
            |r| r.get(0),
        )?;
        conn.execute(
            "INSERT INTO graph_versions(id, run_id, version, graph_json, created_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                autoharness_core::new_id(),
                run_id,
                version,
                serde_json::to_string(graph)?,
                now_ms()
            ],
        )?;
        Ok(version)
    }

    /// Latest graph version for a run.
    pub fn latest_graph(&self, run_id: &str) -> Result<Option<(i64, serde_json::Value)>> {
        let conn = self.conn.lock().expect("store mutex poisoned");
        conn.query_row(
            "SELECT version, graph_json FROM graph_versions
             WHERE run_id = ?1 ORDER BY version DESC LIMIT 1",
            params![run_id],
            |r| {
                let version: i64 = r.get(0)?;
                let json: String = r.get(1)?;
                Ok((version, json))
            },
        )
        .optional()?
        .map(|(version, json)| Ok((version, serde_json::from_str(&json)?)))
        .transpose()
    }

    /// Record a node attempt's state. Attempts are append-only history, so a
    /// retried node keeps the evidence of why the first try failed.
    pub fn record_node_attempt(
        &self,
        run_id: &str,
        node_id: &str,
        attempt: i64,
        state: &str,
        detail: Option<&str>,
    ) -> Result<()> {
        let conn = self.conn.lock().expect("store mutex poisoned");
        conn.execute(
            "INSERT INTO node_attempts(id, run_id, node_id, attempt, state, started_at_ms, finished_at_ms, detail)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                autoharness_core::new_id(),
                run_id,
                node_id,
                attempt,
                state,
                now_ms(),
                if state == "running" { None } else { Some(now_ms()) },
                detail
            ],
        )?;
        Ok(())
    }

    /// Every recorded attempt for a run, oldest first.
    pub fn node_attempts(&self, run_id: &str) -> Result<Vec<(String, i64, String)>> {
        let conn = self.conn.lock().expect("store mutex poisoned");
        let mut stmt = conn.prepare(
            "SELECT node_id, attempt, state FROM node_attempts
             WHERE run_id = ?1 ORDER BY started_at_ms, rowid",
        )?;
        let rows = stmt.query_map(params![run_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub fn latest_node_attempt(
        &self,
        run_id: &str,
        node_id: &str,
    ) -> Result<Option<NodeAttemptRecord>> {
        let conn = self.conn.lock().expect("store mutex poisoned");
        conn.query_row(
            "SELECT node_id, attempt, state, detail FROM node_attempts
             WHERE run_id = ?1 AND node_id = ?2
             ORDER BY attempt DESC, rowid DESC LIMIT 1",
            params![run_id, node_id],
            |row| {
                Ok(NodeAttemptRecord {
                    node_id: row.get(0)?,
                    attempt: row.get(1)?,
                    state: row.get(2)?,
                    detail: row.get(3)?,
                })
            },
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn next_node_attempt(&self, run_id: &str, node_id: &str) -> Result<i64> {
        let conn = self.conn.lock().expect("store mutex poisoned");
        conn.query_row(
            "SELECT COALESCE(MAX(attempt), 0) + 1 FROM node_attempts
             WHERE run_id = ?1 AND node_id = ?2",
            params![run_id, node_id],
            |row| row.get(0),
        )
        .map_err(Into::into)
    }

    // ---- verified project memory (Phase 8) ----

    /// Record a memory fact. Evidence is REQUIRED for a verified fact: a fact
    /// with nothing backing it is a model's guess, and PLAN.md forbids
    /// injecting those. Unverified facts are stored as proposals.
    pub fn add_memory_fact(&self, fact: &autoharness_core::MemoryFact) -> Result<()> {
        if fact.verified && fact.evidence.is_empty() {
            return Err(StoreError::Corruption(
                "a verified memory fact must carry evidence".into(),
            ));
        }
        let mut conn = self.conn.lock().expect("store mutex poisoned");
        let tx = conn.transaction()?;
        // Superseding is materialized, not derived. If it were derived from a
        // live pointer, forgetting the newer fact would resurrect the stale
        // one — and a project that changed would be described by what it used
        // to be. The full history stays in the event ledger.
        if let Some(old) = &fact.supersedes {
            tx.execute("DELETE FROM memory_facts WHERE id = ?1", params![old])?;
        }
        tx.execute(
            "INSERT INTO memory_facts(id, project_id, kind, statement, evidence, confidence, supersedes, verified, created_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                fact.id,
                fact.project_id,
                serde_json::to_string(&fact.kind)?.trim_matches('"').to_string(),
                fact.statement,
                serde_json::to_string(&fact.evidence)?,
                fact.confidence,
                fact.supersedes,
                fact.verified as i64,
                fact.created_at_ms,
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Facts for a project, newest first. `verified_only` is what the injector
    /// uses; the UI lists proposals too so a human can promote them.
    pub fn list_memory_facts(
        &self,
        project_id: &str,
        verified_only: bool,
    ) -> Result<Vec<autoharness_core::MemoryFact>> {
        let conn = self.conn.lock().expect("store mutex poisoned");
        let sql = format!(
            "SELECT id, project_id, kind, statement, evidence, confidence, supersedes, verified, created_at_ms
             FROM memory_facts
             WHERE project_id = ?1 {}
             ORDER BY created_at_ms DESC",
            if verified_only { "AND verified = 1" } else { "" }
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params![project_id], row_to_fact)?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// Lexical search over verified facts (FTS5). Superseded facts are
    /// excluded: a project that changed must not be described by what it used
    /// to be.
    pub fn search_memory(
        &self,
        project_id: &str,
        query: &str,
        limit: usize,
    ) -> Result<Vec<autoharness_core::MemoryFact>> {
        let cleaned = sanitize_fts_query(query);
        if cleaned.is_empty() {
            return Ok(vec![]);
        }
        let conn = self.conn.lock().expect("store mutex poisoned");
        let mut stmt = conn.prepare(
            "SELECT f.id, f.project_id, f.kind, f.statement, f.evidence, f.confidence,
                    f.supersedes, f.verified, f.created_at_ms
             FROM memory_fts fts
             JOIN memory_facts f ON f.rowid = fts.rowid
             WHERE memory_fts MATCH ?1
               AND f.project_id = ?2
               AND f.verified = 1
             ORDER BY bm25(memory_fts)
             LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![cleaned, project_id, limit as i64], row_to_fact)?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// Promote a proposal to verified. Refuses when there is no evidence.
    pub fn verify_memory_fact(&self, fact_id: &str) -> Result<bool> {
        let conn = self.conn.lock().expect("store mutex poisoned");
        let evidence: Option<String> = conn
            .query_row(
                "SELECT evidence FROM memory_facts WHERE id = ?1",
                params![fact_id],
                |r| r.get(0),
            )
            .optional()?;
        let Some(evidence) = evidence else {
            return Ok(false);
        };
        if evidence.trim() == "[]" {
            return Err(StoreError::Corruption(
                "cannot verify a fact with no evidence".into(),
            ));
        }
        let changed = conn.execute(
            "UPDATE memory_facts SET verified = 1 WHERE id = ?1",
            params![fact_id],
        )?;
        Ok(changed > 0)
    }

    /// Forget a fact outright. The user asked; memory is theirs.
    pub fn forget_memory_fact(&self, fact_id: &str) -> Result<bool> {
        let conn = self.conn.lock().expect("store mutex poisoned");
        Ok(conn.execute("DELETE FROM memory_facts WHERE id = ?1", params![fact_id])? > 0)
    }

    // ---- policy versions (Phase 8) ----

    /// Store a candidate policy. Candidates are never promoted here — only a
    /// human, through `promote_policy`, can do that.
    pub fn add_policy_candidate(&self, data: &serde_json::Value) -> Result<i64> {
        let conn = self.conn.lock().expect("store mutex poisoned");
        let version: i64 = conn.query_row(
            "SELECT COALESCE(MAX(version), 0) + 1 FROM policy_versions",
            [],
            |r| r.get(0),
        )?;
        conn.execute(
            "INSERT INTO policy_versions(id, version, data_json, promoted, created_at_ms)
             VALUES (?1, ?2, ?3, 0, ?4)",
            params![
                autoharness_core::new_id(),
                version,
                serde_json::to_string(data)?,
                now_ms()
            ],
        )?;
        Ok(version)
    }

    /// All policy versions, newest first.
    pub fn list_policies(&self) -> Result<Vec<(i64, bool, serde_json::Value)>> {
        let conn = self.conn.lock().expect("store mutex poisoned");
        let mut stmt = conn.prepare(
            "SELECT version, promoted, data_json FROM policy_versions ORDER BY version DESC",
        )?;
        let rows = stmt.query_map([], |r| {
            let version: i64 = r.get(0)?;
            let promoted: i64 = r.get(1)?;
            let json: String = r.get(2)?;
            Ok((version, promoted == 1, json))
        })?;
        rows.map(|row| {
            let (version, promoted, json) = row?;
            Ok((version, promoted, serde_json::from_str(&json)?))
        })
        .collect()
    }

    /// The policy currently in force, if any has been promoted.
    pub fn promoted_policy(&self) -> Result<Option<(i64, serde_json::Value)>> {
        let conn = self.conn.lock().expect("store mutex poisoned");
        conn.query_row(
            "SELECT version, data_json FROM policy_versions
             WHERE promoted = 1 ORDER BY version DESC LIMIT 1",
            [],
            |r| {
                let version: i64 = r.get(0)?;
                let json: String = r.get(1)?;
                Ok((version, json))
            },
        )
        .optional()?
        .map(|(version, json)| Ok((version, serde_json::from_str(&json)?)))
        .transpose()
    }

    /// Promote exactly one version. Promotion is exclusive so "the policy in
    /// force" is never ambiguous, and rollback is just promoting an older one.
    pub fn promote_policy(&self, version: i64) -> Result<bool> {
        let mut conn = self.conn.lock().expect("store mutex poisoned");
        let tx = conn.transaction()?;
        let exists: i64 = tx.query_row(
            "SELECT COUNT(*) FROM policy_versions WHERE version = ?1",
            params![version],
            |r| r.get(0),
        )?;
        if exists == 0 {
            return Ok(false);
        }
        tx.execute("UPDATE policy_versions SET promoted = 0", [])?;
        tx.execute(
            "UPDATE policy_versions SET promoted = 1 WHERE version = ?1",
            params![version],
        )?;
        tx.commit()?;
        Ok(true)
    }

    // ---- integrity, export, and privacy (Phase 9) ----

    /// SQLite's own integrity check plus the ledger invariants this product
    /// depends on. Reported, never "repaired" silently: an append-only audit
    /// log that quietly rewrites itself is not an audit log.
    pub fn integrity_report(&self) -> Result<IntegrityReport> {
        let conn = self.conn.lock().expect("store mutex poisoned");
        let sqlite_ok: String = conn.query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
        let foreign_key_violations: i64 = {
            let mut stmt = conn.prepare("PRAGMA foreign_key_check")?;
            let mut rows = stmt.query([])?;
            let mut count = 0;
            while rows.next()?.is_some() {
                count += 1;
            }
            count
        };
        // Every event must belong to a run that exists, or to no run at all.
        let orphan_events: i64 = conn.query_row(
            "SELECT COUNT(*) FROM events
             WHERE run_id IS NOT NULL AND run_id NOT IN (SELECT id FROM runs)",
            [],
            |r| r.get(0),
        )?;
        // A run in a state the domain does not define cannot be reasoned about.
        let mut stmt = conn.prepare("SELECT DISTINCT state FROM runs")?;
        let states: Vec<String> = stmt
            .query_map([], |r| r.get(0))?
            .collect::<std::result::Result<Vec<String>, _>>()?;
        let unknown_run_states: Vec<String> = states
            .into_iter()
            .filter(|s| s.parse::<autoharness_core::RunState>().is_err())
            .collect();

        Ok(IntegrityReport {
            healthy: sqlite_ok == "ok"
                && foreign_key_violations == 0
                && orphan_events == 0
                && unknown_run_states.is_empty(),
            sqlite: sqlite_ok,
            foreign_key_violations,
            orphan_events,
            unknown_run_states,
        })
    }

    /// Everything the user's database holds, as JSON. Their data is theirs:
    /// this is the escape hatch out of the product.
    pub fn export_json(&self) -> Result<serde_json::Value> {
        let conn = self.conn.lock().expect("store mutex poisoned");
        let mut out = serde_json::Map::new();
        for table in [
            "projects",
            "runs",
            "chat_messages",
            "events",
            "graph_versions",
            "node_attempts",
            "artifacts",
            "checkpoints",
            "memory_facts",
            "policy_versions",
            "external_history",
            "app_settings",
            "worktrees",
            "queue_items",
            "queue_mutations",
        ] {
            let mut stmt = conn.prepare(&format!("SELECT * FROM {table}"))?;
            let columns: Vec<String> = stmt.column_names().iter().map(|c| c.to_string()).collect();
            let rows = stmt.query_map([], |row| {
                let mut object = serde_json::Map::new();
                for (i, name) in columns.iter().enumerate() {
                    let value = match row.get_ref(i)? {
                        rusqlite::types::ValueRef::Null => serde_json::Value::Null,
                        rusqlite::types::ValueRef::Integer(v) => v.into(),
                        rusqlite::types::ValueRef::Real(v) => v.into(),
                        rusqlite::types::ValueRef::Text(v) => {
                            String::from_utf8_lossy(v).into_owned().into()
                        }
                        rusqlite::types::ValueRef::Blob(v) => v.len().into(),
                    };
                    object.insert(name.clone(), value);
                }
                Ok(serde_json::Value::Object(object))
            })?;
            let collected: Vec<serde_json::Value> =
                rows.collect::<std::result::Result<Vec<_>, _>>()?;
            out.insert(table.to_string(), serde_json::Value::Array(collected));
        }
        Ok(serde_json::Value::Object(out))
    }

    /// Erase everything belonging to one project, including its runs' events,
    /// chat, and memory. Irreversible by design — a privacy control that left
    /// residue would not be one.
    pub fn purge_project(&self, project_id: &str) -> Result<PurgeReport> {
        let mut conn = self.conn.lock().expect("store mutex poisoned");
        let project_path: String = conn
            .query_row(
                "SELECT path FROM projects WHERE id = ?1",
                params![project_id],
                |row| row.get(0),
            )
            .optional()?
            .ok_or_else(|| StoreError::NotFound(format!("project {project_id}")))?;
        let project_path = std::fs::canonicalize(&project_path)
            .unwrap_or_else(|_| std::path::PathBuf::from(project_path));
        let tx = conn.transaction()?;
        let run_ids: Vec<String> = {
            let mut stmt = tx.prepare("SELECT id FROM runs WHERE project_id = ?1")?;
            let rows = stmt.query_map(params![project_id], |r| r.get(0))?;
            rows.collect::<std::result::Result<Vec<_>, _>>()?
        };
        let history_to_delete: Vec<(String, String)> = {
            let mut stmt = tx
                .prepare("SELECT provider, source_id, cwd, adopted_run_id FROM external_history")?;
            let rows = stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            })?;
            rows.collect::<std::result::Result<Vec<_>, _>>()?
                .into_iter()
                .filter_map(|(provider, source_id, cwd, adopted_run_id)| {
                    let adopted_by_project = adopted_run_id
                        .as_ref()
                        .is_some_and(|run_id| run_ids.contains(run_id));
                    let belongs_to_project = cwd.as_deref().is_some_and(|cwd| {
                        let cwd = std::fs::canonicalize(cwd)
                            .unwrap_or_else(|_| std::path::PathBuf::from(cwd));
                        cwd.starts_with(&project_path)
                    });
                    (adopted_by_project || belongs_to_project).then_some((provider, source_id))
                })
                .collect()
        };
        for (provider, source_id) in history_to_delete {
            tx.execute(
                "DELETE FROM external_history WHERE provider = ?1 AND source_id = ?2",
                params![provider, source_id],
            )?;
        }
        let mut events = 0usize;
        for run_id in &run_ids {
            tx.execute(
                "DELETE FROM queue_mutations WHERE item_id IN
                 (SELECT id FROM queue_items WHERE run_id = ?1)",
                params![run_id],
            )?;
            tx.execute("DELETE FROM queue_items WHERE run_id = ?1", params![run_id])?;
            events += tx.execute("DELETE FROM events WHERE run_id = ?1", params![run_id])?;
            tx.execute(
                "DELETE FROM chat_messages WHERE run_id = ?1",
                params![run_id],
            )?;
            tx.execute("DELETE FROM checkpoints WHERE run_id = ?1", params![run_id])?;
            tx.execute(
                "DELETE FROM graph_versions WHERE run_id = ?1",
                params![run_id],
            )?;
            tx.execute(
                "DELETE FROM node_attempts WHERE run_id = ?1",
                params![run_id],
            )?;
            tx.execute("DELETE FROM artifacts WHERE run_id = ?1", params![run_id])?;
            tx.execute(
                "DELETE FROM engine_sessions WHERE run_id = ?1",
                params![run_id],
            )?;
        }
        let facts = tx.execute(
            "DELETE FROM memory_facts WHERE project_id = ?1",
            params![project_id],
        )?;
        let runs = tx.execute(
            "DELETE FROM runs WHERE project_id = ?1",
            params![project_id],
        )?;
        let projects = tx.execute("DELETE FROM projects WHERE id = ?1", params![project_id])?;
        tx.commit()?;
        Ok(PurgeReport {
            project_removed: projects > 0,
            runs,
            events,
            memory_facts: facts,
        })
    }

    /// Reclaim space after a purge. Separate from `purge_project` because it
    /// rewrites the whole file and should be the user's explicit choice.
    pub fn vacuum(&self) -> Result<()> {
        let conn = self.conn.lock().expect("store mutex poisoned");
        conn.execute_batch("VACUUM")?;
        Ok(())
    }

    pub fn create_checkpoint(
        &self,
        run_id: &str,
        node_id: Option<&str>,
        snapshot: serde_json::Value,
    ) -> Result<CheckpointRow> {
        let conn = self.conn.lock().expect("store mutex poisoned");
        let last_seq: i64 = conn.query_row(
            "SELECT COALESCE(MAX(seq), 0) FROM events WHERE run_id = ?1",
            params![run_id],
            |r| r.get(0),
        )?;
        let cp = CheckpointRow {
            id: autoharness_core::new_id(),
            run_id: run_id.to_string(),
            node_id: node_id.map(str::to_string),
            last_event_seq: last_seq,
            snapshot_json: snapshot,
            created_at_ms: now_ms(),
        };
        conn.execute(
            "INSERT INTO checkpoints(id, run_id, node_id, last_event_seq, snapshot_json, created_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                cp.id,
                cp.run_id,
                cp.node_id,
                cp.last_event_seq,
                serde_json::to_string(&cp.snapshot_json)?,
                cp.created_at_ms
            ],
        )?;
        Ok(cp)
    }

    pub fn latest_checkpoint(&self, run_id: &str) -> Result<Option<CheckpointRow>> {
        let conn = self.conn.lock().expect("store mutex poisoned");
        conn.query_row(
            "SELECT id, run_id, node_id, last_event_seq, snapshot_json, created_at_ms
             FROM checkpoints WHERE run_id = ?1 ORDER BY created_at_ms DESC LIMIT 1",
            params![run_id],
            |r| {
                let snapshot: String = r.get(4)?;
                Ok(CheckpointRow {
                    id: r.get(0)?,
                    run_id: r.get(1)?,
                    node_id: r.get(2)?,
                    last_event_seq: r.get(3)?,
                    snapshot_json: serde_json::from_str(&snapshot)
                        .unwrap_or(serde_json::Value::Null),
                    created_at_ms: r.get(5)?,
                })
            },
        )
        .optional()
        .map_err(Into::into)
    }

    /// Replay a run's events from a checkpoint's recorded position.
    pub fn replay_from_checkpoint(&self, checkpoint: &CheckpointRow) -> Result<Vec<EventRecord>> {
        self.events_since(checkpoint.last_event_seq, Some(&checkpoint.run_id))
    }

    /// Expose the inner connection for tests that need a raw query.
    #[cfg(test)]
    pub(crate) fn raw_connection_count(&self, table: &str) -> i64 {
        let conn = self.conn.lock().expect("store mutex poisoned");
        conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
            .unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migration_is_idempotent() {
        let store = Store::open_in_memory().unwrap();
        store.migrate().unwrap();
        store.migrate().unwrap();
        assert_eq!(
            store.raw_connection_count("migrations"),
            MIGRATIONS.len() as i64
        );
        store.integrity_check().unwrap();
    }

    #[test]
    fn wal_reopen_recovers_data() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.db");
        {
            let store = Store::open(&path).unwrap();
            store.add_project("demo", "/tmp/demo").unwrap();
            store
                .append_event(Some("r1"), "run.created", serde_json::json!({}))
                .unwrap();
            assert!(path.with_extension("db-wal").exists() || path.exists());
            // Drop without explicit close/checkpoint: WAL must still recover.
        }
        let store = Store::open(&path).unwrap();
        assert_eq!(store.list_projects().unwrap().len(), 1);
        assert_eq!(store.event_count().unwrap(), 1);
        store.integrity_check().unwrap();
    }

    #[test]
    fn ledger_sequences_are_monotonic_per_run_and_global() {
        let store = Store::open_in_memory().unwrap();
        let a1 = store
            .append_event(Some("a"), "k", serde_json::json!(1))
            .unwrap();
        let b1 = store
            .append_event(Some("b"), "k", serde_json::json!(2))
            .unwrap();
        let a2 = store
            .append_event(Some("a"), "k", serde_json::json!(3))
            .unwrap();
        assert!(a1.seq < b1.seq && b1.seq < a2.seq);
        assert_eq!((a1.run_seq, a2.run_seq), (1, 2));
        assert_eq!(b1.run_seq, 1);
        assert_eq!(store.max_sequence().unwrap(), a2.seq);
    }

    #[test]
    fn replay_from_sequence_returns_exact_suffix() {
        let store = Store::open_in_memory().unwrap();
        for i in 0..5 {
            store
                .append_event(Some("r"), "tick", serde_json::json!({"i": i}))
                .unwrap();
        }
        let all = store.events_since(0, None).unwrap();
        assert_eq!(all.len(), 5);
        let suffix = store.events_since(3, Some("r")).unwrap();
        assert_eq!(suffix.len(), 2);
        assert_eq!(suffix[0].payload["i"], 3);
        assert_eq!(suffix[1].payload["i"], 4);
        // Order strictly increasing.
        autoharness_protocol::assert_event_order(
            &suffix.iter().map(EventRecord::to_event).collect::<Vec<_>>(),
        )
        .unwrap();
    }

    #[test]
    fn checkpoint_snapshot_and_replay() {
        let store = Store::open_in_memory().unwrap();
        store
            .append_event(Some("r"), "a", serde_json::json!(1))
            .unwrap();
        let cp = store
            .create_checkpoint("r", Some("n1"), serde_json::json!({"state": "ok"}))
            .unwrap();
        assert_eq!(cp.last_event_seq, 1);
        store
            .append_event(Some("r"), "b", serde_json::json!(2))
            .unwrap();
        store
            .append_event(Some("r"), "c", serde_json::json!(3))
            .unwrap();

        let loaded = store.latest_checkpoint("r").unwrap().unwrap();
        assert_eq!(loaded.id, cp.id);
        assert_eq!(loaded.snapshot_json["state"], "ok");
        let replayed = store.replay_from_checkpoint(&loaded).unwrap();
        assert_eq!(replayed.len(), 2);
        assert_eq!(replayed[0].kind, "b");
    }

    #[test]
    fn concurrent_reader_and_writer() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("concurrent.db");
        let writer = Store::open(&path).unwrap();

        let reader_path = path.clone();
        let reader_thread = std::thread::spawn(move || {
            let reader = Store::open(&reader_path).unwrap();
            // WAL: reads proceed while the writer holds transactions.
            let mut last = 0;
            for _ in 0..200 {
                let n = reader.event_count().unwrap();
                assert!(n >= last, "event count must be monotonic");
                last = n;
                if n >= 50 {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            assert_eq!(last, 50);
            reader.integrity_check().unwrap();
        });

        for i in 0..50 {
            writer
                .append_event(Some("r"), "tick", serde_json::json!({"i": i}))
                .unwrap();
        }
        reader_thread.join().unwrap();
    }

    #[test]
    fn projects_runs_chat_round_trip() {
        let store = Store::open_in_memory().unwrap();
        let p = store.add_project("demo", "/tmp/demo").unwrap();
        assert_eq!(store.list_projects().unwrap().len(), 1);

        let run = store.create_run(&p.id, "codex", "do a thing").unwrap();
        assert_eq!(run.state, "draft");
        let fetched = store.get_run(&run.id).unwrap();
        assert_eq!(fetched.objective, "do a thing");
        store.set_run_state(&run.id, "running").unwrap();
        assert_eq!(store.get_run(&run.id).unwrap().state, "running");
        assert!(matches!(
            store.get_run("nope"),
            Err(StoreError::NotFound(_))
        ));

        let msg = store.append_chat(&run.id, "user", "hello").unwrap();
        assert!(msg.id > 0);
        assert_eq!(store.list_chat(&run.id).unwrap().len(), 1);

        // Removing a project that owns runs violates the FK constraint.
        assert!(store.remove_project(&p.id).is_err());

        // Removing a run-free project succeeds.
        let empty = store.add_project("empty", "/tmp/empty").unwrap();
        assert!(store.remove_project(&empty.id).unwrap());
        assert_eq!(store.list_projects().unwrap().len(), 1);
    }

    #[test]
    fn adding_the_same_canonical_project_path_is_idempotent() {
        let store = Store::open_in_memory().unwrap();
        let first = store
            .add_project("first label", "/tmp/same-project")
            .unwrap();
        let second = store.add_project("new label", "/tmp/same-project").unwrap();

        assert_eq!(second.id, first.id);
        assert_eq!(second.name, first.name);
        assert_eq!(store.list_projects().unwrap().len(), 1);
    }

    #[test]
    fn check_command_and_runs_in_states() {
        let store = Store::open_in_memory().unwrap();
        let p = store.add_project("demo", "/tmp/demo").unwrap();
        let r1 = store
            .create_run_with_check(&p.id, "codex", "a", Some("cargo test"))
            .unwrap();
        let r2 = store.create_run(&p.id, "claude", "b").unwrap();
        assert_eq!(
            store.get_run(&r1.id).unwrap().check_command.as_deref(),
            Some("cargo test")
        );
        assert_eq!(store.get_run(&r2.id).unwrap().check_command, None);

        assert!(store.runs_in_states(&["running"]).unwrap().is_empty());
        store.set_run_state(&r1.id, "running").unwrap();
        store.set_run_state(&r2.id, "paused").unwrap();
        let active = store.runs_in_states(&["running", "paused"]).unwrap();
        assert_eq!(active.len(), 2);
        assert_eq!(store.runs_in_states(&[]).unwrap().len(), 0);
        assert_eq!(store.runs_in_states(&["succeeded"]).unwrap().len(), 0);
    }

    #[test]
    fn configured_run_selection_survives_queue_idempotency_and_reopen() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("selection.db");
        let request_id = "model-selection-request";
        let run_id = {
            let store = Store::open(&path).unwrap();
            let project = store.add_project("demo", "/tmp/demo-models").unwrap();
            let (run, _, applied) = store
                .enqueue_run_configured(
                    &project.id,
                    "codex",
                    Some("gpt-5.6-sol"),
                    Some("high"),
                    "fix the cache",
                    Some("cargo test"),
                    None,
                    request_id,
                    &serde_json::json!({}),
                )
                .unwrap();
            assert!(applied);
            assert_eq!(run.model.as_deref(), Some("gpt-5.6-sol"));
            assert_eq!(run.reasoning_effort.as_deref(), Some("high"));

            let (same, _, applied) = store
                .enqueue_run_configured(
                    &project.id,
                    "codex",
                    Some("different-model"),
                    Some("low"),
                    "different objective",
                    None,
                    None,
                    request_id,
                    &serde_json::json!({}),
                )
                .unwrap();
            assert!(!applied);
            assert_eq!(same.id, run.id);
            assert_eq!(same.model, run.model);
            assert_eq!(same.reasoning_effort, run.reasoning_effort);
            run.id
        };

        let reopened = Store::open(&path).unwrap();
        let run = reopened.get_run(&run_id).unwrap();
        assert_eq!(run.model.as_deref(), Some("gpt-5.6-sol"));
        assert_eq!(run.reasoning_effort.as_deref(), Some("high"));
    }

    #[test]
    fn engine_sessions_upsert_and_resume_lookup() {
        let store = Store::open_in_memory().unwrap();
        let p = store.add_project("demo", "/tmp/demo").unwrap();
        let run = store.create_run(&p.id, "codex", "objective").unwrap();

        assert!(store.get_engine_session(&run.id).unwrap().is_none());
        store
            .save_engine_session(&run.id, "codex", "thread-1")
            .unwrap();
        let s = store.get_engine_session(&run.id).unwrap().unwrap();
        assert_eq!(s.session_id, "thread-1");
        assert_eq!(s.engine, "codex");

        // Upsert replaces the session id on re-handshake.
        store
            .save_engine_session(&run.id, "codex", "thread-2")
            .unwrap();
        assert_eq!(
            store
                .get_engine_session(&run.id)
                .unwrap()
                .unwrap()
                .session_id,
            "thread-2"
        );

        assert!(store.delete_engine_session(&run.id).unwrap());
        assert!(!store.delete_engine_session(&run.id).unwrap());
        assert!(store.get_engine_session(&run.id).unwrap().is_none());
    }

    #[test]
    fn app_settings_default_update_restart_and_bounds() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.db");
        {
            let store = Store::open(&path).unwrap();
            let defaults = store.app_settings().unwrap();
            assert_eq!(
                defaults.default_engine,
                autoharness_core::EngineKind::codex()
            );
            assert_eq!(
                defaults.default_route_mode,
                autoharness_protocol::params::RouteMode::Auto
            );
            assert!(defaults.automatic_history_scan);

            let (effective, applied) = store
                .update_app_settings(&autoharness_protocol::params::SettingsUpdate {
                    default_engine: Some(autoharness_core::EngineKind::claude()),
                    default_route_mode: Some(autoharness_protocol::params::RouteMode::Parallel),
                    max_parallel_workers: Some(99),
                    max_graph_nodes: Some(0),
                    default_wall_time_minutes: Some(1),
                    retention_days: Some(999),
                    automatic_history_scan: Some(false),
                    notifications_enabled: Some(false),
                    sounds_enabled: Some(false),
                    automatic_update_checks: Some(false),
                    confirm_destructive_actions: Some(false),
                    ..autoharness_protocol::params::SettingsUpdate::default()
                })
                .unwrap();
            assert!(applied);
            assert_eq!(
                effective.default_engine,
                autoharness_core::EngineKind::claude()
            );
            assert_eq!(effective.max_parallel_workers, 4);
            assert_eq!(effective.max_graph_nodes, 1);
            assert_eq!(effective.default_wall_time_minutes, 5);
            assert_eq!(effective.retention_days, 365);
            assert!(!effective.automatic_history_scan);
        }

        let reopened = Store::open(&path).unwrap().app_settings().unwrap();
        assert_eq!(
            reopened.default_engine,
            autoharness_core::EngineKind::claude()
        );
        assert_eq!(
            reopened.default_route_mode,
            autoharness_protocol::params::RouteMode::Parallel
        );
        assert!(!reopened.confirm_destructive_actions);
    }

    #[test]
    fn durable_queue_orders_deduplicates_claims_moves_cancels_and_recovers() {
        use autoharness_protocol::params::{QueueKind, QueueList, QueueState};

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("queue.db");
        let first_item_id;
        let steering_id;
        {
            let store = Store::open(&path).unwrap();
            let project = store.add_project("demo", "/tmp/queue-demo").unwrap();
            let (first_run, first, applied) = store
                .enqueue_run(
                    &project.id,
                    "codex",
                    "first",
                    Some("cargo test"),
                    None,
                    "enqueue-1",
                )
                .unwrap();
            assert!(applied);
            first_item_id = first.id.clone();
            assert_eq!(first.state, QueueState::Pending);

            let (duplicate_run, duplicate, applied) = store
                .enqueue_run(
                    &project.id,
                    "claude",
                    "must not replace",
                    None,
                    None,
                    "enqueue-1",
                )
                .unwrap();
            assert!(!applied);
            assert_eq!(duplicate_run.id, first_run.id);
            assert_eq!(duplicate.id, first.id);

            let (second_run, second, _) = store
                .enqueue_run(&project.id, "codex", "second", None, None, "enqueue-2")
                .unwrap();
            let (_, third, _) = store
                .enqueue_run(&project.id, "codex", "third", None, None, "enqueue-3")
                .unwrap();
            let listed = store
                .list_queue(&QueueList {
                    kind: Some(QueueKind::Objective),
                    ..QueueList::default()
                })
                .unwrap();
            assert_eq!(
                listed
                    .iter()
                    .map(|item| item.content.as_str())
                    .collect::<Vec<_>>(),
                vec!["first", "second", "third"]
            );

            let (moved, applied) = store
                .move_queue_item(&third.id, Some(&second.id), "move-1")
                .unwrap();
            assert!(applied);
            assert_eq!(moved.id, third.id);
            let listed = store
                .list_queue(&QueueList {
                    kind: Some(QueueKind::Objective),
                    ..QueueList::default()
                })
                .unwrap();
            assert_eq!(
                listed
                    .iter()
                    .map(|item| item.content.as_str())
                    .collect::<Vec<_>>(),
                vec!["first", "third", "second"]
            );

            let claimed = store.claim_next_objective().unwrap().unwrap();
            assert_eq!(claimed.id, first.id);
            assert_eq!(claimed.state, QueueState::Dispatching);
            assert!(
                store
                    .move_queue_item(&claimed.id, None, "move-running")
                    .is_err()
            );

            let (cancelled, applied) = store.cancel_queue_item(&second.id, "cancel-1").unwrap();
            assert!(applied);
            assert_eq!(cancelled.state, QueueState::Cancelled);
            assert_eq!(store.get_run(&second_run.id).unwrap().state, "cancelled");
            let (cancelled_again, applied) =
                store.cancel_queue_item(&second.id, "cancel-1").unwrap();
            assert!(!applied);
            assert_eq!(cancelled_again.state, QueueState::Cancelled);

            let (steering, applied) = store
                .enqueue_steering(&first_run.id, "now add tests", "steer-1")
                .unwrap();
            assert!(applied);
            steering_id = steering.id.clone();
            assert_eq!(store.list_chat(&first_run.id).unwrap().len(), 1);
            let duplicate = store
                .enqueue_steering(&first_run.id, "duplicate", "steer-1")
                .unwrap();
            assert!(!duplicate.1);
            assert_eq!(store.list_chat(&first_run.id).unwrap().len(), 1);
            assert_eq!(
                store
                    .claim_next_steering(&first_run.id)
                    .unwrap()
                    .unwrap()
                    .id,
                steering.id
            );
        }

        let reopened = Store::open(&path).unwrap();
        assert_eq!(reopened.reset_dispatching_queue().unwrap(), 2);
        assert_eq!(
            reopened.get_queue_item(&first_item_id).unwrap().state,
            QueueState::Pending
        );
        assert_eq!(
            reopened.get_queue_item(&steering_id).unwrap().state,
            QueueState::Pending
        );
        let active = reopened.list_queue(&QueueList::default()).unwrap();
        assert!(active.iter().all(|item| !matches!(
            item.state,
            QueueState::Completed | QueueState::Failed | QueueState::Cancelled
        )));
    }

    #[test]
    fn usage_summary_uses_provider_semantics_and_time_buckets() {
        let store = Store::open_in_memory().unwrap();
        let project = store.add_project("demo", "/tmp/usage").unwrap();
        let codex = store
            .create_run(&project.id, "codex", "codex work")
            .unwrap();
        let claude = store
            .create_run(&project.id, "claude", "claude work")
            .unwrap();
        store
            .append_event_at_for_tests(
                Some(&codex.id),
                "engine.usage",
                serde_json::json!({ "input_tokens": 100, "output_tokens": 20 }),
                1_775_232_100_000,
            )
            .unwrap();
        store
            .append_event_at_for_tests(
                Some(&codex.id),
                "engine.usage",
                serde_json::json!({ "input_tokens": 150, "output_tokens": 30 }),
                1_775_232_200_000,
            )
            .unwrap();
        store
            .append_event_at_for_tests(
                Some(&claude.id),
                "engine.usage",
                serde_json::json!({ "input_tokens": 100, "output_tokens": 20 }),
                1_775_232_300_000,
            )
            .unwrap();
        store
            .append_event_at_for_tests(
                Some(&claude.id),
                "engine.usage",
                serde_json::json!({ "input_tokens": 150, "output_tokens": 30 }),
                1_775_232_400_000,
            )
            .unwrap();

        let summary = store.usage_summary_at(1_775_232_500_000).unwrap();
        let codex_run = summary
            .runs
            .iter()
            .find(|run| run.run_id == codex.id)
            .unwrap();
        assert_eq!(codex_run.input_tokens, 150);
        assert_eq!(codex_run.output_tokens, 30);
        assert_eq!(codex_run.total_tokens, 180);
        assert_eq!(codex_run.event_count, 2);

        let claude_run = summary
            .runs
            .iter()
            .find(|run| run.run_id == claude.id)
            .unwrap();
        assert_eq!(claude_run.input_tokens, 250);
        assert_eq!(claude_run.output_tokens, 50);
        assert_eq!(claude_run.total_tokens, 300);
        assert_eq!(claude_run.event_count, 2);

        let codex_provider = summary
            .providers
            .iter()
            .find(|provider| provider.provider == "codex")
            .unwrap();
        assert_eq!(codex_provider.today.input_tokens, 150);
        assert_eq!(codex_provider.month.total_tokens, 180);
        assert_eq!(codex_provider.all_time.run_count, 1);
        let claude_provider = summary
            .providers
            .iter()
            .find(|provider| provider.provider == "claude")
            .unwrap();
        assert_eq!(claude_provider.today.input_tokens, 250);
        assert_eq!(claude_provider.month.total_tokens, 300);
        assert_eq!(claude_provider.all_time.run_count, 1);
    }

    // ---- Phase 8: verified memory and policy versions ----

    #[test]
    fn memory_facts_require_evidence_before_they_can_be_injected() {
        let store = Store::open_in_memory().unwrap();
        let project = store.add_project("demo", "/tmp/mem").unwrap();

        let mut fact = autoharness_core::MemoryFact {
            id: autoharness_core::new_id(),
            project_id: project.id.clone(),
            kind: autoharness_core::MemoryFactKind::VerifiedCommand,
            statement: "the test suite is run with cargo nextest".into(),
            evidence: vec![],
            confidence: 0.9,
            supersedes: None,
            verified: true,
            created_at_ms: 1,
        };
        // A verified fact with nothing backing it is a guess.
        assert!(store.add_memory_fact(&fact).is_err());

        // As a proposal it is fine to store, just never injected.
        fact.verified = false;
        store.add_memory_fact(&fact).unwrap();
        assert_eq!(store.list_memory_facts(&project.id, true).unwrap().len(), 0);
        assert_eq!(
            store.list_memory_facts(&project.id, false).unwrap().len(),
            1
        );

        // Promoting it still requires evidence.
        assert!(store.verify_memory_fact(&fact.id).is_err());
    }

    #[test]
    fn verified_facts_are_searchable_and_superseded_ones_are_not() {
        let store = Store::open_in_memory().unwrap();
        let project = store.add_project("demo", "/tmp/mem2").unwrap();
        let make = |statement: &str, supersedes: Option<String>| autoharness_core::MemoryFact {
            id: autoharness_core::new_id(),
            project_id: project.id.clone(),
            kind: autoharness_core::MemoryFactKind::Convention,
            statement: statement.into(),
            evidence: vec!["event:42".into()],
            confidence: 0.9,
            supersedes,
            verified: true,
            created_at_ms: 1,
        };

        let old = make("migrations are applied with diesel", None);
        store.add_memory_fact(&old).unwrap();
        assert_eq!(
            store
                .search_memory(&project.id, "migrations", 10)
                .unwrap()
                .len(),
            1
        );

        // The project changed: the new fact supersedes the old one.
        let new = make("migrations are applied with sqlx", Some(old.id.clone()));
        store.add_memory_fact(&new).unwrap();
        let hits = store.search_memory(&project.id, "migrations", 10).unwrap();
        assert_eq!(hits.len(), 1, "a superseded fact must not resurface");
        assert!(hits[0].statement.contains("sqlx"));

        // Forgetting the current fact must NOT resurrect the stale one.
        assert!(store.forget_memory_fact(&new.id).unwrap());
        assert!(
            store
                .search_memory(&project.id, "migrations", 10)
                .unwrap()
                .is_empty(),
            "forgetting a fact must not bring back what it superseded"
        );
    }

    #[test]
    fn memory_search_survives_punctuation_and_fts_syntax() {
        let store = Store::open_in_memory().unwrap();
        let project = store.add_project("demo", "/tmp/mem3").unwrap();
        store
            .add_memory_fact(&autoharness_core::MemoryFact {
                id: autoharness_core::new_id(),
                project_id: project.id.clone(),
                kind: autoharness_core::MemoryFactKind::Tooling,
                statement: "the daemon socket lives under Application Support".into(),
                evidence: vec!["event:1".into()],
                confidence: 1.0,
                supersedes: None,
                verified: true,
                created_at_ms: 1,
            })
            .unwrap();

        // A raw FTS5 operator or stray quote must not become a syntax error.
        for query in ["socket", "\"socket", "socket NEAR daemon", "socket*", "()"] {
            let result = store.search_memory(&project.id, query, 10);
            assert!(result.is_ok(), "query {query:?} errored: {result:?}");
        }
        assert_eq!(
            store
                .search_memory(&project.id, "socket", 10)
                .unwrap()
                .len(),
            1
        );
        assert!(store.search_memory(&project.id, "", 10).unwrap().is_empty());
    }

    #[test]
    fn policy_promotion_is_exclusive_and_rollback_is_exact() {
        let store = Store::open_in_memory().unwrap();
        assert!(store.promoted_policy().unwrap().is_none());

        let v1 = store
            .add_policy_candidate(
                &serde_json::json!({ "detector_thresholds": { "repeated_action": 3 } }),
            )
            .unwrap();
        let v2 = store
            .add_policy_candidate(
                &serde_json::json!({ "detector_thresholds": { "repeated_action": 5 } }),
            )
            .unwrap();
        assert_eq!((v1, v2), (1, 2));
        // Candidates are never promoted on arrival.
        assert!(store.promoted_policy().unwrap().is_none());

        assert!(store.promote_policy(v2).unwrap());
        let (version, data) = store.promoted_policy().unwrap().unwrap();
        assert_eq!(version, 2);
        assert_eq!(data["detector_thresholds"]["repeated_action"], 5);

        // Rollback is promoting the older version, and it is exact.
        assert!(store.promote_policy(v1).unwrap());
        let (version, data) = store.promoted_policy().unwrap().unwrap();
        assert_eq!(version, 1);
        assert_eq!(data["detector_thresholds"]["repeated_action"], 3);
        // Exclusive: exactly one promoted row.
        let promoted = store.list_policies().unwrap();
        assert_eq!(promoted.iter().filter(|(_, p, _)| *p).count(), 1);

        assert!(!store.promote_policy(99).unwrap());
    }

    // ---- Phase 9: integrity, export, privacy ----

    #[test]
    fn a_healthy_database_reports_healthy() {
        let store = Store::open_in_memory().unwrap();
        let project = store.add_project("demo", "/tmp/health").unwrap();
        let run = store.create_run(&project.id, "codex", "objective").unwrap();
        store
            .append_event(Some(&run.id), "run.created", serde_json::json!({}))
            .unwrap();

        let report = store.integrity_report().unwrap();
        assert!(report.healthy, "{report:?}");
        assert_eq!(report.sqlite, "ok");
        assert_eq!(report.orphan_events, 0);
        assert!(report.unknown_run_states.is_empty());
    }

    /// The report exists to CATCH damage, so it must actually fail on damage.
    #[test]
    fn integrity_reports_orphan_events_and_undefined_run_states() {
        let store = Store::open_in_memory().unwrap();
        let project = store.add_project("demo", "/tmp/damage").unwrap();
        let run = store.create_run(&project.id, "codex", "objective").unwrap();
        store
            .append_event(Some("ghost-run"), "tick", serde_json::json!({}))
            .unwrap();
        store.set_run_state(&run.id, "vibing").unwrap();

        let report = store.integrity_report().unwrap();
        assert!(!report.healthy);
        assert_eq!(report.orphan_events, 1);
        assert_eq!(report.unknown_run_states, vec!["vibing".to_string()]);
    }

    #[test]
    fn export_carries_every_table_the_user_owns() {
        let store = Store::open_in_memory().unwrap();
        let project = store.add_project("demo", "/tmp/export").unwrap();
        let run = store.create_run(&project.id, "codex", "objective").unwrap();
        store.append_chat(&run.id, "user", "hello").unwrap();
        store
            .append_event(Some(&run.id), "run.created", serde_json::json!({"a": 1}))
            .unwrap();

        let export = store.export_json().unwrap();
        for table in [
            "projects",
            "runs",
            "chat_messages",
            "events",
            "graph_versions",
            "node_attempts",
            "artifacts",
            "checkpoints",
            "memory_facts",
            "policy_versions",
        ] {
            assert!(export.get(table).is_some(), "{table} missing from export");
        }
        assert_eq!(export["projects"].as_array().unwrap().len(), 1);
        assert_eq!(export["chat_messages"][0]["content"], "hello");
        assert_eq!(export["runs"][0]["objective"], "objective");
    }

    /// A privacy control that leaves residue is not one.
    #[test]
    fn purging_a_project_leaves_nothing_behind() {
        let store = Store::open_in_memory().unwrap();
        let keep = store.add_project("keep", "/tmp/keep").unwrap();
        let purge = store.add_project("purge", "/tmp/purge").unwrap();

        let kept_run = store.create_run(&keep.id, "codex", "keep me").unwrap();
        store
            .append_event(Some(&kept_run.id), "tick", serde_json::json!({}))
            .unwrap();

        let doomed = store.create_run(&purge.id, "codex", "forget me").unwrap();
        store
            .append_event(Some(&doomed.id), "tick", serde_json::json!({}))
            .unwrap();
        store.append_chat(&doomed.id, "user", "secret").unwrap();
        store
            .save_engine_session(&doomed.id, "codex", "sess-1")
            .unwrap();
        store
            .create_checkpoint(&doomed.id, None, serde_json::json!({"s": 1}))
            .unwrap();
        store
            .add_memory_fact(&autoharness_core::MemoryFact {
                id: autoharness_core::new_id(),
                project_id: purge.id.clone(),
                kind: autoharness_core::MemoryFactKind::Convention,
                statement: "a private convention".into(),
                evidence: vec!["event:1".into()],
                confidence: 1.0,
                supersedes: None,
                verified: true,
                created_at_ms: 1,
            })
            .unwrap();

        let report = store.purge_project(&purge.id).unwrap();
        assert!(report.project_removed);
        assert_eq!(report.runs, 1);
        assert_eq!(report.events, 1);
        assert_eq!(report.memory_facts, 1);

        // Nothing of the purged project survives...
        assert!(store.get_run(&doomed.id).is_err());
        assert!(store.list_chat(&doomed.id).unwrap().is_empty());
        assert!(store.get_engine_session(&doomed.id).unwrap().is_none());
        assert!(store.latest_checkpoint(&doomed.id).unwrap().is_none());
        assert!(
            store
                .list_memory_facts(&purge.id, false)
                .unwrap()
                .is_empty()
        );
        assert!(store.events_since(0, Some(&doomed.id)).unwrap().is_empty());

        // ...and the other project is untouched.
        assert!(store.get_run(&kept_run.id).is_ok());
        assert_eq!(store.events_since(0, Some(&kept_run.id)).unwrap().len(), 1);
        assert_eq!(store.list_projects().unwrap().len(), 1);
        // The database is still coherent afterwards.
        assert!(store.integrity_report().unwrap().healthy);
        store.vacuum().unwrap();
    }

    #[test]
    fn purging_a_project_erases_its_indexed_provider_history_only() {
        let temp = tempfile::tempdir().unwrap();
        let purge_path = temp.path().join("private-project");
        let purge_child = purge_path.join("nested-worktree");
        let keep_path = temp.path().join("private-project-sibling");
        std::fs::create_dir_all(&purge_child).unwrap();
        std::fs::create_dir_all(&keep_path).unwrap();
        let store = Store::open_in_memory().unwrap();
        let purge = store
            .add_project("purge", &purge_path.to_string_lossy())
            .unwrap();
        let keep = store
            .add_project("keep", &keep_path.to_string_lossy())
            .unwrap();
        let doomed_run = store.create_run(&purge.id, "codex", "private").unwrap();

        let entry = |source_id: &str, cwd: &std::path::Path, prompt: &str| ExternalHistoryEntry {
            provider: "codex".into(),
            source_id: source_id.into(),
            transcript_path: format!("/provider/private/{source_id}.jsonl"),
            cwd: Some(cwd.to_string_lossy().into_owned()),
            title: Some(format!("title {source_id} {prompt}")),
            first_prompt: Some(prompt.into()),
            updated_at_ms: 10,
            last_seen_ms: 10,
            missing: false,
            diagnostic: None,
            adopted_run_id: None,
        };
        store
            .upsert_external_history(&entry("exact", &purge_path, "exact secret"))
            .unwrap();
        store
            .upsert_external_history(&entry("child", &purge_child, "child secret"))
            .unwrap();
        store
            .upsert_external_history(&entry("keep", &keep_path, "keep prompt"))
            .unwrap();
        let mut adopted = entry("adopted", &keep_path, "adopted secret");
        adopted.adopted_run_id = Some(doomed_run.id.clone());
        store.upsert_external_history(&adopted).unwrap();

        store.purge_project(&purge.id).unwrap();

        for source_id in ["exact", "child", "adopted"] {
            assert!(
                store
                    .get_external_history("codex", source_id)
                    .unwrap()
                    .is_none(),
                "{source_id} survived project purge"
            );
        }
        assert!(
            store
                .get_external_history("codex", "keep")
                .unwrap()
                .is_some()
        );
        assert!(store.get_project(&keep.id).is_ok());

        let exported = store.export_json().unwrap();
        assert!(
            exported["external_history"]
                .as_array()
                .unwrap()
                .iter()
                .all(|row| row["cwd"]
                    .as_str()
                    .is_none_or(|cwd| !std::path::Path::new(cwd).starts_with(&purge_path)))
        );
        let export = serde_json::to_string(&exported).unwrap();
        for private_value in [
            "exact secret",
            "child secret",
            "adopted secret",
            "title exact exact secret",
            "title child child secret",
            "title adopted adopted secret",
            "/provider/private/exact.jsonl",
            "/provider/private/child.jsonl",
            "/provider/private/adopted.jsonl",
        ] {
            assert!(
                !export.contains(private_value),
                "purged provider metadata leaked through export: {private_value}"
            );
        }
        assert!(export.contains("keep prompt"));
    }

    #[test]
    fn external_history_keyset_pagination_is_stable_across_equal_timestamps() {
        let store = Store::open_in_memory().unwrap();
        for (provider, source_id, cwd, prompt) in [
            ("codex", "b", "/tmp/project-a", "needle beta"),
            ("claude", "a", "/tmp/project-a", "needle alpha"),
            ("codex", "a", "/tmp/project-b", "other"),
            ("codex", "c", "/tmp/project-a", "needle gamma"),
            ("claude", "z", "/tmp/project-a", "needle omega"),
        ] {
            store
                .upsert_external_history(&ExternalHistoryEntry {
                    provider: provider.into(),
                    source_id: source_id.into(),
                    transcript_path: format!("/history/{provider}-{source_id}.jsonl"),
                    cwd: Some(cwd.into()),
                    title: None,
                    first_prompt: Some(prompt.into()),
                    updated_at_ms: 100,
                    last_seen_ms: 100,
                    missing: false,
                    diagnostic: None,
                    adopted_run_id: None,
                })
                .unwrap();
        }

        let first = store
            .list_external_history(&ExternalHistoryFilter {
                limit: 2,
                ..ExternalHistoryFilter::default()
            })
            .unwrap();
        let second = store
            .list_external_history(&ExternalHistoryFilter {
                cursor: first.next_cursor.clone(),
                limit: 2,
                ..ExternalHistoryFilter::default()
            })
            .unwrap();
        let third = store
            .list_external_history(&ExternalHistoryFilter {
                cursor: second.next_cursor.clone(),
                limit: 2,
                ..ExternalHistoryFilter::default()
            })
            .unwrap();
        let keys = first
            .entries
            .iter()
            .chain(&second.entries)
            .chain(&third.entries)
            .map(|entry| (entry.provider.as_str(), entry.source_id.as_str()))
            .collect::<Vec<_>>();
        assert_eq!(
            keys,
            vec![
                ("claude", "a"),
                ("claude", "z"),
                ("codex", "a"),
                ("codex", "b"),
                ("codex", "c"),
            ]
        );
        assert_eq!(first.entries.len(), 2);
        assert_eq!(second.entries.len(), 2);
        assert_eq!(third.entries.len(), 1);
        assert!(third.next_cursor.is_none());

        let filtered = store
            .list_external_history(&ExternalHistoryFilter {
                project_path: Some("/tmp/project-a".into()),
                query: Some("needle".into()),
                limit: 2,
                ..ExternalHistoryFilter::default()
            })
            .unwrap();
        assert_eq!(filtered.entries.len(), 2);
        assert!(filtered.next_cursor.is_some());
        assert!(filtered.entries.iter().all(|entry| {
            entry.cwd.as_deref() == Some("/tmp/project-a")
                && entry.first_prompt.as_deref().unwrap().contains("needle")
        }));
    }

    #[test]
    fn adopting_external_history_atomically_creates_exactly_one_run() {
        let store = std::sync::Arc::new(Store::open_in_memory().unwrap());
        let project = store.add_project("repo", "/tmp/repo").unwrap();
        store
            .upsert_external_history(&ExternalHistoryEntry {
                provider: "codex".into(),
                source_id: "source".into(),
                transcript_path: "/history/source.jsonl".into(),
                cwd: Some("/tmp/repo".into()),
                title: Some("History source".into()),
                first_prompt: Some("continue".into()),
                updated_at_ms: 100,
                last_seen_ms: 100,
                missing: false,
                diagnostic: None,
                adopted_run_id: None,
            })
            .unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
        let mut threads = Vec::new();
        for _ in 0..2 {
            let store = store.clone();
            let barrier = barrier.clone();
            let project_id = project.id.clone();
            threads.push(std::thread::spawn(move || {
                barrier.wait();
                store
                    .adopt_external_history(
                        "codex",
                        "source",
                        &project_id,
                        "codex",
                        "bounded handoff",
                    )
                    .unwrap()
            }));
        }
        barrier.wait();
        let outcomes = threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect::<Vec<_>>();

        let created = outcomes
            .iter()
            .filter(|outcome| matches!(outcome, ExternalHistoryAdoption::Created(_)))
            .count();
        let run_ids = outcomes
            .iter()
            .map(|outcome| match outcome {
                ExternalHistoryAdoption::Created(run) => run.id.as_str(),
                ExternalHistoryAdoption::AlreadyAdopted { run_id } => run_id.as_str(),
            })
            .collect::<Vec<_>>();
        assert_eq!(created, 1);
        assert_eq!(run_ids[0], run_ids[1]);
        assert_eq!(store.runs_in_states(&["draft"]).unwrap().len(), 1);
        assert_eq!(
            store
                .get_external_history("codex", "source")
                .unwrap()
                .unwrap()
                .adopted_run_id
                .as_deref(),
            Some(run_ids[0])
        );
    }

    #[test]
    fn external_history_upsert_filter_pagination_and_mark_adopted() {
        let store = Store::open_in_memory().unwrap();
        let project = store.add_project("repo", "/tmp/repo").unwrap();

        let codex_old = ExternalHistoryEntry {
            provider: "codex".into(),
            source_id: "codex-old".into(),
            transcript_path: "/hist/codex-old.jsonl".into(),
            cwd: Some("/tmp/repo".into()),
            title: Some("Old Codex".into()),
            first_prompt: Some("first prompt".into()),
            updated_at_ms: 10,
            last_seen_ms: 100,
            missing: false,
            diagnostic: None,
            adopted_run_id: None,
        };
        let mut codex_new = codex_old.clone();
        codex_new.source_id = "codex-new".into();
        codex_new.transcript_path = "/hist/codex-new.jsonl".into();
        codex_new.title = Some("New Codex".into());
        codex_new.updated_at_ms = 30;
        let claude = ExternalHistoryEntry {
            provider: "claude".into(),
            source_id: "claude-one".into(),
            transcript_path: "/hist/claude-one.jsonl".into(),
            cwd: Some("/tmp/other".into()),
            title: Some("Claude work".into()),
            first_prompt: None,
            updated_at_ms: 20,
            last_seen_ms: 100,
            missing: false,
            diagnostic: Some("bounded warning".into()),
            adopted_run_id: None,
        };

        store.upsert_external_history(&codex_old).unwrap();
        store.upsert_external_history(&codex_new).unwrap();
        store.upsert_external_history(&claude).unwrap();

        let first = store
            .list_external_history(&ExternalHistoryFilter {
                provider: None,
                project_path: None,
                query: None,
                cursor: None,
                limit: 2,
            })
            .unwrap();
        assert_eq!(
            first
                .entries
                .iter()
                .map(|e| e.source_id.as_str())
                .collect::<Vec<_>>(),
            vec!["codex-new", "claude-one"]
        );
        assert!(first.next_cursor.is_some());

        let second = store
            .list_external_history(&ExternalHistoryFilter {
                cursor: first.next_cursor,
                limit: 2,
                ..ExternalHistoryFilter::default()
            })
            .unwrap();
        assert_eq!(second.entries[0].source_id, "codex-old");
        assert!(second.next_cursor.is_none());

        let codex_only = store
            .list_external_history(&ExternalHistoryFilter {
                provider: Some("codex".into()),
                limit: 10,
                ..ExternalHistoryFilter::default()
            })
            .unwrap();
        assert_eq!(codex_only.entries.len(), 2);

        let current_project = store
            .list_external_history(&ExternalHistoryFilter {
                project_path: Some(project.path.clone()),
                limit: 10,
                ..ExternalHistoryFilter::default()
            })
            .unwrap();
        assert_eq!(current_project.entries.len(), 2);

        let searched = store
            .list_external_history(&ExternalHistoryFilter {
                query: Some("claude".into()),
                limit: 10,
                ..ExternalHistoryFilter::default()
            })
            .unwrap();
        assert_eq!(searched.entries[0].source_id, "claude-one");

        let run = store.create_run(&project.id, "codex", "adopted").unwrap();
        store
            .mark_external_history_adopted("codex", "codex-new", &run.id)
            .unwrap();
        let adopted = store
            .get_external_history("codex", "codex-new")
            .unwrap()
            .unwrap();
        assert_eq!(adopted.adopted_run_id.as_deref(), Some(run.id.as_str()));
        assert_eq!(
            store
                .mark_external_history_missing_except_seen("codex", 200)
                .unwrap(),
            2
        );
        assert!(
            store
                .get_external_history("codex", "codex-new")
                .unwrap()
                .unwrap()
                .missing
        );
    }

    #[test]
    fn latest_node_attempt_and_next_attempt_preserve_retry_history() {
        let store = Store::open_in_memory().unwrap();
        let project = store.add_project("demo", "/tmp/nodes").unwrap();
        let run = store.create_run(&project.id, "codex", "graph").unwrap();
        store
            .record_node_attempt(&run.id, "edit", 1, "running", None)
            .unwrap();
        store
            .record_node_attempt(&run.id, "edit", 1, "failed", Some("red"))
            .unwrap();
        assert_eq!(store.next_node_attempt(&run.id, "edit").unwrap(), 2);
        let latest = store.latest_node_attempt(&run.id, "edit").unwrap().unwrap();
        assert_eq!(latest.attempt, 1);
        assert_eq!(latest.state, "failed");
        assert_eq!(latest.detail.as_deref(), Some("red"));

        store
            .record_node_attempt(&run.id, "edit", 2, "running", None)
            .unwrap();
        assert_eq!(
            store
                .latest_node_attempt(&run.id, "edit")
                .unwrap()
                .unwrap()
                .state,
            "running"
        );
    }
}

#[cfg(test)]
mod execution_default_tests {
    use super::*;

    /// A model choice is remembered per engine.
    ///
    /// Only `default_engine` persisted before, so someone who always wants one
    /// model re-picked it after every engine switch and every time they opened
    /// an old run. A preference the app forgets is not a preference.
    #[test]
    fn a_model_choice_persists_per_engine() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("s.db")).unwrap();

        let update = autoharness_protocol::params::SettingsUpdate {
            default_model: Some(("claude".into(), "opus".into())),
            default_reasoning_effort: Some(("claude".into(), "xhigh".into())),
            ..Default::default()
        };
        let (settings, applied) = store.update_app_settings(&update).unwrap();
        assert!(applied);
        assert_eq!(settings.default_models.get("claude").unwrap(), "opus");
        assert_eq!(
            settings.default_reasoning_efforts.get("claude").unwrap(),
            "xhigh"
        );

        // Another engine keeps its own: a model id means nothing to a
        // different provider.
        let (settings, _) = store
            .update_app_settings(&autoharness_protocol::params::SettingsUpdate {
                default_model: Some(("codex".into(), "gpt-5.6-sol".into())),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(settings.default_models.get("claude").unwrap(), "opus");
        assert_eq!(settings.default_models.get("codex").unwrap(), "gpt-5.6-sol");

        // It survives a reopen, which is the whole point.
        let reopened = Store::open(dir.path().join("s.db")).unwrap();
        let settings = reopened.app_settings().unwrap();
        assert_eq!(settings.default_models.get("claude").unwrap(), "opus");

        // An empty value clears rather than storing an empty model id, so
        // "use the provider default" is expressible.
        let (settings, _) = reopened
            .update_app_settings(&autoharness_protocol::params::SettingsUpdate {
                default_model: Some(("claude".into(), String::new())),
                ..Default::default()
            })
            .unwrap();
        assert!(!settings.default_models.contains_key("claude"));
    }
}
