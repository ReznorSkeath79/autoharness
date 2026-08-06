//! Graph compilation and validation (PLAN.md "Router and graph compiler").
//!
//! A model proposes a plan; **Rust decides whether it may run.** Everything
//! here is pure, total, and refuses by default: a proposal that fails any rule
//! is rejected with the specific reason, and the caller falls back to a bounded
//! loop rather than running a graph nobody validated.
//!
//! The rules exist because each one, violated, produces a specific disaster:
//! a cycle never terminates; an orphan node's work is silently discarded; two
//! editors sharing a file scope corrupt each other's worktree merge; an editing
//! branch with no verification commits unchecked code; and excessive width or
//! depth spends the user's tokens on a plan they never saw.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap, HashSet};

use crate::{Budget, GraphProposal, ProposedNode};

/// What a node does. Parsed leniently from the model's free-text role, because
/// the model's vocabulary is not a contract — but an unrecognized role is
/// treated as `Editor`, the most restricted kind, never the least.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// Edits files. Needs a disjoint file scope and a path to verification.
    Editor,
    /// Reads only. Cannot collide with anything.
    Reader,
    /// Runs checks over someone else's work.
    Verifier,
    /// Combines verified branches into a staging worktree.
    Integration,
}

impl Role {
    pub fn parse(role: &str) -> Role {
        let role = role.to_lowercase();
        if role.contains("integrat") || role.contains("merge") || role.contains("staging") {
            Role::Integration
        } else if role.contains("verif")
            || role.contains("test")
            || role.contains("check")
            || role.contains("critic")
            || role.contains("review")
        {
            Role::Verifier
        } else if role.contains("read")
            || role.contains("research")
            || role.contains("inspect")
            || role.contains("analy")
        {
            Role::Reader
        } else {
            // Unknown means treat it as the most constrained kind.
            Role::Editor
        }
    }

    pub fn edits(self) -> bool {
        matches!(self, Role::Editor | Role::Integration)
    }

    /// Nodes that can discharge an editing branch's verification duty.
    pub fn verifies(self) -> bool {
        matches!(self, Role::Verifier | Role::Integration)
    }
}

/// Why a proposal was refused. One variant per PLAN validation rule, so the
/// UI and the ledger can name the exact problem.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GraphError {
    Empty,
    DuplicateNodeId(String),
    UnknownNodeInEdge(String),
    SelfEdge(String),
    Cycle(Vec<String>),
    /// A node whose work never reaches the final outcome.
    OrphanNode(String),
    /// Several nodes end the plan, so there is no single final outcome to
    /// hand back — the extra branches' work goes nowhere.
    NoSingleFinalOutcome {
        sinks: Vec<String>,
    },
    TooManyNodes {
        nodes: usize,
        max: u32,
    },
    TooDeep {
        depth: usize,
        max: usize,
    },
    TooWide {
        concurrent: usize,
        max: u32,
    },
    /// Two editing nodes may touch the same files.
    ScopeCollision {
        first: String,
        second: String,
        scope: String,
    },
    /// An editing node with no path to a verification or integration node.
    UnverifiedEditingBranch(String),
    /// An editing node that declared no file scope at all.
    MissingFileScope(String),
    /// An editing branch with no acceptance check anywhere on it.
    MissingAcceptanceChecks(String),
}

impl GraphError {
    pub fn describe(&self) -> String {
        match self {
            GraphError::Empty => "the plan has no nodes".into(),
            GraphError::DuplicateNodeId(id) => format!("two nodes share the id {id}"),
            GraphError::UnknownNodeInEdge(id) => format!("an edge references unknown node {id}"),
            GraphError::SelfEdge(id) => format!("node {id} depends on itself"),
            GraphError::Cycle(ids) => format!("the plan contains a cycle: {}", ids.join(" -> ")),
            GraphError::OrphanNode(id) => {
                format!("node {id} never reaches the final outcome; its work would be discarded")
            }
            GraphError::NoSingleFinalOutcome { sinks } => format!(
                "the plan ends in {} places ({}); it must converge on one final outcome",
                sinks.len(),
                sinks.join(", ")
            ),
            GraphError::TooManyNodes { nodes, max } => {
                format!("the plan has {nodes} nodes; V1 allows {max}")
            }
            GraphError::TooDeep { depth, max } => {
                format!("the plan is {depth} levels deep; V1 allows {max}")
            }
            GraphError::TooWide { concurrent, max } => {
                format!("the plan needs {concurrent} concurrent workers; V1 allows {max}")
            }
            GraphError::ScopeCollision {
                first,
                second,
                scope,
            } => format!("{first} and {second} would both edit {scope}"),
            GraphError::UnverifiedEditingBranch(id) => {
                format!("{id} edits files but nothing verifies its work")
            }
            GraphError::MissingFileScope(id) => {
                format!("{id} edits files but declared no file scope")
            }
            GraphError::MissingAcceptanceChecks(id) => {
                format!("{id} edits files but its branch declares no acceptance check")
            }
        }
    }
}

