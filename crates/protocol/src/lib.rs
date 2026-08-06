//! Versioned JSON-RPC 2.0 envelopes and length-delimited framing.
//!
//! Wire format: each frame is a 4-byte big-endian length prefix followed by a
//! JSON document. Three envelope kinds exist: [`Request`], [`Response`], and
//! [`Event`] (a server-to-client notification).

use std::collections::{HashMap, VecDeque};

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Current wire protocol version. Bumped on breaking envelope changes.
pub const PROTOCOL_VERSION: u32 = 1;

/// Hard cap on a single frame (8 MiB) to bound memory on malformed peers.
pub const MAX_FRAME_BYTES: u32 = 8 * 1024 * 1024;

pub const JSONRPC: &str = "2.0";

/// Minimum RPC method set (PLAN.md). `auth.hello` is the connection handshake.
pub mod methods {
    pub const AUTH_HELLO: &str = "auth.hello";

    pub const PROJECT_ADD: &str = "project.add";
    /// Create a brand-new repository in the daemon's managed folder and
    /// register it as a project, so prompting with nothing chosen can still
    /// go somewhere visible instead of being refused.
    pub const PROJECT_CREATE: &str = "project.create";
    pub const PROJECT_LIST: &str = "project.list";
    pub const PROJECT_REMOVE: &str = "project.remove";

    pub const RUN_CREATE: &str = "run.create";
    /// Give a draft run its objective. Draft-only: a run that has already
    /// started has told an engine what to do, and rewriting the record after
    /// the fact would make the ledger disagree with what was actually asked.
    pub const RUN_SET_OBJECTIVE: &str = "run.set_objective";
    pub const RUN_START: &str = "run.start";
    pub const RUN_APPROVE: &str = "run.approve";
    pub const RUN_PAUSE: &str = "run.pause";
    pub const RUN_RESUME: &str = "run.resume";
    pub const RUN_CANCEL: &str = "run.cancel";
    pub const RUN_GET: &str = "run.get";
    pub const RUN_ENQUEUE: &str = "run.enqueue";
    /// Answer one objective several ways at once and compare the results.
    pub const RUN_ATTEMPTS: &str = "run.attempts";

    pub const QUEUE_LIST: &str = "queue.list";
    pub const QUEUE_MOVE: &str = "queue.move";
    pub const QUEUE_CANCEL: &str = "queue.cancel";

    /// Engine detection across registered adapters (setup diagnostics).
    pub const ENGINE_LIST: &str = "engine.list";

    pub const CHAT_SEND: &str = "chat.send";
    pub const CHAT_INTERRUPT: &str = "chat.interrupt";
    /// Answer a question the running engine asked. Distinct from `run.approve`,
    /// which approves a compiled GRAPH before it executes; this answers the
    /// agent itself, mid-run.
    pub const RUN_ANSWER: &str = "run.answer";

    pub const NODE_RETRY: &str = "node.retry";
    pub const NODE_CANCEL: &str = "node.cancel";

    pub const EVENTS_SUBSCRIBE: &str = "events.subscribe";

    /// Re-run sandbox canaries and return structured diagnostics.
    pub const SANDBOX_CHECK: &str = "sandbox.check";

    /// One bundle of everything a user needs when something is wrong:
    /// versions, sandbox, engines, database health, storage paths.
    pub const APP_DIAGNOSTICS: &str = "app.diagnostics";
    /// Export the whole database as JSON. The user's data is theirs.
    pub const APP_EXPORT: &str = "app.export";
    /// Erase everything belonging to one project, irreversibly.
    pub const APP_PURGE_PROJECT: &str = "app.purge_project";

    pub const POLICY_LIST: &str = "policy.list";
    pub const POLICY_PROMOTE: &str = "policy.promote";
    pub const POLICY_ROLLBACK: &str = "policy.rollback";

    pub const MEMORY_LIST: &str = "memory.list";
    pub const MEMORY_FORGET: &str = "memory.forget";

    pub const HISTORY_LIST: &str = "history.list";
    pub const HISTORY_ADOPT: &str = "history.adopt";

    pub const SETTINGS_GET: &str = "settings.get";
    pub const SETTINGS_UPDATE: &str = "settings.update";
    pub const USAGE_SUMMARY: &str = "usage.summary";

    pub const WORKTREE_LIST: &str = "worktree.list";
    pub const WORKTREE_RECLAIM: &str = "worktree.reclaim";

    pub const ARTIFACT_LIST: &str = "artifact.list";

    /// The complete public method set handled by the daemon. Unknown methods
    /// fail with a structured error instead of reaching an implicit fallback.
    pub const IMPLEMENTED: &[&str] = &[
        PROJECT_ADD,
        PROJECT_CREATE,
        PROJECT_LIST,
        PROJECT_REMOVE,
        RUN_CREATE,
        RUN_SET_OBJECTIVE,
        RUN_START,
        RUN_PAUSE,
        RUN_RESUME,
        RUN_CANCEL,
        RUN_GET,
        RUN_ENQUEUE,
        RUN_ATTEMPTS,
        QUEUE_LIST,
        QUEUE_MOVE,
        QUEUE_CANCEL,
        RUN_APPROVE,
        ENGINE_LIST,
        POLICY_LIST,
        POLICY_PROMOTE,
        POLICY_ROLLBACK,
        MEMORY_LIST,
        MEMORY_FORGET,
        HISTORY_LIST,
        HISTORY_ADOPT,
        SETTINGS_GET,
        SETTINGS_UPDATE,
        USAGE_SUMMARY,
        WORKTREE_LIST,
        WORKTREE_RECLAIM,
        ARTIFACT_LIST,
        CHAT_SEND,
        CHAT_INTERRUPT,
        RUN_ANSWER,
        NODE_RETRY,
        NODE_CANCEL,
        EVENTS_SUBSCRIBE,
        SANDBOX_CHECK,
        APP_DIAGNOSTICS,
        APP_EXPORT,
        APP_PURGE_PROJECT,
    ];

