//! The engine adapter contract shared by Fake, Codex, and Claude adapters.

use std::path::PathBuf;

use autoharness_core::{Answer, EngineKind};
use thiserror::Error;

use crate::diagnostics::EngineDiagnostics;
use crate::event::EngineEvent;

/// One provider model the installed CLI reports as selectable.
///
/// `id` is the exact value passed back to the provider. An empty id is the
/// explicit "provider default" pseudo-model used when a CLI has no catalog
/// endpoint (currently Claude Code).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct EngineModel {
    pub id: String,
    pub display_name: String,
    pub description: String,
    #[serde(default)]
    pub reasoning_efforts: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_reasoning_effort: Option<String>,
    #[serde(default)]
    pub is_default: bool,
}

/// Where and how a session should run.
#[derive(Debug, Clone)]
pub struct SessionSpec {
    /// Working directory for the engine process. Phase 3 tightens this to
    /// sandboxed worktree/session roots.
    pub working_dir: PathBuf,
    /// Daemon-managed data dir (`~/Library/Application Support/...`), under
    /// which the per-session fake HOME and TMPDIR are created.
    pub data_dir: PathBuf,
    /// Stable name for this conversation's fake HOME under `data_dir`.
    ///
    /// The provider writes its own resumable transcript inside that HOME, so
    /// every turn meaning to continue one conversation MUST pass the same key.
    /// Generating a fresh one per turn files the previous transcript where
    /// `--resume <id>` cannot see it: the engine then exits before producing a
    /// single token, reported only as "turn failed". Callers that genuinely
    /// want a throwaway session (one-shot probes, planning) pass a new id.
    pub session_key: String,
    /// Exact provider model id/alias. `None` delegates to provider default.
    pub model: Option<String>,
    /// Exact provider-native effort. `None` delegates to provider default.
    pub reasoning_effort: Option<String>,
}

#[derive(Debug, Error)]
pub enum EngineError {
    #[error("engine not installed: {0}")]
    NotInstalled(String),
    #[error("engine not authenticated")]
    NotAuthenticated,
    #[error("protocol error: {0}")]
    Protocol(String),
    #[error("provider error: {0}")]
    Provider(String),
    #[error("operation not supported by this engine: {0}")]
    Unsupported(String),
    #[error("engine process exited: {0}")]
    ProcessExited(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}

/// Async contract every engine adapter implements.
///
/// Lifecycle: `detect` → `start_session`/`resume_session` → `send_turn` +
/// `next_event` loop → terminal `Completed`/`Failed` event or `cancel`.
/// `pause`/`interrupt` affect the in-flight turn; `cancel` ends the session.
#[async_trait::async_trait]
pub trait EngineAdapter: Send {
    fn kind(&self) -> EngineKind;

    /// Capability probe: installed, version, authenticated, structured mode.
    /// Never fails — setup problems live in the returned diagnostics.
    async fn detect(&self) -> EngineDiagnostics;

    /// Token-free model discovery. Implementations may start the provider's
    /// local control process, but must never submit a turn.
    async fn available_models(
        &mut self,
        _data_dir: &std::path::Path,
    ) -> Result<Vec<EngineModel>, EngineError> {
        Ok(Vec::new())
    }

    /// Start a fresh provider session. On success, `session_id()` is set and
    /// the first `next_event` yields `SessionIdentity`.
    async fn start_session(&mut self, spec: &SessionSpec) -> Result<(), EngineError>;

    /// Resume a persisted provider session when safe. Adapters without a
    /// resumable mode return `EngineError::Unsupported` and the caller starts
    /// a fresh session instead.
    async fn resume_session(
        &mut self,
        session_id: &str,
        spec: &SessionSpec,
    ) -> Result<(), EngineError>;

    /// Send one turn of work (objective/prompt). Output streams back via
    /// `next_event`.
    async fn send_turn(&mut self, prompt: &str) -> Result<(), EngineError>;

    /// Pull the next normalized event. `Ok(None)` means no more events are
    /// coming (session ended). Must not panic on provider garbage; malformed
    /// messages become `EngineError::Protocol` or are skipped.
    async fn next_event(&mut self) -> Result<Option<EngineEvent>, EngineError>;

    /// Pause work at the next safe point. Providers without a real pause
    /// implement this as a soft pause (interrupt the current turn); a
    /// subsequent `send_turn` continues normally.
    async fn pause(&mut self) -> Result<(), EngineError>;

    /// Cancel the in-flight provider turn immediately, keeping the session.
    async fn interrupt(&mut self) -> Result<(), EngineError>;

    /// Terminate the session and its process tree.
    async fn cancel(&mut self) -> Result<(), EngineError>;

    /// Answer a question the engine asked, as [`EngineEvent::Question`].
    ///
    /// Adapters whose provider has no such concept return `Unsupported`, and
    /// the caller reports that rather than pretending an answer was delivered.
    /// A terminal agent blocks forever on an unanswered prompt, so a silent
    /// no-op here would look exactly like a hung run.
    async fn answer(&mut self, _answer: Answer) -> Result<(), EngineError> {
        Err(EngineError::Unsupported(
            "this engine does not take answers out of band".into(),
        ))
    }

    /// Provider session/thread ID, once established.
    fn session_id(&self) -> Option<&str>;
}