/// A validated plan. Constructing one is the only way to get a runnable graph,
/// so anything holding a `CompiledGraph` knows the rules already passed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompiledGraph {
    pub nodes: Vec<CompiledNode>,
    pub edges: Vec<(String, String)>,
    pub integration_strategy: String,
    /// Execution waves: every node in a wave may run concurrently, and every
    /// wave depends only on earlier ones.
    pub waves: Vec<Vec<String>>,
    pub depth: usize,
    pub max_concurrency: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompiledNode {
    pub id: String,
    pub role: Role,
    pub objective: String,
    pub file_scope: Vec<String>,
    pub acceptance_checks: Vec<String>,
    pub depends_on: Vec<String>,
}

impl CompiledGraph {
    /// Nodes that edit files, and therefore need their own worktree.
    pub fn editing_nodes(&self) -> impl Iterator<Item = &CompiledNode> {
        self.nodes.iter().filter(|n| n.role.edits())
    }

    pub fn node(&self, id: &str) -> Option<&CompiledNode> {
        self.nodes.iter().find(|n| n.id == id)
    }
}

/// V1 structural limits (PLAN.md). Depth is not in [`Budget`] because it is a
/// property of the plan, not a spend.
pub const MAX_DEPTH: usize = 3;

/// Validate a proposal into a runnable graph, or explain every reason it
/// cannot run. Returns ALL violations, not just the first: a user deciding
/// whether to approve deserves the whole picture.
pub fn compile(
    proposal: &GraphProposal,
    budget: &Budget,
) -> Result<CompiledGraph, Vec<GraphError>> {
    let mut errors = Vec::new();

    if proposal.nodes.is_empty() {
        return Err(vec![GraphError::Empty]);
    }

    // Identity.
    let mut seen = HashSet::new();
    for node in &proposal.nodes {
        if !seen.insert(node.id.as_str()) {
            errors.push(GraphError::DuplicateNodeId(node.id.clone()));
        }
    }
    for (from, to) in &proposal.edges {
        if from == to {
            errors.push(GraphError::SelfEdge(from.clone()));
        }
        for id in [from, to] {
            if !seen.contains(id.as_str()) {
                errors.push(GraphError::UnknownNodeInEdge(id.clone()));
            }
        }
    }
    if !errors.is_empty() {
        // Structure is unusable; further analysis would be noise.
        return Err(errors);
    }

    if proposal.nodes.len() > budget.max_graph_nodes as usize {
        errors.push(GraphError::TooManyNodes {
            nodes: proposal.nodes.len(),
            max: budget.max_graph_nodes,
        });
    }

    // Acyclicity, via Kahn's algorithm: whatever is left over is in a cycle.
    let (waves, leftover) = topological_waves(&proposal.nodes, &proposal.edges);
    if !leftover.is_empty() {
        errors.push(GraphError::Cycle(leftover.into_iter().collect()));
        return Err(errors);
    }

    let depth = waves.len();
    if depth > MAX_DEPTH {
        errors.push(GraphError::TooDeep {
            depth,
            max: MAX_DEPTH,
        });
    }
    let max_concurrency = waves.iter().map(Vec::len).max().unwrap_or(0);
    if max_concurrency > budget.max_concurrent_workers as usize {
        errors.push(GraphError::TooWide {
            concurrent: max_concurrency,
            max: budget.max_concurrent_workers,
        });
    }

    // "Acyclic and connected to a final outcome" (PLAN.md). In a finite DAG
    // every node reaches SOME sink, so the rule that bites is that there must
    // be exactly one: a plan ending in several places has branches whose work
    // nobody consumes.
    let sinks: Vec<&str> = proposal
        .nodes
        .iter()
        .map(|n| n.id.as_str())
        .filter(|id| !proposal.edges.iter().any(|(from, _)| from == id))
        .collect();
    match sinks.as_slice() {
        [] => {} // impossible in a DAG, and the cycle check already returned
        [final_node] => {
            let target: HashSet<&str> = std::iter::once(*final_node).collect();
            for node in &proposal.nodes {
                if !reaches_any(&node.id, &proposal.edges, &target) {
                    errors.push(GraphError::OrphanNode(node.id.clone()));
                }
            }
        }
        many => errors.push(GraphError::NoSingleFinalOutcome {
            sinks: many.iter().map(|s| s.to_string()).collect(),
        }),
    }

    let roles: HashMap<&str, Role> = proposal
        .nodes
        .iter()
        .map(|n| (n.id.as_str(), Role::parse(&n.role)))
        .collect();

    // Editing nodes: declared scope, disjoint from other editors, and a path
    // to something that verifies them.
    let editors: Vec<&ProposedNode> = proposal
        .nodes
        .iter()
        .filter(|n| roles[n.id.as_str()].edits())
        .collect();

    for node in &editors {
        if node.file_scope.is_empty() {
            errors.push(GraphError::MissingFileScope(node.id.clone()));
        }
    }
    for (i, first) in editors.iter().enumerate() {
        for second in editors.iter().skip(i + 1) {
            // Integration deliberately spans the branches it merges, so it is
            // exempt from collision with the editors feeding it.
            if roles[first.id.as_str()] == Role::Integration
                || roles[second.id.as_str()] == Role::Integration
            {
                continue;
            }
            if let Some(scope) = colliding_scope(&first.file_scope, &second.file_scope) {
                errors.push(GraphError::ScopeCollision {
                    first: first.id.clone(),
                    second: second.id.clone(),
                    scope,
                });
            }
        }
    }
    for node in &editors {
        if roles[node.id.as_str()] == Role::Integration {
            continue;
        }
        let verified = downstream(&node.id, &proposal.edges)
            .into_iter()
            .any(|id| roles.get(id.as_str()).is_some_and(|r| r.verifies()));
        if !verified {
            errors.push(GraphError::UnverifiedEditingBranch(node.id.clone()));
        }
        let has_check = !node.acceptance_checks.is_empty()
            || downstream(&node.id, &proposal.edges).into_iter().any(|id| {
                proposal
                    .nodes
                    .iter()
                    .any(|n| n.id == id && !n.acceptance_checks.is_empty())
            });
        if !has_check {
            errors.push(GraphError::MissingAcceptanceChecks(node.id.clone()));
        }
    }

    if !errors.is_empty() {
        return Err(errors);
    }

    let nodes = proposal
        .nodes
        .iter()
        .map(|n| CompiledNode {
            id: n.id.clone(),
            role: roles[n.id.as_str()],
            objective: n.objective.clone(),
            file_scope: n.file_scope.clone(),
            acceptance_checks: n.acceptance_checks.clone(),
            depends_on: proposal
                .edges
                .iter()
                .filter(|(_, to)| *to == n.id)
                .map(|(from, _)| from.clone())
                .collect(),
        })
        .collect();

    Ok(CompiledGraph {
        nodes,
        edges: proposal.edges.clone(),
        integration_strategy: proposal.integration_strategy.clone(),
        waves,
        depth,
        max_concurrency,
    })
}