    pub fn is_known(method: &str) -> bool {
        const ALL: &[&str] = &[
            AUTH_HELLO,
            PROJECT_ADD,
            PROJECT_LIST,
            PROJECT_REMOVE,
            RUN_CREATE,
            RUN_SET_OBJECTIVE,
            RUN_START,
            RUN_APPROVE,
            RUN_PAUSE,
            RUN_RESUME,
            RUN_CANCEL,
            RUN_GET,
            RUN_ENQUEUE,
            RUN_ATTEMPTS,
            QUEUE_LIST,
            QUEUE_MOVE,
            QUEUE_CANCEL,
            ENGINE_LIST,
            CHAT_SEND,
            CHAT_INTERRUPT,
            RUN_ANSWER,
            NODE_RETRY,
            NODE_CANCEL,
            EVENTS_SUBSCRIBE,
            SANDBOX_CHECK,
            POLICY_LIST,
            POLICY_PROMOTE,
            POLICY_ROLLBACK,
            MEMORY_LIST,
            MEMORY_FORGET,
            HISTORY_LIST,
            HISTORY_ADOPT,
            SETTINGS_GET,
            SETTINGS_UPDATE,
            USAGE_SUMMARY,
            WORKTREE_LIST,
            WORKTREE_RECLAIM,
            ARTIFACT_LIST,
            APP_DIAGNOSTICS,
            APP_EXPORT,
            APP_PURGE_PROJECT,
        ];
        ALL.contains(&method)
    }
}

/// Error codes in the application range (-32099..=-32000) plus JSON-RPC's own.
pub mod codes {
    pub const PARSE_ERROR: i64 = -32700;
    pub const INVALID_REQUEST: i64 = -32600;
    pub const METHOD_NOT_FOUND: i64 = -32601;
    pub const INVALID_PARAMS: i64 = -32602;
    pub const INTERNAL: i64 = -32603;

    pub const UNAUTHORIZED: i64 = -32001;
    pub const DUPLICATE_REQUEST: i64 = -32002;
    pub const NOT_IMPLEMENTED: i64 = -32010;
}

#[derive(Debug, Error)]
pub enum ProtocolError {
    #[error("frame length {0} exceeds maximum {MAX_FRAME_BYTES}")]
    FrameTooLarge(u32),
    #[error("malformed frame: {0}")]
    Malformed(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}

/// Client request envelope.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
    pub jsonrpc: String,
    pub protocol_version: u32,
    /// Client-chosen request ID; used for idempotency/duplicate detection.
    pub id: String,
    pub method: String,
    #[serde(default)]
    pub params: serde_json::Value,
}

impl Request {
    pub fn new(
        id: impl Into<String>,
        method: impl Into<String>,
        params: serde_json::Value,
    ) -> Self {
        Self {
            jsonrpc: JSONRPC.into(),
            protocol_version: PROTOCOL_VERSION,
            id: id.into(),
            method: method.into(),
            params,
        }
    }
}

/// Server response envelope: exactly one of `result`/`error` is set.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Response {
    pub jsonrpc: String,
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

impl Response {
    pub fn ok(id: impl Into<String>, result: serde_json::Value) -> Self {
        Self {
            jsonrpc: JSONRPC.into(),
            id: id.into(),
            result: Some(result),
            error: None,
        }
    }

    pub fn err(id: impl Into<String>, code: i64, message: impl Into<String>) -> Self {
        Self::err_with_data(id, code, message, None)
    }

    pub fn err_with_data(
        id: impl Into<String>,
        code: i64,
        message: impl Into<String>,
        data: Option<serde_json::Value>,
    ) -> Self {
        Self {
            jsonrpc: JSONRPC.into(),
            id: id.into(),
            result: None,
            error: Some(RpcError {
                code,
                message: message.into(),
                data,
            }),
        }
    }

    pub fn not_implemented(id: impl Into<String>, method: &str) -> Self {
        Self::err_with_data(
            id,
            codes::NOT_IMPLEMENTED,
            format!("method '{method}' is not yet implemented"),
            Some(serde_json::json!({ "method": method, "phase": 1 })),
        )
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

/// Server-to-client event notification. Sequence is monotonic per the
/// daemon's append-only ledger; clients resume with `since_sequence`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub jsonrpc: String,
    pub protocol_version: u32,
    /// Monotonic sequence assigned by the store before broadcast.
    pub sequence: u64,
    /// Run this event belongs to, if any.
    pub run_id: Option<String>,
    /// Per-run sequence, 0 for run-less events.
    pub run_sequence: u64,
    pub timestamp_ms: i64,
    #[serde(rename = "type")]
    pub kind: String,
    pub payload: serde_json::Value,
}

impl Event {
    pub fn new(
        sequence: u64,
        run_id: Option<String>,
        run_sequence: u64,
        timestamp_ms: i64,
        kind: impl Into<String>,
        payload: serde_json::Value,
    ) -> Self {
        Self {
            jsonrpc: JSONRPC.into(),
            protocol_version: PROTOCOL_VERSION,
            sequence,
            run_id,
            run_sequence,
            timestamp_ms,
            kind: kind.into(),
            payload,
        }
    }
}

/// Any frame that can arrive on the wire.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Frame {
    Request(Request),
    Response(Response),
    Event(Event),
}

