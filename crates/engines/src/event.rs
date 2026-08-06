//! Normalized engine event model.
//!
//! Every provider-specific message (Codex app-server notification, Claude
//! stream-json line, FakeEngine script step) is mapped into these shared
//! variants. The daemon persists them through its emit path with a run_id
//! and ledger sequence; `EngineEvent::kind_str` is the ledger event kind.

use serde::{Deserialize, Serialize};

/// Provider-neutral stream of engine activity.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EngineEvent {
    /// Provider session/thread identity, emitted once per (re)connection.
    SessionIdentity { session_id: String },
    /// A complete text message from the agent.
    Text { text: String },
    /// An incremental text fragment (providers that stream deltas).
    TextDelta { delta: String },
    /// A line an agent PAINTED on its terminal.
    ///
    /// Deliberately not `TextDelta`. A delta is a model typing a message, and
    /// the UI filters those as noise because twenty of them say only "it is
    /// typing". A terminal line is the opposite: for an agent that speaks no
    /// structured protocol it is the entire account of what happened, and
    /// dropping it leaves the run looking like it is doing nothing.
    TerminalOutput { line: String },
    /// Tool invocation lifecycle.
    ToolActivity {
        name: String,
        status: ToolStatus,
        /// Provider-normalized detail, e.g. {"command": ...} or {"exit_code": ...}.
        detail: serde_json::Value,
    },
    /// A file the engine created, modified, or deleted.
    FileChange { path: String, kind: FileChangeKind },
    /// A check the engine ran and its outcome.
    Check {
        result: autoharness_core::CheckResult,
    },
    /// The engine is asking for approval or input. Phase 2 records the
    /// question; answering arrives with orchestration in Phase 4.
    Question { id: String, prompt: String },
    /// Token usage for the turn/session. Cost is deliberately excluded from
    /// the normalized model: it is provider-specific and not comparable.
    Usage {
        input_tokens: u64,
        output_tokens: u64,
    },
    /// The engine finished its work successfully.
    Completed { summary: Option<String> },
    /// The engine failed. `recoverable` marks provider-retryable errors.
    Failed { message: String, recoverable: bool },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolStatus {
    Started,
    Updated,
    Finished,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileChangeKind {
    Created,
    Modified,
    Deleted,
}

impl EngineEvent {
    /// Ledger event kind, e.g. `engine.text`.
    pub fn kind_str(&self) -> &'static str {
        match self {
            EngineEvent::SessionIdentity { .. } => "engine.session",
            EngineEvent::Text { .. } => "engine.text",
            EngineEvent::TextDelta { .. } => "engine.text_delta",
            EngineEvent::TerminalOutput { .. } => "engine.terminal",
            EngineEvent::ToolActivity { .. } => "engine.tool",
            EngineEvent::FileChange { .. } => "engine.file",
            EngineEvent::Check { .. } => "engine.check",
            EngineEvent::Question { .. } => "engine.question",
            EngineEvent::Usage { .. } => "engine.usage",
            EngineEvent::Completed { .. } => "engine.completed",
            EngineEvent::Failed { .. } => "engine.failed",
        }
    }

    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            EngineEvent::Completed { .. } | EngineEvent::Failed { .. }
        )
    }
}
