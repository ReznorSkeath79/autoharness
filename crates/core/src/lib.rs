//! AutoHarness core domain types, run/node state machines, and routing contracts.
//!
//! Everything in this crate is serialization-stable contract surface: the daemon,
//! protocol, store, and UI all share these definitions.

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub mod detector;
pub mod graph;
pub mod policy;
pub mod router;
pub mod version;

/// Which agent engine executes a run. One engine per run, never switched
/// mid-run: continuing on a different engine is a new turn of the thread.
///
/// This was a two-variant enum, `Codex | Claude`, and `other()` meant "the one
/// that isn't this one". That shape is why AutoHarness could only ever drive
/// two agents. Most coding CLIs speak no structured protocol at all — they
/// paint a terminal — and supporting them means an engine is identified by a
/// manifest id, not by a variant somebody added to an enum.
///
/// The two built-ins keep their ids, so every existing `runs.engine` row and
/// every wire message still reads and writes exactly as before.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct EngineKind(std::sync::Arc<str>);

impl EngineKind {
    /// Codex, over its structured app-server protocol.
    pub const CODEX: &'static str = "codex";
    /// Claude Code, over its structured stream-json protocol.
    pub const CLAUDE: &'static str = "claude";

    /// The engines that speak a structured protocol and therefore have a
    /// hand-written adapter. Everything else is driven through a PTY.
    pub fn builtins() -> [EngineKind; 2] {
        [Self::codex(), Self::claude()]
    }

    /// The engines the product offers end to end today. Every other manifest
    /// stays registered and testable, but every surface that lists engines
    /// shows it as coming soon rather than runnable — one place decides, so
    /// the picker, the sidebar, settings, and `/attempts` cannot disagree.
    pub fn is_generally_available(&self) -> bool {
        matches!(self.as_str(), Self::CODEX | Self::CLAUDE)
    }

    pub fn codex() -> Self {
        Self::new(Self::CODEX)
    }

    pub fn claude() -> Self {
        Self::new(Self::CLAUDE)
    }

    /// An engine id. Callers that take this from user or provider input should
    /// prefer [`FromStr`], which rejects ids that could not name a manifest.
    pub fn new(id: impl AsRef<str>) -> Self {
        Self(std::sync::Arc::from(id.as_ref()))
    }

    /// Stable lowercase identifier, as stored in the runs table.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn is_codex(&self) -> bool {
        self.as_str() == Self::CODEX
    }

    pub fn is_claude(&self) -> bool {
        self.as_str() == Self::CLAUDE
    }

    /// Whether this engine has a hand-written structured adapter, as opposed
    /// to being driven through a terminal.
    pub fn is_builtin(&self) -> bool {
        self.is_codex() || self.is_claude()
    }
}

impl std::str::FromStr for EngineKind {
    type Err = String;

    /// Accepts any plausible manifest id. The character class is deliberately
    /// narrow: an engine id names a file on disk and is interpolated into
    /// diagnostics, so it must not be able to carry a path segment, a shell
    /// metacharacter, or whitespace.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.is_empty() || s.len() > 64 {
            return Err(format!("engine id must be 1-64 characters: {s:?}"));
        }
        if !s
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '.')
        {
            return Err(format!(
                "engine id may only contain lowercase letters, digits, '-' and '.': {s:?}"
            ));
        }
        Ok(Self::new(s))
    }
}