/// Typed params for the Phase-1 implemented methods.
pub mod params {
    use serde::{Deserialize, Serialize};

    pub const MIN_PARALLEL_WORKERS: u32 = 1;
    pub const MAX_PARALLEL_WORKERS: u32 = 4;
    pub const MIN_ACTIVE_RUNS: u32 = 1;
    pub const MAX_ACTIVE_RUNS: u32 = 8;
    pub const MIN_GRAPH_NODES: u32 = 1;
    pub const MAX_GRAPH_NODES: u32 = 8;
    pub const MIN_WALL_TIME_MINUTES: u32 = 5;
    pub const MAX_WALL_TIME_MINUTES: u32 = 120;
    pub const MIN_RETENTION_DAYS: u32 = 1;
    pub const MAX_RETENTION_DAYS: u32 = 365;

    const fn default_max_active_runs() -> u32 {
        2
    }

    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(rename_all = "snake_case")]
    pub enum RouteMode {
        #[default]
        Auto,
        Priority,
        Parallel,
    }

    impl RouteMode {
        pub fn as_str(self) -> &'static str {
            match self {
                RouteMode::Auto => "auto",
                RouteMode::Priority => "priority",
                RouteMode::Parallel => "parallel",
            }
        }
    }

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    pub struct AppSettings {
        pub version: u32,
        pub default_engine: autoharness_core::EngineKind,
        /// The model a new run starts on, per engine.
        ///
        /// Only `default_engine` persisted before, so a user who always wants
        /// one model re-picked it after every engine switch and every time
        /// they opened an old run. Keyed by engine because a model id means
        /// nothing to another provider.
        #[serde(default)]
        pub default_models: std::collections::BTreeMap<String, String>,
        /// Reasoning effort per engine, same reasoning.
        #[serde(default)]
        pub default_reasoning_efforts: std::collections::BTreeMap<String, String>,
        pub default_route_mode: RouteMode,
        #[serde(default = "default_max_active_runs")]
        pub max_active_runs: u32,
        pub max_parallel_workers: u32,
        pub max_graph_nodes: u32,
        pub default_wall_time_minutes: u32,
        pub retention_days: u32,
        pub automatic_history_scan: bool,
        pub notifications_enabled: bool,
        pub sounds_enabled: bool,
        pub automatic_update_checks: bool,
        pub confirm_destructive_actions: bool,
    }

    impl Default for AppSettings {
        fn default() -> Self {
            Self {
                version: 1,
                default_engine: autoharness_core::EngineKind::codex(),
                default_models: std::collections::BTreeMap::new(),
                default_reasoning_efforts: std::collections::BTreeMap::new(),
                default_route_mode: RouteMode::Auto,
                max_active_runs: default_max_active_runs(),
                max_parallel_workers: 2,
                max_graph_nodes: 8,
                default_wall_time_minutes: 30,
                retention_days: 90,
                automatic_history_scan: true,
                notifications_enabled: true,
                sounds_enabled: true,
                automatic_update_checks: true,
                confirm_destructive_actions: true,
            }
        }
    }

    impl AppSettings {
        pub fn clamped(mut self) -> Self {
            self.version = 1;
            self.max_active_runs = self.max_active_runs.clamp(MIN_ACTIVE_RUNS, MAX_ACTIVE_RUNS);
            self.max_parallel_workers = self
                .max_parallel_workers
                .clamp(MIN_PARALLEL_WORKERS, MAX_PARALLEL_WORKERS);
            self.max_graph_nodes = self.max_graph_nodes.clamp(MIN_GRAPH_NODES, MAX_GRAPH_NODES);
            self.default_wall_time_minutes = self
                .default_wall_time_minutes
                .clamp(MIN_WALL_TIME_MINUTES, MAX_WALL_TIME_MINUTES);
            self.retention_days = self
                .retention_days
                .clamp(MIN_RETENTION_DAYS, MAX_RETENTION_DAYS);
            self
        }
    }

    #[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
    pub struct SettingsUpdate {
        /// `(engine, model)` — the model to start new runs on for that engine.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub default_model: Option<(String, String)>,
        /// `(engine, effort)`, same shape.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub default_reasoning_effort: Option<(String, String)>,
        #[serde(default)]
        pub request_id: Option<String>,
        #[serde(default)]
        pub default_engine: Option<autoharness_core::EngineKind>,
        #[serde(default)]
        pub default_route_mode: Option<RouteMode>,
        #[serde(default)]
        pub max_active_runs: Option<u32>,
        #[serde(default)]
        pub max_parallel_workers: Option<u32>,
        #[serde(default)]
        pub max_graph_nodes: Option<u32>,
        #[serde(default)]
        pub default_wall_time_minutes: Option<u32>,
        #[serde(default)]
        pub retention_days: Option<u32>,
        #[serde(default)]
        pub automatic_history_scan: Option<bool>,
        #[serde(default)]
        pub notifications_enabled: Option<bool>,
        #[serde(default)]
        pub sounds_enabled: Option<bool>,
        #[serde(default)]
        pub automatic_update_checks: Option<bool>,
        #[serde(default)]
        pub confirm_destructive_actions: Option<bool>,
    }

    #[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
    pub struct UsageBucket {
        pub input_tokens: u64,
        pub output_tokens: u64,
        pub total_tokens: u64,
        pub event_count: u64,
        pub run_count: u64,
    }

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub struct UsageProviderSummary {
        pub provider: String,
        pub today: UsageBucket,
        pub month: UsageBucket,
        pub all_time: UsageBucket,
    }

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub struct UsageRunSummary {
        pub run_id: String,
        pub provider: String,
        pub input_tokens: u64,
        pub output_tokens: u64,
        pub total_tokens: u64,
        pub event_count: u64,
    }

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub struct UsageSummaryResult {
        pub generated_at_ms: i64,
        pub today_start_ms: i64,
        pub month_start_ms: i64,
        pub providers: Vec<UsageProviderSummary>,
        pub runs: Vec<UsageRunSummary>,
    }

    /// A bounded reference to something a run produced.
    ///
    /// The `summary` is an excerpt, never the whole thing. Large evidence
    /// stays where it was written and is named by `path`.
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub struct Artifact {
        pub id: String,
        pub run_id: String,
        pub node_id: Option<String>,
        pub kind: String,
        pub name: String,
        pub path: Option<String>,
        pub byte_size: Option<i64>,
        pub summary: String,
        pub created_at_ms: i64,
    }

    #[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
    pub struct ArtifactList {
        pub run_id: String,
        #[serde(default)]
        pub limit: Option<usize>,
    }

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub struct ArtifactListResult {
        pub run_id: String,
        pub artifacts: Vec<Artifact>,
    }

    /// Why a worktree cannot be reclaimed. Reclaim is allowed only when this
    /// list is empty: every unknown or unverifiable condition adds a blocker
    /// rather than being ignored.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(rename_all = "snake_case")]
    pub enum WorktreeBlocker {
        /// No live row in the daemon's worktree index.
        NotIndexed,
        /// Already reclaimed; nothing left to do.
        AlreadyReclaimed,
        /// The canonical path is not the indexed one (symlink or traversal).
        PathMismatch,
        /// Outside the daemon's worktree storage directory.
        OutsideStorage,
        /// This is the user's primary checkout, not a daemon worktree.
        PrimaryCheckout,
        /// The owning run is executing right now.
        RunActive,
        /// The owning run has not reached a terminal state.
        RunNotTerminal,
        /// Uncommitted changes are present.
        DirtyWorktree,
        /// The branch carries commits past the recorded base commit.
        CommitsBeyondBase,
        /// A process holds a file under the directory.
        ProcessInUse,
        /// The process check could not answer; treated as in use.
        ProcessCheckUnknown,
        /// A git query failed, so the state is unknown.
        GitCheckFailed,
        /// `confirm_path` was absent or did not match exactly.
        ConfirmPathMismatch,
        /// Another reclaim of this path is in flight.
        ReclaimInProgress,
    }

    #[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
    pub struct WorktreeList {
        /// Include rows already reclaimed (audit view).
        #[serde(default)]
        pub include_reclaimed: bool,
        /// Only worktrees owned by this run.
        #[serde(default)]
        pub run_id: Option<String>,
        /// Only worktrees that would pass every reclaim check.
        #[serde(default)]
        pub only_eligible: bool,
    }

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub struct WorktreeEntry {
        pub path: String,
        pub kind: String,
        pub run_id: String,
        pub node_id: Option<String>,
        pub repo_path: String,
        pub branch: String,
        pub base_commit: String,
        pub created_at_ms: i64,
        pub removed_at_ms: Option<i64>,
        pub run_state: String,
        /// Whether the directory is still on disk.
        pub exists: bool,
        pub eligible: bool,
        pub blockers: Vec<WorktreeBlocker>,
    }

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub struct WorktreeListResult {
        pub storage_root: String,
        pub entries: Vec<WorktreeEntry>,
    }

    #[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
    pub struct WorktreeReclaim {
        pub path: String,
        /// Diagnostics only: run every check and change nothing.
        #[serde(default)]
        pub dry_run: bool,
        /// Must equal `path` exactly for a non-dry-run reclaim.
        #[serde(default)]
        pub confirm_path: Option<String>,
        #[serde(default)]
        pub request_id: Option<String>,
    }

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub struct WorktreeReclaimResult {
        pub path: String,
        pub dry_run: bool,
        /// True when this call removed the worktree.
        pub reclaimed: bool,
        /// True when a previous call already removed it (idempotent repeat).
        pub already_reclaimed: bool,
        pub eligible: bool,
        pub blockers: Vec<WorktreeBlocker>,
        pub entry: Option<WorktreeEntry>,
    }

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    pub struct AuthHello {
        pub token: String,
    }

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    pub struct ProjectAdd {
        pub name: String,
        pub path: String,
    }

    /// `project.create`: make a fresh repository named `name` under the
    /// daemon's managed folder. The daemon sanitizes the name and picks a
    /// free directory; the reply is the registered project.
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    pub struct ProjectCreate {
        pub name: String,
    }

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    pub struct ProjectRemove {
        pub project_id: String,
    }

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    pub struct RunCreate {
        pub project_id: String,
        /// Optional because clients can rely on persisted default_engine.
        #[serde(default)]
        pub engine: Option<autoharness_core::EngineKind>,
        /// Provider model id/alias pinned for the lifetime of this run.
        /// `None` delegates to the provider's current default.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub model: Option<String>,
        /// Provider-native reasoning effort. The daemon validates the bounded
        /// vocabulary before persisting it and the adapter applies it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub reasoning_effort: Option<String>,
        pub objective: String,
        /// Verification command run in the worktree after the engine
        /// completes (direct runs).
        #[serde(default)]
        pub check_command: Option<String>,
        /// The run this one continues. Its engine session is resumed, so a
        /// follow-up keeps the model's context instead of starting over.
        #[serde(default)]
        pub parent_run_id: Option<String>,
    }

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    pub struct RunGet {
        pub run_id: String,
    }

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub struct NodeControl {
        pub run_id: String,
        pub node_id: String,
    }

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub struct NodeControlResult {
        pub run_id: String,
        pub node_id: String,
        pub command: String,
        pub accepted: bool,
        pub attempt: i64,
    }

    #[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
    pub struct BudgetOverrides {
        #[serde(default)]
        pub max_parallel_workers: Option<u32>,
        #[serde(default)]
        pub max_graph_nodes: Option<u32>,
        #[serde(default)]
        pub wall_time_minutes: Option<u32>,
    }

    /// One way of answering the objective.
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    pub struct AttemptSpec {
        pub engine: autoharness_core::EngineKind,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub model: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub reasoning_effort: Option<String>,
    }

    /// Fan one objective out across several engines or models.
    ///
    /// The check is deliberately shared: attempts that were not judged the
    /// same way cannot be compared, and an unverified race is just several
    /// opinions with no adjudicator.
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    pub struct RunAttempts {
        pub project_id: String,
        pub objective: String,
        pub attempts: Vec<AttemptSpec>,
        #[serde(default)]
        pub check_command: Option<String>,
        pub request_id: String,
    }

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    pub struct RunSetObjective {
        pub run_id: String,
        pub objective: String,
    }

    #[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
    pub struct RunStart {
        pub run_id: String,
        #[serde(default)]
        pub engine: Option<autoharness_core::EngineKind>,
        #[serde(default)]
        pub route_mode: Option<RouteMode>,
        #[serde(default)]
        pub budget: Option<BudgetOverrides>,
    }

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    pub struct RunEnqueue {
        pub project_id: String,
        /// Queue a draft that already exists instead of creating one. The
        /// sidebar's `+` creates the draft first so the row is visible and
        /// dated immediately; this is how that same run is later started.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub run_id: Option<String>,
        #[serde(default)]
        pub engine: Option<autoharness_core::EngineKind>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub model: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub reasoning_effort: Option<String>,
        pub objective: String,
        #[serde(default)]
        pub check_command: Option<String>,
        #[serde(default)]
        pub parent_run_id: Option<String>,
        #[serde(default)]
        pub route_mode: Option<RouteMode>,
        #[serde(default)]
        pub budget: Option<BudgetOverrides>,
        pub request_id: String,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(rename_all = "snake_case")]
    pub enum QueueKind {
        Objective,
        Steering,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(rename_all = "snake_case")]
    pub enum QueueState {
        Pending,
        Dispatching,
        Completed,
        Failed,
        Cancelled,
    }

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub struct QueueItem {
        pub id: String,
        pub kind: QueueKind,
        pub state: QueueState,
        pub run_id: String,
        pub project_id: String,
        pub content: String,
        pub position: i64,
        pub created_at_ms: i64,
        pub updated_at_ms: i64,
        #[serde(default)]
        pub error: Option<String>,
    }

    #[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
    pub struct QueueList {
        #[serde(default)]
        pub run_id: Option<String>,
        #[serde(default)]
        pub kind: Option<QueueKind>,
        #[serde(default)]
        pub include_terminal: bool,
    }

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub struct QueueListResult {
        pub items: Vec<QueueItem>,
    }

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub struct QueueMove {
        pub item_id: String,
        #[serde(default)]
        pub before_item_id: Option<String>,
        pub request_id: String,
    }

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub struct QueueCancel {
        pub item_id: String,
        pub request_id: String,
    }

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub struct RunEnqueueResult {
        pub run_id: String,
        pub item: QueueItem,
    }

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    pub struct ChatSend {
        pub run_id: String,
        pub message: String,
        #[serde(default)]
        pub request_id: Option<String>,
    }

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    pub struct RunAnswer {
        pub run_id: String,
        pub answer: autoharness_core::Answer,
    }

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    pub struct ChatInterrupt {
        pub run_id: String,
        pub message: String,
    }

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    pub struct MemoryList {
        pub project_id: String,
        /// Only facts backed by evidence and marked verified.
        #[serde(default)]
        pub verified_only: bool,
        /// Optional lexical query (FTS5); empty lists everything.
        #[serde(default)]
        pub query: Option<String>,
    }

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    pub struct MemoryForget {
        pub fact_id: String,
    }

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    pub struct HistoryList {
        #[serde(default)]
        pub provider: Option<String>,
        #[serde(default)]
        pub project_id: Option<String>,
        #[serde(default)]
        pub project_path: Option<String>,
        #[serde(default)]
        pub query: Option<String>,
        #[serde(default)]
        pub cursor: Option<String>,
        #[serde(default)]
        pub limit: Option<usize>,
    }

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    pub struct HistoryEntry {
        pub provider: String,
        pub source_id: String,
        pub transcript_path: String,
        #[serde(default)]
        pub cwd: Option<String>,
        #[serde(default)]
        pub title: Option<String>,
        #[serde(default)]
        pub first_prompt: Option<String>,
        pub updated_at_ms: i64,
        pub eligible: bool,
        #[serde(default)]
        pub reason: Option<String>,
        #[serde(default)]
        pub adopted_run_id: Option<String>,
    }

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    pub struct HistoryListResult {
        pub entries: Vec<HistoryEntry>,
        #[serde(default)]
        pub next_cursor: Option<String>,
        #[serde(default)]
        pub diagnostics: Vec<String>,
        pub scan_enabled: bool,
    }

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    pub struct HistoryAdopt {
        pub provider: String,
        pub source_id: String,
        pub project_id: String,
        pub engine: autoharness_core::EngineKind,
        pub request_id: String,
    }

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    pub struct HistoryAdoptResult {
        pub run_id: String,
        pub provider: String,
        pub source_id: String,
        pub already_adopted: bool,
    }

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    pub struct PolicyPromote {
        pub version: i64,
    }

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    pub struct EventsSubscribe {
        /// Replay all events with sequence strictly greater than this.
        #[serde(default)]
        pub since_sequence: u64,
        /// Optional run filter.
        #[serde(default)]
        pub run_id: Option<String>,
    }
}