/// Kahn's algorithm, grouped into waves. Returns the waves plus any nodes that
/// could never be scheduled — those are exactly the nodes in a cycle.
fn topological_waves(
    nodes: &[ProposedNode],
    edges: &[(String, String)],
) -> (Vec<Vec<String>>, BTreeSet<String>) {
    let mut remaining: BTreeSet<String> = nodes.iter().map(|n| n.id.clone()).collect();
    let mut waves: Vec<Vec<String>> = Vec::new();

    while !remaining.is_empty() {
        // Ready = no remaining dependency.
        let ready: Vec<String> = remaining
            .iter()
            .filter(|id| {
                !edges
                    .iter()
                    .any(|(from, to)| to == *id && remaining.contains(from))
            })
            .cloned()
            .collect();
        if ready.is_empty() {
            // Everything left depends on something left: a cycle.
            return (waves, remaining);
        }
        for id in &ready {
            remaining.remove(id);
        }
        waves.push(ready);
    }
    (waves, BTreeSet::new())
}

/// Every node reachable from `start` (excluding itself).
fn downstream(start: &str, edges: &[(String, String)]) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut stack = vec![start.to_string()];
    let mut out = Vec::new();
    while let Some(current) = stack.pop() {
        for (from, to) in edges {
            if *from == current && seen.insert(to.clone()) {
                out.push(to.clone());
                stack.push(to.clone());
            }
        }
    }
    out
}