impl std::fmt::Display for EngineKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl From<&str> for EngineKind {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

impl Serialize for EngineKind {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

/// Deserialization goes through `FromStr`, so an id arriving over the socket
/// or out of the database is held to the same rules as one typed by a user.
/// An engine id reaches a filesystem lookup and a diagnostics string; it is
/// not a place to accept arbitrary text.
impl<'de> Deserialize<'de> for EngineKind {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        raw.parse().map_err(serde::de::Error::custom)
    }
}

/// A reply to a question an engine asked mid-run.
///
/// Lives here rather than beside the adapter contract because the protocol
/// carries it over the wire and the engines crate delivers it, and both of
/// those depend on this crate rather than on each other.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "text")]
pub enum Answer {
    /// Yes, using whatever keystrokes this agent's manifest says mean yes.
    Approve,
    /// No, likewise. Refusing must be as available as approving: a wrong
    /// approval is the expensive one.
    Deny,
    /// Free text, for a prompt that is a question rather than a choice.
    Text(String),
}

/// How the router decided to execute an objective.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionShape {
    Direct,
    BoundedLoop,
    Swarm,
    DynamicDag,
}

/// Lifecycle of a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    Draft,
    AwaitingApproval,
    Running,
    Paused,
    Blocked,
    Succeeded,
    Failed,
    Cancelled,
}

/// Lifecycle of a single graph node (unit of work).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeState {
    Pending,
    Ready,
    Running,
    Verifying,
    Succeeded,
    Failed,
    Blocked,
    Cancelled,
}

/// Error returned for an illegal state transition. Never a panic.
#[derive(Debug, Clone, PartialEq, Eq, Error, Serialize, Deserialize)]
#[error("illegal {machine} transition: {from} -> {to}")]
pub struct TransitionError {
    pub machine: &'static str,
    pub from: String,
    pub to: String,
}

impl RunState {
    pub const ALL: [RunState; 8] = [
        RunState::Draft,
        RunState::AwaitingApproval,
        RunState::Running,
        RunState::Paused,
        RunState::Blocked,
        RunState::Succeeded,
        RunState::Failed,
        RunState::Cancelled,
    ];

    /// Terminal states have no outbound transitions.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            RunState::Succeeded | RunState::Failed | RunState::Cancelled
        )
    }

    /// Whether `next` is a legal successor of `self`.
    pub fn can_transition_to(self, next: RunState) -> bool {
        use RunState::*;
        matches!(
            (self, next),
            // A draft starts automatically (direct/loop), awaits approval
            // (swarm/DAG), or blocks before execution when a required engine,
            // sandbox, or owned worktree cannot be prepared.
            (Draft, AwaitingApproval) | (Draft, Running) | (Draft, Blocked) | (Draft, Cancelled)
            // Approval starts the run; rejection cancels it.
            | (AwaitingApproval, Running) | (AwaitingApproval, Cancelled)
            // Active run: pause, detector/scheduler block, finish, fail, or cancel.
            | (Running, Paused) | (Running, Blocked) | (Running, Succeeded)
            | (Running, Failed) | (Running, Cancelled)
            // Resume or give up from a pause; restart reconciliation blocks it.
            | (Paused, Running) | (Paused, Blocked) | (Paused, Cancelled)
            // Recover from a block into execution or a newly compiled plan,
            // fail terminally, or cancel.
            | (Blocked, AwaitingApproval) | (Blocked, Running)
            | (Blocked, Failed) | (Blocked, Cancelled)
        )
    }

    /// Perform the transition, returning the new state or a [`TransitionError`].
    pub fn transition(self, next: RunState) -> Result<RunState, TransitionError> {
        if self.can_transition_to(next) {
            Ok(next)
        } else {
            Err(TransitionError {
                machine: "run",
                from: format!("{self:?}").to_lowercase(),
                to: format!("{next:?}").to_lowercase(),
            })
        }
    }

    /// Stable lowercase identifier, as stored in the runs table.
    pub fn as_str(self) -> &'static str {
        match self {
            RunState::Draft => "draft",
            RunState::AwaitingApproval => "awaiting_approval",
            RunState::Running => "running",
            RunState::Paused => "paused",
            RunState::Blocked => "blocked",
            RunState::Succeeded => "succeeded",
            RunState::Failed => "failed",
            RunState::Cancelled => "cancelled",
        }
    }
}