/// Write one length-delimited JSON frame.
pub async fn write_frame<W: AsyncWrite + Unpin, T: Serialize>(
    writer: &mut W,
    value: &T,
) -> Result<(), ProtocolError> {
    let body = serde_json::to_vec(value)?;
    if body.len() as u64 > MAX_FRAME_BYTES as u64 {
        return Err(ProtocolError::FrameTooLarge(body.len() as u32));
    }
    writer.write_u32(body.len() as u32).await?;
    writer.write_all(&body).await?;
    writer.flush().await?;
    Ok(())
}

/// Read one length-delimited frame, returning the raw JSON body.
/// Returns `Ok(None)` on clean EOF before any length bytes.
pub async fn read_frame_bytes<R: AsyncRead + Unpin>(
    reader: &mut R,
) -> Result<Option<Vec<u8>>, ProtocolError> {
    let mut len_buf = [0u8; 4];
    match reader.read_exact(&mut len_buf).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let len = u32::from_be_bytes(len_buf);
    if len > MAX_FRAME_BYTES {
        return Err(ProtocolError::FrameTooLarge(len));
    }
    let mut body = vec![0u8; len as usize];
    reader
        .read_exact(&mut body)
        .await
        .map_err(|e| ProtocolError::Malformed(format!("truncated frame body: {e}")))?;
    Ok(Some(body))
}

