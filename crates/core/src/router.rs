//! Task profiling and route selection (PLAN.md "Router and graph compiler").
//!
//! Deterministic repository/task facts decide the route; a structured planning
//! call to the engine may *propose* a richer shape, and Rust decides whether to
//! believe it. Pure: no IO, no engine call in here. The daemon gathers
//! [`TaskFacts`], optionally asks the engine for a [`GraphProposal`], and calls
//! [`route`].
//!
//! The bias is deliberate and one-directional: **when in doubt, run something
//! simpler.** An uncertain graph is worse than a bounded loop, because a graph
//! commits several workers to a plan nobody validated. So a low-confidence
//! proposal falls back to `BoundedLoop`, never up to `Swarm`.

use serde::{Deserialize, Serialize};

use crate::{Budget, ExecutionShape, GraphProposal, RouteDecision};

/// Deterministic facts about the objective and the repository. Everything here
/// is measured, not inferred by a model.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TaskFacts {
    pub objective: String,
    /// An explicit verification command was supplied for the run.
    pub has_check_command: bool,
    /// Files the objective explicitly names.
    pub named_files: Vec<String>,
    /// Tracked files in the repository.
    pub repo_file_count: usize,
    /// The user's checkout had uncommitted changes when the run started.
    pub repo_dirty: bool,
}

/// Words that mark work as inherently multi-step rather than one atomic edit.
const MULTI_STEP_MARKERS: &[&str] = &[
    "refactor",
    "migrate",
    "migration",
    "rewrite",
    "redesign",
    "investigate",
    "debug",
    "diagnose",
    "audit",
    "optimize",
    "upgrade",
];

/// Words that mark work as decomposable into independent branches. Used only
/// to permit a proposal, never to manufacture one.
const PARALLEL_MARKERS: &[&str] = &[
    "each",
    "every",
    "all of",
    "across",
    "and then",
    "as well as",
];

/// Router knobs a promoted policy version may tune (PLAN.md "Self-evolution":
/// bounded routing thresholds are promotable; capability boundaries are not).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct RouterThresholds {
    /// Below this, a model proposal is not trusted and the route falls back.
    pub min_graph_confidence: f32,
    /// An objective longer than this is not treated as one atomic edit.
    pub direct_max_words: usize,
    /// More named files than this means the change is not atomic.
    pub direct_max_named_files: usize,
}

impl Default for RouterThresholds {
    fn default() -> Self {
        Self {
            min_graph_confidence: 0.7,
            direct_max_words: 60,
            direct_max_named_files: 3,
        }
    }
}

/// Profile of the objective, derived only from facts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskProfile {
    pub word_count: usize,
    pub multi_step: bool,
    pub parallelizable_language: bool,
    pub named_file_count: usize,
    pub has_check_command: bool,
    pub large_repo: bool,
}

/// Whether a structured planning call is worth making. Asking a model to
/// decompose work the facts say is sequential spends the user's tokens to be
/// told what we already know — and the router would refuse the proposal
/// anyway ("does not describe independent work").
pub fn worth_planning(facts: &TaskFacts) -> bool {
    profile(facts).parallelizable_language
}

pub fn profile(facts: &TaskFacts) -> TaskProfile {
    let lower = facts.objective.to_lowercase();
    TaskProfile {
        word_count: facts.objective.split_whitespace().count(),
        multi_step: MULTI_STEP_MARKERS.iter().any(|m| lower.contains(m)),
        parallelizable_language: PARALLEL_MARKERS.iter().any(|m| lower.contains(m)),
        named_file_count: facts.named_files.len(),
        has_check_command: facts.has_check_command,
        large_repo: facts.repo_file_count > 5_000,
    }
}

