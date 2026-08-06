//! Swarm and DAG execution (PLAN.md "Router and graph compiler", "Worktrees
//! and integration").
//!
//! A compiled graph runs wave by wave. Every editing node gets its own
//! worktree and branch, so parallel workers cannot corrupt each other; the
//! integration node then merges the verified branches into a staging worktree
//! and reruns the checks there.
//!
//! Failure is contained, not propagated blindly: a failed node marks its
//! dependents unreachable rather than letting them run against work that was
//! never produced. Nothing here can touch the user's checked-out branch.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use autoharness_core::graph::{CompiledGraph, CompiledNode, Role};
use autoharness_core::{NodeState, RunState};
use autoharness_engines::process::SessionDirs;
use autoharness_engines::{EngineEvent, SessionSpec};
use serde_json::json;
use tokio::sync::{mpsc, oneshot};

use crate::AppState;
use crate::runner::RunCommand;
use crate::worktree::{self, RunWorktree};

/// Bound on the check output a node event carries. The ledger keeps evidence,
/// not whole build logs.
const CHECK_OUTPUT_BYTES: usize = 16_000;

/// Outcome of one node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum NodeOutcome {
    Succeeded {
        commit: Option<String>,
        branch: Option<String>,
    },
    Failed {
        reason: String,
    },
    /// A dependency failed, so this node's inputs never existed.
    Skipped {
        reason: String,
    },
    Cancelled {
        reason: String,
    },
}

impl NodeOutcome {
    fn state(&self) -> NodeState {
        match self {
            NodeOutcome::Succeeded { .. } => NodeState::Succeeded,
            NodeOutcome::Failed { .. } => NodeState::Failed,
            NodeOutcome::Skipped { .. } | NodeOutcome::Cancelled { .. } => NodeState::Cancelled,
        }
    }

    fn detail(&self) -> String {
        match self {
            NodeOutcome::Succeeded { commit, .. } => {
                commit.clone().unwrap_or_else(|| "no changes".into())
            }
            NodeOutcome::Failed { reason }
            | NodeOutcome::Skipped { reason }
            | NodeOutcome::Cancelled { reason } => reason.clone(),
        }
    }
}

/// Run a compiled graph to completion. The run is already Running and
/// approved; this drives it to a terminal state.
#[derive(Debug, Clone)]
pub(crate) struct GraphEngineSelection {
    pub engine: autoharness_core::EngineKind,
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
}

pub(crate) async fn run_graph(
    state: Arc<AppState>,
    run_id: String,
    graph: CompiledGraph,
    repo_path: std::path::PathBuf,
    selection: GraphEngineSelection,
    mut cmd_rx: mpsc::Receiver<RunCommand>,
) {
    let mut outcomes: HashMap<String, NodeOutcome> = HashMap::new();
    let mut worktrees: Vec<RunWorktree> = Vec::new();
    let mut scope = graph
        .nodes
        .iter()
        .map(|node| node.id.clone())
        .collect::<HashSet<_>>();

    'graph: loop {
        if execute_scope(
            &state,
            &run_id,
            &graph,
            &repo_path,
            &selection,
            &scope,
            &mut outcomes,
            &mut worktrees,
            &mut cmd_rx,
        )
        .await
        {
            let _ = state.emit(
                Some(&run_id),
                "graph.finished",
                json!({ "run_id": run_id, "cancelled": true }),
            );
            if state.transition_run(&run_id, RunState::Cancelled).is_ok() {
                let _ = state.emit(Some(&run_id), "run.cancelled", json!({ "run_id": run_id }));
            }
            break;
        }

        let failed = outcomes
            .iter()
            .filter(|(_, outcome)| !matches!(outcome, NodeOutcome::Succeeded { .. }))
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        if failed.is_empty() {
            let _ = state.emit(
                Some(&run_id),
                "graph.finished",
                json!({
                    "run_id": run_id,
                    "nodes": outcomes.len(),
                    "failed": failed,
                    "worktrees": worktrees.iter().map(|worktree| worktree.path.clone()).collect::<Vec<_>>(),
                }),
            );
            if state.transition_run(&run_id, RunState::Succeeded).is_ok() {
                let _ = state.emit(
                    Some(&run_id),
                    "run.succeeded",
                    json!({ "run_id": run_id, "failed_nodes": [] }),
                );
            }
            break;
        }

        if state.transition_run(&run_id, RunState::Blocked).is_ok() {
            let _ = state.emit(
                Some(&run_id),
                "graph.blocked",
                json!({ "run_id": run_id, "failed_nodes": failed, "retryable": true }),
            );
            let _ = state.emit(
                Some(&run_id),
                "run.blocked",
                json!({
                    "run_id": run_id,
                    "reason": "graph_nodes_failed",
                    "failed_nodes": failed,
                }),
            );
        }

        loop {
            match cmd_rx.recv().await {
                Some(RunCommand::NodeRetry { node_id }) => {
                    scope = dependent_subgraph(&graph, &node_id);
                    if scope.is_empty() {
                        let _ = state.emit(
                            Some(&run_id),
                            "node.control_rejected",
                            json!({
                                "run_id": run_id,
                                "node_id": node_id,
                                "reason": "node is not in the approved graph",
                            }),
                        );
                        continue;
                    }
                    for retried in &scope {
                        outcomes.remove(retried);
                    }
                    if state.transition_run(&run_id, RunState::Running).is_err() {
                        break 'graph;
                    }
                    let _ = state.emit(
                        Some(&run_id),
                        "node.retry_started",
                        json!({ "run_id": run_id, "node_id": node_id, "nodes": scope }),
                    );
                    break;
                }
                Some(RunCommand::Cancel) | None => {
                    if state.transition_run(&run_id, RunState::Cancelled).is_ok() {
                        let _ =
                            state.emit(Some(&run_id), "run.cancelled", json!({ "run_id": run_id }));
                    }
                    break 'graph;
                }
                Some(RunCommand::NodeCancel { node_id }) => {
                    let _ = state.emit(
                        Some(&run_id),
                        "node.control_rejected",
                        json!({ "run_id": run_id, "node_id": node_id, "reason": "node is not running" }),
                    );
                }
                Some(_) => {}
            }
        }
    }
    crate::queue::runner_stopped(&state, &run_id).await;
}