/// Read and decode one frame into a concrete envelope type.
pub async fn read_frame<R: AsyncRead + Unpin, T: DeserializeOwned>(
    reader: &mut R,
) -> Result<Option<T>, ProtocolError> {
    let Some(body) = read_frame_bytes(reader).await? else {
        return Ok(None);
    };
    let value = serde_json::from_slice(&body)
        .map_err(|e| ProtocolError::Malformed(format!("invalid JSON frame: {e}")))?;
    Ok(Some(value))
}

/// Bounded per-server idempotency cache: a repeated request ID yields the
/// cached response instead of re-executing the handler.
#[derive(Debug)]
pub struct DedupCache {
    responses: HashMap<String, Response>,
    order: VecDeque<String>,
    capacity: usize,
}

impl DedupCache {
    pub fn new(capacity: usize) -> Self {
        Self {
            responses: HashMap::new(),
            order: VecDeque::new(),
            capacity: capacity.max(1),
        }
    }

    /// Look up a previously processed request ID.
    pub fn get(&self, id: &str) -> Option<&Response> {
        self.responses.get(id)
    }

    /// Record a completed response, evicting the oldest entry at capacity.
    pub fn store(&mut self, id: String, response: Response) {
        if self.responses.contains_key(&id) {
            return;
        }
        if self.order.len() >= self.capacity
            && let Some(oldest) = self.order.pop_front()
        {
            self.responses.remove(&oldest);
        }
        self.order.push_back(id.clone());
        self.responses.insert(id, response);
    }