/// Choose the execution shape.
///
/// `proposal` is the engine's structured planning output, if one was made.
/// `proposal_confidence` is the engine's own confidence in it. Neither can
/// escalate past what the facts support.
pub fn route(
    facts: &TaskFacts,
    proposal: Option<GraphProposal>,
    proposal_confidence: f32,
    thresholds: RouterThresholds,
    policy_version: &str,
) -> RouteDecision {
    let profile = profile(facts);
    let mut reasons = Vec::new();

    // Direct: one atomic, low-risk task WITH an explicit check. The check is
    // required, not preferred — a direct run has no loop to catch a mistake,
    // so its correctness rests entirely on that command.
    //
    // "each", "every", "across" disqualify atomicity even in a short sentence:
    // "add a docstring to each public function across the adapters" is twelve
    // words describing dozens of edit sites.
    let atomic = !profile.multi_step
        && !profile.parallelizable_language
        && profile.word_count <= thresholds.direct_max_words
        && profile.named_file_count <= thresholds.direct_max_named_files;

    if atomic && profile.has_check_command {
        reasons.push(format!(
            "objective is atomic ({} words, {} named files) and has an explicit check",
            profile.word_count, profile.named_file_count
        ));
        if facts.repo_dirty {
            reasons.push("user checkout is dirty; the run works in its own worktree".into());
        }
        return decide(
            ExecutionShape::Direct,
            0.9,
            reasons,
            vec![ExecutionShape::BoundedLoop],
            None,
            policy_version,
        );
    }

    if atomic && !profile.has_check_command {
        reasons.push(
            "objective is atomic but has no explicit check; a loop verifies its own work".into(),
        );
        return decide(
            ExecutionShape::BoundedLoop,
            0.8,
            reasons,
            vec![ExecutionShape::Direct],
            None,
            policy_version,
        );
    }

    // A proposal can only be believed if the engine is confident AND the facts
    // agree the work actually decomposes.
    if let Some(graph) = proposal {
        if proposal_confidence < thresholds.min_graph_confidence {
            reasons.push(format!(
                "planning confidence {proposal_confidence:.2} is below {:.2}; running a bounded loop instead of an unvalidated graph",
                thresholds.min_graph_confidence
            ));
        } else if !profile.parallelizable_language && graph.nodes.len() > 1 {
            reasons.push(
                "the plan proposes parallel branches but the objective does not describe independent work"
                    .into(),
            );
        } else {
            reasons.push(format!(
                "plan decomposes into {} nodes with confidence {proposal_confidence:.2}",
                graph.nodes.len()
            ));
            let shape = if graph.edges.is_empty() {
                ExecutionShape::Swarm
            } else {
                ExecutionShape::DynamicDag
            };
            return decide(
                shape,
                proposal_confidence,
                reasons,
                vec![ExecutionShape::BoundedLoop],
                Some(graph),
                policy_version,
            );
        }
    }

    if profile.multi_step {
        reasons.push("objective describes inspect/change/verify work over one context".into());
    }
    if profile.large_repo {
        reasons
            .push("large repository; a single persistent context keeps the search coherent".into());
    }
    if reasons.is_empty() {
        reasons.push("no evidence for a richer shape".into());
    }
    decide(
        ExecutionShape::BoundedLoop,
        0.75,
        reasons,
        vec![ExecutionShape::Direct],
        None,
        policy_version,
    )
}

/// Shapes that start on their own. Swarms and dynamic DAGs commit several
/// workers to a plan, so a human approves them first.
pub fn starts_automatically(shape: ExecutionShape) -> bool {
    matches!(shape, ExecutionShape::Direct | ExecutionShape::BoundedLoop)
}

/// Budget for a shape. A loop needs turns to converge; a direct run does not.
pub fn budget_for(shape: ExecutionShape) -> Budget {
    let base = Budget::default();
    match shape {
        ExecutionShape::Direct => Budget {
            wall_time_secs: 900,
            max_turns: 12,
            max_tool_calls: 80,
            max_retries: 1,
            max_concurrent_workers: 1,
            max_graph_nodes: 1,
        },
        ExecutionShape::BoundedLoop => Budget {
            wall_time_secs: 1800,
            max_turns: 40,
            max_tool_calls: 240,
            max_retries: 2,
            max_concurrent_workers: 1,
            max_graph_nodes: 1,
        },
        ExecutionShape::Swarm | ExecutionShape::DynamicDag => base,
    }
}

