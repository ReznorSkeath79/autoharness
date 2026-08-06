//! Daemon client for the desktop shell.
//!
//! One background thread owns a tokio runtime, the Unix-socket connection, and
//! the `auth.hello` handshake; the first connection replays from sequence zero
//! and reconnects resume after the highest sequence already folded. The winit
//! thread never blocks on IO: it pushes [`Command`]s and reads a snapshot of
//! [`UiState`].
//!
//! Reader and writer are separate tasks because `read_frame` is not
//! cancel-safe — dropping it mid-frame inside a `select!` would desynchronize
//! the stream. The reader enqueues follow-up requests (project refresh,
//! `run.start` after `run.create`) through the same command channel.

use std::collections::{HashMap, hash_map::Entry};
use std::fs::{File, OpenOptions};
use std::io::Write as _;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use autoharness_daemon::{DaemonConfig, load_or_create_token};
use autoharness_protocol as proto;
use autoharness_protocol::{Frame, Request, methods};
use serde_json::{Value, json};
use tokio::sync::mpsc;

/// Maximum event lines kept for the right-hand tail.
const EVENT_TAIL: usize = 200;
/// Painted lines kept per run. Enough to see what an agent has been doing
/// without holding a session's entire scrollback in the client.
const TERMINAL_TAIL: usize = 500;

pub const DETAIL_MESSAGE_CAP: usize = 400;
pub const DETAIL_ACTIVITY_CAP: usize = 400;
pub const DETAIL_CHANGED_FILE_CAP: usize = 200;
pub const DETAIL_CHECK_CAP: usize = 100;
pub const DETAIL_ARTIFACT_CAP: usize = 100;
pub const CHECK_OUTPUT_BYTE_CAP: usize = 64 * 1024;