#[allow(clippy::too_many_arguments)]
async fn execute_scope(
    state: &Arc<AppState>,
    run_id: &str,
    graph: &CompiledGraph,
    repo_path: &Path,
    selection: &GraphEngineSelection,
    scope: &HashSet<String>,
    outcomes: &mut HashMap<String, NodeOutcome>,
    worktrees: &mut Vec<RunWorktree>,
    cmd_rx: &mut mpsc::Receiver<RunCommand>,
) -> bool {
    let mut run_cancelled = false;
    for (wave_index, compiled_wave) in graph.waves.iter().enumerate() {
        let wave = compiled_wave
            .iter()
            .filter(|node_id| scope.contains(*node_id))
            .cloned()
            .collect::<Vec<_>>();
        if wave.is_empty() {
            continue;
        }
        let _ = state.emit(
            Some(run_id),
            "graph.wave_started",
            json!({ "run_id": run_id, "wave": wave_index, "nodes": wave }),
        );

        let (result_tx, mut result_rx) = mpsc::channel(wave.len().max(1));
        let mut controls = HashMap::new();
        let mut remaining = 0usize;
        for node_id in &wave {
            let Some(node) = graph.node(node_id) else {
                continue;
            };
            if let Some(failed) = node.depends_on.iter().find(|dependency| {
                !matches!(
                    outcomes.get(*dependency),
                    Some(NodeOutcome::Succeeded { .. })
                )
            }) {
                let outcome = NodeOutcome::Skipped {
                    reason: format!("dependency {failed} did not succeed"),
                };
                let attempt = state.store.next_node_attempt(run_id, &node.id).unwrap_or(1);
                record(state, run_id, &node.id, attempt, &outcome);
                outcomes.insert(node.id.clone(), outcome);
                continue;
            }
            let attempt = state.store.next_node_attempt(run_id, &node.id).unwrap_or(1);
            let (cancel_tx, cancel_rx) = oneshot::channel();
            controls.insert(node.id.clone(), cancel_tx);
            let result_tx = result_tx.clone();
            let task_state = Arc::clone(state);
            let task_run_id = run_id.to_string();
            let task_node = node.clone();
            let task_repo = repo_path.to_path_buf();
            let task_selection = selection.clone();
            let branches = collect_branches(graph, node, outcomes);
            tokio::spawn(async move {
                let result = run_node(
                    NodeRun {
                        state: task_state,
                        run_id: task_run_id,
                        node: task_node,
                        repo_path: task_repo,
                        selection: task_selection,
                        merge_branches: branches,
                        attempt,
                    },
                    cancel_rx,
                )
                .await;
                let _ = result_tx.send((attempt, result)).await;
            });
            remaining += 1;
        }
        drop(result_tx);

        while remaining > 0 {
            tokio::select! {
                command = cmd_rx.recv() => match command {
                    Some(RunCommand::NodeCancel { node_id }) => {
                        if let Some(cancel) = controls.remove(&node_id) {
                            let _ = cancel.send(());
                            let _ = state.emit(
                                Some(run_id),
                                "node.cancel_requested",
                                json!({ "run_id": run_id, "node_id": node_id }),
                            );
                        } else {
                            let _ = state.emit(
                                Some(run_id),
                                "node.control_rejected",
                                json!({ "run_id": run_id, "node_id": node_id, "reason": "node is not running" }),
                            );
                        }
                    }
                    Some(RunCommand::Cancel) | None => {
                        run_cancelled = true;
                        for (_, cancel) in controls.drain() {
                            let _ = cancel.send(());
                        }
                    }
                    Some(RunCommand::NodeRetry { node_id }) => {
                        let _ = state.emit(
                            Some(run_id),
                            "node.control_rejected",
                            json!({ "run_id": run_id, "node_id": node_id, "reason": "node is still running" }),
                        );
                    }
                    Some(_) => {}
                },
                result = result_rx.recv() => {
                    let Some((attempt, (node_id, outcome, worktree))) = result else {
                        break;
                    };
                    remaining = remaining.saturating_sub(1);
                    controls.remove(&node_id);
                    record(state, run_id, &node_id, attempt, &outcome);
                    outcomes.insert(node_id, outcome);
                    if let Some(worktree) = worktree {
                        worktrees.push(worktree);
                    }
                }
            }
        }
        if run_cancelled {
            return true;
        }
    }
    false
}