fn reaches_any(start: &str, edges: &[(String, String)], targets: &HashSet<&str>) -> bool {
    if targets.contains(start) {
        return true;
    }
    downstream(start, edges)
        .iter()
        .any(|id| targets.contains(id.as_str()))
}

/// Two scopes collide when either could match a file the other could.
/// Deliberately conservative — a false collision costs a bounded loop, a
/// missed one corrupts a merge.
fn colliding_scope(first: &[String], second: &[String]) -> Option<String> {
    for a in first {
        for b in second {
            if globs_overlap(a, b) {
                return Some(a.clone());
            }
        }
    }
    None
}

/// Overlap test for the glob subset that matters here: a literal prefix
/// followed by an optional wildcard tail. `src/a/**` and `src/a/b.rs` overlap;
/// `src/a/**` and `src/b/**` do not.
fn globs_overlap(a: &str, b: &str) -> bool {
    let (a_prefix, a_wild) = split_glob(a);
    let (b_prefix, b_wild) = split_glob(b);
    match (a_wild, b_wild) {
        // Both wildcards: overlap when either prefix contains the other.
        (true, true) => a_prefix.starts_with(&b_prefix) || b_prefix.starts_with(&a_prefix),
        // One wildcard: it matches anything under its prefix.
        (true, false) => b_prefix.starts_with(&a_prefix),
        (false, true) => a_prefix.starts_with(&b_prefix),
        // Two literals collide only when identical.
        (false, false) => a_prefix == b_prefix,
    }
}