fn title_case(value: &str) -> String {
    let mut chars = value.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Project {
    pub id: String,
    pub name: String,
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryEntryView {
    pub provider: String,
    pub source_id: String,
    pub transcript_path: String,
    pub cwd: Option<String>,
    pub title: Option<String>,
    pub first_prompt: Option<String>,
    pub updated_at_ms: i64,
    pub eligible: bool,
    pub reason: Option<String>,
    pub adopted_run_id: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HistoryPageState {
    pub entries: Vec<HistoryEntryView>,
    pub query: String,
    pub next_cursor: Option<String>,
    pub loading: bool,
    pub scan_enabled: bool,
    pub error: Option<String>,
    pub diagnostics: Vec<String>,
}

/// Which worktrees the panel shows. The daemon still decides eligibility;
/// this only narrows what is asked for.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum WorktreeFilter {
    #[default]
    Live,
    OnlyEligible,
    IncludeReclaimed,
}

impl WorktreeFilter {
    pub fn label(self) -> &'static str {
        match self {
            Self::Live => "Live",
            Self::OnlyEligible => "Eligible only",
            Self::IncludeReclaimed => "Include reclaimed",
        }
    }

    pub fn next(self) -> Self {
        match self {
            Self::Live => Self::OnlyEligible,
            Self::OnlyEligible => Self::IncludeReclaimed,
            Self::IncludeReclaimed => Self::Live,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorktreePageState {
    pub entries: Vec<proto::params::WorktreeEntry>,
    pub storage_root: String,
    pub filter: WorktreeFilter,
    pub loading: bool,
    pub error: Option<String>,
    /// Path the user asked to reclaim, awaiting an explicit confirmation.
    pub pending_confirm: Option<String>,
    /// Result text of the last dry run, keyed by path.
    pub diagnostics: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct SettingsView {
    pub values: proto::params::AppSettings,
    pub loading: bool,
    pub error: Option<String>,
    pub pending_response: Option<String>,
    pub pending_update: Option<proto::params::SettingsUpdate>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UsageSummaryView {
    pub generated_at_ms: i64,
    pub today_start_ms: i64,
    pub month_start_ms: i64,
    pub providers: Vec<proto::params::UsageProviderSummary>,
    pub runs: Vec<proto::params::UsageRunSummary>,
    pub loading: bool,
    pub error: Option<String>,
    pub pending_response: Option<String>,
}

/// Whether this engine is offered as runnable anywhere in the app. Sourced
/// from core so every surface — picker, sidebar, settings, `/attempts` —
/// gives the same answer; everything gated reads as coming soon instead.
pub(crate) fn engine_generally_available(name: &str) -> bool {
    autoharness_core::EngineKind::new(name).is_generally_available()
}

/// The models an engine's catalog offers: exactly what the provider
/// reported. Slash-namespaced routed entries (opencodex's aggregated
/// catalog, `opencode-go/kimi-k2.6` and friends) were filtered here for one
/// session — the user runs that proxy on purpose and wants its models, the
/// tabbed picker holds a large catalog without becoming a haystack, and a
/// route that fails now says so in the transcript instead of vanishing.
pub(crate) fn offered_models(engine: &EngineStatus) -> Vec<&EngineModelView> {
    engine.models.iter().collect()
}

/// The one human sentence inside an engine failure.
///
/// Providers wrap errors in JSON envelopes, sometimes nested; showing
/// `{"error":{"message":…}}` verbatim buries the sentence the user needs.
/// Anything that does not parse passes through untouched.
pub(crate) fn failure_reason(raw: &str) -> String {
    let mut current = raw.trim().to_string();
    for _ in 0..3 {
        let Ok(value) = serde_json::from_str::<Value>(&current) else {
            break;
        };
        let message = value
            .get("error")
            .and_then(|error| error.get("message"))
            .and_then(Value::as_str)
            .or_else(|| value.get("message").and_then(Value::as_str));
        match message {
            Some(message) => current = message.trim().to_string(),
            None => break,
        }
    }
    current
}

/// One engine's setup, as the daemon detected it.
#[derive(Debug, Clone, PartialEq)]
pub struct EngineStatus {
    pub name: String,
    pub ready: bool,
    pub installed: bool,
    pub authenticated: Option<bool>,
    pub version: Option<String>,
    pub problems: Vec<String>,
    /// Provider-reported, token-free model choices. The id is passed through
    /// unchanged when a run is created.
    pub models: Vec<EngineModelView>,
    /// Catalog discovery is allowed to fail without disabling provider-default
    /// execution, so the chooser can explain the fallback honestly.
    pub model_load_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineModelView {
    pub id: String,
    pub display_name: String,
    pub description: String,
    pub reasoning_efforts: Vec<String>,
    pub default_reasoning_effort: Option<String>,
    pub is_default: bool,
}

impl EngineStatus {
    /// One line the user can act on, not a stack trace. The sign-in commands
    /// are the CLIs' own (`codex login`, `claude auth login`).
    pub fn summary(&self) -> String {
        // Kept short: this renders in a 240px sidebar row, and a truncated
        // instruction is worse than none. `sign_in_command` carries the full
        // command for the status line.
        if self.ready {
            let version = self.version.as_deref().unwrap_or("?");
            return format!("{}  {version}", self.name);
        }
        if !self.installed {
            return format!("{}  not installed", self.name);
        }
        if self.authenticated == Some(false) {
            return format!("{}  sign in", self.name);
        }
        let problem = self
            .problems
            .first()
            .map(String::as_str)
            .unwrap_or("not ready");
        format!("{}  {problem}", self.name)
    }

    /// The CLI's own sign-in command, for the status line where there is room.
    pub fn sign_in_command(&self) -> Option<&'static str> {
        if self.ready || !self.installed || self.authenticated != Some(false) {
            return None;
        }
        Some(match self.name.as_str() {
            "codex" => "codex login",
            _ => "claude auth login",
        })
    }
}

/// One node of a run's graph, as the ledger describes it.
#[derive(Debug, Clone, PartialEq)]
pub struct GraphNodeView {
    pub id: String,
    pub role: String,
    /// Human-readable task text; ids remain stable control keys.
    pub objective: String,
    pub file_scope: Vec<String>,
    pub acceptance_checks: Vec<String>,
    pub depends_on: Vec<String>,
    /// Execution wave, used as the column when drawing.
    pub wave: usize,
    /// `pending`, `running`, `succeeded`, `failed`, `cancelled`.
    pub state: String,
    pub detail: String,
    /// Optional progress, only when a payload or deterministic fixture has it.
    pub progress_percent: Option<u8>,
    /// Optional elapsed duration, only when a payload or deterministic fixture has it.
    pub duration_ms: Option<u64>,
    /// Set only by daemon events for states the live graph controller accepts.
    pub can_retry: bool,
    pub can_cancel: bool,
}

/// A run's plan, rebuilt from replay so a relaunched UI shows the same graph.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GraphView {
    pub nodes: Vec<GraphNodeView>,
    pub edges: Vec<(String, String)>,
    pub awaiting_approval: bool,
}

impl GraphView {
    fn node_mut(&mut self, id: &str) -> Option<&mut GraphNodeView> {
        self.nodes.iter_mut().find(|n| n.id == id)
    }

    pub fn wave_count(&self) -> usize {
        self.nodes.iter().map(|n| n.wave + 1).max().unwrap_or(0)
    }
}

/// One line of a unified diff, already classified so rendering is a lookup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffLine {
    /// `diff --git` / `+++` / `---`: the file this hunk belongs to.
    File(String),
    /// `@@ ... @@`: position, shown as its own separator.
    Hunk(String),
    Added(String),
    Removed(String),
    Context(String),
    /// The elision note when a patch was truncated.
    Note(String),
}

/// A parsed patch, plus its totals.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DiffView {
    pub lines: Vec<DiffLine>,
    pub files: usize,
    pub added: usize,
    pub removed: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangedFileStatus {
    Added,
    Modified,
    Deleted,
    Renamed,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangedFileView {
    pub path: String,
    pub status: ChangedFileStatus,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StructuredMessageView {
    pub author: String,
    pub engine: Option<String>,
    /// The model that produced this, when it is known.
    ///
    /// A thread can run three turns on three models. Without this the
    /// transcript reads as one conversation with one model, and "why did it
    /// answer differently that time" has no answer on screen.
    pub model: Option<String>,
    pub text: String,
    pub timestamp_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivityRowView {
    pub timestamp_ms: i64,
    pub engine: Option<String>,
    pub action: String,
    pub detail: String,
    pub path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckView {
    pub name: String,
    pub command: Option<String>,
    pub passed: Option<bool>,
    pub duration_ms: Option<u64>,
    pub output: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactView {
    pub name: String,
    pub path: String,
    pub kind: Option<String>,
    pub size_bytes: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorktreeDetail {
    pub path: Option<String>,
    pub branch: Option<String>,
    pub base: Option<String>,
    pub state: String,
    pub isolation: Option<String>,
    pub reason: Option<String>,
    pub created_at_ms: Option<i64>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct BudgetView {
    pub wall_time_secs: Option<u64>,
    pub max_turns: Option<u32>,
    pub max_tool_calls: Option<u32>,
    pub max_retries: Option<u32>,
    pub max_concurrent_workers: Option<u32>,
    pub max_graph_nodes: Option<u32>,
    pub spent_wall_time_secs: Option<u64>,
    pub spent_turns: Option<u32>,
    pub spent_tool_calls: Option<u32>,
    pub spent_cost_usd: Option<f64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UsageView {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub context_tokens: Option<u64>,
    pub context_limit_tokens: Option<u64>,
}

/// The routing decision, kept whole: the shape, why, and what it implies.
/// `route_shape` predates this and stays for the inspector's compact label.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RouteView {
    pub shape: String,
    pub confidence: Option<f32>,
    pub reasons: Vec<String>,
    pub alternatives: Vec<String>,
    pub max_turns: Option<u32>,
    pub wall_time_secs: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct RunDetailView {
    pub run_id: String,
    /// When the daemon created this run, taken from the `run.created` event's
    /// own envelope timestamp. The sidebar used to date a row only from its
    /// messages and activity, so a run that had not said anything yet — a
    /// draft, or one still starting — rendered as "unknown date".
    pub created_at_ms: Option<i64>,
    /// The question the agent is currently blocked on, if any.
    pub pending_question: Option<String>,
    /// What a terminal-driven agent has painted, most recent last.
    ///
    /// Capped like every other detail collection: an agent can print for
    /// hours, and a client that keeps all of it grows without bound.
    pub terminal: Vec<String>,
    pub engine: Option<String>,
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    pub route_shape: Option<String>,
    pub route: Option<RouteView>,
    pub messages: Vec<StructuredMessageView>,
    pub activity: Vec<ActivityRowView>,
    pub changed_files: Vec<ChangedFileView>,
    pub checks: Vec<CheckView>,
    pub artifacts: Vec<ArtifactView>,
    pub worktree: Option<WorktreeDetail>,
    pub budget: Option<BudgetView>,
    pub usage: UsageView,
    /// The latest patch replaces the previous one. The daemon already bounds
    /// diff payloads before emitting them, so the UI does not accumulate diffs.
    pub diff: Option<DiffView>,
    pub graph: Option<GraphView>,
}

impl RunDetailView {
    pub fn push_artifact(&mut self, artifact: ArtifactView) {
        push_tail_capped(&mut self.artifacts, artifact, DETAIL_ARTIFACT_CAP);
    }
}

/// Parse a unified diff. Total: anything unrecognized is context, so a
/// surprising patch renders plainly instead of vanishing.
pub fn parse_diff(patch: &str) -> DiffView {
    let mut view = DiffView::default();
    for line in patch.lines() {
        // `diff --git a/x b/x` names the file; the `---`/`+++` pair that
        // follows repeats it, so only the header is kept.
        if let Some(rest) = line.strip_prefix("diff --git ") {
            let path = rest
                .split_whitespace()
                .next_back()
                .and_then(|p| p.strip_prefix("b/"))
                .unwrap_or(rest)
                .to_string();
            view.files += 1;
            view.lines.push(DiffLine::File(path));
        } else if line.starts_with("@@") {
            view.lines.push(DiffLine::Hunk(line.to_string()));
        } else if line.starts_with("+++") || line.starts_with("---") {
            // Redundant with the `diff --git` header above.
        } else if line.starts_with("index ")
            || line.starts_with("new file")
            || line.starts_with("deleted file")
            || line.starts_with("similarity ")
            || line.starts_with("rename ")
        {
            // Metadata: real, but not what a reviewer is scanning for.
        } else if let Some(rest) = line.strip_prefix('+') {
            view.added += 1;
            view.lines.push(DiffLine::Added(rest.to_string()));
        } else if let Some(rest) = line.strip_prefix('-') {
            view.removed += 1;
            view.lines.push(DiffLine::Removed(rest.to_string()));
        } else if line.starts_with('…') {
            view.lines.push(DiffLine::Note(line.to_string()));
        } else {
            view.lines.push(DiffLine::Context(
                line.strip_prefix(' ').unwrap_or(line).to_string(),
            ));
        }
    }
    view
}

/// One attempt at a shared objective, and what it produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttemptOutcome {
    pub run_id: String,
    pub engine: String,
    pub model: Option<String>,
    pub state: String,
    /// `None` means the check has not run, which is NOT a pass. An attempt
    /// nobody judged is the thing this feature exists to stop being mistaken
    /// for a working one.
    pub passed: Option<bool>,
    pub changed_files: usize,
    pub selected: bool,
}

/// What the sandbox canaries last reported.
///
/// A run is refused outright when the sandbox is not ready, so this is the
/// difference between a first launch that explains itself and one that accepts
/// an objective and then blocks on something the user was never shown.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SandboxView {
    pub ready: bool,
    pub problems: Vec<String>,
}

/// One run, as replay describes it. The coordinator is a history of these,
/// not a single live conversation — a relaunched app must show what already
/// happened, the way any agent CLI does.
#[derive(Debug, Clone, PartialEq)]
pub struct RunView {
    pub id: String,
    pub project_id: String,
    pub objective: String,
    pub state: String,
    pub engine: String,
    /// The run this one continues; a thread is a parent chain.
    pub parent_run_id: Option<String>,
    /// The competing-attempts group this run belongs to, if any.
    pub attempt_group: Option<String>,
}

/// Everything the renderer draws. Daemon-derived fields are written by the
/// client thread; input/selection fields are owned by the UI thread.
#[derive(Debug, Default)]
pub struct UiState {
    pub connected: bool,
    /// Last status or error line, shown in the top bar.
    pub status: String,
    pub projects: Vec<Project>,
    pub selected_project: usize,
    /// Engine setup as the daemon detected it, refreshed on connect.
    pub engines: Vec<EngineStatus>,
    /// `None` until the canaries have been run: not knowing is a third state,
    /// and reporting it as either pass or fail would be a guess.
    pub sandbox: Option<SandboxView>,
    /// The user picked an engine/model deliberately, so selecting a run to
    /// READ it must not overwrite that choice.
    pub execution_pinned_by_user: bool,
    pub engine: String,
    /// Per-run provider choices. They are copied into `run.enqueue`; changing
    /// these values never mutates an existing run.
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    /// Verification command attached to the next run.
    pub check_command: Option<String>,
    pub input: String,
    /// Every run the ledger knows, oldest first.
    pub runs: Vec<RunView>,
    /// Which run the coordinator is showing. Follows the newest run unless the
    /// user picks an older one.
    pub run_id: Option<String>,
    pub run_state: String,
    /// Conversation per run, so switching runs shows that run's history and a
    /// second run never overwrites the first.
    pub chat_by_run: HashMap<String, Vec<String>>,
    /// Structured detail per run. Selection points into this cache instead of
    /// replacing it, so switching runs never destroys replayed detail.
    pub run_details: HashMap<String, RunDetailView>,
    /// Raw event tail (right pane).
    pub events: Vec<String>,
    /// Structured cards: worktree, checks, diff, commit.
    pub summary: Vec<String>,
    /// The run's plan, when it has one.
    pub graph: Option<GraphView>,
    /// The run's patch, when it committed one.
    pub diff: Option<DiffView>,
    /// The latest check command's output, kept whole so it can be read rather
    /// than summarized into a verdict.
    pub check_output: Vec<String>,
    /// Lines scrolled back from the bottom of the event tail. 0 follows live.
    pub scrollback: usize,
    /// Worktree of the current run, for "open in…".
    pub worktree: Option<String>,
    /// Provider-owned Codex/Claude history metadata, never transcript bodies.
    pub history: HistoryPageState,
    /// Daemon-managed worktrees and their reclaim eligibility.
    pub worktrees: WorktreePageState,
    /// What needs the user, and what has already been read.
    pub attention: crate::attention::AttentionState,
    /// The last update check's verdict.
    pub update_status: crate::update::UpdateStatus,
    /// Highest ledger sequence the daemon called history at subscribe time.
    /// Everything at or below it is replay and must not raise an alert.
    pub replay_through_seq: i64,
    /// Highest globally ordered event already folded into this projection.
    /// Reconnect replay at or below this cursor is ignored, so transcript,
    /// activity, queue, and attention state remain idempotent.
    pub last_sequence: u64,
    /// Persisted app settings mirrored from the daemon.
    pub settings: SettingsView,
    /// Ledger-derived usage summary. This never estimates or prices spend.
    pub usage_summary: UsageSummaryView,
    /// Durable objective and steering intent, ordered by the daemon.
    pub queue: crate::queue::QueueView,
}

impl UiState {
    pub fn selected_project(&self) -> Option<&Project> {
        self.projects.get(self.selected_project)
    }

    pub fn selected_engine_status(&self) -> Option<&EngineStatus> {
        self.engines
            .iter()
            .find(|engine| engine.name == self.engine)
    }

    pub fn selected_model(&self) -> Option<&EngineModelView> {
        let selected = self.model.as_deref().unwrap_or("");
        self.selected_engine_status()?
            .models
            .iter()
            .find(|model| model.id == selected)
    }

    /// Switch providers and choose that provider's declared default model and
    /// effort as one atomic execution selection.
    pub fn set_engine(&mut self, engine: impl Into<String>) {
        self.execution_pinned_by_user = true;
        self.engine = engine.into();
        // Start from what the user chose LAST time for this engine, rather
        // than making them re-pick after every switch. Reconciliation still
        // drops a saved model the provider no longer offers.
        self.model = self
            .settings
            .values
            .default_models
            .get(&self.engine)
            .cloned();
        self.reasoning_effort = self
            .settings
            .values
            .default_reasoning_efforts
            .get(&self.engine)
            .cloned();
        self.reconcile_execution_selection();
    }

    /// Keep the local selection valid after a capability refresh. Missing
    /// catalogs deliberately fall back to provider defaults.
    pub fn reconcile_execution_selection(&mut self) {
        let Some(engine) = self.selected_engine_status() else {
            self.model = None;
            self.reasoning_effort = None;
            return;
        };
        // Reconcile against what the picker actually offers, so a persisted
        // pick of a routed catalog entry snaps back to a real model instead
        // of failing at the provider.
        let offered = offered_models(engine);
        if offered.is_empty() {
            self.model = None;
            self.reasoning_effort = None;
            return;
        }

        let selected_id = self.model.as_deref().unwrap_or("");
        let model = offered
            .iter()
            .find(|model| model.id == selected_id)
            .or_else(|| offered.iter().find(|model| model.is_default))
            .or_else(|| offered.first())
            .copied()
            .expect("non-empty offered catalog")
            .clone();
        self.model = (!model.id.is_empty()).then_some(model.id);
        let current_effort = self.reasoning_effort.as_deref();
        self.reasoning_effort = current_effort
            .filter(|effort| model.reasoning_efforts.iter().any(|item| item == *effort))
            .map(str::to_string)
            .or(model.default_reasoning_effort)
            .or_else(|| model.reasoning_efforts.first().cloned());
    }

    pub fn select_model(&mut self, id: &str) -> bool {
        let Some(model) = self
            .selected_engine_status()
            .and_then(|engine| {
                offered_models(engine)
                    .into_iter()
                    .find(|model| model.id == id)
            })
            .cloned()
        else {
            return false;
        };
        self.execution_pinned_by_user = true;
        self.model = (!model.id.is_empty()).then_some(model.id);
        self.reasoning_effort = model
            .default_reasoning_effort
            .or_else(|| model.reasoning_efforts.first().cloned());
        true
    }

    pub fn select_reasoning_effort(&mut self, effort: &str) -> bool {
        let valid = self
            .selected_model()
            .is_some_and(|model| model.reasoning_efforts.iter().any(|item| item == effort));
        if valid {
            self.execution_pinned_by_user = true;
            self.reasoning_effort = Some(effort.to_string());
        }
        valid
    }

    pub fn model_label(&self) -> String {
        self.selected_model()
            .map(|model| model.display_name.clone())
            .unwrap_or_else(|| format!("{} default", title_case(&self.engine)))
    }

    pub fn execution_selection_label(&self) -> String {
        match self.reasoning_effort.as_deref() {
            Some(effort) => format!("{} · {}", self.model_label(), title_case(effort)),
            None => self.model_label(),
        }
    }

    pub fn selected_detail(&self) -> Option<&RunDetailView> {
        self.run_id
            .as_deref()
            .and_then(|run_id| self.run_details.get(run_id))
    }

    /// The selected run's conversation, including everything it continues.
    /// A follow-up is a new run but the same thread, so the transcript must
    /// read as one conversation rather than restarting on every reply.
    pub fn chat(&self) -> Vec<String> {
        self.thread_run_ids()
            .into_iter()
            .filter_map(|id| self.chat_by_run.get(&id))
            .flatten()
            .cloned()
            .collect()
    }

    /// Every turn of the selected thread, oldest first.
    ///
    /// The sidebar lists THREADS and selects their ROOT, so walking upward to
    /// ancestors finds nothing: a follow-up turn is a DESCENDANT. Walking the
    /// chain downward is what makes a reply appear at all — before it, every
    /// turn after the first was invisible, and switching threads and back read
    /// as the app having dropped the conversation.
    pub fn thread_run_ids(&self) -> Vec<String> {
        let Some(selected) = self.run_id.as_deref() else {
            return Vec::new();
        };
        let root = match self.thread_root(selected) {
            Some(root) => root.id.clone(),
            None => selected.to_string(),
        };
        let mut chain = vec![root.clone()];
        let mut cursor = root;
        while let Some(child) = self
            .runs
            .iter()
            .find(|run| run.parent_run_id.as_deref() == Some(cursor.as_str()))
        {
            if chain.contains(&child.id) {
                break; // Defensive: a cycle would hang the render.
            }
            chain.push(child.id.clone());
            cursor = child.id.clone();
        }
        chain
    }

    /// The attempts answering the same objective as the selected run, with
    /// what each one produced — oldest first.
    ///
    /// Returns empty unless the selected run is part of a group, so this
    /// surface never appears for an ordinary run.
    pub fn sibling_attempts(&self) -> Vec<AttemptOutcome> {
        let Some(selected) = self.run_id.as_deref() else {
            return Vec::new();
        };
        let Some(group) = self
            .runs
            .iter()
            .find(|run| run.id == selected)
            .and_then(|run| run.attempt_group.clone())
        else {
            return Vec::new();
        };
        self.runs
            .iter()
            .filter(|run| run.attempt_group.as_deref() == Some(group.as_str()))
            .map(|run| {
                let detail = self.run_details.get(&run.id);
                // The check is the adjudicator. Its absence is reported as
                // "not judged yet", never as a pass.
                let checks = detail.map(|d| d.checks.as_slice()).unwrap_or(&[]);
                let passed = checks.last().and_then(|check| check.passed);
                AttemptOutcome {
                    run_id: run.id.clone(),
                    engine: run.engine.clone(),
                    model: detail.and_then(|d| d.model.clone()),
                    state: run.state.clone(),
                    passed,
                    changed_files: detail.map(|d| d.changed_files.len()).unwrap_or(0),
                    selected: run.id == selected,
                }
            })
            .collect()
    }

    /// An existing empty draft on `engine` in this project, if there is one.
    ///
    /// Pressing `+` twice without typing anything used to leave two identical
    /// "New run" rows, and nothing ever removed them: they are real persisted
    /// runs, so the client cannot quietly discard one, and the user is left
    /// tidying up after a button they pressed by mistake. Reusing the draft
    /// that is already there means the second press selects the first row
    /// instead of adding another.
    pub fn reusable_draft(&self, project_id: &str, engine: &str) -> Option<&RunView> {
        self.runs.iter().find(|run| {
            run.project_id == project_id
                && run.engine == engine
                && run.state == "draft"
                && run.objective.trim().is_empty()
                // Only a thread root: a draft continuing a thread carries that
                // thread's context and is not interchangeable with a new one.
                && run.parent_run_id.is_none()
        })
    }

    /// The selected run when it is a draft that has not been given an
    /// objective yet — the row the sidebar's `+` created.
    ///
    /// A submit while this is set fills that run in instead of starting a new
    /// one, which is what keeps one click and one objective to one row.
    pub fn selected_draft_awaiting_objective(&self) -> Option<&RunView> {
        let id = self.run_id.as_deref()?;
        self.runs
            .iter()
            .find(|run| run.id == id && run.state == "draft" && run.objective.trim().is_empty())
    }

    /// The root of a run's thread: the first turn the user started.
    pub fn thread_root(&self, run_id: &str) -> Option<&RunView> {
        let mut node = self.runs.iter().find(|r| r.id == run_id)?;
        let mut guard = 0;
        while let Some(parent) = node.parent_run_id.as_deref() {
            let Some(next) = self.runs.iter().find(|r| r.id == parent) else {
                break;
            };
            node = next;
            guard += 1;
            if guard > self.runs.len() {
                break; // Defensive: a cycle must not hang the render.
            }
        }
        Some(node)
    }

    /// Thread roots for a project, in creation order. The sidebar lists
    /// THREADS, not runs: a follow-up is another turn of an existing
    /// conversation, and giving it its own row makes the app look like it
    /// forgot you and started over.
    pub fn threads_of(&self, project_id: &str) -> Vec<&RunView> {
        self.runs
            .iter()
            .filter(|r| r.project_id == project_id && r.parent_run_id.is_none())
            .collect()
    }

    /// The newest turn of the thread `run_id` belongs to. This is both what a
    /// reply continues and what the row's status reflects.
    pub fn tip_of(&self, run_id: &str) -> Option<&RunView> {
        let mut tip = self.runs.iter().find(|r| r.id == run_id)?;
        let mut guard = 0;
        while let Some(child) = self
            .runs
            .iter()
            .find(|r| r.parent_run_id.as_deref() == Some(tip.id.as_str()))
        {
            tip = child;
            guard += 1;
            if guard > self.runs.len() {
                break;
            }
        }
        Some(tip)
    }

    /// Whether `run_id`'s thread is the one on screen.
    pub fn thread_is_selected(&self, run_id: &str) -> bool {
        let Some(selected) = self.run_id.as_deref() else {
            return false;
        };
        match (self.thread_root(run_id), self.thread_root(selected)) {
            (Some(a), Some(b)) => a.id == b.id,
            _ => false,
        }
    }

    /// The run a follow-up should continue: the newest run in the selected
    /// thread, so a reply extends the conversation rather than forking it.
    pub fn thread_tip(&self) -> Option<&RunView> {
        // `tip_of` is the same downward walk, and it has the cycle guard this
        // one was missing.
        self.tip_of(self.run_id.as_deref()?)
    }

    /// Append to a run's history, whether or not it is the one on screen.
    fn say(&mut self, run_id: &str, line: String) {
        self.chat_by_run
            .entry(run_id.to_string())
            .or_default()
            .push(line);
    }

    /// Show this run in the coordinator.
    pub fn select_run(&mut self, run_id: &str) {
        self.run_id = Some(run_id.to_string());
        self.run_state = self
            .runs
            .iter()
            .find(|r| r.id == run_id)
            .map(|r| r.state.clone())
            .unwrap_or_default();
        // Adopt what this run used, so the composer shows where a follow-up
        // would go — UNLESS the user deliberately chose something. Opening an
        // old run to read it used to silently replace a pick they had just
        // made, and the next objective then went somewhere they did not
        // choose. Reading is not choosing.
        if let Some(detail) = self.run_details.get(run_id)
            && !self.execution_pinned_by_user
        {
            if let Some(engine) = detail.engine.clone() {
                self.engine = engine;
            }
            self.model.clone_from(&detail.model);
            self.reasoning_effort.clone_from(&detail.reasoning_effort);
            self.reconcile_execution_selection();
        }
        self.sync_legacy_selected_detail();
    }

    fn detail_mut(&mut self, run_id: &str) -> &mut RunDetailView {
        let engine = self
            .runs
            .iter()
            .find(|r| r.id == run_id)
            .map(|r| r.engine.clone())
            .filter(|engine| !engine.is_empty());
        match self.run_details.entry(run_id.to_string()) {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => entry.insert(RunDetailView {
                run_id: run_id.to_string(),
                engine,
                ..RunDetailView::default()
            }),
        }
    }

    fn sync_legacy_selected_detail(&mut self) {
        self.summary.clear();
        self.graph = None;
        self.diff = None;
        self.check_output.clear();
        self.worktree = None;
        let Some(detail) = self.selected_detail().cloned() else {
            return;
        };
        self.graph = detail.graph.clone();
        self.diff = detail.diff.clone();
        if let Some(shape) = detail.route_shape.as_deref() {
            self.summary.push(format!("routed {shape}"));
        }
        if let Some(worktree) = &detail.worktree
            && let Some(path) = worktree.path.as_deref()
        {
            self.summary.push(match worktree.branch.as_deref() {
                Some(branch) => format!("worktree {path} on {branch}"),
                None => format!("worktree {path}"),
            });
        }
        if let Some(diff) = &detail.diff {
            self.summary.push(format!(
                "{} file(s) +{} -{}",
                diff.files, diff.added, diff.removed
            ));
        }
        for check in &detail.checks {
            let verdict = match check.passed {
                Some(true) => "passed",
                Some(false) => "FAILED",
                None => "unknown",
            };
            self.summary
                .push(format!("check {verdict}: {}", check.name));
        }
        if !detail.artifacts.is_empty() {
            self.summary
                .push(format!("{} artifact(s)", detail.artifacts.len()));
        }
        self.worktree = detail.worktree.and_then(|worktree| worktree.path);
        self.check_output = check_output_lines(&detail.checks);
    }

    /// Whether an event is worth a line in the activity pane.
    ///
    /// Streaming deltas are the bulk of the ledger and say nothing on their
    /// own — twenty `engine.text_delta` rows tell you a model is typing, which
    /// the status mark already does. The pane exists to show what HAPPENED.
    fn is_noise(kind: &str) -> bool {
        matches!(
            kind,
            // NOT engine.terminal: for an agent that speaks no structured
            // protocol, the painted lines are the only account of the work.
            "engine.text_delta" | "engine.usage" | "engine.session" | "chat.message"
        )
    }

    fn push_event(&mut self, line: String) {
        self.events.push(line);
        if self.events.len() > EVENT_TAIL {
            let overflow = self.events.len() - EVENT_TAIL;
            self.events.drain(0..overflow);
        }
    }
}

/// What a response means once it comes back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Pending {
    Projects,
    ProjectAdd,
    /// A repository being created because the user prompted with none
    /// chosen; the held objective starts on the new project when the reply
    /// lands.
    ProjectCreated {
        objective: String,
        engine: String,
        model: Option<String>,
        reasoning_effort: Option<String>,
        check_command: Option<String>,
    },
    Engines,
    /// A draft created by the sidebar's `+`, waiting to be selected so the
    /// composer types into it.
    DraftCreated,
    SandboxCheck,
    RunEnqueue,
    QueueList,
    QueueMutation,
    HistoryList {
        append: bool,
    },
    HistoryAdopt {
        provider: String,
        source_id: String,
    },
    SettingsGet,
    SettingsUpdate,
    UsageSummary,
    WorktreeList,
    WorktreeReclaim {
        path: String,
        dry_run: bool,
    },
    /// Carries the ledger head at subscribe time, which separates the replay
    /// that follows from the live events after it.
    Subscribe,
    /// Response is only interesting when it carries an error.
    Report(&'static str),
}

/// Work the UI asks the daemon to do.
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    AddProject {
        name: String,
        path: String,
    },
    /// Create a brand-new repository under ~/AutoHarness (`project.create`)
    /// and start the held objective on it once it exists. What prompting
    /// with no repository chosen does.
    CreateProjectAndStart {
        name: String,
        objective: String,
        engine: String,
        model: Option<String>,
        reasoning_effort: Option<String>,
        check_command: Option<String>,
    },
    RefreshProjects,
    RefreshEngines,
    /// Re-run the sandbox canaries and report what they found.
    CheckSandbox,
    /// Answer one objective several ways at once and compare the results.
    StartAttempts {
        project_id: String,
        objective: String,
        attempts: Vec<(String, Option<String>, Option<String>)>,
        check_command: Option<String>,
    },
    /// Reply to a question the running engine asked.
    AnswerRun {
        run_id: String,
        answer: autoharness_core::Answer,
    },
    /// Create a run that exists but has not been asked to do anything yet.
    ///
    /// This is what the sidebar's `+` sends. The row has to appear — and be
    /// dated — the moment it is clicked, and the only honest way to show a run
    /// is for one to exist; a placeholder drawn by the client would be a claim
    /// about state the daemon has never heard of.
    CreateDraftRun {
        project_id: String,
        engine: String,
        model: Option<String>,
        reasoning_effort: Option<String>,
    },
    /// Give a draft its objective and queue it.
    StartDraft {
        run_id: String,
        project_id: String,
        objective: String,
        check_command: Option<String>,
    },
    StartRun {
        project_id: String,
        engine: String,
        model: Option<String>,
        reasoning_effort: Option<String>,
        objective: String,
        check_command: Option<String>,
        /// Continue this run's thread instead of starting a new one.
        parent_run_id: Option<String>,
    },
    Approve {
        run_id: String,
    },
    Chat {
        run_id: String,
        message: String,
    },
    Interrupt {
        run_id: String,
        message: String,
    },
    Control {
        run_id: String,
        method: &'static str,
    },
    RefreshHistory {
        cursor: Option<String>,
    },
    AdoptHistory {
        provider: String,
        source_id: String,
        project_id: String,
        engine: String,
    },
    RefreshSettings,
    UpdateSettings(proto::params::SettingsUpdate),
    RefreshUsage,
    RefreshQueue,
    MoveQueueItem {
        item_id: String,
        before_item_id: Option<String>,
    },
    CancelQueueItem {
        item_id: String,
    },
    NodeControl {
        run_id: String,
        node_id: String,
        retry: bool,
    },
    RefreshWorktrees(WorktreeFilter),
    /// A dry run reports the blockers and changes nothing. A real reclaim
    /// echoes the path back as `confirm_path`, which the daemon requires.
    ReclaimWorktree {
        path: String,
        dry_run: bool,
    },
}

/// Handle held by the UI thread.
pub struct DaemonClient {
    commands: mpsc::UnboundedSender<Command>,
    pub state: Arc<Mutex<UiState>>,
    preview: bool,
}

impl DaemonClient {
    /// Spawn the client thread. Never blocks; connection problems surface in
    /// `state.status` rather than failing the UI.
    pub fn spawn() -> Self {
        let state = Arc::new(Mutex::new(UiState {
            engine: "codex".into(),
            status: "connecting to daemon…".into(),
            ..UiState::default()
        }));
        let (commands, rx) = mpsc::unbounded_channel();
        let thread_state = Arc::clone(&state);
        let thread_commands = commands.clone();
        std::thread::Builder::new()
            .name("autoharness-daemon-client".into())
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(e) => {
                        set_status(&thread_state, format!("client runtime failed: {e}"));
                        return;
                    }
                };
                runtime.block_on(run_client(thread_state, thread_commands, rx));
            })
            .expect("spawn daemon client thread");
        Self {
            commands,
            state,
            preview: false,
        }
    }

    /// Build a UI-only client around a deterministic fixture.
    ///
    /// Preview mode deliberately owns no daemon connection and starts no
    /// background IO. The channel has no receiver, so attempted commands fail
    /// locally and are ignored by [`send`].
    pub fn preview(state: UiState) -> Self {
        let (commands, rx) = mpsc::unbounded_channel();
        drop(rx);
        Self {
            commands,
            state: Arc::new(Mutex::new(state)),
            preview: true,
        }
    }

    pub fn send(&self, command: Command) {
        let _ = self.commands.send(command);
    }

    pub fn is_preview(&self) -> bool {
        self.preview
    }
}

fn set_status(state: &Arc<Mutex<UiState>>, status: String) {
    if let Ok(mut state) = state.lock() {
        state.status = status;
    }
}

/// The git repository the app was launched from, if any.
///
/// Launching from a terminal inside a project should be enough — the same way
/// a CLI agent already knows where it is. Walking up for `.git` finds the repo
/// root from anywhere inside it, so `cd crates/ui && autoharness` still picks
/// the workspace, not a subdirectory.
pub fn repo_at_cwd() -> Option<(String, String)> {
    let mut dir = std::env::current_dir().ok()?;
    loop {
        // `.git` is a directory in a normal clone and a FILE in a worktree;
        // both mark a usable repository root.
        if dir.join(".git").exists() {
            let name = dir
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| dir.to_string_lossy().into_owned());
            return Some((name, dir.to_string_lossy().into_owned()));
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// Locate `autoharnessd`. The shipped bundle puts both executables in the same
/// directory; during development the caller may itself live in `examples/` or
/// `deps/`, so the parent is searched too, then PATH.
fn find_daemon_binary() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let mut roots = Vec::new();
    if let Some(dir) = exe.parent() {
        roots.push(dir.to_path_buf());
        if let Some(up) = dir.parent() {
            roots.push(up.to_path_buf());
        }
    }
    roots
        .into_iter()
        .map(|dir| dir.join("autoharnessd"))
        .find(|candidate| candidate.is_file())
        .or_else(|| autoharness_engines::process::find_binary("autoharnessd"))
}

fn open_daemon_log(data_dir: &Path) -> std::io::Result<File> {
    std::fs::create_dir_all(data_dir)?;
    let path = data_dir.join("daemon.log");
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)?;
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    Ok(file)
}

fn bounded_log_tail(bytes: &[u8], max_bytes: usize) -> String {
    if bytes.is_empty() || max_bytes == 0 {
        return String::new();
    }
    let mut start = bytes.len().saturating_sub(max_bytes);
    if start > 0
        && bytes[start - 1] != b'\n'
        && let Some(next_line) = bytes[start..].iter().position(|byte| *byte == b'\n')
    {
        start += next_line + 1;
    }
    String::from_utf8_lossy(&bytes[start..]).trim().to_string()
}

fn daemon_log_tail(data_dir: &Path) -> String {
    std::fs::read(data_dir.join("daemon.log"))
        .map(|bytes| bounded_log_tail(&bytes, 2_048))
        .unwrap_or_default()
}

fn append_daemon_diagnostic(data_dir: &Path, diagnostic: &str) {
    if let Ok(mut log) = open_daemon_log(data_dir) {
        let _ = writeln!(log, "AutoHarness launcher: {diagnostic}");
    }
}

/// Start the daemon. It owns all state and must outlive this window.
fn spawn_daemon(data_dir: &Path) -> std::io::Result<std::process::Child> {
    let daemon = find_daemon_binary().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "autoharnessd not found next to this binary or on PATH",
        )
    })?;
    let mut log = open_daemon_log(data_dir)?;
    writeln!(log, "AutoHarness launcher: starting {}", daemon.display())?;
    let stdout = log.try_clone()?;
    std::process::Command::new(&daemon)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(stdout))
        .stderr(std::process::Stdio::from(log))
        .spawn()
}

/// Opening the app should be enough — the user should never have to start a
/// background service by hand.
///
/// **A failed connection is the evidence, not the socket file.** A daemon that
/// exits without cleaning up leaves the socket behind, and treating that file
/// as "a daemon is coming" strands the app forever on the most common case:
/// the previous run crashed or was killed. A live daemon accepts connections;
/// if the connect failed, nothing is serving that path, so it is safe to start
/// one. The daemon unlinks and rebinds the socket at startup, which clears the
/// stale file.
async fn connect_starting_daemon_if_needed(
    state: &Arc<Mutex<UiState>>,
    config: &DaemonConfig,
) -> Option<tokio::net::UnixStream> {
    if let Ok(stream) = tokio::net::UnixStream::connect(&config.socket_path).await {
        return Some(stream);
    }
    set_status(state, "starting the AutoHarness daemon…".into());
    let mut child = match spawn_daemon(&config.data_dir) {
        Ok(child) => child,
        Err(e) => {
            append_daemon_diagnostic(&config.data_dir, &format!("spawn failed: {e}"));
            set_status(state, format!("cannot start autoharnessd: {e}"));
            return None;
        }
    };

    // Startup runs the sandbox canaries before binding, so allow real time.
    // Spawned once only: retrying the spawn would race a daemon that is simply
    // slow, and the second would rebind the socket out from under the first.
    for _ in 0..120 {
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        if let Ok(stream) = tokio::net::UnixStream::connect(&config.socket_path).await {
            return Some(stream);
        }
        if let Ok(Some(exit)) = child.try_wait() {
            let tail = daemon_log_tail(&config.data_dir);
            let diagnostic = if tail.is_empty() {
                format!("daemon exited during startup ({exit})")
            } else {
                format!("daemon exited during startup ({exit}): {tail}")
            };
            set_status(state, diagnostic);
            return None;
        }
    }
    let tail = daemon_log_tail(&config.data_dir);
    let diagnostic = if tail.is_empty() {
        format!(
            "daemon did not come up at {} — see {}",
            config.socket_path.display(),
            config.data_dir.join("daemon.log").display()
        )
    } else {
        format!("daemon startup timed out: {tail}")
    };
    set_status(state, diagnostic);
    None
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ConnectionExit {
    Retry { reason: String, was_connected: bool },
    Shutdown,
}

enum ReaderOutcome {
    Frame(Frame),
    Closed,
    Error(String),
}

fn current_status(state: &Arc<Mutex<UiState>>) -> String {
    state
        .lock()
        .map(|state| state.status.clone())
        .unwrap_or_else(|_| "daemon connection failed".into())
}

async fn write_command<W: tokio::io::AsyncWrite + Unpin>(
    writer: &mut W,
    pending: &mut HashMap<String, Pending>,
    client_session: &str,
    counter: &mut u64,
    command: Command,
) -> Result<(), String> {
    let (method, params, kind) = encode(command);
    *counter += 1;
    let id = format!("ui-{client_session}-{counter}");
    let request = Request::new(id.clone(), method, params);
    proto::write_frame(writer, &request)
        .await
        .map_err(|error| error.to_string())?;
    pending.insert(id, kind);
    Ok(())
}

/// Keep reconnecting until the process exits. A socket loss is a recoverable
/// transport condition, not a reason to strand a still-open desktop window.
async fn run_client(
    state: Arc<Mutex<UiState>>,
    commands: mpsc::UnboundedSender<Command>,
    mut rx: mpsc::UnboundedReceiver<Command>,
) {
    let config = DaemonConfig::default_paths();
    let mut retry_delay = std::time::Duration::from_millis(500);
    loop {
        match run_connection(&state, &commands, &mut rx, &config).await {
            ConnectionExit::Shutdown => return,
            ConnectionExit::Retry {
                reason,
                was_connected,
            } => {
                if let Ok(mut state) = state.lock() {
                    state.connected = false;
                    state.status = format!("{reason} · retrying automatically");
                }
                if was_connected {
                    retry_delay = std::time::Duration::from_millis(500);
                }
                tokio::time::sleep(retry_delay).await;
                retry_delay = (retry_delay * 2).min(std::time::Duration::from_secs(5));
                set_status(&state, "reconnecting to the AutoHarness daemon…".into());
            }
        }
    }
}

/// Connect, authenticate, subscribe, then pump frames until the socket closes.
/// `read_frame` owns a dedicated task because it is not cancel-safe; the outer
/// select only polls channels, so commands survive across reconnects.
async fn run_connection(
    state: &Arc<Mutex<UiState>>,
    commands: &mpsc::UnboundedSender<Command>,
    rx: &mut mpsc::UnboundedReceiver<Command>,
    config: &DaemonConfig,
) -> ConnectionExit {
    let token = match load_or_create_token(&config.data_dir) {
        Ok((token, _)) => token,
        Err(e) => {
            return ConnectionExit::Retry {
                reason: format!("cannot read client token: {e}"),
                was_connected: false,
            };
        }
    };

    let stream = match connect_starting_daemon_if_needed(state, config).await {
        Some(stream) => stream,
        None => {
            return ConnectionExit::Retry {
                reason: current_status(state),
                was_connected: false,
            };
        }
    };
    let (mut reader, mut writer) = tokio::io::split(stream);
    let client_session = uuid::Uuid::new_v4().simple().to_string();

    let hello = Request::new(
        format!("auth-{client_session}"),
        methods::AUTH_HELLO,
        json!({ "token": token }),
    );
    if let Err(e) = proto::write_frame(&mut writer, &hello).await {
        return ConnectionExit::Retry {
            reason: format!("handshake failed: {e}"),
            was_connected: false,
        };
    }
    match proto::read_frame::<_, proto::Response>(&mut reader).await {
        Ok(Some(response)) if response.error.is_none() => {}
        Ok(Some(response)) => {
            let message = response
                .error
                .map(|e| e.message)
                .unwrap_or_else(|| "unknown".into());
            return ConnectionExit::Retry {
                reason: format!("unauthorized: {message}"),
                was_connected: false,
            };
        }
        Ok(None) => {
            return ConnectionExit::Retry {
                reason: "daemon closed during authentication".into(),
                was_connected: false,
            };
        }
        Err(e) => {
            return ConnectionExit::Retry {
                reason: format!("handshake failed: {e}"),
                was_connected: false,
            };
        }
    }
    if let Ok(mut state) = state.lock() {
        state.connected = true;
        state.status = "connected".into();
    }

    let mut pending = HashMap::<String, Pending>::new();

    // The first connection replays everything. A reconnect asks only for the
    // missing suffix and still rejects any duplicate sequence defensively.
    let since_sequence = state.lock().map(|state| state.last_sequence).unwrap_or(0);
    let subscribe = Request::new(
        format!("sub-{client_session}"),
        methods::EVENTS_SUBSCRIBE,
        json!({ "since_sequence": since_sequence }),
    );
    pending.insert(subscribe.id.clone(), Pending::Subscribe);
    if let Err(error) = proto::write_frame(&mut writer, &subscribe).await {
        return ConnectionExit::Retry {
            reason: format!("subscribe failed: {error}"),
            was_connected: true,
        };
    }
    if let Ok(mut state) = state.lock() {
        state.history.loading = true;
        state.settings.loading = true;
        state.settings.pending_response = Some(methods::SETTINGS_GET.into());
        state.usage_summary.loading = true;
        state.usage_summary.pending_response = Some(methods::USAGE_SUMMARY.into());
        state.queue.loading = true;
    }
    let (reader_tx, mut reader_rx) = mpsc::unbounded_channel();
    let reader_task = tokio::spawn(async move {
        loop {
            let outcome = match proto::read_frame::<_, Frame>(&mut reader).await {
                Ok(Some(frame)) => ReaderOutcome::Frame(frame),
                Ok(None) => ReaderOutcome::Closed,
                Err(error) => ReaderOutcome::Error(error.to_string()),
            };
            let terminal = !matches!(outcome, ReaderOutcome::Frame(_));
            if reader_tx.send(outcome).is_err() || terminal {
                return;
            }
        }
    });

    // Request ids are the daemon's idempotency key and its dedup cache outlives
    // one socket. The random session prefix prevents a relaunched or
    // reconnected UI from receiving an unrelated cached answer.
    let mut counter = 0_u64;
    for command in startup_commands() {
        if let Err(error) = write_command(
            &mut writer,
            &mut pending,
            &client_session,
            &mut counter,
            command,
        )
        .await
        {
            reader_task.abort();
            return ConnectionExit::Retry {
                reason: format!("startup request failed: {error}"),
                was_connected: true,
            };
        }
    }

    loop {
        tokio::select! {
            command = rx.recv() => {
                let Some(command) = command else {
                    reader_task.abort();
                    return ConnectionExit::Shutdown;
                };
                if let Err(error) = write_command(
                    &mut writer,
                    &mut pending,
                    &client_session,
                    &mut counter,
                    command,
                ).await {
                    reader_task.abort();
                    return ConnectionExit::Retry {
                        reason: format!("connection lost while sending: {error}"),
                        was_connected: true,
                    };
                }
            }
            outcome = reader_rx.recv() => match outcome {
                Some(ReaderOutcome::Frame(Frame::Event(event))) => apply_event(state, event),
                Some(ReaderOutcome::Frame(Frame::Response(response))) => {
                    let kind = pending.remove(&response.id);
                    apply_response(state, commands, kind, response);
                }
                // The daemon never sends requests to the UI.
                Some(ReaderOutcome::Frame(Frame::Request(_))) => {}
                Some(ReaderOutcome::Closed) => {
                    return ConnectionExit::Retry {
                        reason: "daemon closed the connection".into(),
                        was_connected: true,
                    };
                }
                Some(ReaderOutcome::Error(error)) => {
                    return ConnectionExit::Retry {
                        reason: format!("connection error: {error}"),
                        was_connected: true,
                    };
                }
                None => {
                    return ConnectionExit::Retry {
                        reason: "daemon reader stopped unexpectedly".into(),
                        was_connected: true,
                    };
                }
            }
        }
    }
}

fn startup_commands() -> Vec<Command> {
    vec![
        Command::RefreshProjects,
        // Engine setup up front: the user should learn they need to sign in
        // before they type an objective, not after a run is refused.
        Command::RefreshEngines,
        // Same reason: the sandbox is fail-closed, so a user who cannot run
        // anything should be told at launch rather than at their first
        // objective. The canaries run real sandbox-exec probes, which is why
        // this is asked for once here rather than polled.
        Command::CheckSandbox,
        Command::RefreshHistory { cursor: None },
        Command::RefreshSettings,
        Command::RefreshUsage,
        Command::RefreshQueue,
    ]
}

fn encode(command: Command) -> (&'static str, Value, Pending) {
    if let Some(encoded) = crate::history::encode_history_command(&command) {
        return encoded;
    }
    match command {
        Command::RefreshProjects => (methods::PROJECT_LIST, json!({}), Pending::Projects),
        Command::RefreshEngines => (methods::ENGINE_LIST, json!({}), Pending::Engines),
        Command::CheckSandbox => (methods::SANDBOX_CHECK, json!({}), Pending::SandboxCheck),
        Command::StartAttempts {
            project_id,
            objective,
            attempts,
            check_command,
        } => (
            methods::RUN_ATTEMPTS,
            json!({
                "project_id": project_id,
                "objective": objective,
                "check_command": check_command,
                "attempts": attempts
                    .into_iter()
                    .map(|(engine, model, reasoning_effort)| json!({
                        "engine": engine,
                        "model": model,
                        "reasoning_effort": reasoning_effort,
                    }))
                    .collect::<Vec<_>>(),
                "request_id": autoharness_core::new_id(),
            }),
            Pending::Report("run.attempts"),
        ),
        Command::AnswerRun { run_id, answer } => (
            methods::RUN_ANSWER,
            json!({ "run_id": run_id, "answer": answer }),
            Pending::Report("run.answer"),
        ),
        Command::AddProject { name, path } => (
            methods::PROJECT_ADD,
            json!({ "name": name, "path": path }),
            Pending::ProjectAdd,
        ),
        Command::CreateProjectAndStart {
            name,
            objective,
            engine,
            model,
            reasoning_effort,
            check_command,
        } => (
            methods::PROJECT_CREATE,
            json!({ "name": name }),
            Pending::ProjectCreated {
                objective,
                engine,
                model,
                reasoning_effort,
                check_command,
            },
        ),
        Command::StartRun {
            project_id,
            engine,
            model,
            reasoning_effort,
            objective,
            check_command,
            parent_run_id,
        } => (
            methods::RUN_ENQUEUE,
            json!({
                "project_id": project_id,
                "engine": engine,
                "model": model,
                "reasoning_effort": reasoning_effort,
                "objective": objective,
                "check_command": check_command,
                "parent_run_id": parent_run_id,
                "request_id": autoharness_core::new_id(),
            }),
            Pending::RunEnqueue,
        ),
        Command::CreateDraftRun {
            project_id,
            engine,
            model,
            reasoning_effort,
        } => (
            methods::RUN_CREATE,
            json!({
                "project_id": project_id,
                "engine": engine,
                "model": model,
                "reasoning_effort": reasoning_effort,
                "objective": "",
            }),
            Pending::DraftCreated,
        ),
        Command::StartDraft {
            run_id,
            project_id,
            objective,
            check_command,
        } => (
            methods::RUN_ENQUEUE,
            json!({
                "project_id": project_id,
                "run_id": run_id,
                "objective": objective,
                "check_command": check_command,
                "request_id": autoharness_core::new_id(),
            }),
            Pending::RunEnqueue,
        ),
        Command::Approve { run_id } => (
            methods::RUN_APPROVE,
            json!({ "run_id": run_id }),
            Pending::Report("run.approve"),
        ),
        Command::Chat { run_id, message } => (
            methods::CHAT_SEND,
            json!({
                "run_id": run_id,
                "message": message,
                "request_id": autoharness_core::new_id(),
            }),
            Pending::Report("chat.send"),
        ),
        Command::Interrupt { run_id, message } => (
            methods::CHAT_INTERRUPT,
            json!({ "run_id": run_id, "message": message }),
            Pending::Report("chat.interrupt"),
        ),
        Command::Control { run_id, method } => (
            method,
            json!({ "run_id": run_id }),
            Pending::Report("run control"),
        ),
        Command::RefreshSettings => (methods::SETTINGS_GET, json!({}), Pending::SettingsGet),
        Command::UpdateSettings(update) => (
            methods::SETTINGS_UPDATE,
            serde_json::to_value(update).unwrap_or_else(|_| json!({})),
            Pending::SettingsUpdate,
        ),
        Command::RefreshUsage => (methods::USAGE_SUMMARY, json!({}), Pending::UsageSummary),
        Command::RefreshQueue => (
            methods::QUEUE_LIST,
            json!({ "include_terminal": false }),
            Pending::QueueList,
        ),
        Command::MoveQueueItem {
            item_id,
            before_item_id,
        } => (
            methods::QUEUE_MOVE,
            json!({
                "item_id": item_id,
                "before_item_id": before_item_id,
                "request_id": autoharness_core::new_id(),
            }),
            Pending::QueueMutation,
        ),
        Command::CancelQueueItem { item_id } => (
            methods::QUEUE_CANCEL,
            json!({
                "item_id": item_id,
                "request_id": autoharness_core::new_id(),
            }),
            Pending::QueueMutation,
        ),
        Command::NodeControl {
            run_id,
            node_id,
            retry,
        } => (
            if retry {
                methods::NODE_RETRY
            } else {
                methods::NODE_CANCEL
            },
            json!({ "run_id": run_id, "node_id": node_id }),
            Pending::Report(if retry { "node.retry" } else { "node.cancel" }),
        ),
        Command::RefreshWorktrees(filter) => (
            methods::WORKTREE_LIST,
            json!({
                "include_reclaimed": filter == WorktreeFilter::IncludeReclaimed,
                "only_eligible": filter == WorktreeFilter::OnlyEligible,
            }),
            Pending::WorktreeList,
        ),
        Command::ReclaimWorktree { path, dry_run } => (
            methods::WORKTREE_RECLAIM,
            json!({
                "path": path,
                "dry_run": dry_run,
                // The daemon refuses a real reclaim without this exact echo.
                "confirm_path": if dry_run { None } else { Some(path.clone()) },
            }),
            Pending::WorktreeReclaim { path, dry_run },
        ),
        Command::RefreshHistory { .. } | Command::AdoptHistory { .. } => {
            unreachable!("handled above")
        }
    }
}

fn apply_response(
    state: &Arc<Mutex<UiState>>,
    commands: &mpsc::UnboundedSender<Command>,
    kind: Option<Pending>,
    response: proto::Response,
) {
    if let Some(error) = &response.error {
        let label = match &kind {
            Some(Pending::Projects) => "project.list",
            Some(Pending::ProjectAdd) => "project.add",
            Some(Pending::ProjectCreated { .. }) => methods::PROJECT_CREATE,
            Some(Pending::Engines) => "engine.list",
            Some(Pending::DraftCreated) => methods::RUN_CREATE,
            Some(Pending::SandboxCheck) => methods::SANDBOX_CHECK,
            Some(Pending::RunEnqueue) => methods::RUN_ENQUEUE,
            Some(Pending::QueueList) => methods::QUEUE_LIST,
            Some(Pending::QueueMutation) => "queue mutation",
            Some(Pending::HistoryList { .. }) => "history.list",
            Some(Pending::HistoryAdopt { .. }) => "history.adopt",
            Some(Pending::SettingsGet) => methods::SETTINGS_GET,
            Some(Pending::SettingsUpdate) => methods::SETTINGS_UPDATE,
            Some(Pending::UsageSummary) => methods::USAGE_SUMMARY,
            Some(Pending::WorktreeList) => methods::WORKTREE_LIST,
            Some(Pending::WorktreeReclaim { .. }) => methods::WORKTREE_RECLAIM,
            Some(Pending::Subscribe) => methods::EVENTS_SUBSCRIBE,
            Some(Pending::Report(name)) => name,
            None => "request",
        };
        set_status(state, format!("{label}: {}", error.message));
        if let Ok(mut state) = state.lock() {
            let message = response.error.map(|e| e.message);
            match kind {
                Some(Pending::HistoryList { .. }) | Some(Pending::HistoryAdopt { .. }) => {
                    state.history.loading = false;
                    state.history.error = message;
                }
                // A rejected settings write leaves the optimistic value on
                // screen but marked failed; the next settings.get or
                // settings.updated is what corrects it.
                Some(Pending::SettingsGet) | Some(Pending::SettingsUpdate) => {
                    state.settings.loading = false;
                    state.settings.pending_response = None;
                    state.settings.pending_update = None;
                    state.settings.error = message;
                }
                Some(Pending::UsageSummary) => {
                    state.usage_summary.loading = false;
                    state.usage_summary.pending_response = None;
                    state.usage_summary.error = message;
                }
                Some(Pending::QueueList) | Some(Pending::QueueMutation) => {
                    state.queue.loading = false;
                    state.queue.error = message;
                }
                Some(Pending::WorktreeList) | Some(Pending::WorktreeReclaim { .. }) => {
                    state.worktrees.loading = false;
                    state.worktrees.pending_confirm = None;
                    state.worktrees.error = message;
                }
                _ => {}
            }
        }
        return;
    }
    let Some(result) = response.result else {
        return;
    };
    match kind {
        Some(Pending::Projects) => {
            let projects: Vec<Project> = result
                .as_array()
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|p| {
                            Some(Project {
                                id: p.get("id")?.as_str()?.to_string(),
                                name: p.get("name")?.as_str()?.to_string(),
                                path: p.get("path")?.as_str()?.to_string(),
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            let detected = repo_at_cwd();
            let mut add: Option<Command> = None;
            if let Ok(mut state) = state.lock() {
                state.selected_project =
                    state.selected_project.min(projects.len().saturating_sub(1));
                state.projects = projects;

                // Select the repo the app was launched from; register it first
                // if this is the first time it has been seen.
                if let Some((name, path)) = detected {
                    match state.projects.iter().position(|p| p.path == path) {
                        Some(index) => {
                            state.selected_project = index;
                            state.status = format!("{name} — detected from this directory");
                        }
                        None => {
                            state.status = format!("adding {name} from this directory");
                            add = Some(Command::AddProject { name, path });
                        }
                    }
                }
            }
            if let Some(command) = add {
                let _ = commands.send(command);
                // Re-list so the new project is selected by the branch above.
                let _ = commands.send(Command::RefreshProjects);
            }
        }
        Some(Pending::ProjectAdd) => {
            let Some(project) = (|| {
                Some(Project {
                    id: result.get("id")?.as_str()?.to_string(),
                    name: result.get("name")?.as_str()?.to_string(),
                    path: result.get("path")?.as_str()?.to_string(),
                })
            })() else {
                set_status(state, "project.add returned an invalid project".into());
                return;
            };
            if let Ok(mut state) = state.lock() {
                let index = state
                    .projects
                    .iter()
                    .position(|existing| existing.id == project.id || existing.path == project.path)
                    .unwrap_or_else(|| {
                        state.projects.push(project.clone());
                        state.projects.len() - 1
                    });
                state.projects[index] = project.clone();
                state.selected_project = index;
                state.status = format!("repository: {}", project.name);
            }
        }
        Some(Pending::ProjectCreated {
            objective,
            engine,
            model,
            reasoning_effort,
            check_command,
        }) => {
            let Some(project) = (|| {
                Some(Project {
                    id: result.get("id")?.as_str()?.to_string(),
                    name: result.get("name")?.as_str()?.to_string(),
                    path: result.get("path")?.as_str()?.to_string(),
                })
            })() else {
                set_status(state, "project.create returned an invalid project".into());
                return;
            };
            if let Ok(mut state) = state.lock() {
                let index = state
                    .projects
                    .iter()
                    .position(|existing| existing.id == project.id || existing.path == project.path)
                    .unwrap_or_else(|| {
                        state.projects.push(project.clone());
                        state.projects.len() - 1
                    });
                state.projects[index] = project.clone();
                state.selected_project = index;
                // The visible half of "just make one": where it went, by name.
                state.status = format!("created {}", project.path);
            }
            // The reply is authoritative, so the held objective starts on the
            // project the daemon actually made — never on a guess.
            let _ = commands.send(Command::StartRun {
                project_id: project.id,
                engine,
                model,
                reasoning_effort,
                objective,
                check_command,
                parent_run_id: None,
            });
        }
        Some(Pending::Engines) => {
            let engines: Vec<EngineStatus> = result
                .as_array()
                .map(|items| items.iter().map(parse_engine).collect())
                .unwrap_or_default();
            if let Ok(mut state) = state.lock() {
                // Preselect an engine that can actually run.
                if let Some(ready) = engines.iter().find(|e| e.ready)
                    && !engines.iter().any(|e| e.name == state.engine && e.ready)
                {
                    state.engine = ready.name.clone();
                }
                state.engines = engines;
                state.reconcile_execution_selection();
            }
        }
        Some(Pending::SandboxCheck) => {
            let ready = result
                .get("ready")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let problems: Vec<String> = result
                .get("problems")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            if let Ok(mut state) = state.lock() {
                state.sandbox = Some(SandboxView { ready, problems });
            }
        }
        Some(Pending::DraftCreated) => {
            let Some(run_id) = result.get("id").and_then(Value::as_str) else {
                set_status(state, "run.create returned no id".into());
                return;
            };
            if let Ok(mut state) = state.lock() {
                // Select it so the composer's next submit fills in THIS run
                // rather than starting another one beside it.
                state.run_id = Some(run_id.to_string());
                state.run_state = "draft".into();
                state.summary.clear();
                state.graph = None;
                state.diff = None;
                state.status = "new run — describe the objective".into();
            }
        }
        Some(Pending::RunEnqueue) => {
            let Some(run_id) = result.get("run_id").and_then(Value::as_str) else {
                set_status(state, "run.enqueue returned no run_id".into());
                return;
            };
            if let Ok(mut state) = state.lock() {
                state.run_id = Some(run_id.to_string());
                state.run_state = "draft".into();
                state.summary.clear();
                if let Some(item) = result.get("item")
                    && let Ok(item) =
                        serde_json::from_value::<proto::params::QueueItem>(item.clone())
                {
                    crate::queue::reduce_event(
                        &mut state.queue,
                        "queue.enqueued",
                        &serde_json::to_value(item).unwrap_or_default(),
                    );
                }
            }
        }
        Some(Pending::QueueList) => {
            let Ok(list) = serde_json::from_value::<proto::params::QueueListResult>(result.clone())
            else {
                set_status(state, "queue.list response was not typed".into());
                return;
            };
            if let Ok(mut state) = state.lock() {
                state.queue.replace(list.items);
            }
        }
        Some(Pending::QueueMutation) => {
            let Ok(item) = serde_json::from_value::<proto::params::QueueItem>(result.clone())
            else {
                set_status(state, "queue mutation response was not typed".into());
                return;
            };
            if let Ok(mut state) = state.lock() {
                crate::queue::reduce_event(
                    &mut state.queue,
                    "queue.moved",
                    &serde_json::to_value(item).unwrap_or_default(),
                );
                state.queue.error = None;
            }
        }
        Some(Pending::HistoryList { append }) => {
            let entries = result
                .get("entries")
                .and_then(Value::as_array)
                .map(|items| items.iter().filter_map(parse_history_entry).collect())
                .unwrap_or_default();
            let next_cursor = result
                .get("next_cursor")
                .and_then(Value::as_str)
                .map(str::to_string);
            if let Ok(mut state) = state.lock() {
                if !append {
                    state.history.entries.clear();
                }
                crate::history::merge_page(&mut state, entries, next_cursor);
                state.history.loading = false;
                state.history.error = None;
                state.history.scan_enabled = result
                    .get("scan_enabled")
                    .and_then(Value::as_bool)
                    .unwrap_or(true);
                state.history.diagnostics = result
                    .get("diagnostics")
                    .and_then(Value::as_array)
                    .map(|items| {
                        items
                            .iter()
                            .filter_map(Value::as_str)
                            .map(str::to_string)
                            .collect()
                    })
                    .unwrap_or_default();
            }
        }
        Some(Pending::HistoryAdopt {
            provider,
            source_id,
        }) => {
            let Some(run_id) = result.get("run_id").and_then(Value::as_str) else {
                set_status(state, "history.adopt returned no run_id".into());
                return;
            };
            if let Ok(mut state) = state.lock() {
                crate::history::mark_adopted(&mut state, &provider, &source_id, run_id, true);
                state.history.loading = false;
                state.status = format!("adopted {provider} history into {run_id}");
            }
        }
        Some(Pending::SettingsGet) | Some(Pending::SettingsUpdate) => {
            let Ok(settings) = serde_json::from_value::<proto::params::AppSettings>(result.clone())
            else {
                set_status(state, "settings response was not typed settings".into());
                return;
            };
            if let Ok(mut state) = state.lock() {
                apply_settings(&mut state, settings);
            }
        }
        Some(Pending::UsageSummary) => {
            let Ok(summary) =
                serde_json::from_value::<proto::params::UsageSummaryResult>(result.clone())
            else {
                set_status(
                    state,
                    "usage.summary response was not a typed summary".into(),
                );
                return;
            };
            if let Ok(mut state) = state.lock() {
                state.usage_summary = UsageSummaryView {
                    generated_at_ms: summary.generated_at_ms,
                    today_start_ms: summary.today_start_ms,
                    month_start_ms: summary.month_start_ms,
                    providers: summary.providers,
                    runs: summary.runs,
                    loading: false,
                    error: None,
                    pending_response: None,
                };
            }
        }
        Some(Pending::WorktreeList) => {
            let Ok(listed) =
                serde_json::from_value::<proto::params::WorktreeListResult>(result.clone())
            else {
                set_status(state, "worktree.list response was not a typed list".into());
                return;
            };
            if let Ok(mut state) = state.lock() {
                state.worktrees.entries = listed.entries;
                state.worktrees.storage_root = listed.storage_root;
                state.worktrees.loading = false;
                state.worktrees.error = None;
            }
        }
        Some(Pending::WorktreeReclaim { path, dry_run }) => {
            let Ok(outcome) =
                serde_json::from_value::<proto::params::WorktreeReclaimResult>(result.clone())
            else {
                set_status(state, "worktree.reclaim response was not typed".into());
                return;
            };
            let mut refresh = None;
            if let Ok(mut state) = state.lock() {
                state.worktrees.loading = false;
                state.worktrees.error = None;
                state.worktrees.diagnostics = describe_reclaim(&path, &outcome);
                if dry_run && outcome.eligible {
                    // Eligible in a dry run is exactly the moment to ask.
                    state.worktrees.pending_confirm = Some(outcome.path.clone());
                } else {
                    state.worktrees.pending_confirm = None;
                }
                if !dry_run {
                    state.status = if outcome.reclaimed {
                        format!("reclaimed {}", outcome.path)
                    } else {
                        format!("kept {}: {:?}", outcome.path, outcome.blockers)
                    };
                    // The list is stale the moment a reclaim lands.
                    state.worktrees.loading = true;
                    refresh = Some(Command::RefreshWorktrees(state.worktrees.filter));
                }
            }
            if let Some(command) = refresh {
                let _ = commands.send(command);
            }
        }
        Some(Pending::Subscribe) => {
            // Everything at or below this sequence is history. Recording it
            // before the replay arrives is what keeps old alerts quiet.
            if let Ok(mut state) = state.lock() {
                state.replay_through_seq = result
                    .get("replay_through_seq")
                    .and_then(Value::as_i64)
                    .unwrap_or(0);
            }
        }
        Some(Pending::Report(_)) | None => {}
    }
}

/// Plain-language diagnostics for one reclaim answer.
fn describe_reclaim(
    requested: &str,
    outcome: &proto::params::WorktreeReclaimResult,
) -> Vec<String> {
    let mut lines = vec![format!("requested {requested}")];
    lines.push(format!("resolved {}", outcome.path));
    if outcome.reclaimed {
        lines.push("reclaimed".into());
    } else if outcome.already_reclaimed {
        lines.push("already reclaimed".into());
    } else if outcome.eligible {
        lines.push("eligible — confirm to reclaim".into());
    }
    for blocker in &outcome.blockers {
        lines.push(format!("blocked: {}", blocker_label(*blocker)));
    }
    lines
}

pub fn blocker_label(blocker: proto::params::WorktreeBlocker) -> &'static str {
    use proto::params::WorktreeBlocker as B;
    match blocker {
        B::NotIndexed => "not a daemon-managed worktree",
        B::AlreadyReclaimed => "already reclaimed",
        B::PathMismatch => "path does not resolve to the indexed one",
        B::OutsideStorage => "outside daemon worktree storage",
        B::PrimaryCheckout => "this is the primary checkout",
        B::RunActive => "the owning run is active",
        B::RunNotTerminal => "the owning run has not finished",
        B::DirtyWorktree => "uncommitted changes present",
        B::CommitsBeyondBase => "the branch has commits past its base",
        B::ProcessInUse => "a process is using the directory",
        B::ProcessCheckUnknown => "the process check could not answer",
        B::GitCheckFailed => "a git check failed",
        B::ConfirmPathMismatch => "confirmation path did not match",
        B::ReclaimInProgress => "another reclaim is in flight",
    }
}

/// The daemon is the authority on persisted defaults. A selected run keeps
/// its own immutable execution selection so a late startup settings response
/// cannot relabel a Claude/high run as the Codex default.
fn apply_settings(state: &mut UiState, settings: proto::params::AppSettings) {
    if state.run_id.is_none() {
        state.set_engine(settings.default_engine.as_str());
    }
    state.settings.values = settings;
    state.settings.loading = false;
    state.settings.error = None;
    state.settings.pending_response = None;
    state.settings.pending_update = None;
}

fn parse_history_entry(value: &Value) -> Option<HistoryEntryView> {
    Some(HistoryEntryView {
        provider: value.get("provider")?.as_str()?.to_string(),
        source_id: value.get("source_id")?.as_str()?.to_string(),
        transcript_path: value.get("transcript_path")?.as_str()?.to_string(),
        cwd: value.get("cwd").and_then(Value::as_str).map(str::to_string),
        title: value
            .get("title")
            .and_then(Value::as_str)
            .map(str::to_string),
        first_prompt: value
            .get("first_prompt")
            .and_then(Value::as_str)
            .map(str::to_string),
        updated_at_ms: value.get("updated_at_ms")?.as_i64()?,
        eligible: value
            .get("eligible")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        reason: value
            .get("reason")
            .and_then(Value::as_str)
            .map(str::to_string),
        adopted_run_id: value
            .get("adopted_run_id")
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

/// One activity line: what happened, in words, not an event name.
fn describe_event(event: &proto::Event) -> String {
    let p = &event.payload;
    let text = |key: &str| p.get(key).and_then(Value::as_str).unwrap_or("").to_string();
    match event.kind.as_str() {
        "project.added" => format!("added project {}", text("name")),
        "run.created" => "run created".to_string(),
        "run.routed" => format!("routed as {}", text("shape")),
        "run.worktree_created" => format!("worktree on {}", text("branch")),
        "run.worktree_removed" => "worktree reclaimed (no changes)".into(),
        "run.worktree_preserved" => format!("worktree kept: {}", text("reason")),
        "run.started" => format!("started on {}", text("engine")),
        "run.planned" => format!("plan proposed: {} nodes", p["nodes"]),
        "run.handoff" => format!("handed over to {}", text("to_engine")),
        "history.adopted" => format!("adopted {} history", text("provider")),
        "run.loop_iteration" => format!("retrying: {}", text("reason")),
        "run.detector" => format!("{} -> {}", text("evidence"), text("recovery")),
        "run.checkpoint_restart" => "restarted from the last verified state".into(),
        "engine.tool" => format!("{} {}", text("name"), text("status")),
        "engine.file" | "engine.file_change" => format!("edited {}", text("path")),
        "engine.question" => format!("asked: {}", text("prompt")),
        "run.answered" => "answer delivered".to_string(),
        // A button that did nothing must say so rather than leaving the run
        // looking answered.
        "run.answer_failed" => format!("the answer could not be delivered: {}", text("error")),
        "engine.completed" => "turn complete".into(),
        "engine.failed" => format!("engine error: {}", text("message")),
        "run.check" => format!(
            "check {}: {}",
            if p.get("passed").and_then(Value::as_bool) == Some(true) {
                "passed"
            } else {
                "FAILED"
            },
            text("command")
        ),
        "run.commit" => match p.get("commit").and_then(Value::as_str) {
            Some(commit) => format!("committed {commit}"),
            None => "nothing to commit".into(),
        },
        "run.diff" => "patch recorded".into(),
        "run.succeeded" => "succeeded".into(),
        "run.failed" => format!("failed: {}", text("message")),
        "run.cancelled" => "cancelled".into(),
        "run.blocked" => format!("blocked: {}", text("reason")),
        "run.awaiting_approval" => "plan awaiting your approval".into(),
        "run.approved" => "plan approved".into(),
        "graph.rejected" => "plan refused; running a bounded loop".into(),
        "node.started" => format!("node {} started", text("node_id")),
        "node.finished" => format!("node {} {}", text("node_id"), text("state")),
        "sandbox.proxy.denied" => format!("blocked network access to {}", text("domain")),
        other => other.to_string(),
    }
}

/// Rebuild a graph view from a `run.awaiting_approval` payload.
fn string_array(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn parse_graph(payload: &Value) -> GraphView {
    let graph = payload.get("graph").unwrap_or(&Value::Null);
    let waves: Vec<Vec<String>> = graph
        .get("waves")
        .and_then(Value::as_array)
        .map(|waves| {
            waves
                .iter()
                .map(|wave| {
                    wave.as_array()
                        .map(|ids| {
                            ids.iter()
                                .filter_map(Value::as_str)
                                .map(str::to_string)
                                .collect()
                        })
                        .unwrap_or_default()
                })
                .collect()
        })
        .unwrap_or_default();
    let wave_of = |id: &str| {
        waves
            .iter()
            .position(|w| w.iter().any(|n| n == id))
            .unwrap_or(0)
    };

    let nodes = graph
        .get("nodes")
        .and_then(Value::as_array)
        .map(|nodes| {
            nodes
                .iter()
                .map(|n| {
                    let id = n
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or("?")
                        .to_string();
                    GraphNodeView {
                        wave: wave_of(&id),
                        role: n
                            .get("role")
                            .and_then(Value::as_str)
                            .unwrap_or("editor")
                            .to_string(),
                        objective: n
                            .get("objective")
                            .and_then(Value::as_str)
                            .unwrap_or(&id)
                            .to_string(),
                        file_scope: string_array(n.get("file_scope")),
                        acceptance_checks: string_array(n.get("acceptance_checks")),
                        depends_on: string_array(n.get("depends_on")),
                        id,
                        state: "pending".into(),
                        detail: n
                            .get("detail")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                        progress_percent: n
                            .get("progress_percent")
                            .and_then(Value::as_u64)
                            .and_then(|value| value.try_into().ok()),
                        duration_ms: n.get("duration_ms").and_then(Value::as_u64),
                        can_retry: false,
                        can_cancel: false,
                    }
                })
                .collect()
        })
        .unwrap_or_default();

    let edges = graph
        .get("edges")
        .and_then(Value::as_array)
        .map(|edges| {
            edges
                .iter()
                .filter_map(|e| {
                    let pair = e.as_array()?;
                    Some((
                        pair.first()?.as_str()?.to_string(),
                        pair.get(1)?.as_str()?.to_string(),
                    ))
                })
                .collect()
        })
        .unwrap_or_default();

    GraphView {
        nodes,
        edges,
        awaiting_approval: true,
    }
}

fn parse_engine(value: &Value) -> EngineStatus {
    EngineStatus {
        name: value
            .get("engine")
            .and_then(Value::as_str)
            .unwrap_or("engine")
            .to_string(),
        ready: value.get("ready").and_then(Value::as_bool).unwrap_or(false),
        installed: value
            .get("installed")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        authenticated: value.get("authenticated").and_then(Value::as_bool),
        version: value
            .get("version")
            .and_then(Value::as_str)
            .map(str::to_string),
        problems: value
            .get("problems")
            .and_then(Value::as_array)
            .map(|p| {
                p.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default(),
        models: value
            .get("models")
            .and_then(Value::as_array)
            .map(|models| {
                models
                    .iter()
                    .filter_map(|model| {
                        Some(EngineModelView {
                            id: model.get("id")?.as_str()?.to_string(),
                            display_name: model.get("display_name")?.as_str()?.to_string(),
                            description: model
                                .get("description")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string(),
                            reasoning_efforts: model
                                .get("reasoning_efforts")
                                .and_then(Value::as_array)
                                .map(|efforts| {
                                    efforts
                                        .iter()
                                        .filter_map(Value::as_str)
                                        .map(str::to_string)
                                        .collect()
                                })
                                .unwrap_or_default(),
                            default_reasoning_effort: model
                                .get("default_reasoning_effort")
                                .and_then(Value::as_str)
                                .map(str::to_string),
                            is_default: model
                                .get("is_default")
                                .and_then(Value::as_bool)
                                .unwrap_or(false),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default(),
        model_load_error: value
            .get("model_load_error")
            .and_then(Value::as_str)
            .map(str::to_string),
    }
}

fn event_engine(state: &UiState, run_id: Option<&str>, payload: &Value) -> Option<String> {
    payload
        .get("engine")
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|engine| !engine.is_empty())
        .or_else(|| {
            let run_id = run_id?;
            state
                .runs
                .iter()
                .find(|run| run.id == run_id)
                .map(|run| run.engine.clone())
                .filter(|engine| !engine.is_empty())
        })
}

fn display_engine(engine: Option<&str>) -> String {
    match engine.unwrap_or("Engine") {
        "codex" => "Codex".into(),
        "claude" => "Claude".into(),
        "" => "Engine".into(),
        other => {
            let mut chars = other.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => "Engine".into(),
            }
        }
    }
}

fn activity_row_from_event(event: &proto::Event, engine: Option<String>) -> ActivityRowView {
    let p = &event.payload;
    let path = p
        .get("path")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| p.get("branch").and_then(Value::as_str).map(str::to_string));
    let text = |key: &str| p.get(key).and_then(Value::as_str).unwrap_or("").to_string();
    let (action, detail) = match event.kind.as_str() {
        "run.created" => ("Created run".into(), text("objective")),
        "run.routed" => ("Routed".into(), text("shape")),
        "run.worktree_created" => ("Prepared worktree".into(), text("path")),
        "run.worktree_preserved" => ("Preserved worktree".into(), text("reason")),
        "run.worktree_removed" => ("Removed worktree".into(), String::new()),
        "run.check" | "node.check" | "engine.check" => (
            "Ran check".into(),
            p.get("name")
                .and_then(Value::as_str)
                .or_else(|| p.get("command").and_then(Value::as_str))
                .unwrap_or("")
                .to_string(),
        ),
        "run.diff" => ("Recorded diff".into(), "Patch captured".into()),
        "engine.file" | "engine.file_change" => ("Edited file".into(), text("path")),
        "engine.text" => ("Message".into(), text("text")),
        "engine.tool" => (
            "Tool".into(),
            format!("{} {}", text("name"), text("status")),
        ),
        "engine.question" => ("Asked question".into(), text("prompt")),
        "engine.completed" => ("Completed turn".into(), String::new()),
        "engine.failed" => ("Engine failed".into(), text("message")),
        "run.started" => ("Started run".into(), text("engine")),
        "history.adopted" => ("Adopted history".into(), text("provider")),
        "run.succeeded" => ("Completed run".into(), String::new()),
        "run.failed" => ("Failed run".into(), text("message")),
        "run.blocked" => ("Blocked run".into(), text("reason")),
        "node.started" => ("Started task".into(), text("node_id")),
        "node.finished" => ("Completed task".into(), text("node_id")),
        other => (other.to_string(), describe_event(event)),
    };
    ActivityRowView {
        timestamp_ms: event.timestamp_ms,
        engine: engine.map(|engine| display_engine(Some(&engine))),
        action,
        detail,
        path,
    }
}

fn status_from_kind(kind: &str) -> ChangedFileStatus {
    match kind {
        "created" | "added" | "new" => ChangedFileStatus::Added,
        "modified" | "updated" | "changed" => ChangedFileStatus::Modified,
        "deleted" | "removed" => ChangedFileStatus::Deleted,
        "renamed" | "moved" => ChangedFileStatus::Renamed,
        _ => ChangedFileStatus::Unknown,
    }
}

fn route_from_payload(payload: &Value) -> RouteView {
    let strings = |key: &str| {
        payload
            .get(key)
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    };
    RouteView {
        shape: payload
            .get("shape")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        confidence: payload
            .get("confidence")
            .and_then(Value::as_f64)
            .map(|confidence| confidence as f32),
        reasons: strings("reasons"),
        alternatives: strings("alternatives"),
        max_turns: payload
            .get("budgets")
            .and_then(|budgets| budgets.get("max_turns"))
            .and_then(Value::as_u64)
            .map(|turns| turns as u32),
        wall_time_secs: payload
            .get("budgets")
            .and_then(|budgets| budgets.get("wall_time_secs"))
            .and_then(Value::as_u64),
    }
}

/// The routing decision as the user reads it: what, why (all of it), and what
/// the shape implies. One line hid the thinking behind the first reason only.
pub(crate) fn route_message(route: &RouteView) -> String {
    let shape_label = route.shape.replace('_', " ");
    let mut text = shape_label.clone();
    if let Some(confidence) = route.confidence {
        text.push_str(&format!(" · confidence {:.0}%", confidence * 100.0));
    }
    if !route.reasons.is_empty() {
        text.push_str(&format!(" — {}", route.reasons.join("; ")));
    }
    let mut implications = Vec::new();
    if let Some(turns) = route.max_turns {
        implications.push(format!("up to {turns} turns"));
    }
    if let Some(secs) = route.wall_time_secs {
        implications.push(format!("{} min budget", secs / 60));
    }
    if !implications.is_empty() {
        text.push_str(&format!("\n{}", implications.join(" · ")));
    }
    if !route.alternatives.is_empty() {
        let alternatives = route
            .alternatives
            .iter()
            .map(|shape| shape.replace('_', " "))
            .collect::<Vec<_>>()
            .join(", ");
        text.push_str(&format!("\nconsidered: {alternatives}"));
    }
    text
}

fn push_tail_capped<T>(items: &mut Vec<T>, item: T, cap: usize) {
    if cap == 0 {
        return;
    }
    items.push(item);
    trim_tail(items, cap);
}

fn trim_tail<T>(items: &mut Vec<T>, cap: usize) {
    if items.len() > cap {
        let overflow = items.len() - cap;
        items.drain(0..overflow);
    }
}

fn tail_string_bytes(text: &str, cap: usize) -> String {
    if text.len() <= cap {
        return text.to_string();
    }
    let mut start = text.len() - cap;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    text[start..].to_string()
}

fn upsert_changed_file(files: &mut Vec<ChangedFileView>, path: String, status: ChangedFileStatus) {
    if path.is_empty() {
        return;
    }
    if let Some(existing) = files.iter_mut().find(|file| file.path == path) {
        existing.status = status;
    } else {
        push_tail_capped(
            files,
            ChangedFileView { path, status },
            DETAIL_CHANGED_FILE_CAP,
        );
    }
}

fn changed_files_from_patch(patch: &str) -> Vec<ChangedFileView> {
    let mut files = Vec::new();
    let mut current_path: Option<String> = None;
    let mut current_status = ChangedFileStatus::Modified;
    let flush = |files: &mut Vec<ChangedFileView>,
                 path: &mut Option<String>,
                 status: &mut ChangedFileStatus| {
        if let Some(path) = path.take() {
            upsert_changed_file(files, path, *status);
        }
        *status = ChangedFileStatus::Modified;
    };

    for line in patch.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            flush(&mut files, &mut current_path, &mut current_status);
            current_path = rest
                .split_whitespace()
                .next_back()
                .and_then(|p| p.strip_prefix("b/"))
                .map(str::to_string);
        } else if line.starts_with("new file") {
            current_status = ChangedFileStatus::Added;
        } else if line.starts_with("deleted file") {
            current_status = ChangedFileStatus::Deleted;
        } else if line.starts_with("rename ") {
            current_status = ChangedFileStatus::Renamed;
        } else if let Some(path) = line.strip_prefix("rename to ") {
            current_path = Some(path.to_string());
        }
    }
    flush(&mut files, &mut current_path, &mut current_status);
    files
}

fn optional_u64(payload: &Value, key: &str) -> Option<u64> {
    payload.get(key).and_then(Value::as_u64)
}

fn optional_u32(payload: &Value, key: &str) -> Option<u32> {
    optional_u64(payload, key).and_then(|value| value.try_into().ok())
}

fn budget_from_payload(payload: &Value) -> BudgetView {
    BudgetView {
        wall_time_secs: optional_u64(payload, "wall_time_secs"),
        max_turns: optional_u32(payload, "max_turns"),
        max_tool_calls: optional_u32(payload, "max_tool_calls"),
        max_retries: optional_u32(payload, "max_retries"),
        max_concurrent_workers: optional_u32(payload, "max_concurrent_workers"),
        max_graph_nodes: optional_u32(payload, "max_graph_nodes"),
        spent_wall_time_secs: optional_u64(payload, "spent_wall_time_secs"),
        spent_turns: optional_u32(payload, "spent_turns"),
        spent_tool_calls: optional_u32(payload, "spent_tool_calls"),
        spent_cost_usd: payload.get("spent_cost_usd").and_then(Value::as_f64),
    }
}

fn check_from_payload(payload: &Value) -> CheckView {
    let command = payload
        .get("command")
        .and_then(Value::as_str)
        .map(str::to_string);
    let name = payload
        .get("name")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| {
            payload
                .get("node_id")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .or_else(|| command.clone())
        .unwrap_or_else(|| "Check".into());
    let output = ["stdout", "stderr", "output"]
        .into_iter()
        .filter_map(|key| payload.get(key).and_then(Value::as_str))
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    CheckView {
        name,
        command,
        passed: payload.get("passed").and_then(Value::as_bool),
        duration_ms: optional_u64(payload, "duration_ms")
            .or_else(|| optional_u64(payload, "elapsed_ms")),
        output: (!output.is_empty()).then(|| tail_string_bytes(&output, CHECK_OUTPUT_BYTE_CAP)),
    }
}

fn check_output_lines(checks: &[CheckView]) -> Vec<String> {
    let output = checks
        .iter()
        .filter_map(|check| check.output.as_deref())
        .collect::<Vec<_>>()
        .join("\n");
    tail_string_bytes(&output, CHECK_OUTPUT_BYTE_CAP)
        .lines()
        .map(str::to_string)
        .collect()
}

fn text(payload: &Value, key: &str) -> String {
    payload
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

/// The process-wide notification adapter.
///
/// One per process because the operating system's notification centre is one
/// per process. Tests use [`apply_event_with`] to pass a fake instead.
fn platform_sink() -> &'static dyn crate::notify::NotificationSink {
    crate::notify::system_sink()
}

/// Fold one ledger event into the rendered state. Replay and live events go
/// through the same path, so a relaunched UI reconstructs the same view.
fn apply_event(state: &Arc<Mutex<UiState>>, event: proto::Event) {
    apply_event_with(state, event, platform_sink());
}

fn apply_event_with(
    state: &Arc<Mutex<UiState>>,
    event: proto::Event,
    sink: &dyn crate::notify::NotificationSink,
) {
    let Ok(mut state) = state.lock() else {
        return;
    };
    if event.sequence <= state.last_sequence {
        return;
    }
    state.last_sequence = event.sequence;
    crate::queue::reduce_event(&mut state.queue, &event.kind, &event.payload);
    if !UiState::is_noise(&event.kind) {
        state.push_event(describe_event(&event));
    }

    // Attention runs before the rest of the fold, so `selected_run` is still
    // what the user was looking at when the event arrived.
    let live = event.sequence > state.replay_through_seq.max(0) as u64;
    let selected_run = state.run_id.clone();
    let ctx = crate::attention::AttentionContext {
        live,
        selected_run: selected_run.as_deref(),
        notifications_enabled: state.settings.values.notifications_enabled,
        sounds_enabled: state.settings.values.sounds_enabled,
    };
    let mut attention = std::mem::take(&mut state.attention);
    let effect = crate::attention::reduce(&mut attention, &event, ctx);
    state.attention = attention;
    if let Some(effect) = effect
        && let Some(item) = state.attention.items.back().cloned()
    {
        crate::notify::deliver(sink, effect, &item);
    }

    if event.kind == "settings.updated" {
        // The daemon wraps the settings with the originating request id;
        // accept the bare form too so replay of either shape converges.
        let body = event
            .payload
            .get("settings")
            .unwrap_or(&event.payload)
            .clone();
        if let Ok(settings) = serde_json::from_value::<proto::params::AppSettings>(body) {
            apply_settings(&mut state, settings);
        }
    }

    // Keep the run list's states current regardless of what is on screen, so
    // the sidebar is right the moment a run is selected.
    if let Some(id) = event.run_id.clone()
        && let Some(next) = match event.kind.as_str() {
            "run.started" | "run.resumed" => Some("running"),
            "run.paused" => Some("paused"),
            "run.succeeded" => Some("succeeded"),
            "run.failed" => Some("failed"),
            "run.cancelled" => Some("cancelled"),
            "run.blocked" | "run.reconciled" => Some("blocked"),
            "run.awaiting_approval" => Some("awaiting_approval"),
            _ => None,
        }
        && let Some(run) = state.runs.iter_mut().find(|r| r.id == id)
    {
        run.state = next.to_string();
    }

    let payload = &event.payload;
    // A run's own history is always recorded, even when another run is on
    // screen. Only the *rendered* panes follow the selection.
    let of_run = event.run_id.clone();
    let engine_for_event = event_engine(&state, of_run.as_deref(), payload);
    if let Some(id) = of_run.as_deref()
        && !UiState::is_noise(&event.kind)
    {
        let row = activity_row_from_event(&event, engine_for_event.clone());
        let detail = state.detail_mut(id);
        if detail.engine.is_none() {
            detail.engine = engine_for_event.clone();
        }
        push_tail_capped(&mut detail.activity, row, DETAIL_ACTIVITY_CAP);
    }
    let this_run = state.run_id.as_deref() == event.run_id.as_deref();
    if let Some(id) = of_run.as_deref() {
        match event.kind.as_str() {
            "engine.usage" => {
                // Current provider contract: Codex reports cumulative session
                // usage, while Claude reports per-turn deltas. Use the run's
                // current engine identity when the event itself is silent.
                let engine = engine_for_event.as_deref().unwrap_or_default();
                let input = optional_u64(payload, "input_tokens").unwrap_or(0);
                let output = optional_u64(payload, "output_tokens").unwrap_or(0);
                let detail = state.detail_mut(id);
                if engine.eq_ignore_ascii_case("codex") {
                    detail.usage.input_tokens = detail.usage.input_tokens.max(input);
                    detail.usage.output_tokens = detail.usage.output_tokens.max(output);
                } else {
                    detail.usage.input_tokens += input;
                    detail.usage.output_tokens += output;
                }
                detail.usage.context_tokens =
                    optional_u64(payload, "context_tokens").or(detail.usage.context_tokens);
                detail.usage.context_limit_tokens = optional_u64(payload, "context_limit_tokens")
                    .or_else(|| optional_u64(payload, "context_window_tokens"))
                    .or(detail.usage.context_limit_tokens);
            }
            "run.approved" => {
                if let Some(graph) = &mut state.detail_mut(id).graph {
                    graph.awaiting_approval = false;
                }
            }
            "node.started" => {
                let node_id = text(payload, "node_id");
                if let Some(graph) = &mut state.detail_mut(id).graph
                    && let Some(node) = graph.node_mut(&node_id)
                {
                    node.state = "running".into();
                    node.can_retry = payload
                        .get("can_retry")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    node.can_cancel = payload
                        .get("can_cancel")
                        .and_then(Value::as_bool)
                        .unwrap_or(true);
                }
            }
            "node.finished" => {
                let node_id = text(payload, "node_id");
                if let Some(graph) = &mut state.detail_mut(id).graph
                    && let Some(node) = graph.node_mut(&node_id)
                {
                    node.state = text(payload, "state");
                    node.detail = text(payload, "detail");
                    node.can_retry = payload
                        .get("can_retry")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    node.can_cancel = payload
                        .get("can_cancel")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                }
            }
            "engine.check" => {
                push_tail_capped(
                    &mut state.detail_mut(id).checks,
                    check_from_payload(payload),
                    DETAIL_CHECK_CAP,
                );
            }
            "run.worktree_created" if !this_run => {
                state.detail_mut(id).worktree = Some(WorktreeDetail {
                    path: payload
                        .get("path")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    branch: payload
                        .get("branch")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    base: payload
                        .get("base")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    state: "created".into(),
                    isolation: payload
                        .get("isolation")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    reason: None,
                    created_at_ms: Some(event.timestamp_ms),
                });
            }
            "run.worktree_preserved" if !this_run => {
                let detail = state.detail_mut(id);
                let mut worktree = detail.worktree.clone().unwrap_or_default();
                worktree.path = payload
                    .get("path")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .or(worktree.path);
                worktree.state = "preserved".into();
                worktree.reason = payload
                    .get("reason")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                detail.worktree = Some(worktree);
            }
            "run.worktree_removed" if !this_run => {
                let detail = state.detail_mut(id);
                let mut worktree = detail.worktree.clone().unwrap_or_default();
                worktree.state = "removed".into();
                detail.worktree = Some(worktree);
            }
            "run.routed" if !this_run => {
                let route = route_from_payload(payload);
                if !route.shape.is_empty() {
                    state.detail_mut(id).route_shape = Some(route.shape.clone());
                }
                state.detail_mut(id).route = Some(route);
                if let Some(budgets) = payload.get("budgets") {
                    state.detail_mut(id).budget = Some(budget_from_payload(budgets));
                }
            }
            "run.awaiting_approval" if !this_run => {
                let detail = state.detail_mut(id);
                detail.graph = Some(parse_graph(payload));
                if let Some(budgets) = payload.get("budgets") {
                    detail.budget = Some(budget_from_payload(budgets));
                }
                let shape = text(payload, "shape");
                if !shape.is_empty() {
                    detail.route_shape = Some(shape);
                }
            }
            "node.check" | "run.check" if !this_run => {
                push_tail_capped(
                    &mut state.detail_mut(id).checks,
                    check_from_payload(payload),
                    DETAIL_CHECK_CAP,
                );
            }
            "run.diff" if !this_run => {
                let view = parse_diff(&text(payload, "patch"));
                let changed_files = changed_files_from_patch(&text(payload, "patch"));
                let detail = state.detail_mut(id);
                for file in changed_files {
                    upsert_changed_file(&mut detail.changed_files, file.path, file.status);
                }
                detail.diff = Some(view);
            }
            _ => {}
        }
    }
    // Anything meaning the agent moved on clears the prompt, so a stale
    // question cannot sit under a live run offering a button that does
    // nothing. Done before dispatch: several of these kinds already have
    // handlers, and an extra arm ahead of them would shadow every one.
    if matches!(
        event.kind.as_str(),
        "run.answered" | "engine.completed" | "run.succeeded" | "run.failed" | "run.cancelled"
    ) && let Some(id) = &of_run
    {
        state.detail_mut(id).pending_question = None;
    }
    match event.kind.as_str() {
        "project.added" | "project.removed" => {
            // The list is refreshed by the next explicit project.list.
            state.status = format!("projects changed ({})", event.kind);
        }
        "run.created" => {
            let Some(id) = of_run.clone() else { return };
            let objective = text(payload, "objective");
            let engine = text(payload, "engine");
            let model = payload
                .get("model")
                .and_then(Value::as_str)
                .map(str::to_string);
            let reasoning_effort = payload
                .get("reasoning_effort")
                .and_then(Value::as_str)
                .map(str::to_string);
            if !state.runs.iter().any(|r| r.id == id) {
                let parent = payload
                    .get("parent_run_id")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                state.runs.push(RunView {
                    id: id.clone(),
                    project_id: text(payload, "project_id"),
                    objective: objective.clone(),
                    state: "draft".into(),
                    engine: engine.clone(),
                    parent_run_id: parent,
                    attempt_group: payload
                        .get("attempt_group")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                });
            }
            // Dated from the ledger envelope, not from anything the run has
            // said yet, so a row has a real time from the moment it exists.
            let detail = state.detail_mut(&id);
            if detail.created_at_ms.is_none() {
                detail.created_at_ms = Some(event.timestamp_ms);
            }
            if !objective.is_empty() {
                state.say(&id, format!("you  {objective}"));
                push_tail_capped(
                    &mut state.detail_mut(&id).messages,
                    StructuredMessageView {
                        author: "You".into(),
                        engine: None,
                        model: None,
                        text: objective.clone(),
                        timestamp_ms: event.timestamp_ms,
                    },
                    DETAIL_MESSAGE_CAP,
                );
            }
            if !engine.is_empty() {
                let detail = state.detail_mut(&id);
                detail.engine = Some(engine.clone());
                detail.model = model.clone();
                detail.reasoning_effort = reasoning_effort.clone();
            }
            // Follow the newest run, which is what a user opening the app
            // expects to be looking at.
            state.select_run(&id);
            if !engine.is_empty() {
                state.engine = engine;
                state.model = model;
                state.reasoning_effort = reasoning_effort;
                state.reconcile_execution_selection();
            }
        }
        "history.adopted" => {
            if let Some(run_id) = of_run.as_deref() {
                let provider = text(payload, "provider");
                let source_id = text(payload, "source_id");
                crate::history::mark_adopted(&mut state, &provider, &source_id, run_id, false);
            }
        }
        "chat.message" => {
            if let Some(id) = &of_run {
                let content = text(payload, "content");
                let line = format!("you  {content}");
                state.say(&id.clone(), line);
                push_tail_capped(
                    &mut state.detail_mut(id).messages,
                    StructuredMessageView {
                        author: "You".into(),
                        engine: None,
                        model: None,
                        text: content,
                        timestamp_ms: event.timestamp_ms,
                    },
                    DETAIL_MESSAGE_CAP,
                );
            }
        }
        "chat.steering_queued" => {
            if let Some(id) = &of_run {
                let line = format!("queued: {}", text(payload, "message"));
                state.say(&id.clone(), line);
            }
        }
        "chat.steering_sent" => {
            if let Some(id) = &of_run {
                let line = format!("sent: {}", text(payload, "message"));
                state.say(&id.clone(), line);
            }
        }
        // A run that fails or blocks says so IN ITS TRANSCRIPT. The status
        // line already reported it, but only for the selected run and only
        // until the next status — a failed run whose conversation shows
        // nothing but the objective reads as "nothing happened".
        "run.failed" => {
            if let Some(id) = &of_run {
                let reason = failure_reason(&text(payload, "message"));
                state.say(&id.clone(), format!("failed: {reason}"));
                push_tail_capped(
                    &mut state.detail_mut(id).messages,
                    StructuredMessageView {
                        author: "Run failed".into(),
                        engine: None,
                        model: None,
                        text: reason.clone(),
                        timestamp_ms: event.timestamp_ms,
                    },
                    DETAIL_MESSAGE_CAP,
                );
                if this_run {
                    state.run_state = "failed".into();
                    state.status = format!("failed: {reason}");
                }
            }
        }
        "run.blocked" => {
            if let Some(id) = &of_run {
                let mut reason = failure_reason(&text(payload, "reason"));
                let error = text(payload, "error");
                if !error.is_empty() {
                    reason = format!("{reason} {error}");
                }
                state.say(&id.clone(), format!("blocked: {reason}"));
                push_tail_capped(
                    &mut state.detail_mut(id).messages,
                    StructuredMessageView {
                        author: "Run blocked".into(),
                        engine: None,
                        model: None,
                        text: reason.clone(),
                        timestamp_ms: event.timestamp_ms,
                    },
                    DETAIL_MESSAGE_CAP,
                );
                if this_run {
                    state.run_state = "blocked".into();
                    state.status = format!("blocked: {reason}");
                }
            }
        }
        "engine.text" => {
            if let Some(id) = &of_run {
                let text = text(payload, "text");
                let line = format!("bot  {text}");
                state.say(&id.clone(), line);
                let engine = event_engine(&state, Some(id), payload);
                // The run's pinned model, so a thread whose turns ran on
                // different models says which said what.
                let model = state.run_details.get(id).and_then(|d| d.model.clone());
                push_tail_capped(
                    &mut state.detail_mut(id).messages,
                    StructuredMessageView {
                        author: display_engine(engine.as_deref()),
                        engine,
                        model,
                        text,
                        timestamp_ms: event.timestamp_ms,
                    },
                    DETAIL_MESSAGE_CAP,
                );
            }
        }
        "engine.tool" => {
            if let Some(id) = &of_run {
                let line = format!("tool {} {}", text(payload, "name"), text(payload, "status"));
                state.say(&id.clone(), line);
            }
        }
        "engine.file" | "engine.file_change" => {
            if let Some(id) = &of_run {
                let path = text(payload, "path");
                let line = format!("edit {path}");
                state.say(&id.clone(), line);
                let status = payload
                    .get("kind")
                    .and_then(Value::as_str)
                    .map(status_from_kind)
                    .unwrap_or(ChangedFileStatus::Modified);
                upsert_changed_file(&mut state.detail_mut(id).changed_files, path, status);
            }
        }
        "engine.terminal" => {
            if let Some(id) = &of_run {
                let line = text(payload, "line");
                let detail = state.detail_mut(id);
                push_tail_capped(&mut detail.terminal, line, TERMINAL_TAIL);
            }
        }
        "engine.question" => {
            if let Some(id) = &of_run {
                let prompt = text(payload, "prompt");
                let line = format!("ask  {prompt}");
                state.say(&id.clone(), line);
                // The agent is BLOCKED on this. Until it can be answered the
                // run reads as running and simply never moves, which is the
                // same shape as a hang.
                state.detail_mut(id).pending_question = Some(prompt);
            }
        }
        "run.worktree_created" if this_run => {
            state.worktree = Some(text(payload, "path"));
            state.summary.push(format!(
                "worktree {} on {}",
                text(payload, "path"),
                text(payload, "branch")
            ));
            if let Some(id) = &of_run {
                state.detail_mut(id).worktree = Some(WorktreeDetail {
                    path: payload
                        .get("path")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    branch: payload
                        .get("branch")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    base: payload
                        .get("base")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    state: "created".into(),
                    isolation: payload
                        .get("isolation")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    reason: None,
                    created_at_ms: Some(event.timestamp_ms),
                });
            }
        }
        "run.awaiting_approval" if this_run => {
            state.run_state = "awaiting_approval".into();
            let graph = parse_graph(payload);
            if let Some(id) = &of_run {
                state.detail_mut(id).graph = Some(graph.clone());
                if let Some(budgets) = payload.get("budgets") {
                    state.detail_mut(id).budget = Some(budget_from_payload(budgets));
                }
                let shape = text(payload, "shape");
                if !shape.is_empty() {
                    state.detail_mut(id).route_shape = Some(shape);
                }
            }
            state.graph = Some(graph);
            state.status = "plan ready for review — /approve to run it".into();
        }
        "graph.rejected" if this_run => {
            state.status = "the proposed plan was refused; running a bounded loop".into();
            for problem in payload
                .get("problems")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
            {
                state.summary.push(format!("plan refused: {problem}"));
            }
        }
        "run.approved" if this_run => {
            if let Some(graph) = &mut state.graph {
                graph.awaiting_approval = false;
            }
        }
        "node.started" if this_run => {
            let id = text(payload, "node_id");
            if let Some(graph) = &mut state.graph
                && let Some(node) = graph.node_mut(&id)
            {
                node.state = "running".into();
                node.can_retry = false;
                node.can_cancel = payload
                    .get("can_cancel")
                    .and_then(Value::as_bool)
                    .unwrap_or(true);
            }
            if let Some(run_id) = &of_run
                && let Some(graph) = &mut state.detail_mut(run_id).graph
                && let Some(node) = graph.node_mut(&id)
            {
                node.state = "running".into();
            }
        }
        "node.finished" if this_run => {
            let id = text(payload, "node_id");
            let node_state = text(payload, "state");
            let detail = text(payload, "detail");
            if let Some(graph) = &mut state.graph
                && let Some(node) = graph.node_mut(&id)
            {
                node.state = node_state;
                node.detail = detail;
                node.can_retry = payload
                    .get("can_retry")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                node.can_cancel = payload
                    .get("can_cancel")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
            }
            if let Some(run_id) = &of_run
                && let Some(graph) = &mut state.detail_mut(run_id).graph
                && let Some(node) = graph.node_mut(&id)
            {
                node.state = text(payload, "state");
                node.detail = text(payload, "detail");
                node.can_retry = payload
                    .get("can_retry")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                node.can_cancel = payload
                    .get("can_cancel")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
            }
        }
        "node.check" if this_run => {
            let passed = payload.get("passed").and_then(Value::as_bool) == Some(true);
            state.summary.push(format!(
                "{} {} · {}",
                if passed {
                    "check passed"
                } else {
                    "CHECK FAILED"
                },
                text(payload, "node_id"),
                text(payload, "command")
            ));
            if let Some(id) = &of_run {
                push_tail_capped(
                    &mut state.detail_mut(id).checks,
                    check_from_payload(payload),
                    DETAIL_CHECK_CAP,
                );
            }
        }
        "artifact.created" => {
            // Artifacts belong to their run whether or not it is on screen,
            // so a replay after a restart rebuilds the same list.
            if let Some(id) = &of_run {
                let name = text(payload, "name");
                state.detail_mut(id).push_artifact(ArtifactView {
                    name: name.clone(),
                    path: payload
                        .get("path")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .unwrap_or(name),
                    kind: payload
                        .get("kind")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    size_bytes: optional_u64(payload, "byte_size"),
                });
            }
            if this_run {
                state.summary.push(format!(
                    "artifact {} · {}",
                    text(payload, "kind"),
                    text(payload, "name")
                ));
            }
        }
        "node.diff" if this_run => {
            state.summary.push(format!(
                "{} changed {}",
                text(payload, "node_id"),
                text(payload, "diff_stat")
                    .lines()
                    .last()
                    .unwrap_or("files")
                    .trim()
            ));
        }
        "node.conflict" if this_run => {
            state.summary.push(format!(
                "merge conflict integrating {} — the integration node resolves it",
                text(payload, "branch")
            ));
        }
        "run.detector" if this_run => {
            state.summary.push(format!(
                "{} → {}",
                text(payload, "evidence"),
                text(payload, "recovery")
            ));
        }
        "run.routed" => {
            let route = route_from_payload(payload);
            let shape = route.shape.clone();
            let reason = route.reasons.first().cloned().unwrap_or_default();
            if this_run {
                state.summary.push(format!("routed {shape} · {reason}"));
            }
            if let Some(id) = &of_run {
                if !shape.is_empty() {
                    state.detail_mut(id).route_shape = Some(shape.clone());
                    state.detail_mut(id).route = Some(route.clone());
                    // The routing decision lands IN THE TRANSCRIPT, with its
                    // reasoning. It was on the ledger and in the inspector, but
                    // the conversation — where the user actually looks — never
                    // said which shape the router picked or why, so "is
                    // routing even on?" had no answer on screen.
                    push_tail_capped(
                        &mut state.detail_mut(id).messages,
                        StructuredMessageView {
                            author: "Routed".into(),
                            engine: None,
                            model: None,
                            text: route_message(&route),
                            timestamp_ms: event.timestamp_ms,
                        },
                        DETAIL_MESSAGE_CAP,
                    );
                }
                if let Some(budgets) = payload.get("budgets") {
                    state.detail_mut(id).budget = Some(budget_from_payload(budgets));
                }
            }
        }
        "run.worktree_preserved" if this_run => {
            state.summary.push(format!(
                "worktree preserved ({}): {}",
                text(payload, "reason"),
                text(payload, "path")
            ));
            if let Some(id) = &of_run {
                let detail = state.detail_mut(id);
                let mut worktree = detail.worktree.clone().unwrap_or_default();
                worktree.path = payload
                    .get("path")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .or(worktree.path);
                worktree.state = "preserved".into();
                worktree.reason = payload
                    .get("reason")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                detail.worktree = Some(worktree);
            }
        }
        "run.worktree_removed" if this_run => {
            state.summary.push("worktree removed (no changes)".into());
            if let Some(id) = &of_run {
                let detail = state.detail_mut(id);
                let mut worktree = detail.worktree.clone().unwrap_or_default();
                worktree.state = "removed".into();
                detail.worktree = Some(worktree);
            }
        }
        "run.check" if this_run => {
            let passed = payload.get("passed").and_then(Value::as_bool) == Some(true);
            state.summary.push(format!(
                "check {}: {}",
                if passed { "passed" } else { "FAILED" },
                text(payload, "command")
            ));
            // Keep the output itself. A failing check's verdict is useless
            // without the reason, and scrolling to it beats re-running the
            // command in a terminal.
            let mut output: Vec<String> = Vec::new();
            for stream in ["stdout", "stderr"] {
                for line in text(payload, stream).lines() {
                    output.push(line.to_string());
                }
            }
            state.check_output = tail_string_bytes(&output.join("\n"), CHECK_OUTPUT_BYTE_CAP)
                .lines()
                .map(str::to_string)
                .collect();
            if let Some(id) = &of_run {
                push_tail_capped(
                    &mut state.detail_mut(id).checks,
                    check_from_payload(payload),
                    DETAIL_CHECK_CAP,
                );
            }
        }
        "run.diff" if this_run => {
            let view = parse_diff(&text(payload, "patch"));
            state.summary.push(format!(
                "{} file(s) +{} -{}",
                view.files, view.added, view.removed
            ));
            let changed_files = changed_files_from_patch(&text(payload, "patch"));
            if let Some(id) = &of_run {
                let detail = state.detail_mut(id);
                for file in changed_files {
                    upsert_changed_file(&mut detail.changed_files, file.path, file.status);
                }
                detail.diff = Some(view.clone());
            }
            state.diff = Some(view);
        }
        "run.commit" if this_run => {
            let commit = payload
                .get("commit")
                .and_then(Value::as_str)
                .unwrap_or("(nothing to commit)");
            state.summary.push(format!("commit {commit}"));
            for line in text(payload, "diff_stat").lines() {
                state.summary.push(line.to_string());
            }
        }
        "run.handoff" => {
            if let Some(id) = &of_run {
                let line = format!(
                    "handed off to {} — context carried over from the previous agent",
                    text(payload, "to_engine")
                );
                state.say(&id.clone(), line);
            }
        }
        "run.started" if this_run => {
            state.run_state = "running".into();
            state.status = format!("run started on {}", text(payload, "engine"));
        }
        "run.paused" if this_run => state.run_state = "paused".into(),
        "run.resumed" if this_run => state.run_state = "running".into(),
        "run.reconciled" if this_run => {
            state.run_state = "blocked".into();
            // The recovery is real and worth saying: the turn was lost, the
            // WORK was not. A follow-up continues the thread, and a thread's
            // turns share one worktree, so it picks up on the same branch with
            // whatever the interrupted turn had already committed. Saying only
            // "reconciled" left the user to guess whether to start over.
            state.status = if payload
                .get("worktree_preserved")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                "the daemon restarted mid-run; its worktree is intact — send a follow-up to \
                 continue on the same branch"
                    .into()
            } else {
                "the daemon restarted mid-run and this run left nothing behind; ask again to \
                 start over"
                    .into()
            };
        }
        "run.succeeded" if this_run => {
            state.run_state = "succeeded".into();
            state.status = "run succeeded".into();
        }
        "run.cancelled" if this_run => {
            state.run_state = "cancelled".into();
            state.status = "run cancelled".into();
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    fn event(seq: u64, run: &str, kind: &str, payload: Value) -> proto::Event {
        proto::Event::new(seq, Some(run.into()), seq, 0, kind, payload)
    }

    /// A failed run's reason lands in ITS transcript, unwrapped from the
    /// provider's JSON envelope. The regression: this machine's ledger had a
    /// failed run whose conversation showed nothing but the objective —
    /// "nothing happened" with a failure sitting right there in the events.
    #[test]
    fn a_failed_run_says_why_in_its_own_transcript() {
        let state = Arc::new(Mutex::new(UiState::default()));
        apply_event(
            &state,
            event(
                1,
                "r1",
                "run.created",
                json!({ "engine": "codex", "objective": "yoo" }),
            ),
        );
        let raw =
            r#"{"error":{"message":"litellm.BadRequestError: upstream rejected the request"}}"#;
        apply_event(
            &state,
            event(2, "r1", "run.failed", json!({ "message": raw })),
        );

        let state = state.lock().unwrap();
        let detail = state.selected_detail().expect("run detail");
        let last = detail.messages.last().expect("failure message present");
        assert_eq!(last.author, "Run failed");
        assert_eq!(
            last.text,
            "litellm.BadRequestError: upstream rejected the request"
        );
        assert_eq!(state.run_state, "failed");
        assert!(state.status.contains("litellm.BadRequestError"));
    }

    /// A blocked run gets the same treatment, reason and error together.
    #[test]
    fn a_blocked_run_says_why_in_its_own_transcript() {
        let state = Arc::new(Mutex::new(UiState::default()));
        apply_event(&state, event(1, "r1", "run.created", json!({})));
        apply_event(
            &state,
            event(
                2,
                "r1",
                "run.blocked",
                json!({ "reason": "sandbox unavailable", "error": "canary failed" }),
            ),
        );

        let state = state.lock().unwrap();
        let detail = state.selected_detail().expect("run detail");
        let last = detail.messages.last().expect("blocked message present");
        assert_eq!(last.author, "Run blocked");
        assert_eq!(last.text, "sandbox unavailable canary failed");
        assert_eq!(state.run_state, "blocked");
    }

    /// Routing answers "what will run this and why" in the conversation
    /// itself, not only in the inspector.
    #[test]
    fn the_routing_decision_lands_in_the_transcript_with_its_reason() {
        let state = Arc::new(Mutex::new(UiState::default()));
        apply_event(
            &state,
            event(1, "r1", "run.created", json!({ "objective": "x" })),
        );
        apply_event(
            &state,
            event(
                2,
                "r1",
                "run.routed",
                json!({
                    "shape": "bounded_loop",
                    "confidence": 0.8,
                    "reasons": ["objective is atomic but has no explicit check"],
                    "alternatives": ["direct"],
                    "budgets": { "max_turns": 40, "wall_time_secs": 1800 },
                }),
            ),
        );

        let state = state.lock().unwrap();
        let detail = state.selected_detail().expect("run detail");
        let last = detail.messages.last().expect("routed message present");
        assert_eq!(last.author, "Routed");
        assert!(last.text.contains("bounded loop"));
        assert!(last.text.contains("no explicit check"));
        // The thinking arrives with the decision: confidence, what it implies,
        // and what was refused — not just the shape's name.
        assert!(last.text.contains("80%"), "{}", last.text);
        assert!(last.text.contains("40 turns"), "{}", last.text);
        assert!(last.text.contains("30 min budget"), "{}", last.text);
        assert!(last.text.contains("considered: direct"), "{}", last.text);
        assert_eq!(
            detail.route.as_ref().and_then(|route| route.max_turns),
            Some(40)
        );
    }

    #[test]
    fn failure_reason_unwraps_nested_json_envelopes() {
        let nested = serde_json::json!({
            "error": { "message": r#"{"error":{"message":"quota exhausted"}}"# }
        })
        .to_string();
        assert_eq!(failure_reason(&nested), "quota exhausted");
        assert_eq!(failure_reason("plain sentence"), "plain sentence");
        assert_eq!(failure_reason(r#"{"unrelated":1}"#), r#"{"unrelated":1}"#);
    }

    /// The catalog is offered exactly as the provider reported it — routed
    /// entries included, because the user's proxy setup is theirs — and a
    /// persisted pick of any listed model survives reconciliation.
    #[test]
    fn the_catalog_is_offered_as_reported_and_listed_picks_survive() {
        let own = EngineModelView {
            id: "gpt-5.6-sol".into(),
            display_name: "GPT-5.6-Sol".into(),
            description: String::new(),
            reasoning_efforts: vec!["low".into(), "high".into()],
            default_reasoning_effort: Some("low".into()),
            is_default: true,
        };
        let routed = EngineModelView {
            id: "opencode-go/kimi-k2.6".into(),
            display_name: "Kimi K2.6 (opencode Go)".into(),
            description: String::new(),
            reasoning_efforts: vec!["high".into()],
            default_reasoning_effort: Some("high".into()),
            is_default: false,
        };
        let engine = EngineStatus {
            name: "codex".into(),
            ready: true,
            installed: true,
            authenticated: Some(true),
            version: None,
            problems: Vec::new(),
            models: vec![own.clone(), routed],
            model_load_error: None,
        };
        assert_eq!(
            offered_models(&engine)
                .into_iter()
                .map(|model| model.id.as_str())
                .collect::<Vec<_>>(),
            vec!["gpt-5.6-sol", "opencode-go/kimi-k2.6"]
        );

        let mut state = UiState {
            engine: "codex".into(),
            engines: vec![engine],
            model: Some("opencode-go/kimi-k2.6".into()),
            reasoning_effort: Some("high".into()),
            ..UiState::default()
        };
        state.reconcile_execution_selection();
        assert_eq!(
            state.model.as_deref(),
            Some("opencode-go/kimi-k2.6"),
            "a listed pick is the user's pick"
        );
        assert!(state.select_model("opencode-go/kimi-k2.6"));
        // A model the catalog no longer lists still snaps to a real one.
        state.model = Some("gone-model".into());
        state.reconcile_execution_selection();
        assert_eq!(state.model.as_deref(), Some("gpt-5.6-sol"));
    }

    #[test]
    fn daemon_diagnostics_are_bounded_to_the_newest_complete_tail() {
        let bytes =
            b"old failure that should be dropped\nnew failure line one\nnew failure line two\n";
        let tail = bounded_log_tail(bytes, 42);
        assert!(!tail.contains("old failure"));
        assert_eq!(tail, "new failure line one\nnew failure line two");
    }

    #[test]
    fn daemon_log_is_private_and_append_only() {
        let temp = tempfile::tempdir().unwrap();
        let mut first = open_daemon_log(temp.path()).unwrap();
        writeln!(first, "first launch").unwrap();
        drop(first);
        let mut second = open_daemon_log(temp.path()).unwrap();
        writeln!(second, "second launch").unwrap();
        drop(second);

        let path = temp.path().join("daemon.log");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "first launch\nsecond launch\n"
        );
        assert_eq!(
            std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    fn event_at(
        seq: u64,
        run: &str,
        timestamp_ms: i64,
        kind: &str,
        payload: Value,
    ) -> proto::Event {
        proto::Event::new(seq, Some(run.into()), seq, timestamp_ms, kind, payload)
    }

    fn approval_graph_payload() -> Value {
        json!({
            "shape": "parallel",
            "graph": {
                "nodes": [
                    { "id": "plan", "role": "codex" },
                    { "id": "edit", "role": "claude" }
                ],
                "edges": [["plan", "edit"]],
                "waves": [["plan"], ["edit"]]
            }
        })
    }

    #[test]
    fn a_preview_client_preserves_its_fixture_without_connecting() {
        let fixture = UiState {
            status: "preview".into(),
            connected: true,
            engine: "codex".into(),
            ..UiState::default()
        };

        let client = DaemonClient::preview(fixture);
        let state = client.state.lock().unwrap();

        assert_eq!(state.status, "preview");
        assert!(state.connected);
        assert_eq!(state.engine, "codex");
        assert!(state.projects.is_empty());
    }

    #[test]
    fn preview_client_send_leaves_disconnected_fixture_unchanged() {
        let fixture = UiState {
            status: "preview fixture".into(),
            connected: false,
            engine: "claude".into(),
            input: "keep me".into(),
            runs: vec![RunView {
                id: "fixture-run".into(),
                project_id: "p1".into(),
                objective: "fixture objective".into(),
                state: "running".into(),
                engine: "claude".into(),
                parent_run_id: None,
                attempt_group: None,
            }],
            run_id: Some("fixture-run".into()),
            ..UiState::default()
        };

        let client = DaemonClient::preview(fixture);
        client.send(Command::RefreshProjects);
        client.send(Command::StartRun {
            project_id: "p1".into(),
            engine: "codex".into(),
            model: Some("gpt-5.6-sol".into()),
            reasoning_effort: Some("high".into()),
            objective: "mutate fixture".into(),
            check_command: Some("cargo test".into()),
            parent_run_id: None,
        });

        let state = client.state.lock().unwrap();
        assert!(!state.connected);
        assert_eq!(state.status, "preview fixture");
        assert_eq!(state.engine, "claude");
        assert_eq!(state.input, "keep me");
        assert_eq!(state.runs.len(), 1);
        assert_eq!(state.run_id.as_deref(), Some("fixture-run"));
    }

    #[test]
    fn replay_builds_selected_run_structured_detail_from_existing_events() {
        let state = Arc::new(Mutex::new(UiState::default()));
        let patch = "diff --git a/src/cache/key.ts b/src/cache/key.ts\n\
index 111..222 100644\n\
--- a/src/cache/key.ts\n\
+++ b/src/cache/key.ts\n\
@@ -42,3 +42,5 @@ export function stableKey(input: Input): string {\n\
-const normalized = JSON.stringify(input)\n\
+const normalized = canonicalize(input, { sortKeys: true })\n\
+return createHash('sha256').update(normalized).digest('hex')\n\
diff --git a/test/cache/cross-version.test.ts b/test/cache/cross-version.test.ts\n\
new file mode 100644\n\
index 000..333\n\
--- /dev/null\n\
+++ b/test/cache/cross-version.test.ts\n\
@@ -0,0 +1,2 @@\n\
+test('stable across node versions', () => {})\n";

        for e in [
            event_at(
                1,
                "r-cache",
                1_773_311_100_000,
                "run.created",
                json!({
                    "project_id": "p1",
                    "objective": "Make the cache key stable across Node 18 and 20.",
                    "engine": "codex"
                }),
            ),
            event_at(
                2,
                "r-cache",
                1_773_311_160_000,
                "run.worktree_created",
                json!({
                    "path": "~/.worktrees/run-20250508-1525",
                    "branch": "autoharness/run-20250508-1525",
                    "base": "main (a1b2c3d)",
                    "isolation": "full"
                }),
            ),
            event_at(
                3,
                "r-cache",
                1_773_311_220_000,
                "run.routed",
                json!({
                    "shape": "parallel",
                    "budgets": {
                        "wall_time_secs": 1800,
                        "max_turns": 200,
                        "max_tool_calls": 500,
                        "max_retries": 3,
                        "max_concurrent_workers": 2,
                        "max_graph_nodes": 8
                    }
                }),
            ),
            event_at(
                4,
                "r-cache",
                1_773_311_280_000,
                "engine.text",
                json!({ "text": "I'll analyze the cache key generation and propose a plan." }),
            ),
            event_at(
                5,
                "r-cache",
                1_773_311_340_000,
                "engine.file",
                json!({ "path": "src/cache/key.ts", "kind": "modified" }),
            ),
            event_at(
                6,
                "r-cache",
                1_773_311_400_000,
                "run.diff",
                json!({ "patch": patch }),
            ),
            event_at(
                7,
                "r-cache",
                1_773_311_460_000,
                "run.check",
                json!({
                    "name": "Unit tests",
                    "command": "npm test -- cache",
                    "passed": true,
                    "duration_ms": 42_000,
                    "stdout": "PASS test/cache/cross-version.test.ts\n"
                }),
            ),
            event_at(
                8,
                "r-cache",
                1_773_311_520_000,
                "engine.usage",
                json!({ "input_tokens": 12_000, "output_tokens": 4_000 }),
            ),
        ] {
            apply_event(&state, e);
        }

        let state = state.lock().unwrap();
        let detail = state
            .selected_detail()
            .expect("selected run must expose structured detail");
        assert_eq!(detail.run_id, "r-cache");
        // Objective, the routing decision, then the engine's reply — the
        // route now speaks in the transcript like everything else.
        assert_eq!(detail.messages.len(), 3);
        assert_eq!(detail.messages[0].author, "You");
        assert_eq!(detail.messages[0].timestamp_ms, 1_773_311_100_000);
        assert_eq!(detail.messages[1].author, "Routed");
        assert!(detail.messages[1].text.contains("parallel"));
        assert_eq!(detail.messages[2].engine.as_deref(), Some("codex"));
        assert!(detail.messages[2].text.contains("analyze the cache key"));
        assert_eq!(detail.route_shape.as_deref(), Some("parallel"));
        assert_eq!(detail.budget.as_ref().unwrap().wall_time_secs, Some(1800));
        assert_eq!(
            detail.worktree.as_ref().unwrap().path.as_deref(),
            Some("~/.worktrees/run-20250508-1525")
        );
        assert_eq!(detail.worktree.as_ref().unwrap().state, "created");
        assert_eq!(detail.changed_files.len(), 2);
        assert_eq!(detail.changed_files[0].path, "src/cache/key.ts");
        assert_eq!(detail.changed_files[0].status, ChangedFileStatus::Modified);
        assert_eq!(
            detail.changed_files[1].path,
            "test/cache/cross-version.test.ts"
        );
        assert_eq!(detail.changed_files[1].status, ChangedFileStatus::Added);
        assert_eq!(detail.diff.as_ref().unwrap().files, 2);
        assert_eq!(detail.checks[0].name, "Unit tests");
        assert_eq!(detail.checks[0].passed, Some(true));
        assert_eq!(detail.checks[0].duration_ms, Some(42_000));
        assert!(detail.checks[0].output.as_deref().unwrap().contains("PASS"));
        assert_eq!(detail.usage.input_tokens, 12_000);
        assert_eq!(detail.usage.output_tokens, 4_000);
        assert!(detail.artifacts.is_empty());
        assert!(
            detail
                .activity
                .iter()
                .any(|row| row.path.as_deref() == Some("src/cache/key.ts"))
        );
    }

    #[test]
    fn repeated_usage_aggregates_tokens_without_activity_spam() {
        let state = Arc::new(Mutex::new(UiState::default()));
        apply_event(
            &state,
            event(1, "r1", "run.created", json!({ "engine": "claude" })),
        );
        apply_event(
            &state,
            event(
                2,
                "r1",
                "engine.usage",
                json!({ "input_tokens": 10, "output_tokens": 3 }),
            ),
        );
        apply_event(
            &state,
            event(
                3,
                "r1",
                "engine.usage",
                json!({ "input_tokens": 7, "output_tokens": 11 }),
            ),
        );

        let state = state.lock().unwrap();
        let detail = state.selected_detail().unwrap();
        assert_eq!(detail.usage.input_tokens, 17);
        assert_eq!(detail.usage.output_tokens, 14);
        assert_eq!(
            detail
                .activity
                .iter()
                .filter(|row| row.action == "usage")
                .count(),
            0
        );
        assert!(!state.events.iter().any(|line| line.contains("usage")));
    }

    #[test]
    fn engine_file_records_changed_file_and_legacy_file_change_still_works() {
        let state = Arc::new(Mutex::new(UiState::default()));
        apply_event(
            &state,
            event(1, "r1", "run.created", json!({ "engine": "claude" })),
        );
        apply_event(
            &state,
            event(
                2,
                "r1",
                "engine.file",
                json!({ "path": "src/cache/key.ts", "kind": "modified" }),
            ),
        );
        apply_event(
            &state,
            event(
                3,
                "r1",
                "engine.file_change",
                json!({ "path": "docs/cache.md", "kind": "created" }),
            ),
        );

        let state = state.lock().unwrap();
        let detail = state.selected_detail().unwrap();
        assert_eq!(
            detail
                .changed_files
                .iter()
                .map(|file| (file.path.as_str(), file.status))
                .collect::<Vec<_>>(),
            vec![
                ("src/cache/key.ts", ChangedFileStatus::Modified),
                ("docs/cache.md", ChangedFileStatus::Added),
            ]
        );
        assert!(
            state
                .chat()
                .iter()
                .any(|line| line == "edit src/cache/key.ts")
        );
        assert!(
            state
                .events
                .iter()
                .any(|line| line.contains("edited src/cache/key.ts"))
        );
    }

    #[test]
    fn approval_updates_detail_graph_when_switching_away_and_back() {
        let state = Arc::new(Mutex::new(UiState::default()));
        apply_event(
            &state,
            event(1, "r-approval", "run.created", json!({ "engine": "codex" })),
        );
        apply_event(
            &state,
            event(
                2,
                "r-approval",
                "run.awaiting_approval",
                approval_graph_payload(),
            ),
        );
        apply_event(
            &state,
            event(3, "r-other", "run.created", json!({ "engine": "claude" })),
        );
        apply_event(&state, event(4, "r-approval", "run.approved", json!({})));

        {
            let state = state.lock().unwrap();
            assert_eq!(state.run_id.as_deref(), Some("r-other"));
            assert!(
                !state.run_details["r-approval"]
                    .graph
                    .as_ref()
                    .unwrap()
                    .awaiting_approval
            );
        }

        state.lock().unwrap().select_run("r-approval");
        let state = state.lock().unwrap();
        assert!(
            !state
                .graph
                .as_ref()
                .expect("legacy graph should be restored on select")
                .awaiting_approval
        );
    }

    #[test]
    fn background_node_progress_updates_detail_before_selecting_that_run() {
        let state = Arc::new(Mutex::new(UiState::default()));
        apply_event(
            &state,
            event(1, "r-bg", "run.created", json!({ "engine": "codex" })),
        );
        apply_event(
            &state,
            event(2, "r-bg", "run.awaiting_approval", approval_graph_payload()),
        );
        apply_event(
            &state,
            event(
                3,
                "r-foreground",
                "run.created",
                json!({ "engine": "claude" }),
            ),
        );
        apply_event(
            &state,
            event(4, "r-bg", "node.started", json!({ "node_id": "edit" })),
        );
        apply_event(
            &state,
            event(
                5,
                "r-bg",
                "node.finished",
                json!({ "node_id": "edit", "state": "succeeded", "detail": "commit abc123" }),
            ),
        );

        state.lock().unwrap().select_run("r-bg");
        let state = state.lock().unwrap();
        let node = state
            .graph
            .as_ref()
            .unwrap()
            .nodes
            .iter()
            .find(|node| node.id == "edit")
            .unwrap();
        assert_eq!(node.state, "succeeded");
        assert_eq!(node.detail, "commit abc123");
    }

    #[test]
    fn usage_semantics_follow_provider_contract_without_activity_spam() {
        let state = Arc::new(Mutex::new(UiState::default()));
        apply_event(
            &state,
            event(1, "r-codex", "run.created", json!({ "engine": "codex" })),
        );
        apply_event(
            &state,
            event(
                2,
                "r-codex",
                "engine.usage",
                json!({ "input_tokens": 100, "output_tokens": 20 }),
            ),
        );
        apply_event(
            &state,
            event(
                3,
                "r-codex",
                "engine.usage",
                json!({ "input_tokens": 150, "output_tokens": 30 }),
            ),
        );
        apply_event(
            &state,
            event(4, "r-claude", "run.created", json!({ "engine": "claude" })),
        );
        apply_event(
            &state,
            event(
                5,
                "r-claude",
                "engine.usage",
                json!({ "input_tokens": 100, "output_tokens": 20 }),
            ),
        );
        apply_event(
            &state,
            event(
                6,
                "r-claude",
                "engine.usage",
                json!({ "input_tokens": 150, "output_tokens": 30 }),
            ),
        );

        let state = state.lock().unwrap();
        let codex = &state.run_details["r-codex"];
        assert_eq!(codex.usage.input_tokens, 150);
        assert_eq!(codex.usage.output_tokens, 30);
        let claude = &state.run_details["r-claude"];
        assert_eq!(claude.usage.input_tokens, 250);
        assert_eq!(claude.usage.output_tokens, 50);
        assert_eq!(
            codex
                .activity
                .iter()
                .filter(|row| row.action == "usage")
                .count(),
            0
        );
        assert_eq!(
            claude
                .activity
                .iter()
                .filter(|row| row.action == "usage")
                .count(),
            0
        );
    }

    #[test]
    fn per_run_detail_projections_are_tail_capped() {
        let state = Arc::new(Mutex::new(UiState::default()));
        apply_event(
            &state,
            event(1, "r-cap", "run.created", json!({ "engine": "codex" })),
        );
        for index in 0..410 {
            apply_event(
                &state,
                event(
                    10 + index,
                    "r-cap",
                    "engine.text",
                    json!({ "text": format!("message-{index:03}") }),
                ),
            );
        }
        for index in 0..210 {
            apply_event(
                &state,
                event(
                    1_000 + index,
                    "r-cap",
                    "engine.file",
                    json!({ "path": format!("src/file-{index:03}.rs"), "kind": "modified" }),
                ),
            );
        }
        for index in 0..110 {
            apply_event(
                &state,
                event(
                    2_000 + index,
                    "r-cap",
                    "run.check",
                    json!({
                        "name": format!("check-{index:03}"),
                        "passed": true,
                        "stdout": format!("ok-{index:03}")
                    }),
                ),
            );
        }
        let long_output = format!("{}TAIL-MARKER", "x".repeat(70 * 1024));
        apply_event(
            &state,
            event(
                3_000,
                "r-cap",
                "run.check",
                json!({
                    "name": "long output",
                    "passed": false,
                    "stdout": long_output
                }),
            ),
        );

        let state = state.lock().unwrap();
        let detail = state.selected_detail().unwrap();
        assert_eq!(detail.messages.len(), 400);
        assert_eq!(detail.messages.first().unwrap().text, "message-010");
        assert_eq!(detail.messages.last().unwrap().text, "message-409");
        assert_eq!(detail.activity.len(), 400);
        assert!(
            !detail
                .activity
                .iter()
                .any(|row| row.detail == "message-000")
        );
        assert!(
            detail
                .activity
                .iter()
                .any(|row| row.detail == "long output")
        );
        assert_eq!(detail.changed_files.len(), 200);
        assert_eq!(
            detail.changed_files.first().unwrap().path,
            "src/file-010.rs"
        );
        assert_eq!(detail.checks.len(), 100);
        assert_eq!(detail.checks.first().unwrap().name, "check-011");
        let output = detail.checks.last().unwrap().output.as_deref().unwrap();
        assert!(output.len() <= 64 * 1024);
        assert!(output.ends_with("TAIL-MARKER"));
    }

    /// The whole point of `replay_through_seq`: a relaunched app rebuilds the
    /// attention list from the ledger without ringing for any of it, then
    /// alerts normally on the first genuinely new event.
    #[test]
    fn replay_is_silent_and_the_first_live_event_alerts() {
        let state = Arc::new(Mutex::new(UiState::default()));
        let (tx, _rx) = mpsc::unbounded_channel();
        let sink = crate::notify::FakeNotificationSink::granted();

        apply_response(
            &state,
            &tx,
            Some(Pending::Subscribe),
            proto::Response::ok(
                "sub-1",
                json!({ "subscribed": true, "replay_through_seq": 42 }),
            ),
        );
        assert_eq!(state.lock().unwrap().replay_through_seq, 42);

        for seq in [7_u64, 42] {
            apply_event_with(
                &state,
                proto::Event::new(seq, Some("run-old".into()), seq, 0, "run.failed", json!({})),
                &sink,
            );
        }
        {
            let guard = state.lock().unwrap();
            assert_eq!(guard.attention.items.len(), 2, "history is still recorded");
            assert_eq!(guard.attention.unseen(), 0, "but arrives already read");
            assert_eq!(guard.attention.banner, None);
        }
        assert!(sink.posted().is_empty(), "replay must not notify");
        assert_eq!(sink.sound_count(), 0, "replay must not make a sound");

        apply_event_with(
            &state,
            proto::Event::new(43, Some("run-new".into()), 1, 0, "run.failed", json!({})),
            &sink,
        );
        let guard = state.lock().unwrap();
        assert_eq!(guard.attention.unseen(), 1);
        assert!(guard.attention.banner.is_some());
        assert_eq!(sink.posted().len(), 1);
        assert_eq!(sink.sound_count(), 1);
    }

    #[test]
    fn reconnect_replay_does_not_fold_the_same_global_sequence_twice() {
        let state = Arc::new(Mutex::new(UiState::default()));
        let duplicate = event(1, "run-1", "run.failed", json!({}));

        apply_event(&state, duplicate.clone());
        apply_event(&state, duplicate);

        let guard = state.lock().unwrap();
        assert_eq!(guard.events.len(), 1, "the event tail must not duplicate");
        assert_eq!(
            guard.attention.items.len(),
            1,
            "attention must not duplicate after reconnect replay"
        );
    }

    #[test]
    fn persisted_settings_gate_notifications_and_sounds_at_the_sink() {
        let state = Arc::new(Mutex::new(UiState::default()));
        let sink = crate::notify::FakeNotificationSink::granted();
        {
            let mut guard = state.lock().unwrap();
            guard.settings.values.notifications_enabled = false;
            guard.settings.values.sounds_enabled = false;
        }
        apply_event_with(
            &state,
            proto::Event::new(1, Some("run-1".into()), 1, 0, "engine.question", json!({})),
            &sink,
        );
        let guard = state.lock().unwrap();
        assert!(sink.posted().is_empty());
        assert_eq!(sink.sound_count(), 0);
        // The in-app banner still appears; nothing is silently dropped.
        assert!(guard.attention.banner.is_some());
        assert_eq!(guard.attention.unseen(), 1);
    }

    #[test]
    fn artifact_events_project_into_the_run_that_owns_them() {
        let state = Arc::new(Mutex::new(UiState::default()));
        {
            let mut guard = state.lock().unwrap();
            guard.run_id = Some("run-on-screen".into());
        }
        for (sequence, (run, name, kind, size)) in [
            ("run-on-screen", "src/cache/key.ts", "file", 2048_u64),
            ("run-in-background", "coverage/lcov.info", "file", 88_000),
        ]
        .into_iter()
        .enumerate()
        {
            apply_event(
                &state,
                proto::Event::new(
                    sequence as u64 + 1,
                    Some(run.into()),
                    1,
                    0,
                    "artifact.created",
                    json!({
                        "run_id": run,
                        "id": "a1",
                        "kind": kind,
                        "name": name,
                        "path": name,
                        "byte_size": size,
                        "summary": " 1 file changed",
                    }),
                ),
            );
        }

        let state = state.lock().unwrap();
        let shown = &state.run_details["run-on-screen"].artifacts;
        assert_eq!(shown.len(), 1);
        assert_eq!(shown[0].name, "src/cache/key.ts");
        assert_eq!(shown[0].size_bytes, Some(2048));
        assert_eq!(shown[0].kind.as_deref(), Some("file"));
        // A background run keeps its own artifacts, so switching to it shows
        // them without a refetch.
        let background = &state.run_details["run-in-background"].artifacts;
        assert_eq!(background.len(), 1);
        assert_eq!(background[0].name, "coverage/lcov.info");
    }

    #[test]
    fn node_check_details_reach_the_check_card() {
        let state = Arc::new(Mutex::new(UiState::default()));
        {
            let mut guard = state.lock().unwrap();
            guard.run_id = Some("run-1".into());
        }
        apply_event(
            &state,
            proto::Event::new(
                1,
                Some("run-1".into()),
                1,
                0,
                "node.check",
                json!({
                    "run_id": "run-1",
                    "node_id": "verify",
                    "command": "npm test",
                    "passed": false,
                    "exit_code": 1,
                    "duration_ms": 42_000,
                    "stdout": "3 passing",
                    "stderr": "1 failing",
                }),
            ),
        );
        let state = state.lock().unwrap();
        let check = state.run_details["run-1"].checks.last().unwrap();
        assert_eq!(check.name, "verify");
        assert_eq!(check.passed, Some(false));
        assert_eq!(check.duration_ms, Some(42_000));
        let output = check.output.as_deref().unwrap();
        assert!(output.contains("3 passing"), "{output}");
        assert!(output.contains("1 failing"), "{output}");
    }

    #[test]
    fn artifact_projection_is_tail_capped() {
        let mut detail = RunDetailView::default();
        for index in 0..110 {
            detail.push_artifact(ArtifactView {
                name: format!("artifact-{index:03}"),
                path: format!("artifacts/{index:03}.json"),
                kind: Some("json".into()),
                size_bytes: Some(index),
            });
        }

        assert_eq!(detail.artifacts.len(), 100);
        assert_eq!(detail.artifacts.first().unwrap().name, "artifact-010");
        assert_eq!(detail.artifacts.last().unwrap().name, "artifact-109");
    }

    #[test]
    fn events_rebuild_chat_and_summary_in_replay_order() {
        let state = Arc::new(Mutex::new(UiState::default()));
        for e in [
            event(1, "r1", "run.created", json!({})),
            event(
                2,
                "r1",
                "run.worktree_created",
                json!({ "path": "/wt", "branch": "ah/run-1" }),
            ),
            event(3, "r1", "run.started", json!({ "engine": "codex" })),
            event(4, "r1", "chat.message", json!({ "content": "do it" })),
            event(5, "r1", "engine.text", json!({ "text": "done" })),
            event(
                6,
                "r1",
                "run.check",
                json!({ "passed": true, "command": "cargo test" }),
            ),
            event(
                7,
                "r1",
                "run.commit",
                json!({ "commit": "abc123", "diff_stat": " a.rs | 1 +" }),
            ),
            event(8, "r1", "run.succeeded", json!({})),
        ] {
            apply_event(&state, e);
        }
        let state = state.lock().unwrap();
        assert_eq!(state.run_id.as_deref(), Some("r1"));
        assert_eq!(state.run_state, "succeeded");
        assert_eq!(state.chat(), ["you  do it", "bot  done"]);
        assert_eq!(
            state.summary,
            [
                "worktree /wt on ah/run-1",
                "check passed: cargo test",
                "commit abc123",
                " a.rs | 1 +",
            ]
        );
        // The activity pane says what HAPPENED, in words. A chat message
        // belongs in the transcript, not duplicated as a log line.
        assert_eq!(state.events.len(), 7);
        assert!(state.events.iter().any(|e| e.contains("check passed")));
        assert!(state.events.iter().any(|e| e.contains("committed abc123")));
        assert!(!state.events.iter().any(|e| e.contains("chat.message")));
    }

    #[test]
    fn events_for_other_runs_do_not_leak_into_the_chat() {
        let state = Arc::new(Mutex::new(UiState::default()));
        apply_event(&state, event(1, "r1", "run.created", json!({})));
        apply_event(
            &state,
            event(2, "r2", "engine.text", json!({ "text": "x" })),
        );
        let state = state.lock().unwrap();
        assert!(state.chat().is_empty());
        // The activity tail still shows both, since it is not run-scoped.
        assert_eq!(state.events.len(), 2);
    }

    fn engine(name: &str, ready: bool, installed: bool, auth: Option<bool>) -> EngineStatus {
        EngineStatus {
            name: name.into(),
            ready,
            installed,
            authenticated: auth,
            version: Some("1.2.3".into()),
            problems: vec![],
            models: vec![],
            model_load_error: None,
        }
    }

    /// A not-ready engine must tell the user what to DO, not what broke.
    #[test]
    fn engine_summary_names_the_fix() {
        // A ready engine shows its version; "ready" is carried by the mark.
        assert!(
            engine("codex", true, true, Some(true))
                .summary()
                .contains("1.2.3")
        );
        assert!(
            engine("claude", false, false, None)
                .summary()
                .contains("not installed")
        );
        // The row says what is wrong; the command lives where there is room.
        assert!(
            engine("codex", false, true, Some(false))
                .summary()
                .contains("sign in")
        );
        assert_eq!(
            engine("codex", false, true, Some(false)).sign_in_command(),
            Some("codex login")
        );
        assert_eq!(
            engine("claude", false, true, Some(false)).sign_in_command(),
            Some("claude auth login")
        );
        // A ready engine has nothing to sign into.
        assert_eq!(
            engine("codex", true, true, Some(true)).sign_in_command(),
            None
        );
        // Rows must fit a 240px sidebar.
        for e in [
            engine("codex", true, true, Some(true)),
            engine("claude", false, true, Some(false)),
            engine("claude", false, false, None),
        ] {
            assert!(
                e.summary().chars().count() <= 28,
                "too wide: {:?}",
                e.summary()
            );
        }
        // No verdict and no problem text still yields something printable.
        assert!(!engine("codex", false, true, None).summary().is_empty());
    }

    #[test]
    fn engine_list_preselects_a_usable_engine() {
        let state = Arc::new(Mutex::new(UiState {
            engine: "codex".into(),
            ..UiState::default()
        }));
        let (tx, _rx) = mpsc::unbounded_channel();
        // codex is unusable, claude is ready: the UI must switch rather than
        // let the user type an objective into an engine that cannot run.
        let result = json!([
            { "engine": "codex", "ready": false, "installed": true, "authenticated": false },
            { "engine": "claude", "ready": true, "installed": true, "authenticated": true,
              "version": "2.1.221" },
        ]);
        apply_response(
            &state,
            &tx,
            Some(Pending::Engines),
            proto::Response::ok("ui-1", result),
        );
        let state = state.lock().unwrap();
        assert_eq!(state.engine, "claude");
        assert_eq!(state.engines.len(), 2);
        assert_eq!(state.engines[0].sign_in_command(), Some("codex login"));
    }

    #[test]
    fn project_add_response_selects_the_daemon_resolved_repository() {
        let state = Arc::new(Mutex::new(UiState::default()));
        let (tx, _rx) = mpsc::unbounded_channel();
        apply_response(
            &state,
            &tx,
            Some(Pending::ProjectAdd),
            proto::Response::ok(
                "project",
                json!({
                    "id": "repo-1",
                    "name": "autoharness",
                    "path": "/code/autoharness"
                }),
            ),
        );

        let state = state.lock().unwrap();
        assert_eq!(state.projects.len(), 1);
        assert_eq!(state.selected_project().unwrap().id, "repo-1");
        assert_eq!(state.status, "repository: autoharness");
    }

    #[test]
    fn model_and_reasoning_selection_reconcile_as_one_provider_capability() {
        let state = Arc::new(Mutex::new(UiState {
            engine: "codex".into(),
            ..UiState::default()
        }));
        let (tx, _rx) = mpsc::unbounded_channel();
        apply_response(
            &state,
            &tx,
            Some(Pending::Engines),
            proto::Response::ok(
                "models",
                json!([{
                    "engine": "codex",
                    "ready": true,
                    "installed": true,
                    "authenticated": true,
                    "models": [
                        {
                            "id": "sol",
                            "display_name": "5.6-Sol",
                            "description": "frontier",
                            "reasoning_efforts": ["low", "high"],
                            "default_reasoning_effort": "high",
                            "is_default": true
                        },
                        {
                            "id": "terra",
                            "display_name": "5.6-Terra",
                            "reasoning_efforts": ["low", "medium"],
                            "default_reasoning_effort": "medium",
                            "is_default": false
                        }
                    ]
                }]),
            ),
        );

        let mut state = state.lock().unwrap();
        assert_eq!(state.model.as_deref(), Some("sol"));
        assert_eq!(state.reasoning_effort.as_deref(), Some("high"));
        assert_eq!(state.execution_selection_label(), "5.6-Sol · High");
        assert!(state.select_model("terra"));
        assert_eq!(state.reasoning_effort.as_deref(), Some("medium"));
        assert!(!state.select_reasoning_effort("high"));
        assert!(state.select_reasoning_effort("low"));
        assert_eq!(state.execution_selection_label(), "5.6-Terra · Low");
    }

    #[test]
    fn startup_asks_for_everything_a_user_needs_before_their_first_objective() {
        assert_eq!(
            startup_commands(),
            vec![
                Command::RefreshProjects,
                Command::RefreshEngines,
                // Both preconditions that can refuse a run are measured at
                // launch, not at the first objective: an engine that is not
                // signed in, and a sandbox whose canaries fail.
                Command::CheckSandbox,
                Command::RefreshHistory { cursor: None },
                Command::RefreshSettings,
                Command::RefreshUsage,
                Command::RefreshQueue,
            ]
        );
    }

    #[test]
    fn objective_steering_and_queue_controls_use_durable_typed_rpcs() {
        let (method, params, pending) = encode(Command::StartRun {
            project_id: "project-1".into(),
            engine: "codex".into(),
            model: Some("gpt-5.6-sol".into()),
            reasoning_effort: Some("high".into()),
            objective: "finish it".into(),
            check_command: Some("cargo test".into()),
            parent_run_id: None,
        });
        assert_eq!(method, methods::RUN_ENQUEUE);
        assert_eq!(params["objective"], "finish it");
        assert_eq!(params["model"], "gpt-5.6-sol");
        assert_eq!(params["reasoning_effort"], "high");
        assert!(
            params["request_id"]
                .as_str()
                .is_some_and(|id| !id.is_empty())
        );
        assert_eq!(pending, Pending::RunEnqueue);

        let (method, params, _) = encode(Command::Chat {
            run_id: "run-1".into(),
            message: "also add tests".into(),
        });
        assert_eq!(method, methods::CHAT_SEND);
        assert!(
            params["request_id"]
                .as_str()
                .is_some_and(|id| !id.is_empty())
        );

        let (method, params, pending) = encode(Command::RefreshQueue);
        assert_eq!(method, methods::QUEUE_LIST);
        assert_eq!(params["include_terminal"], false);
        assert_eq!(pending, Pending::QueueList);

        let (method, params, pending) = encode(Command::MoveQueueItem {
            item_id: "queue-2".into(),
            before_item_id: Some("queue-1".into()),
        });
        assert_eq!(method, methods::QUEUE_MOVE);
        assert_eq!(params["before_item_id"], "queue-1");
        assert_eq!(pending, Pending::QueueMutation);

        let (method, params, pending) = encode(Command::CancelQueueItem {
            item_id: "queue-2".into(),
        });
        assert_eq!(method, methods::QUEUE_CANCEL);
        assert!(params["request_id"].is_string());
        assert_eq!(pending, Pending::QueueMutation);
    }

    #[test]
    fn node_controls_encode_only_typed_retry_and_cancel_rpcs() {
        for (retry, expected) in [(true, methods::NODE_RETRY), (false, methods::NODE_CANCEL)] {
            let (method, params, pending) = encode(Command::NodeControl {
                run_id: "run-1".into(),
                node_id: "edit".into(),
                retry,
            });
            assert_eq!(method, expected);
            assert_eq!(params, json!({ "run_id": "run-1", "node_id": "edit" }));
            assert!(matches!(pending, Pending::Report(_)));
        }
    }

    #[test]
    fn queue_list_response_and_events_project_authoritative_order_and_state() {
        let state = Arc::new(Mutex::new(UiState::default()));
        let (tx, _rx) = mpsc::unbounded_channel();
        apply_response(
            &state,
            &tx,
            Some(Pending::QueueList),
            proto::Response::ok(
                "queue-list",
                json!({
                    "items": [
                        {
                            "id": "queue-2", "kind": "objective", "state": "pending",
                            "run_id": "run-2", "project_id": "project-1",
                            "content": "second", "position": 2048,
                            "created_at_ms": 2, "updated_at_ms": 2, "error": null
                        },
                        {
                            "id": "queue-1", "kind": "objective", "state": "dispatching",
                            "run_id": "run-1", "project_id": "project-1",
                            "content": "first", "position": 1024,
                            "created_at_ms": 1, "updated_at_ms": 1, "error": null
                        }
                    ]
                }),
            ),
        );
        {
            let state = state.lock().unwrap();
            assert_eq!(
                state
                    .queue
                    .items
                    .iter()
                    .map(|item| item.content.as_str())
                    .collect::<Vec<_>>(),
                vec!["first", "second"]
            );
            assert_eq!(state.queue.pending_objectives(), 1);
        }

        apply_event(
            &state,
            event(
                1,
                "run-2",
                "queue.cancelled",
                json!({ "queue_id": "queue-2", "run_id": "run-2" }),
            ),
        );
        let state = state.lock().unwrap();
        assert_eq!(
            state.queue.items[1].state,
            proto::params::QueueState::Cancelled
        );
        assert_eq!(state.queue.pending_objectives(), 0);
    }

    #[test]
    fn settings_and_usage_commands_encode_with_protocol_methods_and_typed_fields() {
        let (method, params, pending) = encode(Command::RefreshSettings);
        assert_eq!(method, methods::SETTINGS_GET);
        assert_eq!(params, json!({}));
        assert_eq!(pending, Pending::SettingsGet);

        let update: proto::params::SettingsUpdate = serde_json::from_value(json!({
            "default_engine": "claude",
            "default_route_mode": "parallel",
            "max_parallel_workers": 4,
            "notifications_enabled": false,
        }))
        .unwrap();
        let (method, params, pending) = encode(Command::UpdateSettings(update));
        assert_eq!(method, methods::SETTINGS_UPDATE);
        assert_eq!(params["default_engine"], "claude");
        assert_eq!(params["default_route_mode"], "parallel");
        assert_eq!(params["max_parallel_workers"], 4);
        assert_eq!(params["notifications_enabled"], false);
        assert_eq!(pending, Pending::SettingsUpdate);

        let (method, params, pending) = encode(Command::RefreshUsage);
        assert_eq!(method, methods::USAGE_SUMMARY);
        assert_eq!(params, json!({}));
        assert_eq!(pending, Pending::UsageSummary);
    }

    #[test]
    fn worktree_commands_encode_filters_and_only_confirm_real_reclaims() {
        let (method, params, pending) =
            encode(Command::RefreshWorktrees(WorktreeFilter::OnlyEligible));
        assert_eq!(method, methods::WORKTREE_LIST);
        assert_eq!(params["only_eligible"], true);
        assert_eq!(params["include_reclaimed"], false);
        assert_eq!(pending, Pending::WorktreeList);

        let (_, params, _) = encode(Command::RefreshWorktrees(WorktreeFilter::IncludeReclaimed));
        assert_eq!(params["include_reclaimed"], true);
        assert_eq!(params["only_eligible"], false);

        // A dry run never carries a confirmation, so it can never remove.
        let (method, params, pending) = encode(Command::ReclaimWorktree {
            path: "/data/worktrees/run-1".into(),
            dry_run: true,
        });
        assert_eq!(method, methods::WORKTREE_RECLAIM);
        assert_eq!(params["dry_run"], true);
        assert!(params["confirm_path"].is_null());
        assert_eq!(
            pending,
            Pending::WorktreeReclaim {
                path: "/data/worktrees/run-1".into(),
                dry_run: true,
            }
        );

        let (_, params, _) = encode(Command::ReclaimWorktree {
            path: "/data/worktrees/run-1".into(),
            dry_run: false,
        });
        assert_eq!(params["dry_run"], false);
        assert_eq!(params["confirm_path"], params["path"]);
    }

    #[test]
    fn a_finished_reclaim_refreshes_the_list_and_a_dry_run_arms_confirmation() {
        let state = Arc::new(Mutex::new(UiState::default()));
        let (tx, mut rx) = mpsc::unbounded_channel();
        let eligible = json!({
            "path": "/data/worktrees/run-1",
            "dry_run": true,
            "reclaimed": false,
            "already_reclaimed": false,
            "eligible": true,
            "blockers": [],
            "entry": null
        });
        apply_response(
            &state,
            &tx,
            Some(Pending::WorktreeReclaim {
                path: "/data/worktrees/run-1".into(),
                dry_run: true,
            }),
            proto::Response::ok("dry", eligible),
        );
        {
            let state = state.lock().unwrap();
            assert_eq!(
                state.worktrees.pending_confirm.as_deref(),
                Some("/data/worktrees/run-1")
            );
            assert!(
                state
                    .worktrees
                    .diagnostics
                    .iter()
                    .any(|line| line.contains("eligible"))
            );
        }
        assert!(rx.try_recv().is_err(), "a dry run asks for nothing else");

        let done = json!({
            "path": "/data/worktrees/run-1",
            "dry_run": false,
            "reclaimed": true,
            "already_reclaimed": false,
            "eligible": true,
            "blockers": [],
            "entry": null
        });
        apply_response(
            &state,
            &tx,
            Some(Pending::WorktreeReclaim {
                path: "/data/worktrees/run-1".into(),
                dry_run: false,
            }),
            proto::Response::ok("real", done),
        );
        let state_guard = state.lock().unwrap();
        assert_eq!(state_guard.worktrees.pending_confirm, None);
        assert!(state_guard.status.contains("reclaimed"));
        drop(state_guard);
        assert_eq!(
            rx.try_recv().unwrap(),
            Command::RefreshWorktrees(WorktreeFilter::Live),
            "the list is stale after a reclaim"
        );
    }

    #[test]
    fn blocked_reclaims_report_the_reason_and_arm_nothing() {
        let state = Arc::new(Mutex::new(UiState::default()));
        let (tx, _rx) = mpsc::unbounded_channel();
        let blocked = json!({
            "path": "/data/worktrees/run-1",
            "dry_run": true,
            "reclaimed": false,
            "already_reclaimed": false,
            "eligible": false,
            "blockers": ["dirty_worktree", "process_check_unknown"],
            "entry": null
        });
        apply_response(
            &state,
            &tx,
            Some(Pending::WorktreeReclaim {
                path: "/data/worktrees/run-1".into(),
                dry_run: true,
            }),
            proto::Response::ok("dry", blocked),
        );
        let state = state.lock().unwrap();
        assert_eq!(state.worktrees.pending_confirm, None);
        let text = state.worktrees.diagnostics.join(" | ");
        assert!(text.contains("uncommitted changes present"), "{text}");
        assert!(text.contains("could not answer"), "{text}");
    }

    #[test]
    fn responses_update_typed_settings_and_ledger_usage_state() {
        let state = Arc::new(Mutex::new(UiState::default()));
        let (tx, _rx) = mpsc::unbounded_channel();
        let settings: proto::params::AppSettings = serde_json::from_value(json!({
            "version": 1,
            "default_engine": "claude",
            "default_route_mode": "priority",
            "max_parallel_workers": 3,
            "max_graph_nodes": 8,
            "default_wall_time_minutes": 30,
            "retention_days": 90,
            "automatic_history_scan": true,
            "notifications_enabled": false,
            "sounds_enabled": true,
            "automatic_update_checks": true,
            "confirm_destructive_actions": true,
        }))
        .unwrap();

        apply_response(
            &state,
            &tx,
            Some(Pending::SettingsGet),
            proto::Response::ok("settings", serde_json::to_value(&settings).unwrap()),
        );
        {
            let state = state.lock().unwrap();
            assert_eq!(state.settings.values, settings);
            assert!(!state.settings.loading);
            assert_eq!(state.engine, "claude");
            assert_eq!(state.settings.error, None);
        }

        let usage = json!({
            "generated_at_ms": 1775260800000_i64,
            "today_start_ms": 1775260800000_i64,
            "month_start_ms": 1775260800000_i64,
            "providers": [{
                "provider": "codex",
                "today": { "input_tokens": 100, "output_tokens": 40, "total_tokens": 140, "event_count": 2, "run_count": 1 },
                "month": { "input_tokens": 100, "output_tokens": 40, "total_tokens": 140, "event_count": 2, "run_count": 1 },
                "all_time": { "input_tokens": 220, "output_tokens": 80, "total_tokens": 300, "event_count": 5, "run_count": 2 }
            }],
            "runs": [{
                "run_id": "r-usage",
                "provider": "codex",
                "input_tokens": 100,
                "output_tokens": 40,
                "total_tokens": 140,
                "event_count": 2
            }]
        });
        apply_response(
            &state,
            &tx,
            Some(Pending::UsageSummary),
            proto::Response::ok("usage", usage),
        );
        let state = state.lock().unwrap();
        assert!(!state.usage_summary.loading);
        assert_eq!(state.usage_summary.providers[0].provider, "codex");
        assert_eq!(state.usage_summary.providers[0].all_time.total_tokens, 300);
        assert_eq!(state.usage_summary.runs[0].run_id, "r-usage");
    }

    #[test]
    fn late_settings_response_does_not_overwrite_selected_run_execution() {
        let mut state = UiState {
            run_id: Some("run-claude".into()),
            engine: "claude".into(),
            model: None,
            reasoning_effort: Some("high".into()),
            ..UiState::default()
        };
        let settings = proto::params::AppSettings {
            default_engine: autoharness_core::EngineKind::codex(),
            ..Default::default()
        };

        apply_settings(&mut state, settings.clone());

        assert_eq!(state.engine, "claude");
        assert_eq!(state.model, None);
        assert_eq!(state.reasoning_effort.as_deref(), Some("high"));
        assert_eq!(state.settings.values, settings);
    }

    #[test]
    fn settings_updated_event_replays_into_settings_state() {
        let state = Arc::new(Mutex::new(UiState::default()));
        apply_event(
            &state,
            proto::Event::new(
                1,
                None,
                1,
                0,
                "settings.updated",
                json!({
                    "version": 1,
                    "default_engine": "claude",
                    "default_route_mode": "parallel",
                    "max_parallel_workers": 4,
                    "max_graph_nodes": 10,
                    "default_wall_time_minutes": 45,
                    "retention_days": 30,
                    "automatic_history_scan": false,
                    "notifications_enabled": true,
                    "sounds_enabled": false,
                    "automatic_update_checks": true,
                    "confirm_destructive_actions": true
                }),
            ),
        );
        let state = state.lock().unwrap();
        assert_eq!(state.settings.values.default_engine.to_string(), "claude");
        assert_eq!(
            state.settings.values.default_route_mode.as_str(),
            "parallel"
        );
        assert_eq!(state.settings.values.max_parallel_workers, 4);
        assert_eq!(state.settings.error, None);
    }

    #[test]
    fn event_tail_is_bounded() {
        let state = Arc::new(Mutex::new(UiState::default()));
        for seq in 0..(EVENT_TAIL as u64 + 50) {
            apply_event(&state, event(seq, "r1", "tick", json!({})));
        }
        assert_eq!(state.lock().unwrap().events.len(), EVENT_TAIL);
    }

    /// The graph a user reviews must be rebuildable from replay alone, and it
    /// must track node state as the run proceeds.
    #[test]
    fn the_plan_and_its_live_node_states_rebuild_from_replay() {
        let state = Arc::new(Mutex::new(UiState::default()));
        apply_event(&state, event(1, "r1", "run.created", json!({})));
        apply_event(
            &state,
            event(
                2,
                "r1",
                "run.awaiting_approval",
                json!({
                    "graph": {
                        "nodes": [
                            {
                                "id": "edit_a",
                                "role": "editor",
                                "objective": "Edit module A",
                                "file_scope": ["src/a/**"],
                                "acceptance_checks": ["cargo test -p a"],
                                "depends_on": []
                            },
                            { "id": "edit_b", "role": "editor" },
                            {
                                "id": "merge",
                                "role": "integration",
                                "objective": "Integrate verified branches",
                                "depends_on": ["edit_a", "edit_b"]
                            },
                        ],
                        "edges": [["edit_a", "merge"], ["edit_b", "merge"]],
                        "waves": [["edit_a", "edit_b"], ["merge"]],
                    }
                }),
            ),
        );

        {
            let s = state.lock().unwrap();
            let graph = s.graph.as_ref().expect("the plan must be shown for review");
            assert!(graph.awaiting_approval);
            assert_eq!(graph.wave_count(), 2);
            assert_eq!(graph.nodes.len(), 3);
            // Wave placement drives the layout, so it must be right.
            assert_eq!(
                graph.nodes.iter().find(|n| n.id == "merge").unwrap().wave,
                1
            );
            assert_eq!(graph.edges.len(), 2);
            let edit_a = graph.nodes.iter().find(|node| node.id == "edit_a").unwrap();
            assert_eq!(edit_a.objective, "Edit module A");
            assert_eq!(edit_a.file_scope, vec!["src/a/**"]);
            assert_eq!(edit_a.acceptance_checks, vec!["cargo test -p a"]);
            assert_eq!(
                graph
                    .nodes
                    .iter()
                    .find(|node| node.id == "merge")
                    .unwrap()
                    .depends_on,
                vec!["edit_a", "edit_b"]
            );
            assert_eq!(s.run_state, "awaiting_approval");
        }

        apply_event(&state, event(3, "r1", "run.approved", json!({})));
        apply_event(
            &state,
            event(4, "r1", "node.started", json!({ "node_id": "edit_a" })),
        );
        apply_event(
            &state,
            event(
                5,
                "r1",
                "node.finished",
                json!({ "node_id": "edit_a", "state": "succeeded", "detail": "abc123" }),
            ),
        );

        let s = state.lock().unwrap();
        let graph = s.graph.as_ref().unwrap();
        assert!(!graph.awaiting_approval);
        let node = graph.nodes.iter().find(|n| n.id == "edit_a").unwrap();
        assert_eq!(node.state, "succeeded");
        assert_eq!(node.detail, "abc123");
        // Untouched nodes stay pending rather than inheriting a sibling's state.
        assert_eq!(
            graph.nodes.iter().find(|n| n.id == "merge").unwrap().state,
            "pending"
        );
    }

    /// A refused plan must tell the user what was wrong with it, not just
    /// silently run something else.
    #[test]
    fn a_rejected_plan_surfaces_its_problems() {
        let state = Arc::new(Mutex::new(UiState::default()));
        apply_event(&state, event(1, "r1", "run.created", json!({})));
        apply_event(
            &state,
            event(
                2,
                "r1",
                "graph.rejected",
                json!({ "problems": ["edit_a and edit_b would both edit src/**"] }),
            ),
        );
        let s = state.lock().unwrap();
        assert!(
            s.summary
                .iter()
                .any(|line| line.contains("would both edit"))
        );
        assert!(s.status.contains("bounded loop"));
    }

    /// Node states must reach the shared status vocabulary, or the graph is
    /// decoration. The colours themselves are asserted in `theme`.
    #[test]
    fn node_states_map_onto_distinct_statuses() {
        use crate::theme::Status;
        let running = Status::of_node("running");
        let succeeded = Status::of_node("succeeded");
        let failed = Status::of_node("failed");
        assert_ne!(running, succeeded);
        assert_ne!(succeeded, failed);
        assert_ne!(running, failed);
        assert_ne!(running.color("codex"), succeeded.color("codex"));
        assert_ne!(succeeded.color("codex"), failed.color("codex"));
    }

    /// Launching from inside a project should be enough. Walking up for `.git`
    /// The current directory is process-global, so the tests that change it
    /// must not run at the same time as each other.
    static CWD_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// means `cd crates/ui && autoharness` still finds the workspace root.
    #[test]
    fn the_launch_directory_resolves_to_its_repository_root() {
        let _guard = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("myrepo");
        let nested = repo.join("crates").join("ui").join("src");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::create_dir_all(repo.join(".git")).unwrap();

        let previous = std::env::current_dir().unwrap();
        std::env::set_current_dir(&nested).unwrap();
        let detected = repo_at_cwd();
        std::env::set_current_dir(previous).unwrap();

        let (name, path) = detected.expect("a repo above the cwd must be found");
        assert_eq!(name, "myrepo");
        assert!(path.ends_with("myrepo"), "{path}");
    }

    /// A worktree's `.git` is a file, not a directory, and must still count.
    #[test]
    fn a_worktree_checkout_counts_as_a_repository() {
        let _guard = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let root = tempfile::tempdir().unwrap();
        let worktree = root.path().join("wt");
        std::fs::create_dir_all(&worktree).unwrap();
        std::fs::write(worktree.join(".git"), "gitdir: /elsewhere\n").unwrap();

        let previous = std::env::current_dir().unwrap();
        std::env::set_current_dir(&worktree).unwrap();
        let detected = repo_at_cwd();
        std::env::set_current_dir(previous).unwrap();

        assert_eq!(detected.map(|(name, _)| name), Some("wt".to_string()));
    }

    /// A patch must survive parsing intact: counts right, line numbers right,
    /// and nothing silently dropped.
    #[test]
    fn a_unified_diff_parses_into_reviewable_lines() {
        let patch = "diff --git a/src/lib.rs b/src/lib.rs\n\
index 111..222 100644\n\
--- a/src/lib.rs\n\
+++ b/src/lib.rs\n\
@@ -10,3 +10,4 @@ fn existing() {\n\
 context line\n\
-let old = 1;\n\
+let new = 2;\n\
+let extra = 3;\n";
        let view = parse_diff(patch);
        assert_eq!(view.files, 1);
        assert_eq!(view.added, 2);
        assert_eq!(view.removed, 1);
        assert_eq!(view.lines[0], DiffLine::File("src/lib.rs".into()));
        // Metadata and the redundant ---/+++ pair are not shown as content.
        assert!(
            !view
                .lines
                .iter()
                .any(|l| matches!(l, DiffLine::Context(t) if t.starts_with("index")))
        );
        assert!(matches!(view.lines[1], DiffLine::Hunk(_)));
        assert_eq!(view.lines[2], DiffLine::Context("context line".into()));
        assert_eq!(view.lines[3], DiffLine::Removed("let old = 1;".into()));
    }

    /// A truncation note must reach the UI, or a partial patch reads as a
    /// complete one.
    #[test]
    fn a_truncated_patch_keeps_its_note() {
        let view = parse_diff("diff --git a/a b/a\n@@ -1 +1 @@\n+x\n… diff truncated at 10 bytes");
        assert!(matches!(view.lines.last(), Some(DiffLine::Note(_))));
    }

    #[test]
    fn an_empty_or_surprising_patch_does_not_panic() {
        assert_eq!(parse_diff("").lines.len(), 0);
        // Anything unrecognized renders plainly rather than vanishing.
        let view = parse_diff("total nonsense\nmore nonsense");
        assert_eq!(view.lines.len(), 2);
        assert_eq!(view.files, 0);
    }

    fn threaded_state() -> UiState {
        let run = |id: &str, parent: Option<&str>, objective: &str, state: &str| RunView {
            id: id.into(),
            project_id: "p1".into(),
            objective: objective.into(),
            state: state.into(),
            engine: "codex".into(),
            parent_run_id: parent.map(str::to_string),
            attempt_group: None,
        };
        UiState {
            runs: vec![
                run("r1", None, "Fix the typo", "succeeded"),
                run("r2", Some("r1"), "also update the heading", "succeeded"),
                run("r3", Some("r2"), "and the footer", "running"),
                run("other", None, "Unrelated work", "succeeded"),
            ],
            ..UiState::default()
        }
    }

    /// The bug this exists to prevent: selecting a thread selects its ROOT, so
    /// a transcript built by walking to ANCESTORS showed the opening turn and
    /// hid every reply after it. Switching away and back read as the app
    /// having erased the conversation.
    #[test]
    fn the_transcript_covers_every_turn_of_the_thread() {
        let mut state = threaded_state();
        state.run_id = Some("r1".into());
        assert_eq!(state.thread_run_ids(), ["r1", "r2", "r3"]);
        // Opening any turn shows the whole conversation, not a suffix of it.
        state.run_id = Some("r3".into());
        assert_eq!(state.thread_run_ids(), ["r1", "r2", "r3"]);
        // A separate thread stays separate.
        state.run_id = Some("other".into());
        assert_eq!(state.thread_run_ids(), ["other"]);
    }

    /// The bug this exists to prevent: a reply must extend the conversation,
    /// not appear to start a new one.
    /// Submitting into the draft that `+` created must fill THAT run in.
    ///
    /// The regression this guards is two rows for one piece of work: clicking
    /// `+` creates a run, and if the composer then took the ordinary
    /// create-a-run path there would be a second one beside it, with the first
    /// left as an empty draft forever.
    /// Opening a run to READ it must not silently replace a choice the user
    /// just made.
    ///
    /// `select_run` adopts the run's engine and model so the composer shows
    /// where a follow-up would go. That is right by default and wrong after a
    /// deliberate pick: choose Opus, click an old Codex run to look at it, and
    /// the next objective went to Codex without anything saying so.
    #[test]
    fn reading_an_old_run_does_not_overwrite_a_deliberate_choice() {
        // A catalog, so reconciliation keeps a model that really exists rather
        // than falling back to the provider default.
        let mut state = UiState {
            engines: vec![EngineStatus {
                name: "codex".into(),
                ready: true,
                installed: true,
                authenticated: Some(true),
                version: None,
                problems: Vec::new(),
                models: vec![EngineModelView {
                    id: "gpt-5.6-sol".into(),
                    display_name: "GPT-5.6-Sol".into(),
                    description: String::new(),
                    reasoning_efforts: vec!["low".into()],
                    default_reasoning_effort: Some("low".into()),
                    is_default: true,
                }],
                model_load_error: None,
            }],
            ..UiState::default()
        };
        state.runs.push(RunView {
            id: "old".into(),
            project_id: "p1".into(),
            objective: "earlier".into(),
            state: "succeeded".into(),
            engine: "codex".into(),
            parent_run_id: None,
            attempt_group: None,
        });
        state.run_details.insert(
            "old".into(),
            RunDetailView {
                run_id: "old".into(),
                engine: Some("codex".into()),
                model: Some("gpt-5.6-sol".into()),
                ..Default::default()
            },
        );

        // No deliberate pick: selecting adopts what that run used, which is
        // the useful default.
        state.select_run("old");
        assert_eq!(state.engine, "codex");
        assert_eq!(state.model.as_deref(), Some("gpt-5.6-sol"));

        // A deliberate pick survives reading another run.
        state.set_engine("claude");
        assert!(state.execution_pinned_by_user);
        state.select_run("old");
        assert_eq!(
            state.engine, "claude",
            "the pick the user made is still the pick"
        );
        assert!(state.model.is_none() || state.model.as_deref() != Some("gpt-5.6-sol"));
    }

    /// An attempt nobody judged is never shown as one that worked.
    ///
    /// That mistake is the whole reason to verify: a run that finished is not
    /// a run that succeeded, and reading "succeeded" as "the check passed" is
    /// how a harness reports work it never validated.
    #[test]
    fn an_unjudged_attempt_is_not_reported_as_a_passing_one() {
        let mut state = UiState::default();
        let group = Some("g1".to_string());
        for (id, engine) in [("a", "codex"), ("b", "claude")] {
            state.runs.push(RunView {
                id: id.into(),
                project_id: "p1".into(),
                objective: "same question".into(),
                state: "succeeded".into(),
                engine: engine.into(),
                parent_run_id: None,
                attempt_group: group.clone(),
            });
        }
        state.run_id = Some("a".into());

        // Both finished; neither has been checked.
        let outcomes = state.sibling_attempts();
        assert_eq!(outcomes.len(), 2);
        assert!(
            outcomes.iter().all(|o| o.passed.is_none()),
            "finishing is not passing"
        );

        // One passes its check, the other fails.
        state.run_details.entry("a".into()).or_default().checks = vec![CheckView {
            name: "check".into(),
            command: Some("cargo test".into()),
            passed: Some(true),
            duration_ms: None,
            output: None,
        }];
        state.run_details.entry("b".into()).or_default().checks = vec![CheckView {
            name: "check".into(),
            command: Some("cargo test".into()),
            passed: Some(false),
            duration_ms: None,
            output: None,
        }];
        let outcomes = state.sibling_attempts();
        let a = outcomes.iter().find(|o| o.run_id == "a").unwrap();
        let b = outcomes.iter().find(|o| o.run_id == "b").unwrap();
        assert_eq!(a.passed, Some(true));
        assert_eq!(b.passed, Some(false));
        assert!(a.selected && !b.selected);
    }

    /// An ordinary run never gets a comparison it did not ask for.
    #[test]
    fn a_run_outside_a_group_has_no_siblings() {
        let mut state = UiState::default();
        state.runs.push(RunView {
            id: "solo".into(),
            project_id: "p1".into(),
            objective: "one".into(),
            state: "running".into(),
            engine: "codex".into(),
            parent_run_id: None,
            attempt_group: None,
        });
        state.run_id = Some("solo".into());
        assert!(state.sibling_attempts().is_empty());
    }

    /// Pressing `+` twice must not leave two identical "New run" rows.
    ///
    /// Drafts are real persisted runs, so nothing can quietly discard the
    /// spare — which means the user ends up tidying after a mis-click. The
    /// second press reuses the draft that is already there.
    #[test]
    fn a_second_new_run_reuses_the_empty_draft_rather_than_stacking_one() {
        let mut state = UiState::default();
        state.runs.push(RunView {
            id: "draft-codex".into(),
            project_id: "p1".into(),
            objective: String::new(),
            state: "draft".into(),
            engine: "codex".into(),
            parent_run_id: None,
            attempt_group: None,
        });

        assert_eq!(
            state.reusable_draft("p1", "codex").map(|r| r.id.as_str()),
            Some("draft-codex")
        );
        // A different engine is a different run: the draft carries the engine.
        assert!(state.reusable_draft("p1", "claude").is_none());
        // So is a different project.
        assert!(state.reusable_draft("p2", "codex").is_none());

        // Once it has an objective it is somebody's work, never recycled.
        state.runs[0].objective = "do the thing".into();
        assert!(state.reusable_draft("p1", "codex").is_none());

        // And a draft that continues a thread carries that thread's context,
        // so it is not interchangeable with a fresh one.
        state.runs[0].objective = String::new();
        state.runs[0].parent_run_id = Some("earlier".into());
        assert!(state.reusable_draft("p1", "codex").is_none());
    }

    #[test]
    fn a_draft_awaiting_an_objective_is_the_run_a_submit_fills_in() {
        let mut state = UiState::default();
        state.runs.push(RunView {
            id: "draft".into(),
            project_id: "p1".into(),
            objective: String::new(),
            state: "draft".into(),
            engine: "codex".into(),
            parent_run_id: None,
            attempt_group: None,
        });
        state.run_id = Some("draft".into());
        assert_eq!(
            state
                .selected_draft_awaiting_objective()
                .map(|r| r.id.as_str()),
            Some("draft")
        );

        // Once it has an objective it is an ordinary run, and a further submit
        // continues the thread instead of overwriting the ask.
        state.runs[0].objective = "do the thing".into();
        assert!(state.selected_draft_awaiting_objective().is_none());

        // A started run is never treated as a fillable draft, whatever its
        // objective says.
        state.runs[0].objective = String::new();
        state.runs[0].state = "running".into();
        assert!(state.selected_draft_awaiting_objective().is_none());
    }

    #[test]
    fn a_thread_is_one_row_whichever_turn_is_selected() {
        let mut state = threaded_state();
        // Two conversations, four runs.
        assert_eq!(state.threads_of("p1").len(), 2);

        // Whichever turn is on screen, the same row stays selected.
        for turn in ["r1", "r2", "r3"] {
            state.run_id = Some(turn.into());
            assert!(state.thread_is_selected("r1"), "turn {turn}");
            assert!(!state.thread_is_selected("other"), "turn {turn}");
        }
    }

    /// A reply continues the newest turn, so the engine session resumes and
    /// the thread does not fork.
    #[test]
    fn a_reply_continues_the_newest_turn() {
        let mut state = threaded_state();
        for turn in ["r1", "r2", "r3"] {
            state.run_id = Some(turn.into());
            assert_eq!(state.thread_tip().map(|r| r.id.as_str()), Some("r3"));
        }
        // And the row reports the newest turn's state, which is what is
        // actually happening now.
        assert_eq!(
            state.tip_of("r1").map(|r| r.state.as_str()),
            Some("running")
        );
    }

    /// The transcript reads as one conversation, oldest turn first.
    #[test]
    fn the_transcript_spans_every_turn_of_the_thread() {
        let mut state = threaded_state();
        state.say("r1", "you  Fix the typo".into());
        state.say("r1", "bot  done".into());
        state.say("r2", "you  also update the heading".into());
        state.say("r3", "you  and the footer".into());
        state.say("other", "you  unrelated".into());

        state.run_id = Some("r3".into());
        assert_eq!(
            state.chat(),
            [
                "you  Fix the typo",
                "bot  done",
                "you  also update the heading",
                "you  and the footer",
            ]
        );
        // A different conversation stays out of it.
        state.run_id = Some("other".into());
        assert_eq!(state.chat(), ["you  unrelated"]);
    }

    /// A malformed parent chain must not hang the render.
    #[test]
    fn a_cyclic_parent_chain_terminates() {
        let mut state = threaded_state();
        state.runs[0].parent_run_id = Some("r3".into());
        state.run_id = Some("r3".into());
        let _ = state.chat();
        let _ = state.thread_root("r3");
        let _ = state.tip_of("r1");
    }

    /// Streaming deltas are most of the ledger and say nothing on their own.
    /// The pane exists to show what happened, not that a model is typing.
    #[test]
    fn streaming_noise_never_reaches_the_activity_pane() {
        let state = Arc::new(Mutex::new(UiState::default()));
        apply_event(&state, event(1, "r1", "run.created", json!({})));
        for seq in 2..40 {
            apply_event(
                &state,
                event(seq, "r1", "engine.text_delta", json!({ "text": "x" })),
            );
        }
        apply_event(
            &state,
            event(
                40,
                "r1",
                "engine.file_change",
                json!({ "path": "src/lib.rs" }),
            ),
        );
        let state = state.lock().unwrap();
        assert!(
            !state.events.iter().any(|e| e.contains("delta")),
            "{:?}",
            state.events
        );
        assert!(state.events.iter().any(|e| e.contains("edited src/lib.rs")));
    }

    /// Every line is a sentence, not an event name.
    #[test]
    fn activity_lines_are_readable() {
        let state = Arc::new(Mutex::new(UiState::default()));
        for (sequence, (kind, payload, expected)) in [
            (
                "run.routed",
                json!({ "shape": "bounded_loop" }),
                "routed as bounded_loop",
            ),
            (
                "run.handoff",
                json!({ "to_engine": "claude" }),
                "handed over to claude",
            ),
            (
                "engine.file_change",
                json!({ "path": "a.rs" }),
                "edited a.rs",
            ),
            ("run.succeeded", json!({}), "succeeded"),
        ]
        .into_iter()
        .enumerate()
        {
            apply_event(&state, event(sequence as u64 + 1, "r1", kind, payload));
            let s = state.lock().unwrap();
            assert_eq!(s.events.last().unwrap(), expected);
        }
    }
}