fn dependent_subgraph(graph: &CompiledGraph, root: &str) -> HashSet<String> {
    if graph.node(root).is_none() {
        return HashSet::new();
    }
    let mut selected = HashSet::from([root.to_string()]);
    loop {
        let before = selected.len();
        for node in &graph.nodes {
            if node
                .depends_on
                .iter()
                .any(|dependency| selected.contains(dependency))
            {
                selected.insert(node.id.clone());
            }
        }
        if selected.len() == before {
            return selected;
        }
    }
}

/// Branches an integration node must merge: every successful editing
/// dependency's branch.
fn collect_branches(
    graph: &CompiledGraph,
    node: &CompiledNode,
    outcomes: &HashMap<String, NodeOutcome>,
) -> Vec<String> {
    if node.role != Role::Integration {
        return Vec::new();
    }
    let mut branches = Vec::new();
    let mut seen = HashSet::new();
    for dep in &node.depends_on {
        if graph
            .node(dep)
            .is_some_and(|dependency| dependency.role.edits())
            && seen.insert(dep.clone())
            && let Some(NodeOutcome::Succeeded {
                branch: Some(branch),
                ..
            }) = outcomes.get(dep)
        {
            branches.push(branch.clone());
        }
    }
    branches
}

/// Branch name for a node's worktree. Deterministic so integration can find it.
fn node_branch(node_id: &str, attempt: i64) -> String {
    if attempt == 1 {
        format!("ah/node-{node_id}")
    } else {
        format!("ah/node-{node_id}-attempt-{attempt}")
    }
}

fn record(state: &Arc<AppState>, run_id: &str, node_id: &str, attempt: i64, outcome: &NodeOutcome) {
    let node_state = outcome.state();
    let _ = state.store.record_node_attempt(
        run_id,
        node_id,
        attempt,
        node_state.as_str(),
        Some(&outcome.detail()),
    );
    let _ = state.emit(
        Some(run_id),
        "node.finished",
        json!({
            "run_id": run_id,
            "node_id": node_id,
            "attempt": attempt,
            "state": node_state.as_str(),
            "detail": outcome.detail(),
            "can_retry": matches!(node_state, NodeState::Failed | NodeState::Blocked | NodeState::Cancelled),
            "can_cancel": false,
        }),
    );
}

/// One owned graph-node execution. Grouping the immutable inputs makes it
/// impossible to swap adjacent string/vector arguments at the spawn site.
struct NodeRun {
    state: Arc<AppState>,
    run_id: String,
    node: CompiledNode,
    repo_path: std::path::PathBuf,
    selection: GraphEngineSelection,
    merge_branches: Vec<String>,
    attempt: i64,
}