    pub fn len(&self) -> usize {
        self.responses.len()
    }

    pub fn is_empty(&self) -> bool {
        self.responses.is_empty()
    }
}

/// Validate that a slice of events is strictly monotonic in `sequence`,
/// as the ledger and replay logic guarantee.
pub fn assert_event_order(events: &[Event]) -> Result<(), ProtocolError> {
    for pair in events.windows(2) {
        if pair[1].sequence <= pair[0].sequence {
            return Err(ProtocolError::Malformed(format!(
                "event sequence regression: {} then {}",
                pair[0].sequence, pair[1].sequence
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn sample_event(seq: u64) -> Event {
        Event::new(
            seq,
            Some("run-1".into()),
            seq,
            1_700_000_000_000 + seq as i64,
            "run.progress",
            serde_json::json!({ "pct": seq }),
        )
    }

    #[tokio::test]
    async fn framing_round_trip_all_envelopes() {
        let request = Request::new("r1", methods::PROJECT_ADD, serde_json::json!({"name": "x"}));
        let response = Response::ok("r1", serde_json::json!({"ok": true}));
        let event = sample_event(1);

        let mut buf = Vec::new();
        write_frame(&mut buf, &request).await.unwrap();
        write_frame(&mut buf, &response).await.unwrap();
        write_frame(&mut buf, &event).await.unwrap();

        let mut cursor = Cursor::new(buf);
        let r: Request = read_frame(&mut cursor).await.unwrap().unwrap();
        assert_eq!(r, request);
        let resp: Response = read_frame(&mut cursor).await.unwrap().unwrap();
        assert_eq!(resp, response);
        let e: Event = read_frame(&mut cursor).await.unwrap().unwrap();
        assert_eq!(e, event);
        // Clean EOF after the last frame.
        let none: Option<Request> = read_frame(&mut cursor).await.unwrap();
        assert!(none.is_none());
    }

    /// Clients read responses and events off one socket as [`Frame`]. The
    /// untagged variants must never be confused for each other.
    #[tokio::test]
    async fn frame_discriminates_responses_from_events() {
        let request = Request::new("r1", methods::RUN_START, serde_json::json!({}));
        let ok = Response::ok("r1", serde_json::json!({ "started": true }));
        let err = Response::err("r2", codes::INVALID_PARAMS, "nope");
        let event = sample_event(7);

        let mut buf = Vec::new();
        for frame in [
            Frame::Request(request.clone()),
            Frame::Response(ok.clone()),
            Frame::Response(err.clone()),
            Frame::Event(event.clone()),
        ] {
            write_frame(&mut buf, &frame).await.unwrap();
        }

        let mut cursor = Cursor::new(buf);
        let mut decoded = Vec::new();
        while let Some(frame) = read_frame::<_, Frame>(&mut cursor).await.unwrap() {
            decoded.push(frame);
        }
        assert_eq!(
            decoded,
            [
                Frame::Request(request),
                Frame::Response(ok),
                Frame::Response(err),
                Frame::Event(event),
            ]
        );
    }

    #[tokio::test]
    async fn malformed_frame_rejects_oversized_length() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&(MAX_FRAME_BYTES + 1).to_be_bytes());
        buf.extend_from_slice(&[0u8; 16]);
        let mut cursor = Cursor::new(buf);
        let err = read_frame_bytes(&mut cursor).await.unwrap_err();
        assert!(matches!(err, ProtocolError::FrameTooLarge(_)));
    }

    #[tokio::test]
    async fn malformed_frame_rejects_invalid_json() {
        let body = b"{not json";
        let mut buf = Vec::new();
        buf.extend_from_slice(&(body.len() as u32).to_be_bytes());
        buf.extend_from_slice(body);
        let mut cursor = Cursor::new(buf);
        let err = read_frame::<_, Request>(&mut cursor).await.unwrap_err();
        assert!(matches!(err, ProtocolError::Malformed(_)));
    }

    #[tokio::test]
    async fn malformed_frame_rejects_truncated_body() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&100u32.to_be_bytes());
        buf.extend_from_slice(b"{}");
        let mut cursor = Cursor::new(buf);
        let err = read_frame_bytes(&mut cursor).await.unwrap_err();
        assert!(matches!(err, ProtocolError::Malformed(_)));
    }

    #[test]
    fn event_ordering_validator() {
        let good: Vec<Event> = (1..=5).map(sample_event).collect();
        assert_event_order(&good).unwrap();

        let mut bad = good.clone();
        bad.swap(1, 3);
        assert!(assert_event_order(&bad).is_err());

        let dup = vec![sample_event(2), sample_event(2)];
        assert!(assert_event_order(&dup).is_err());
    }

    #[test]
    fn duplicate_requests_return_cached_response() {
        let mut cache = DedupCache::new(2);
        assert!(cache.get("a").is_none());

        let resp = Response::ok("a", serde_json::json!({"created": 1}));
        cache.store("a".into(), resp.clone());
        assert_eq!(cache.get("a"), Some(&resp));

        // Storing under the same ID again must not overwrite.
        cache.store(
            "a".into(),
            Response::ok("a", serde_json::json!({"created": 2})),
        );
        assert_eq!(cache.get("a"), Some(&resp));

        // Capacity eviction: oldest ("a") is dropped.
        cache.store("b".into(), Response::ok("b", serde_json::json!(null)));
        cache.store("c".into(), Response::ok("c", serde_json::json!(null)));
        assert_eq!(cache.len(), 2);
        assert!(cache.get("a").is_none());
        assert!(cache.get("c").is_some());
    }

    #[test]
    fn response_variants_are_well_formed() {
        let ok = Response::ok("1", serde_json::json!(null));
        assert!(ok.error.is_none() && ok.result.is_some());
        let err = Response::not_implemented("1", methods::RUN_START);
        assert_eq!(err.error.as_ref().unwrap().code, codes::NOT_IMPLEMENTED);
        assert!(err.result.is_none());
    }

    #[test]
    fn history_methods_are_typed_known_and_implemented() {
        assert_eq!(methods::HISTORY_LIST, "history.list");
        assert_eq!(methods::HISTORY_ADOPT, "history.adopt");
        assert!(methods::is_known(methods::HISTORY_LIST));
        assert!(methods::is_known(methods::HISTORY_ADOPT));
        assert!(methods::IMPLEMENTED.contains(&methods::HISTORY_LIST));
        assert!(methods::IMPLEMENTED.contains(&methods::HISTORY_ADOPT));

        let list = params::HistoryList {
            provider: Some("codex".into()),
            project_id: Some("project-1".into()),
            project_path: Some("/repo".into()),
            query: Some("router".into()),
            cursor: Some("cursor-1".into()),
            limit: Some(25),
        };
        let encoded = serde_json::to_value(&list).unwrap();
        assert_eq!(encoded["provider"], "codex");
        let decoded: params::HistoryList = serde_json::from_value(encoded).unwrap();
        assert_eq!(decoded, list);

        let adopt = params::HistoryAdopt {
            provider: "claude".into(),
            source_id: "source-1".into(),
            project_id: "project-1".into(),
            engine: autoharness_core::EngineKind::codex(),
            request_id: "adopt-1".into(),
        };
        let encoded = serde_json::to_value(&adopt).unwrap();
        assert_eq!(encoded["engine"], "codex");
        let decoded: params::HistoryAdopt = serde_json::from_value(encoded).unwrap();
        assert_eq!(decoded, adopt);
    }

    #[test]
    fn settings_and_usage_methods_are_typed_known_and_implemented() {
        assert_eq!(methods::SETTINGS_GET, "settings.get");
        assert_eq!(methods::SETTINGS_UPDATE, "settings.update");
        assert_eq!(methods::USAGE_SUMMARY, "usage.summary");
        for method in [
            methods::SETTINGS_GET,
            methods::SETTINGS_UPDATE,
            methods::USAGE_SUMMARY,
        ] {
            assert!(methods::is_known(method), "{method} should be known");
            assert!(
                methods::IMPLEMENTED.contains(&method),
                "{method} should be implemented"
            );
        }

        let update = params::SettingsUpdate {
            request_id: Some("settings-1".into()),
            default_engine: Some(autoharness_core::EngineKind::claude()),
            default_route_mode: Some(params::RouteMode::Parallel),
            max_parallel_workers: Some(99),
            automatic_history_scan: Some(false),
            ..params::SettingsUpdate::default()
        };
        let encoded = serde_json::to_value(&update).unwrap();
        assert_eq!(encoded["default_engine"], "claude");
        assert_eq!(encoded["default_route_mode"], "parallel");
        let decoded: params::SettingsUpdate = serde_json::from_value(encoded).unwrap();
        assert_eq!(decoded, update);

        let clamped = params::AppSettings {
            max_parallel_workers: 99,
            max_graph_nodes: 99,
            default_wall_time_minutes: 1,
            retention_days: 999,
            ..params::AppSettings::default()
        }
        .clamped();
        assert_eq!(clamped.max_parallel_workers, params::MAX_PARALLEL_WORKERS);
        assert_eq!(clamped.max_graph_nodes, params::MAX_GRAPH_NODES);
        assert_eq!(
            clamped.default_wall_time_minutes,
            params::MIN_WALL_TIME_MINUTES
        );
        assert_eq!(clamped.retention_days, params::MAX_RETENTION_DAYS);
    }

    #[test]
    fn queue_methods_are_typed_known_and_settings_remain_backward_compatible() {
        for method in [
            methods::RUN_ENQUEUE,
            methods::QUEUE_LIST,
            methods::QUEUE_MOVE,
            methods::QUEUE_CANCEL,
        ] {
            assert!(methods::is_known(method), "{method} should be known");
            assert!(
                methods::IMPLEMENTED.contains(&method),
                "{method} should be implemented"
            );
        }

        let enqueue = params::RunEnqueue {
            project_id: "project-1".into(),
            run_id: None,
            engine: Some(autoharness_core::EngineKind::claude()),
            model: Some("opus".into()),
            reasoning_effort: Some("high".into()),
            objective: "finish the queue".into(),
            check_command: Some("cargo test".into()),
            parent_run_id: None,
            route_mode: Some(params::RouteMode::Parallel),
            budget: Some(params::BudgetOverrides {
                max_parallel_workers: Some(2),
                ..params::BudgetOverrides::default()
            }),
            request_id: "enqueue-1".into(),
        };
        let encoded = serde_json::to_value(&enqueue).unwrap();
        let decoded: params::RunEnqueue = serde_json::from_value(encoded).unwrap();
        assert_eq!(decoded, enqueue);

        let backward_compatible: params::RunEnqueue = serde_json::from_value(serde_json::json!({
            "project_id": "project-1",
            "engine": "codex",
            "objective": "use provider defaults",
            "request_id": "enqueue-old"
        }))
        .unwrap();
        assert_eq!(backward_compatible.model, None);
        assert_eq!(backward_compatible.reasoning_effort, None);

        let item = params::QueueItem {
            id: "queue-1".into(),
            kind: params::QueueKind::Objective,
            state: params::QueueState::Pending,
            run_id: "run-1".into(),
            project_id: "project-1".into(),
            content: "finish the queue".into(),
            position: 10,
            created_at_ms: 1,
            updated_at_ms: 1,
            error: None,
        };
        let decoded: params::QueueItem =
            serde_json::from_value(serde_json::to_value(&item).unwrap()).unwrap();
        assert_eq!(decoded, item);

        let old_settings = serde_json::json!({
            "version": 1,
            "default_engine": "codex",
            "default_route_mode": "auto",
            "max_parallel_workers": 2,
            "max_graph_nodes": 8,
            "default_wall_time_minutes": 30,
            "retention_days": 90,
            "automatic_history_scan": true,
            "notifications_enabled": true,
            "sounds_enabled": true,
            "automatic_update_checks": true,
            "confirm_destructive_actions": true
        });
        let decoded: params::AppSettings = serde_json::from_value(old_settings).unwrap();
        assert_eq!(decoded.max_active_runs, 2);

        let clamped = params::AppSettings {
            max_active_runs: 99,
            ..params::AppSettings::default()
        }
        .clamped();
        assert_eq!(clamped.max_active_runs, params::MAX_ACTIVE_RUNS);
    }
}