fn decide(
    shape: ExecutionShape,
    confidence: f32,
    reasons: Vec<String>,
    alternatives: Vec<ExecutionShape>,
    proposed_graph: Option<GraphProposal>,
    policy_version: &str,
) -> RouteDecision {
    RouteDecision {
        shape,
        confidence,
        reasons,
        alternatives,
        proposed_graph,
        budgets: budget_for(shape),
        policy_version: policy_version.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ProposedNode;

    fn facts(objective: &str) -> TaskFacts {
        TaskFacts {
            objective: objective.into(),
            has_check_command: true,
            ..TaskFacts::default()
        }
    }

    fn graph(nodes: usize, edges: Vec<(String, String)>) -> GraphProposal {
        GraphProposal {
            nodes: (0..nodes)
                .map(|i| ProposedNode {
                    id: format!("n{i}"),
                    role: "editor".into(),
                    objective: "do a thing".into(),
                    file_scope: vec![format!("src/mod{i}/**")],
                    acceptance_checks: vec!["cargo test".into()],
                })
                .collect(),
            edges,
            integration_strategy: "staging worktree".into(),
        }
    }

    fn route_default(f: &TaskFacts, p: Option<GraphProposal>, c: f32) -> RouteDecision {
        route(f, p, c, RouterThresholds::default(), "v1")
    }

    #[test]
    fn atomic_task_with_a_check_runs_direct() {
        let decision = route_default(&facts("Fix the typo in the README heading"), None, 0.0);
        assert_eq!(decision.shape, ExecutionShape::Direct);
        assert!(starts_automatically(decision.shape));
        assert_eq!(decision.budgets.max_concurrent_workers, 1);
    }

    /// A direct run has no loop to catch a mistake, so without a check there is
    /// nothing to verify it. That is a loop, not a direct run.
    #[test]
    fn atomic_task_without_a_check_becomes_a_loop() {
        let mut f = facts("Fix the typo in the README heading");
        f.has_check_command = false;
        let decision = route_default(&f, None, 0.0);
        assert_eq!(decision.shape, ExecutionShape::BoundedLoop);
        assert!(
            decision
                .reasons
                .iter()
                .any(|r| r.contains("no explicit check"))
        );
    }

    #[test]
    fn multi_step_language_routes_to_a_bounded_loop() {
        for objective in [
            "Refactor the auth module to use the new session type",
            "Investigate why the nightly job started failing",
            "Migrate the store to the v3 schema",
        ] {
            let decision = route_default(&facts(objective), None, 0.0);
            assert_eq!(
                decision.shape,
                ExecutionShape::BoundedLoop,
                "{objective} must not be treated as one atomic edit"
            );
        }
    }

    #[test]
    fn a_confident_plan_for_independent_work_becomes_a_swarm() {
        let f = facts("Add a docstring to each public function across the four adapter modules");
        let decision = route_default(&f, Some(graph(3, vec![])), 0.9);
        assert_eq!(decision.shape, ExecutionShape::Swarm);
        assert!(decision.proposed_graph.is_some());
        // Swarms wait for a human.
        assert!(!starts_automatically(decision.shape));
    }

    #[test]
    fn a_confident_plan_with_dependencies_becomes_a_dag() {
        let f = facts("Extract the parser and then migrate every call site across the crates");
        let decision = route_default(&f, Some(graph(3, vec![("n0".into(), "n1".into())])), 0.9);
        assert_eq!(decision.shape, ExecutionShape::DynamicDag);
        assert!(!starts_automatically(decision.shape));
    }

    /// The single most important routing rule: uncertainty must fall DOWN.
    #[test]
    fn a_low_confidence_plan_falls_back_to_a_loop() {
        let f = facts("Add a docstring to each public function across the adapters");
        let decision = route_default(&f, Some(graph(4, vec![])), 0.4);
        assert_eq!(decision.shape, ExecutionShape::BoundedLoop);
        assert!(
            decision.reasons.iter().any(|r| r.contains("below")),
            "the fallback must say why: {:?}",
            decision.reasons
        );
    }

    /// A model may propose parallel work for an objective that is plainly
    /// sequential. The facts win.
    #[test]
    fn a_plan_the_objective_does_not_support_is_refused() {
        let f = facts("Refactor the auth module to use the new session type");
        let decision = route_default(&f, Some(graph(4, vec![])), 0.95);
        assert_eq!(decision.shape, ExecutionShape::BoundedLoop);
        assert!(
            decision
                .reasons
                .iter()
                .any(|r| r.contains("does not describe independent work"))
        );
    }

    #[test]
    fn budgets_scale_with_shape_and_never_exceed_v1_limits() {
        let direct = budget_for(ExecutionShape::Direct);
        let loops = budget_for(ExecutionShape::BoundedLoop);
        let swarm = budget_for(ExecutionShape::Swarm);
        assert!(direct.max_turns < loops.max_turns);
        assert!(loops.max_turns < swarm.max_turns);
        for budget in [direct, loops, swarm] {
            // PLAN.md V1 validation limits.
            assert!(budget.max_concurrent_workers <= 4);
            assert!(budget.max_graph_nodes <= 8);
        }
    }

    #[test]
    fn every_decision_carries_its_reasoning_and_policy_version() {
        for (objective, proposal, confidence) in [
            ("Fix the typo", None, 0.0),
            ("Refactor everything", None, 0.0),
            (
                "Add tests to each module across the workspace",
                Some(graph(2, vec![])),
                0.9,
            ),
        ] {
            let decision = route_default(&facts(objective), proposal, confidence);
            assert!(!decision.reasons.is_empty(), "{objective} had no reasons");
            assert_eq!(decision.policy_version, "v1");
            assert!(decision.confidence > 0.0);
            assert!(!decision.alternatives.is_empty());
        }
    }

    /// The planning call is not free. It is made only when the objective
    /// actually describes independent work.
    #[test]
    fn planning_is_only_worth_it_for_decomposable_work() {
        assert!(worth_planning(&facts(
            "Add a docstring to each public function across the adapters"
        )));
        assert!(worth_planning(&facts(
            "Extract the parser and then migrate every call site"
        )));
        assert!(!worth_planning(&facts(
            "Fix the typo in the README heading"
        )));
        assert!(!worth_planning(&facts(
            "Refactor the auth module to use the new session type"
        )));
        assert!(!worth_planning(&facts(
            "Investigate and repair the failing build"
        )));
    }

    #[test]
    fn thresholds_are_tunable_by_policy() {
        let f = facts("Add a docstring to each public function across the adapters");
        // A policy that trusts plans more readily accepts this one.
        let permissive = RouterThresholds {
            min_graph_confidence: 0.3,
            ..RouterThresholds::default()
        };
        let decision = route(&f, Some(graph(2, vec![])), 0.4, permissive, "v2");
        assert_eq!(decision.shape, ExecutionShape::Swarm);
        assert_eq!(decision.policy_version, "v2");
    }
}