/// Execute one node: its own worktree, its own engine session, its own checks.
async fn run_node(
    task: NodeRun,
    cancel: oneshot::Receiver<()>,
) -> (String, NodeOutcome, Option<RunWorktree>) {
    let NodeRun {
        state,
        run_id,
        node,
        repo_path,
        selection,
        merge_branches,
        attempt,
    } = task;
    let node_id = node.id.clone();
    let _ = state
        .store
        .record_node_attempt(&run_id, &node_id, attempt, "running", None);
    let _ = state.emit(
        Some(&run_id),
        "node.started",
        json!({
            "run_id": run_id,
            "node_id": node_id,
            "attempt": attempt,
            "role": node.role,
            "objective": node.objective,
            "file_scope": node.file_scope,
            "can_retry": false,
            "can_cancel": true,
        }),
    );

    let Some(sandbox) = state.sandbox.sandbox() else {
        return (
            node_id,
            NodeOutcome::Failed {
                reason: "sandbox unavailable".into(),
            },
            None,
        );
    };
    let session_key = if attempt == 1 {
        format!("{run_id}-{node_id}")
    } else {
        format!("{run_id}-{node_id}-attempt-{attempt}")
    };
    let dirs = match SessionDirs::create(&state.data_dir, &session_key) {
        Ok(dirs) => dirs,
        Err(e) => {
            return (
                node_id,
                NodeOutcome::Failed {
                    reason: format!("session dirs: {e}"),
                },
                None,
            );
        }
    };

    // Every editing node gets its own worktree and branch. Read-only nodes
    // still get one, because sharing the base checkout with a live worker is
    // exactly the collision the worktrees exist to prevent.
    let worktree = match worktree::create_named(
        sandbox,
        &dirs,
        &state.store,
        &repo_path,
        &session_key,
        &node_branch(&node_id, attempt),
        &state.data_dir,
        worktree::WorktreeOwner::node(&run_id, &node_id),
    )
    .await
    {
        Ok(worktree) => worktree,
        Err(e) => {
            return (
                node_id,
                NodeOutcome::Failed {
                    reason: format!("worktree: {e}"),
                },
                None,
            );
        }
    };

    // Integration merges its verified dependencies into this staging worktree
    // BEFORE the engine sees it, so the model works on the combined result.
    for branch in &merge_branches {
        if let Err(e) = worktree::merge_branch(sandbox, &dirs, &worktree, branch).await {
            let _ = state.emit(
                Some(&run_id),
                "node.conflict",
                json!({
                    "run_id": run_id,
                    "node_id": node_id,
                    "branch": branch,
                    "error": e.to_string(),
                }),
            );
            // A conflict is real work for the model, not a failure: leave the
            // conflicted state in place and let the integration node resolve it.
        }
    }

    let outcome = drive_node(&state, &run_id, &node, &worktree, &dirs, selection, cancel).await;
    (node_id, outcome, Some(worktree))
}