impl std::str::FromStr for RunState {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "draft" => RunState::Draft,
            "awaiting_approval" => RunState::AwaitingApproval,
            "running" => RunState::Running,
            "paused" => RunState::Paused,
            "blocked" => RunState::Blocked,
            "succeeded" => RunState::Succeeded,
            "failed" => RunState::Failed,
            "cancelled" => RunState::Cancelled,
            other => return Err(format!("unknown run state: {other}")),
        })
    }
}

impl std::fmt::Display for RunState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl NodeState {
    pub const ALL: [NodeState; 8] = [
        NodeState::Pending,
        NodeState::Ready,
        NodeState::Running,
        NodeState::Verifying,
        NodeState::Succeeded,
        NodeState::Failed,
        NodeState::Blocked,
        NodeState::Cancelled,
    ];

    pub fn is_terminal(self) -> bool {
        matches!(self, NodeState::Succeeded | NodeState::Cancelled)
    }

    /// Stable lowercase identifier, as stored in `node_attempts`.
    pub fn as_str(self) -> &'static str {
        match self {
            NodeState::Pending => "pending",
            NodeState::Ready => "ready",
            NodeState::Running => "running",
            NodeState::Verifying => "verifying",
            NodeState::Succeeded => "succeeded",
            NodeState::Failed => "failed",
            NodeState::Blocked => "blocked",
            NodeState::Cancelled => "cancelled",
        }
    }

    /// Whether `next` is a legal successor of `self`.
    pub fn can_transition_to(self, next: NodeState) -> bool {
        use NodeState::*;
        matches!(
            (self, next),
            // Scheduling: dependencies satisfied, or the run is cancelled first.
            (Pending, Ready) | (Pending, Cancelled)
            // Dispatch or cancel before dispatch.
            | (Ready, Running) | (Ready, Cancelled)
            // Work done -> verify; or it can fail, stall (blocked), or be cancelled.
            | (Running, Verifying) | (Running, Failed) | (Running, Blocked) | (Running, Cancelled)
            // Verification gate decides; checks can also stall or be cancelled.
            | (Verifying, Succeeded) | (Verifying, Failed) | (Verifying, Blocked) | (Verifying, Cancelled)
            // Retry: node.retry requeues a failed node; blocked nodes requeue after
            // human/recovery-ladder intervention.
            | (Failed, Ready) | (Failed, Cancelled)
            | (Blocked, Ready) | (Blocked, Cancelled)
        )
    }

    /// Perform the transition, returning the new state or a [`TransitionError`].
    pub fn transition(self, next: NodeState) -> Result<NodeState, TransitionError> {
        if self.can_transition_to(next) {
            Ok(next)
        } else {
            Err(TransitionError {
                machine: "node",
                from: format!("{self:?}").to_lowercase(),
                to: format!("{next:?}").to_lowercase(),
            })
        }
    }
}

/// Router output: which shape was chosen and why.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RouteDecision {
    pub shape: ExecutionShape,
    /// Confidence in [0, 1]; low-confidence proposals fall back to `BoundedLoop`.
    pub confidence: f32,
    pub reasons: Vec<String>,
    pub alternatives: Vec<ExecutionShape>,
    pub proposed_graph: Option<GraphProposal>,
    pub budgets: Budget,
    pub policy_version: String,
}

/// A proposed execution graph (pre-validation model output).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GraphProposal {
    pub nodes: Vec<ProposedNode>,
    /// `(from_node, to_node)` dependency edges.
    pub edges: Vec<(String, String)>,
    pub integration_strategy: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProposedNode {
    pub id: String,
    pub role: String,
    pub objective: String,
    /// Declared file globs this node may edit; editing nodes need disjoint scopes
    /// or separate worktrees.
    pub file_scope: Vec<String>,
    pub acceptance_checks: Vec<String>,
}

/// Hard limits enforced by the scheduler and loop detector.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Budget {
    pub wall_time_secs: u64,
    pub max_turns: u32,
    pub max_tool_calls: u32,
    pub max_retries: u32,
    pub max_concurrent_workers: u32,
    pub max_graph_nodes: u32,
}