fn split_glob(pattern: &str) -> (String, bool) {
    match pattern.find(['*', '?', '[']) {
        Some(idx) => (pattern[..idx].to_string(), true),
        None => (pattern.to_string(), false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: &str, role: &str, scope: &[&str], checks: &[&str]) -> ProposedNode {
        ProposedNode {
            id: id.into(),
            role: role.into(),
            objective: format!("work for {id}"),
            file_scope: scope.iter().map(|s| s.to_string()).collect(),
            acceptance_checks: checks.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn edge(from: &str, to: &str) -> (String, String) {
        (from.into(), to.into())
    }

    /// Two editors on disjoint scopes, both feeding one integration node.
    fn healthy() -> GraphProposal {
        GraphProposal {
            nodes: vec![
                node("edit_a", "editor", &["src/a/**"], &["cargo test"]),
                node("edit_b", "editor", &["src/b/**"], &["cargo test"]),
                node("merge", "integration", &["src/**"], &["cargo test"]),
            ],
            edges: vec![edge("edit_a", "merge"), edge("edit_b", "merge")],
            integration_strategy: "staging worktree".into(),
        }
    }

    fn errors(proposal: &GraphProposal) -> Vec<GraphError> {
        compile(proposal, &Budget::default()).unwrap_err()
    }

    #[test]
    fn a_healthy_plan_compiles_into_waves() {
        let graph = compile(&healthy(), &Budget::default()).unwrap();
        assert_eq!(graph.depth, 2);
        assert_eq!(graph.max_concurrency, 2);
        assert_eq!(graph.waves[0], vec!["edit_a", "edit_b"]);
        assert_eq!(graph.waves[1], vec!["merge"]);
        assert_eq!(graph.editing_nodes().count(), 3);
        assert_eq!(graph.node("merge").unwrap().depends_on.len(), 2);
    }

    #[test]
    fn an_empty_plan_is_refused() {
        let proposal = GraphProposal {
            nodes: vec![],
            edges: vec![],
            integration_strategy: String::new(),
        };
        assert_eq!(errors(&proposal), vec![GraphError::Empty]);
    }

    // ---- the PLAN validation matrix ----

    #[test]
    fn cycles_are_refused() {
        let mut proposal = healthy();
        proposal.edges.push(edge("merge", "edit_a"));
        assert!(
            errors(&proposal)
                .iter()
                .any(|e| matches!(e, GraphError::Cycle(_))),
            "a cycle would never terminate"
        );
    }

    #[test]
    fn self_edges_are_refused() {
        let mut proposal = healthy();
        proposal.edges.push(edge("edit_a", "edit_a"));
        assert!(
            errors(&proposal)
                .iter()
                .any(|e| matches!(e, GraphError::SelfEdge(_)))
        );
    }

    /// PLAN's "orphan nodes" case: a branch whose work nobody consumes. In a
    /// DAG that shows up as the plan ending in more than one place.
    #[test]
    fn a_plan_that_does_not_converge_is_refused() {
        let mut proposal = healthy();
        proposal.nodes.push(node("lonely", "reader", &[], &[]));
        let found = errors(&proposal);
        assert!(
            found.iter().any(
                |e| matches!(e, GraphError::NoSingleFinalOutcome { sinks } if sinks.len() == 2)
            ),
            "expected a no-single-outcome refusal: {found:?}"
        );
    }

    /// The mirror: one node is its own final outcome and must be allowed.
    #[test]
    fn a_single_node_plan_is_its_own_final_outcome() {
        let proposal = GraphProposal {
            nodes: vec![node("only", "verifier", &[], &["cargo test"])],
            edges: vec![],
            integration_strategy: "none".into(),
        };
        let graph = compile(&proposal, &Budget::default()).unwrap();
        assert_eq!(graph.depth, 1);
    }

    #[test]
    fn excessive_growth_is_refused() {
        let mut proposal = healthy();
        for i in 0..10 {
            proposal
                .nodes
                .push(node(&format!("extra{i}"), "reader", &[], &[]));
            proposal.edges.push(edge(&format!("extra{i}"), "merge"));
        }
        let found = errors(&proposal);
        assert!(
            found
                .iter()
                .any(|e| matches!(e, GraphError::TooManyNodes { .. })),
            "{found:?}"
        );
        assert!(
            found
                .iter()
                .any(|e| matches!(e, GraphError::TooWide { .. })),
            "13 nodes in one wave exceeds 4 workers: {found:?}"
        );
    }

    #[test]
    fn excessive_depth_is_refused() {
        // A chain of five: deeper than V1 allows.
        let nodes: Vec<ProposedNode> = (0..5)
            .map(|i| {
                node(
                    &format!("n{i}"),
                    if i == 4 { "verifier" } else { "editor" },
                    &["src/only/**"],
                    &["cargo test"],
                )
            })
            .collect();
        let edges: Vec<(String, String)> = (0..4)
            .map(|i| edge(&format!("n{i}"), &format!("n{}", i + 1)))
            .collect();
        let proposal = GraphProposal {
            nodes,
            edges,
            integration_strategy: "chain".into(),
        };
        let found = errors(&proposal);
        assert!(
            found
                .iter()
                .any(|e| matches!(e, GraphError::TooDeep { .. })),
            "{found:?}"
        );
    }

    #[test]
    fn scope_collisions_are_refused() {
        let mut proposal = healthy();
        // Both editors now claim the same subtree.
        proposal.nodes[1].file_scope = vec!["src/a/inner/**".into()];
        let found = errors(&proposal);
        assert!(
            found
                .iter()
                .any(|e| matches!(e, GraphError::ScopeCollision { .. })),
            "overlapping editor scopes corrupt the merge: {found:?}"
        );
    }

    #[test]
    fn disjoint_sibling_scopes_do_not_collide() {
        // The mirror of the previous test: sibling subtrees must be allowed,
        // or every real swarm would be refused.
        assert!(compile(&healthy(), &Budget::default()).is_ok());
    }

    #[test]
    fn an_editing_node_with_no_declared_scope_is_refused() {
        let mut proposal = healthy();
        proposal.nodes[0].file_scope.clear();
        assert!(
            errors(&proposal)
                .iter()
                .any(|e| matches!(e, GraphError::MissingFileScope(_)))
        );
    }

    #[test]
    fn an_unverified_editing_branch_is_refused() {
        // An editor whose only downstream is another editor: nothing checks it.
        let proposal = GraphProposal {
            nodes: vec![
                node("edit_a", "editor", &["src/a/**"], &["cargo test"]),
                node("edit_b", "editor", &["src/b/**"], &["cargo test"]),
            ],
            edges: vec![edge("edit_a", "edit_b")],
            integration_strategy: "none".into(),
        };
        let found = errors(&proposal);
        assert!(
            found
                .iter()
                .any(|e| matches!(e, GraphError::UnverifiedEditingBranch(id) if id == "edit_b")),
            "an editing branch that nothing verifies commits unchecked code: {found:?}"
        );
    }

    #[test]
    fn an_editing_branch_with_no_acceptance_check_is_refused() {
        let mut proposal = healthy();
        for n in &mut proposal.nodes {
            n.acceptance_checks.clear();
        }
        let found = errors(&proposal);
        assert!(
            found
                .iter()
                .any(|e| matches!(e, GraphError::MissingAcceptanceChecks(_))),
            "{found:?}"
        );
    }

    #[test]
    fn budget_overflow_is_refused() {
        // A tighter policy budget refuses a plan the default would allow.
        let tight = Budget {
            max_graph_nodes: 2,
            max_concurrent_workers: 1,
            ..Budget::default()
        };
        let found = compile(&healthy(), &tight).unwrap_err();
        assert!(
            found
                .iter()
                .any(|e| matches!(e, GraphError::TooManyNodes { .. }))
        );
        assert!(
            found
                .iter()
                .any(|e| matches!(e, GraphError::TooWide { .. }))
        );
    }

    #[test]
    fn unknown_nodes_in_edges_are_refused() {
        let mut proposal = healthy();
        proposal.edges.push(edge("edit_a", "ghost"));
        assert!(
            errors(&proposal)
                .iter()
                .any(|e| matches!(e, GraphError::UnknownNodeInEdge(_)))
        );
    }

    #[test]
    fn duplicate_ids_are_refused() {
        let mut proposal = healthy();
        proposal.nodes[1].id = "edit_a".into();
        assert!(
            errors(&proposal)
                .iter()
                .any(|e| matches!(e, GraphError::DuplicateNodeId(_)))
        );
    }

    // ---- role parsing and reporting ----

    #[test]
    fn roles_parse_leniently_but_default_to_the_most_constrained() {
        assert_eq!(Role::parse("editor"), Role::Editor);
        assert_eq!(Role::parse("code writer"), Role::Editor);
        assert_eq!(Role::parse("Integration"), Role::Integration);
        assert_eq!(Role::parse("merge-bot"), Role::Integration);
        assert_eq!(Role::parse("verifier"), Role::Verifier);
        assert_eq!(Role::parse("test runner"), Role::Verifier);
        assert_eq!(Role::parse("critic"), Role::Verifier);
        assert_eq!(Role::parse("researcher"), Role::Reader);
        // An unrecognized role must not be granted the loosest treatment.
        assert_eq!(Role::parse("wizard"), Role::Editor);
    }

    #[test]
    fn every_refusal_explains_itself() {
        let mut proposal = healthy();
        proposal.nodes[1].file_scope = vec!["src/a/**".into()];
        proposal.nodes[0].acceptance_checks.clear();
        for error in errors(&proposal) {
            let text = error.describe();
            assert!(!text.is_empty());
            assert!(text.len() > 10, "unhelpful message: {text}");
        }
    }

    #[test]
    fn glob_overlap_is_conservative() {
        assert!(globs_overlap("src/a/**", "src/a/b.rs"));
        assert!(globs_overlap("src/**", "src/a/**"));
        assert!(globs_overlap("a.rs", "a.rs"));
        assert!(!globs_overlap("src/a/**", "src/b/**"));
        assert!(!globs_overlap("a.rs", "b.rs"));
        // A bare wildcard claims everything, so it collides with anything.
        assert!(globs_overlap("**", "src/a/**"));
    }
}