/// One engine session for one node, then its acceptance checks, then a commit.
async fn drive_node(
    state: &Arc<AppState>,
    run_id: &str,
    node: &CompiledNode,
    worktree: &RunWorktree,
    dirs: &SessionDirs,
    selection: GraphEngineSelection,
    mut cancel: oneshot::Receiver<()>,
) -> NodeOutcome {
    let Some(mut adapter) = state.engines.create(selection.engine.clone()) else {
        return NodeOutcome::Failed {
            reason: format!("no adapter for {}", selection.engine),
        };
    };
    let spec = SessionSpec {
        working_dir: worktree.path.clone(),
        data_dir: state.data_dir.clone(),
        // The key the node's own home was built from — already distinct per
        // attempt — so the engine runs in that home instead of silently
        // allocating another one.
        session_key: dirs.key.clone(),
        model: selection.model,
        reasoning_effort: selection.reasoning_effort,
    };
    if let Err(e) = adapter.start_session(&spec).await {
        return NodeOutcome::Failed {
            reason: format!("session start: {e}"),
        };
    }

    let prompt = format!(
        "{}\n\nYou may only modify files matching: {}\nWhen you are done, the following must pass: {}",
        node.objective,
        node.file_scope.join(", "),
        node.acceptance_checks.join(" && ")
    );
    if let Err(e) = adapter.send_turn(&prompt).await {
        return NodeOutcome::Failed {
            reason: format!("send_turn: {e}"),
        };
    }

    // Stream to the ledger under the node's own id so the timeline can group
    // by node.
    loop {
        let event = tokio::select! {
            _ = &mut cancel => {
                let _ = adapter.cancel().await;
                return NodeOutcome::Cancelled { reason: "cancelled by user".into() };
            }
            event = adapter.next_event() => event,
        };
        match event {
            Ok(Some(event)) => {
                let payload = serde_json::to_value(&event).unwrap_or(serde_json::Value::Null);
                let mut payload = payload;
                if let Some(object) = payload.as_object_mut() {
                    object.insert("node_id".into(), json!(node.id));
                }
                let _ = state.emit(Some(run_id), event.kind_str(), payload);
                match event {
                    EngineEvent::Completed { .. } => break,
                    EngineEvent::Failed {
                        message,
                        recoverable,
                    } if !recoverable => {
                        return NodeOutcome::Failed { reason: message };
                    }
                    _ => {}
                }
            }
            Ok(None) => break,
            Err(e) => {
                return NodeOutcome::Failed {
                    reason: format!("engine error: {e}"),
                };
            }
        }
    }

    let Some(sandbox) = state.sandbox.sandbox() else {
        return NodeOutcome::Failed {
            reason: "sandbox unavailable".into(),
        };
    };

    // Acceptance checks run in the node's own worktree. A node check reports
    // the same evidence a direct run's check does: exit code, how long it
    // took, and bounded output. A red node with no reason is not reviewable.
    for check in &node.acceptance_checks {
        let args = vec!["-c".to_string(), check.clone()];
        let started = std::time::Instant::now();
        let result = tokio::select! {
            _ = &mut cancel => {
                let _ = adapter.cancel().await;
                return NodeOutcome::Cancelled { reason: "cancelled by user during verification".into() };
            }
            result = sandbox.run_command(
                &worktree.path,
                dirs,
                Path::new("/bin/sh"),
                &args,
                &worktree.path,
            ) => result,
        };
        let duration_ms = started.elapsed().as_millis() as u64;
        let passed = matches!(&result, Ok(r) if r.success());
        let (exit_code, stdout, stderr) = match &result {
            Ok(r) => (
                r.status.code(),
                crate::runner::tail(&r.stdout, CHECK_OUTPUT_BYTES),
                crate::runner::tail(&r.stderr, CHECK_OUTPUT_BYTES),
            ),
            Err(e) => (None, String::new(), e.to_string()),
        };
        let _ = state.emit(
            Some(run_id),
            "node.check",
            json!({
                "run_id": run_id,
                "node_id": node.id,
                "command": check,
                "passed": passed,
                "exit_code": exit_code,
                "duration_ms": duration_ms,
                "stdout": stdout,
                "stderr": stderr,
            }),
        );
        crate::record_artifact(
            state,
            crate::ArtifactDraft {
                run_id,
                node_id: Some(&node.id),
                kind: "check_output",
                name: check,
                path: None,
                byte_size: None,
                summary: &format!("exit {exit_code:?} in {duration_ms} ms\n{stdout}\n{stderr}"),
            },
        );
        if !passed {
            return NodeOutcome::Failed {
                reason: format!("acceptance check failed: {check}"),
            };
        }
    }

    match worktree::commit_all(
        sandbox,
        dirs,
        worktree,
        &format!("autoharness: node {}", node.id),
    )
    .await
    {
        Ok(commit) => {
            if commit.is_some() {
                // What this node actually changed, on its own branch. Without
                // it a green node is a claim rather than evidence.
                let stat = worktree::diff_stat(sandbox, dirs, worktree)
                    .await
                    .unwrap_or_default();
                let _ = state.emit(
                    Some(run_id),
                    "node.diff",
                    json!({
                        "run_id": run_id,
                        "node_id": node.id,
                        "commit": commit,
                        "branch": worktree.branch,
                        "base_commit": worktree.base_commit,
                        "diff_stat": stat,
                    }),
                );
                crate::runner::record_change_artifacts(
                    state,
                    run_id,
                    Some(&node.id),
                    sandbox,
                    dirs,
                    worktree,
                    &stat,
                )
                .await;
            }
            NodeOutcome::Succeeded {
                commit,
                branch: Some(worktree.branch.clone()),
            }
        }
        Err(e) => NodeOutcome::Failed {
            reason: format!("commit: {e}"),
        },
    }
}