impl Default for Budget {
    fn default() -> Self {
        // V1 validation limits from PLAN.md.
        Self {
            wall_time_secs: 3600,
            max_turns: 200,
            max_tool_calls: 500,
            max_retries: 3,
            max_concurrent_workers: 4,
            max_graph_nodes: 8,
        }
    }
}

/// Verified output of a node: findings plus the evidence backing them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Artifact {
    pub id: String,
    pub run_id: String,
    pub node_id: Option<String>,
    pub findings: Vec<String>,
    /// Event IDs in the ledger that evidence this artifact.
    pub evidence_event_ids: Vec<u64>,
    pub changed_files: Vec<String>,
    pub commit: Option<String>,
    pub checks: Vec<CheckResult>,
    pub open_questions: Vec<String>,
    pub omissions: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckResult {
    pub command: String,
    pub passed: bool,
    pub summary: String,
}

/// Resumable snapshot of a node, with evidence and last known-good repo state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Checkpoint {
    pub id: String,
    pub run_id: String,
    pub node_id: Option<String>,
    /// Sequence of the last event included in this checkpoint.
    pub last_event_seq: u64,
    pub summary: String,
    pub evidence_event_ids: Vec<u64>,
    pub base_commit: Option<String>,
    pub created_at_ms: i64,
}

/// An evidence-backed project fact. Candidates without evidence stay proposals
/// and are never injected automatically.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryFact {
    pub id: String,
    pub project_id: String,
    pub kind: MemoryFactKind,
    pub statement: String,
    /// Event IDs or repository artifact references backing this fact.
    pub evidence: Vec<String>,
    pub confidence: f32,
    /// ID of the fact this one supersedes, if any.
    pub supersedes: Option<String>,
    /// Whether a human verified this fact (verified facts may be injected).
    pub verified: bool,
    pub created_at_ms: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryFactKind {
    ArchitectureDecision,
    VerifiedCommand,
    FailurePattern,
    Recovery,
    Convention,
    Tooling,
}

/// Immutable, human-promoted policy configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PolicyVersion {
    pub id: String,
    pub version: u32,
    pub router_thresholds: serde_json::Value,
    pub approved_graph_templates: Vec<String>,
    pub detector_thresholds: serde_json::Value,
    pub recovery_budgets: serde_json::Value,
    /// True only after explicit user promotion. Never self-promoted.
    pub promoted: bool,
    pub created_at_ms: i64,
}

/// New unique string ID (UUIDv4).
pub fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// General availability is a product gate, not a capability claim: the
    /// PTY engines keep working in tests, but only the structured builtins
    /// are offered as runnable anywhere in the app.
    #[test]
    fn only_the_structured_builtins_are_generally_available() {
        assert!(EngineKind::codex().is_generally_available());
        assert!(EngineKind::claude().is_generally_available());
        for gated in ["cursor", "grok", "kimi", "opencode", "gemini", "fake"] {
            assert!(
                !EngineKind::new(gated).is_generally_available(),
                "{gated} must read as coming soon"
            );
        }
    }

    /// Every legal run transition, enumerated explicitly.
    const LEGAL_RUN: &[(RunState, RunState)] = &[
        (RunState::Draft, RunState::AwaitingApproval),
        (RunState::Draft, RunState::Running),
        (RunState::Draft, RunState::Blocked),
        (RunState::Draft, RunState::Cancelled),
        (RunState::AwaitingApproval, RunState::Running),
        (RunState::AwaitingApproval, RunState::Cancelled),
        (RunState::Running, RunState::Paused),
        (RunState::Running, RunState::Blocked),
        (RunState::Running, RunState::Succeeded),
        (RunState::Running, RunState::Failed),
        (RunState::Running, RunState::Cancelled),
        (RunState::Paused, RunState::Running),
        (RunState::Paused, RunState::Blocked),
        (RunState::Paused, RunState::Cancelled),
        (RunState::Blocked, RunState::AwaitingApproval),
        (RunState::Blocked, RunState::Running),
        (RunState::Blocked, RunState::Failed),
        (RunState::Blocked, RunState::Cancelled),
    ];

    /// Every legal node transition, enumerated explicitly.
    const LEGAL_NODE: &[(NodeState, NodeState)] = &[
        (NodeState::Pending, NodeState::Ready),
        (NodeState::Pending, NodeState::Cancelled),
        (NodeState::Ready, NodeState::Running),
        (NodeState::Ready, NodeState::Cancelled),
        (NodeState::Running, NodeState::Verifying),
        (NodeState::Running, NodeState::Failed),
        (NodeState::Running, NodeState::Blocked),
        (NodeState::Running, NodeState::Cancelled),
        (NodeState::Verifying, NodeState::Succeeded),
        (NodeState::Verifying, NodeState::Failed),
        (NodeState::Verifying, NodeState::Blocked),
        (NodeState::Verifying, NodeState::Cancelled),
        (NodeState::Failed, NodeState::Ready),
        (NodeState::Failed, NodeState::Cancelled),
        (NodeState::Blocked, NodeState::Ready),
        (NodeState::Blocked, NodeState::Cancelled),
    ];

    #[test]
    fn every_legal_run_transition_succeeds() {
        for &(from, to) in LEGAL_RUN {
            assert!(
                from.can_transition_to(to),
                "{from:?} -> {to:?} must be legal"
            );
            assert_eq!(from.transition(to).unwrap(), to);
        }
    }

    #[test]
    fn every_illegal_run_transition_errors() {
        let mut illegal_count = 0;
        for from in RunState::ALL {
            for to in RunState::ALL {
                let legal = LEGAL_RUN.contains(&(from, to));
                assert_eq!(from.can_transition_to(to), legal, "{from:?} -> {to:?}");
                if legal {
                    continue;
                }
                illegal_count += 1;
                let err = from.transition(to).unwrap_err();
                assert_eq!(err.machine, "run");
                // Errors, not panics; message identifies the pair.
                assert!(err.to_string().contains("run transition"));
            }
        }
        assert_eq!(illegal_count, 64 - LEGAL_RUN.len());
    }

    #[test]
    fn run_terminal_states_have_no_outbound_edges() {
        for state in [RunState::Succeeded, RunState::Failed, RunState::Cancelled] {
            assert!(state.is_terminal());
            for to in RunState::ALL {
                assert!(!state.can_transition_to(to));
                assert!(state.transition(to).is_err());
            }
        }
    }

    #[test]
    fn every_legal_node_transition_succeeds() {
        for &(from, to) in LEGAL_NODE {
            assert!(
                from.can_transition_to(to),
                "{from:?} -> {to:?} must be legal"
            );
            assert_eq!(from.transition(to).unwrap(), to);
        }
    }

    #[test]
    fn every_illegal_node_transition_errors() {
        let mut illegal_count = 0;
        for from in NodeState::ALL {
            for to in NodeState::ALL {
                let legal = LEGAL_NODE.contains(&(from, to));
                assert_eq!(from.can_transition_to(to), legal, "{from:?} -> {to:?}");
                if legal {
                    continue;
                }
                illegal_count += 1;
                let err = from.transition(to).unwrap_err();
                assert_eq!(err.machine, "node");
            }
        }
        assert_eq!(illegal_count, 64 - LEGAL_NODE.len());
    }

    #[test]
    fn node_terminal_states_have_no_outbound_edges() {
        for state in [NodeState::Succeeded, NodeState::Cancelled] {
            assert!(state.is_terminal());
            for to in NodeState::ALL {
                assert!(!state.can_transition_to(to));
            }
        }
    }

    #[test]
    fn budget_defaults_match_v1_limits() {
        let b = Budget::default();
        assert_eq!(b.max_graph_nodes, 8);
        assert_eq!(b.max_concurrent_workers, 4);
    }

    #[test]
    fn state_names_round_trip() {
        use std::str::FromStr;
        for state in RunState::ALL {
            assert_eq!(RunState::from_str(state.as_str()).unwrap(), state);
            assert_eq!(state.to_string(), state.as_str());
        }
        assert!(RunState::from_str("exploded").is_err());
    }

    /// The built-ins keep their exact ids, because every existing `runs.engine`
    /// row and every wire message already contains them.
    #[test]
    fn the_builtin_engines_keep_their_ids() {
        use std::str::FromStr;
        for kind in EngineKind::builtins() {
            assert_eq!(EngineKind::from_str(kind.as_str()).unwrap(), kind);
            assert!(kind.is_builtin());
        }
        assert!(EngineKind::codex().is_codex());
        assert!(EngineKind::claude().is_claude());
    }

    /// The point of the type: an engine is a manifest id, so an agent nobody
    /// wrote a variant for is nameable. The previous test asserted the
    /// opposite — that "gpt" was an error — which is exactly the ceiling that
    /// kept AutoHarness to two engines.
    #[test]
    fn any_agent_id_is_a_valid_engine() {
        use std::str::FromStr;
        for id in [
            "cursor",
            "gemini",
            "aider",
            "opencode",
            "gpt",
            "claude-code.v2",
        ] {
            let kind = EngineKind::from_str(id).expect(id);
            assert_eq!(kind.as_str(), id);
            assert!(!kind.is_builtin() || kind.is_claude() || kind.is_codex());
        }
        assert!(!EngineKind::from_str("cursor").unwrap().is_builtin());
    }

    /// An engine id reaches a filesystem lookup and gets interpolated into
    /// diagnostics, so the shapes that could escape either one are refused.
    #[test]
    fn an_engine_id_cannot_carry_a_path_or_a_shell_metacharacter() {
        use std::str::FromStr;
        for bad in [
            "",
            "../etc/passwd",
            "a/b",
            "with space",
            "Claude",
            "semi;colon",
            "quote'",
            "$(whoami)",
            "new\nline",
        ] {
            assert!(
                EngineKind::from_str(bad).is_err(),
                "{bad:?} must be refused"
            );
        }
        assert!(EngineKind::from_str(&"x".repeat(65)).is_err());
        assert!(EngineKind::from_str(&"x".repeat(64)).is_ok());
    }

    /// Validation is on the deserializer too: an id arriving over the socket
    /// or out of the database is held to the same rules as one typed by hand.
    #[test]
    fn engine_ids_are_validated_on_the_wire_not_only_at_the_keyboard() {
        let kind: EngineKind = serde_json::from_str("\"cursor\"").unwrap();
        assert_eq!(kind.as_str(), "cursor");
        assert_eq!(serde_json::to_string(&kind).unwrap(), "\"cursor\"");
        assert!(serde_json::from_str::<EngineKind>("\"../evil\"").is_err());
        assert!(serde_json::from_str::<EngineKind>("\"\"").is_err());
    }

    #[test]
    fn domain_types_round_trip_through_json() {
        let decision = RouteDecision {
            shape: ExecutionShape::DynamicDag,
            confidence: 0.9,
            reasons: vec!["multi-role".into()],
            alternatives: vec![ExecutionShape::BoundedLoop],
            proposed_graph: Some(GraphProposal {
                nodes: vec![ProposedNode {
                    id: "n1".into(),
                    role: "editor".into(),
                    objective: "change".into(),
                    file_scope: vec!["src/**".into()],
                    acceptance_checks: vec!["cargo test".into()],
                }],
                edges: vec![],
                integration_strategy: "staging-worktree".into(),
            }),
            budgets: Budget::default(),
            policy_version: "p1".into(),
        };
        let json = serde_json::to_string(&decision).unwrap();
        let back: RouteDecision = serde_json::from_str(&json).unwrap();
        assert_eq!(decision, back);
    }
}
