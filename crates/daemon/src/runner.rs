//! Run orchestration: one engine worker in a daemon-managed worktree,
//! steering at turn boundaries, verification + local commit, cancellation
//! cleanup, and restart reconciliation.
//!
//! Two shapes share this task (PLAN.md "Router and graph compiler"):
//!
//! - **Direct** — one atomic task with an explicit check. The check runs once;
//!   whatever it says is the verdict.
//! - **BoundedLoop** — inspect/change/verify over one persistent context. A
//!   failing check is not the end: its output is fed back as the next turn's
//!   evidence, until the check passes or a budget/detector stops it.
//!
//! Every iteration is watched by [`autoharness_core::detector`], and a trigger
//! walks the recovery ladder (nudge, replan, restart from checkpoint, blocked)
//! rather than looping forever.
//!
//! All daemon-owned commands (check, git) go through `Sandbox::run_command`
//! / `Sandbox::run_bookkeeping` — never raw spawns.

use std::path::Path;
use std::sync::Arc;

use autoharness_core::detector::{
    Detector, DetectorThresholds, Fingerprint, Progress, Recovery, Signal, Trigger,
};
use autoharness_core::{Budget, ExecutionShape, RunState};
use autoharness_engines::process::SessionDirs;
use autoharness_engines::{EngineAdapter, EngineEvent};
use autoharness_protocol::params::{QueueItem, QueueState};
use serde_json::json;
use tokio::sync::mpsc;

use crate::AppState;
use crate::worktree::{self, CleanupOutcome, RunWorktree};

/// Commands sent to an active run's task.
#[derive(Debug, Clone)]
pub(crate) enum RunCommand {
    Pause,
    Resume,
    Cancel,
    /// Queue steering text for the next safe turn boundary.
    Steer {
        queue_id: String,
    },
    /// Interrupt the current provider turn, then inject immediately.
    InterruptWithText(String),
    /// Answer a question the engine asked. Unlike steering, this does NOT wait
    /// for a turn boundary: the agent is blocked ON the question, so the next
    /// boundary will never arrive until it is answered.
    Answer(autoharness_core::Answer),
    NodeRetry {
        node_id: String,
    },
    NodeCancel {
        node_id: String,
    },
}

/// Everything the run task needs beyond the adapter.
pub(crate) struct RunContext {
    /// The turn that is executing. A thread's turns share one worktree, so
    /// `worktree.run_id` names the thread rather than this run — worktree
    /// events still have to be attributed to the turn that caused them.
    pub run_id: String,
    /// Which engine is driving. Needed to persist the provider session id the
    /// engine reports mid-stream, so the next turn can resume it.
    pub engine: String,
    pub worktree: RunWorktree,
    pub check_command: Option<String>,
    /// Session dirs for check/git commands (fake HOME/TMPDIR).
    pub dirs: SessionDirs,
    pub shape: ExecutionShape,
    pub budget: Budget,
}

/// Result of one verification pass.
struct CheckOutcome {
    passed: bool,
    /// Trimmed output, fed back to the engine as evidence in a loop.
    evidence: String,
    /// Stable class for the detector: the command plus its exit code.
    class: String,
}

/// Budget consumption for one run. A budget is a hard ceiling, not a hint:
/// crossing one ends the run rather than escalating the ladder.
struct BudgetTracker {
    budget: Budget,
    turns: u32,
    tool_calls: u32,
    started: std::time::Instant,
}

impl BudgetTracker {
    fn new(budget: Budget) -> Self {
        Self {
            budget,
            turns: 0,
            tool_calls: 0,
            started: std::time::Instant::now(),
        }
    }

    /// The ceiling that was crossed, if any.
    fn exhausted(&self) -> Option<String> {
        if self.turns > self.budget.max_turns {
            return Some(format!("max_turns={}", self.budget.max_turns));
        }
        if self.tool_calls > self.budget.max_tool_calls {
            return Some(format!("max_tool_calls={}", self.budget.max_tool_calls));
        }
        if self.started.elapsed().as_secs() > self.budget.wall_time_secs {
            return Some(format!("wall_time_secs={}", self.budget.wall_time_secs));
        }
        None
    }
}

/// Tail helper for check output cards.
pub(crate) fn tail(s: &str, max: usize) -> String {
    let s = s.trim();
    if s.len() <= max {
        s.to_string()
    } else {
        format!("…{}", &s[s.len() - max..])
    }
}

/// Mark a run failed through the state machine and emit `run.failed`.
pub(crate) fn fail_run(state: &AppState, run_id: &str, message: &str) {
    if state.transition_run(run_id, RunState::Failed).is_ok() {
        let _ = state.emit(
            Some(run_id),
            "run.failed",
            json!({ "run_id": run_id, "message": message }),
        );
    }
}

/// Streams one run's engine events into the ledger, handles control commands
/// and steering, watches for loops, and finalizes with verification + commit.
pub(crate) async fn run_task(
    state: Arc<AppState>,
    run_id: String,
    objective: String,
    mut adapter: Box<dyn EngineAdapter>,
    mut cmd_rx: mpsc::Receiver<RunCommand>,
    ctx: RunContext,
) {
    if let Err(e) = adapter.send_turn(&objective).await {
        fail_run(&state, &run_id, &format!("send_turn failed: {e}"));
        preserve_worktree(&state, &ctx, "engine failed before first turn").await;
        crate::queue::runner_stopped(&state, &run_id).await;
        return;
    }

    // Steering is injected at turn boundaries only: the engine is mid-turn
    // for the whole lifetime of this loop, so text queued here is sent when
    // the current turn completes.
    let mut inflight_steering: Option<QueueItem> = None;
    let mut detector = Detector::new(DetectorThresholds::default());
    let mut budget = BudgetTracker::new(ctx.budget.clone());
    budget.turns = 1;
    // Remembered so a check whose result CHANGED counts as progress even when
    // it is still failing — red-to-different-red is how debugging looks.
    let mut last_check_class: Option<String> = None;

    loop {
        tokio::select! {
            command = cmd_rx.recv() => {
                match command {
                    Some(RunCommand::Pause) => {
                        let _ = adapter.pause().await;
                        match state.transition_run(&run_id, RunState::Paused) {
                            Ok(_) => {
                                let _ = state.emit(Some(&run_id), "run.paused", json!({ "run_id": run_id }));
                            }
                            Err(e) => tracing::warn!(%run_id, error = %e, "pause transition rejected"),
                        }
                    }
                    Some(RunCommand::Resume) => {
                        match state.transition_run(&run_id, RunState::Running) {
                            Ok(_) => {
                                let _ = state.emit(Some(&run_id), "run.resumed", json!({ "run_id": run_id }));
                            }
                            Err(e) => tracing::warn!(%run_id, error = %e, "resume transition rejected"),
                        }
                    }
                    Some(RunCommand::Steer { queue_id }) => {
                        // The durable row is already authoritative. This
                        // command only wakes the runner; delivery still waits
                        // for the next provider turn boundary.
                        tracing::debug!(%run_id, %queue_id, "steering runner notified");
                    }
                    Some(RunCommand::InterruptWithText(text)) => {
                        // Cancel the current provider turn FIRST, then inject.
                        let _ = adapter.interrupt().await;
                        let _ = state.emit(Some(&run_id), "chat.interrupt",
                            json!({ "run_id": run_id, "message": text }));
                        if let Err(e) = adapter.send_turn(&text).await {
                            fail_run(&state, &run_id, &format!("interrupt turn failed: {e}"));
                            preserve_worktree(&state, &ctx, "interrupt turn failed").await;
                            break;
                        }
                    }
                    Some(RunCommand::Answer(answer)) => {
                        match adapter.answer(answer.clone()).await {
                            Ok(()) => {
                                let _ = state.emit(Some(&run_id), "run.answered",
                                    json!({ "run_id": run_id, "answer": answer }));
                            }
                            // An engine that cannot be answered is reported, not
                            // swallowed: the run stays blocked and the user has
                            // to know that pressing the button did nothing.
                            Err(e) => {
                                let _ = state.emit(Some(&run_id), "run.answer_failed",
                                    json!({ "run_id": run_id, "error": e.to_string() }));
                            }
                        }
                    }
                    Some(RunCommand::NodeRetry { node_id })
                    | Some(RunCommand::NodeCancel { node_id }) => {
                        let _ = state.emit(
                            Some(&run_id),
                            "node.control_rejected",
                            json!({
                                "run_id": run_id,
                                "node_id": node_id,
                                "reason": "run is not a graph",
                            }),
                        );
                    }
                    Some(RunCommand::Cancel) | None => {
                        let _ = adapter.cancel().await;
                        // Clean up first so the terminal event is last on the
                        // ledger and already reflects the worktree's fate.
                        cleanup_worktree(&state, &ctx).await;
                        if state.transition_run(&run_id, RunState::Cancelled).is_ok() {
                            let _ = state.emit(Some(&run_id), "run.cancelled", json!({ "run_id": run_id }));
                        }
                        break;
                    }
                }
            }
            event = adapter.next_event() => {
                match event {
                    Ok(Some(event)) => {
                        let payload = serde_json::to_value(&event).unwrap_or(serde_json::Value::Null);
                        let _ = state.emit(Some(&run_id), event.kind_str(), payload);

                        // A provider only reveals its session id mid-stream, so
                        // this is the first moment it can be persisted. Nothing
                        // else writes it: without this a follow-up turn has no
                        // session to resume and silently starts a fresh
                        // conversation instead of continuing the thread.
                        if let EngineEvent::SessionIdentity { session_id } = &event
                            && !session_id.is_empty()
                        {
                            let _ = state.store.save_engine_session(
                                &run_id,
                                &ctx.engine,
                                session_id,
                            );
                        }

                        if matches!(event, EngineEvent::ToolActivity { .. }) {
                            budget.tool_calls += 1;
                        }
                        if let Some(signal) = signal_for(&event)
                            && let Some(trigger) = detector.observe(signal)
                        {
                            match apply_recovery(
                                &state, &run_id, &mut detector, &trigger, &mut adapter, &ctx,
                            )
                            .await
                            {
                                RecoveryOutcome::Continued => budget.turns += 1,
                                RecoveryOutcome::Stopped => break,
                            }
                        }
                        if let Some(limit) = budget.exhausted() {
                            let trigger = detector.budget_exhausted(limit);
                            block_run(&state, &run_id, &trigger, &ctx).await;
                            break;
                        }

                        match event {
                            EngineEvent::Completed { .. } => {
                                if let Some(delivered) = inflight_steering.take()
                                    && let Ok(completed) = state.store.finish_queue_item(
                                        &delivered.id,
                                        QueueState::Completed,
                                        None,
                                    )
                                {
                                    let _ = state.emit(
                                        Some(&run_id),
                                        "queue.completed",
                                        json!({
                                            "queue_id": completed.id,
                                            "run_id": run_id,
                                            "kind": "steering",
                                        }),
                                    );
                                }
                                match state.store.claim_next_steering(&run_id) {
                                    Ok(Some(next)) => {
                                        // Steering drives the next turn; the
                                        // run finalizes only when the durable
                                        // queue is empty.
                                        let _ = state.emit(
                                            Some(&run_id),
                                            "queue.dispatching",
                                            crate::queue::queue_payload(&next),
                                        );
                                        let _ = state.emit(Some(&run_id), "chat.steering_sent",
                                            json!({
                                                "queue_id": next.id,
                                                "run_id": run_id,
                                                "message": next.content,
                                            }));
                                        if let Err(e) = adapter.send_turn(&next.content).await {
                                            let _ = state.store.finish_queue_item(
                                                &next.id,
                                                QueueState::Failed,
                                                Some(&e.to_string()),
                                            );
                                            fail_run(&state, &run_id, &format!("steering turn failed: {e}"));
                                            preserve_worktree(&state, &ctx, "steering turn failed").await;
                                            break;
                                        }
                                        inflight_steering = Some(next);
                                        budget.turns += 1;
                                        continue;
                                    }
                                    Ok(None) => {}
                                    Err(error) => {
                                        fail_run(&state, &run_id, &format!("steering queue failed: {error}"));
                                        preserve_worktree(&state, &ctx, "steering queue failed").await;
                                        break;
                                    }
                                }

                                // A bounded loop verifies and, on failure,
                                // feeds the evidence back as the next turn
                                // instead of accepting broken work.
                                if ctx.shape == ExecutionShape::BoundedLoop
                                    && let Some(outcome) = run_check(&state, &run_id, &ctx).await
                                {
                                    let changed = last_check_class.as_deref() != Some(outcome.class.as_str());
                                    last_check_class = Some(outcome.class.clone());
                                    if !outcome.passed {
                                        // A different failure is forward motion.
                                        if changed {
                                            detector.observe(Signal::Progress(Progress::Checks));
                                        } else {
                                            detector.observe(Signal::Error { class: outcome.class.clone() });
                                        }
                                        if let Some(limit) = budget.exhausted() {
                                            let trigger = detector.budget_exhausted(limit);
                                            block_run(&state, &run_id, &trigger, &ctx).await;
                                            break;
                                        }
                                        let prompt = format!(
                                            "The verification command failed. Fix the cause, do not change the check.\n\n                                             $ {}\n{}",
                                            ctx.check_command.as_deref().unwrap_or(""),
                                            outcome.evidence
                                        );
                                        let _ = state.emit(Some(&run_id), "run.loop_iteration", json!({
                                            "run_id": run_id,
                                            "turn": budget.turns,
                                            "reason": "check_failed",
                                        }));
                                        if let Err(e) = adapter.send_turn(&prompt).await {
                                            fail_run(&state, &run_id, &format!("loop turn failed: {e}"));
                                            preserve_worktree(&state, &ctx, "loop turn failed").await;
                                            break;
                                        }
                                        budget.turns += 1;
                                        continue;
                                    }
                                    // Passed: commit without re-running it.
                                    finalize_verified(&state, &run_id, &ctx).await;
                                    break;
                                }

                                finalize_run(&state, &run_id, &ctx).await;
                                break;
                            }
                            EngineEvent::Failed { message, recoverable } => {
                                // Provider-retryable failures (e.g. our own
                                // interrupt) do not fail the run.
                                if !recoverable {
                                    fail_run(&state, &run_id, &message);
                                    preserve_worktree(&state, &ctx, "run failed").await;
                                    break;
                                }
                            }
                            _ => {}
                        }
                    }
                    Ok(None) => {
                        // Stream ended without a terminal event. If the run is
                        // still active, that is an abnormal exit.
                        if let Ok(run) = state.store.get_run(&run_id)
                            && matches!(run.state.as_str(), "running" | "paused")
                        {
                            fail_run(&state, &run_id, "engine stream ended without completion");
                            preserve_worktree(&state, &ctx, "engine stream ended").await;
                        }
                        break;
                    }
                    Err(e) => {
                        fail_run(&state, &run_id, &format!("engine error: {e}"));
                        preserve_worktree(&state, &ctx, "engine error").await;
                        break;
                    }
                }
            }
        }
    }

    crate::queue::runner_stopped(&state, &run_id).await;
}

/// Map a normalized engine event onto a detector signal. Events that carry no
/// evidence either way (text, usage, session identity) map to nothing — the
/// detector must never count narration as activity.
fn signal_for(event: &EngineEvent) -> Option<Signal> {
    match event {
        EngineEvent::ToolActivity { name, detail, .. } => Some(Signal::Action {
            fingerprint: Fingerprint::action(name, &detail.to_string()),
            // The adapters do not distinguish "could not run" from "ran and
            // failed"; treat every tool call as valid and let the repeated
            // action trigger catch a stuck one.
            invalid: false,
        }),
        // A file changed: that is real workspace movement.
        EngineEvent::FileChange { .. } => Some(Signal::Progress(Progress::Workspace)),
        EngineEvent::Failed { message, .. } => Some(Signal::Error {
            class: autoharness_core::detector::normalize(message),
        }),
        _ => None,
    }
}

/// Whether the run continued after a recovery attempt.
enum RecoveryOutcome {
    Continued,
    Stopped,
}

/// Walk one rung of the recovery ladder for a detector trigger (PLAN.md
/// "Recovery ladder"): nudge, then replan, then restart from the latest
/// verified checkpoint, then block with the exact evidence.
async fn apply_recovery(
    state: &Arc<AppState>,
    run_id: &str,
    detector: &mut Detector,
    trigger: &Trigger,
    adapter: &mut Box<dyn EngineAdapter>,
    ctx: &RunContext,
) -> RecoveryOutcome {
    let rung = detector.take_recovery();
    let _ = state.emit(
        Some(run_id),
        "run.detector",
        json!({
            "run_id": run_id,
            "trigger": trigger.kind(),
            "evidence": trigger.describe(),
            "recovery": rung.as_str(),
        }),
    );

    // A run that hit its budget is the one detector trigger that is about a
    // resource rather than about the model's behaviour, and it is the one the
    // user can act on by raising a limit. Say so separately so the attention
    // system can treat it as resource pressure instead of another stall.
    if let Trigger::BudgetExhausted { limit } = trigger {
        let _ = state.emit(
            Some(run_id),
            "run.resource_pressure",
            json!({
                "run_id": run_id,
                "resource": "budget",
                "limit": limit,
                "detail": trigger.describe(),
                "recovery": rung.as_str(),
            }),
        );
    }

    let prompt = match rung {
        Recovery::Nudge => format!(
            "Stop. {}\n\nDo not repeat that action. Before acting again, state what new              evidence you will gather and gather it.",
            trigger.describe()
        ),
        Recovery::Replan => format!(
            "Your current approach is not working: {}\n\nDiscard it. Explain in one              paragraph why it failed, then propose and follow a different approach.",
            trigger.describe()
        ),
        Recovery::RestartFromCheckpoint => {
            let checkpoint = state
                .store
                .latest_checkpoint(run_id)
                .ok()
                .flatten()
                .and_then(|c| {
                    c.snapshot_json
                        .get("summary")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_string)
                })
                .unwrap_or_else(|| "no verified checkpoint was recorded".to_string());
            let _ = state.emit(
                Some(run_id),
                "run.checkpoint_restart",
                json!({ "run_id": run_id, "summary": checkpoint }),
            );
            format!(
                "Restarting from the last verified state: {checkpoint}\n\nEverything since                  then is discarded because {}. Begin again from that state with a different                  approach.",
                trigger.describe()
            )
        }
        Recovery::Blocked => {
            block_run(state, run_id, trigger, ctx).await;
            return RecoveryOutcome::Stopped;
        }
    };

    // The rung is a steering turn, so interrupt the stuck turn first.
    let _ = adapter.interrupt().await;
    if let Err(e) = adapter.send_turn(&prompt).await {
        fail_run(state, run_id, &format!("recovery turn failed: {e}"));
        preserve_worktree(state, ctx, "recovery turn failed").await;
        return RecoveryOutcome::Stopped;
    }
    RecoveryOutcome::Continued
}

/// Out of moves. Blocked is not failure: the work is preserved and the exact
/// evidence is surfaced so a human can decide.
async fn block_run(state: &Arc<AppState>, run_id: &str, trigger: &Trigger, ctx: &RunContext) {
    match state.transition_run(run_id, RunState::Blocked) {
        Ok(_) => {
            let _ = state.emit(
                Some(run_id),
                "run.blocked",
                json!({
                    "run_id": run_id,
                    "reason": trigger.kind(),
                    "evidence": trigger.describe(),
                    "worktree": ctx.worktree.path,
                    "branch": ctx.worktree.branch,
                }),
            );
        }
        Err(e) => tracing::warn!(%run_id, error = %e, "block transition rejected"),
    }
    preserve_worktree(state, ctx, &format!("blocked: {}", trigger.kind())).await;
}

/// Run the verification command once, sandboxed, in the worktree.
/// `None` when there is no check command to run.
async fn run_check(state: &Arc<AppState>, run_id: &str, ctx: &RunContext) -> Option<CheckOutcome> {
    let command = ctx.check_command.as_ref()?;
    let sandbox = state.sandbox.sandbox()?;
    let args = vec!["-c".to_string(), command.clone()];
    let started = std::time::Instant::now();
    let result = sandbox
        .run_command(
            &ctx.worktree.path,
            &ctx.dirs,
            Path::new("/bin/sh"),
            &args,
            &ctx.worktree.path,
        )
        .await;
    // How long the check took is the difference between "the suite is slow"
    // and "the suite hung", so it is recorded whether it passed or failed.
    let duration_ms = started.elapsed().as_millis() as u64;
    let outcome = match result {
        Ok(result) => {
            let passed = result.success();
            let stdout = tail(&result.stdout, 16_000);
            let stderr = tail(&result.stderr, 16_000);
            let _ = state.emit(
                Some(run_id),
                "run.check",
                json!({
                    "run_id": run_id,
                    "command": command,
                    "passed": passed,
                    "exit_code": result.status.code(),
                    "duration_ms": duration_ms,
                    // Enough of a failing build to diagnose it. The tail is
                    // what matters: compilers put the summary last.
                    "stdout": stdout,
                    "stderr": stderr,
                }),
            );
            crate::record_artifact(
                state,
                crate::ArtifactDraft {
                    run_id,
                    node_id: None,
                    kind: "check_output",
                    name: command,
                    path: None,
                    byte_size: None,
                    summary: &format!(
                        "exit {:?} in {duration_ms} ms\n{stdout}\n{stderr}",
                        result.status.code()
                    ),
                },
            );
            CheckOutcome {
                passed,
                evidence: tail(&format!("{}\n{}", result.stdout, result.stderr), 4000),
                class: format!("{command}#{:?}", result.status.code()),
            }
        }
        Err(e) => CheckOutcome {
            passed: false,
            evidence: format!("the check could not run: {e}"),
            class: format!("{command}#error"),
        },
    };
    // A verified state is worth returning to: record it before the next turn
    // can undo it.
    if outcome.passed {
        let _ = state.store.create_checkpoint(
            run_id,
            None,
            json!({
                "summary": format!("verification passed: {command}"),
                "base_commit": ctx.worktree.base_commit,
                "branch": ctx.worktree.branch,
            }),
        );
    }
    Some(outcome)
}

/// The check already passed this iteration: commit without re-running it.
/// Re-running would double the cost of every loop and could disagree with the
/// result the loop just acted on.
async fn finalize_verified(state: &Arc<AppState>, run_id: &str, ctx: &RunContext) {
    commit_and_finish(state, run_id, ctx).await;
}

/// Verification (optional check command) then a local commit on the run
/// branch, then the terminal transition. Never touches the base checkout.
async fn finalize_run(state: &Arc<AppState>, run_id: &str, ctx: &RunContext) {
    // 1. Verification. A direct run gets exactly one attempt: it has no loop
    //    to fix what the check rejects.
    if let Some(outcome) = run_check(state, run_id, ctx).await
        && !outcome.passed
    {
        let command = ctx.check_command.as_deref().unwrap_or("");
        fail_run(state, run_id, &format!("check failed: {command}"));
        preserve_worktree(state, ctx, "check failed").await;
        return;
    }
    commit_and_finish(state, run_id, ctx).await;
}

/// Commit whatever the run produced onto its own branch and finish. Shared by
/// the direct and loop shapes; the caller has already verified.
async fn commit_and_finish(state: &Arc<AppState>, run_id: &str, ctx: &RunContext) {
    let Some(sandbox) = state.sandbox.sandbox() else {
        fail_run(state, run_id, "sandbox unavailable at finalize");
        return;
    };
    let wt = &ctx.worktree;

    // 2. Local commit on the run branch.
    let summary: String = run_id.chars().take(8).collect();
    let message = format!("autoharness: {} run {summary}", shape_label(ctx.shape));
    let commit = match worktree::commit_all(sandbox, &ctx.dirs, wt, &message).await {
        Ok(commit) => commit,
        Err(e) => {
            fail_run(state, run_id, &format!("commit failed: {e}"));
            preserve_worktree(state, ctx, "commit failed").await;
            return;
        }
    };
    let stat = worktree::diff_stat(sandbox, &ctx.dirs, wt)
        .await
        .unwrap_or_default();
    let _ = state.emit(
        Some(run_id),
        "run.commit",
        json!({
            "run_id": run_id,
            "commit": commit,
            "branch": wt.branch,
            "base_commit": wt.base_commit,
            "diff_stat": stat,
        }),
    );

    // The patch itself, so the UI can show what changed rather than a count.
    // Bounded: the ledger is not a place for a regenerated lockfile.
    if commit.is_some()
        && let Ok(patch) = worktree::diff_patch(sandbox, &ctx.dirs, wt, 256 * 1024).await
        && !patch.is_empty()
    {
        let _ = state.emit(
            Some(run_id),
            "run.diff",
            json!({ "run_id": run_id, "commit": commit, "patch": patch }),
        );
    }
    if commit.is_some() {
        record_change_artifacts(state, run_id, None, sandbox, &ctx.dirs, wt, &stat).await;
    }

    // 3. A run that produced nothing leaves no worktree behind. Anything with
    //    a commit or uncommitted changes in it is preserved for review —
    //    `cleanup` decides, and it never discards work.
    if commit.is_none() {
        cleanup_worktree(state, ctx).await;
    }

    // 4. Terminal transition.
    if state.transition_run(run_id, RunState::Succeeded).is_ok() {
        let _ = state.emit(
            Some(run_id),
            "run.succeeded",
            json!({ "run_id": run_id, "commit": commit }),
        );
    }
}

/// Most changed files one commit may register as artifacts.
///
/// A refactor can touch thousands of files. The list is evidence, not an
/// inventory, so it stops at a number a person can read.
const MAX_FILE_ARTIFACTS: usize = 200;

/// Record the diff and the changed files as artifacts.
///
/// Each file artifact is a reference: name, worktree-relative path, and size.
/// The bytes stay in the worktree.
pub(crate) async fn record_change_artifacts(
    state: &AppState,
    run_id: &str,
    node_id: Option<&str>,
    sandbox: &crate::sandbox::Sandbox,
    dirs: &SessionDirs,
    wt: &RunWorktree,
    diff_stat: &str,
) {
    if !diff_stat.is_empty() {
        crate::record_artifact(
            state,
            crate::ArtifactDraft {
                run_id,
                node_id,
                kind: "diff",
                name: &format!("{} diff", wt.branch),
                path: None,
                byte_size: None,
                summary: diff_stat,
            },
        );
    }
    let Ok(files) = worktree::changed_files(sandbox, dirs, wt).await else {
        return;
    };
    let total = files.len();
    for file in files.iter().take(MAX_FILE_ARTIFACTS) {
        let full = wt.path.join(file);
        let byte_size = std::fs::metadata(&full).ok().map(|m| m.len() as i64);
        crate::record_artifact(
            state,
            crate::ArtifactDraft {
                run_id,
                node_id,
                kind: "file",
                name: file,
                path: Some(file.clone()),
                byte_size,
                summary: diff_stat,
            },
        );
    }
    if total > MAX_FILE_ARTIFACTS {
        // Say what was left out rather than let the list read as complete.
        crate::record_artifact(
            state,
            crate::ArtifactDraft {
                run_id,
                node_id,
                kind: "diff",
                name: "changed files truncated",
                path: None,
                byte_size: None,
                summary: &format!(
                    "{total} files changed; the first {MAX_FILE_ARTIFACTS} are listed"
                ),
            },
        );
    }
}

/// Stable lowercase label for commit messages and events.
fn shape_label(shape: ExecutionShape) -> &'static str {
    match shape {
        ExecutionShape::Direct => "direct",
        ExecutionShape::BoundedLoop => "loop",
        ExecutionShape::Swarm => "swarm",
        ExecutionShape::DynamicDag => "dag",
    }
}

/// Cancellation cleanup: remove daemon-owned CLEAN worktrees, preserve and
/// surface dirty ones.
async fn cleanup_worktree(state: &AppState, ctx: &RunContext) {
    let Some(sandbox) = state.sandbox.sandbox() else {
        return;
    };
    match worktree::cleanup(sandbox, &ctx.dirs, &state.store, &ctx.worktree).await {
        Ok(CleanupOutcome::Removed) => {
            let _ = state.emit(
                Some(&ctx.run_id),
                "run.worktree_removed",
                json!({ "run_id": ctx.run_id, "path": ctx.worktree.path }),
            );
        }
        Ok(CleanupOutcome::Preserved(reason)) => {
            preserve_worktree(state, ctx, &reason).await;
        }
        Err(e) => {
            preserve_worktree(state, ctx, &format!("cleanup error: {e}")).await;
        }
    }
}

/// Leave the worktree on disk and tell the UI where it is and why.
async fn preserve_worktree(state: &AppState, ctx: &RunContext, reason: &str) {
    let _ = state.emit(
        Some(&ctx.run_id),
        "run.worktree_preserved",
        json!({
            "run_id": ctx.run_id,
            "path": ctx.worktree.path,
            "branch": ctx.worktree.branch,
            "reason": reason,
        }),
    );
}

/// Restart reconciliation: runs that were active when the previous daemon
/// died have no live process anymore. They are reconciled to Blocked from
/// ledger + worktree state — NEVER reported as stale successes.
pub(crate) async fn reconcile_after_restart(state: &AppState) -> usize {
    let active = match state.store.runs_in_states(&["running", "paused"]) {
        Ok(runs) => runs,
        Err(e) => {
            tracing::error!(error = %e, "reconciliation query failed");
            return 0;
        }
    };
    let mut reconciled = 0;
    for run in active {
        if state.transition_run(&run.id, RunState::Blocked).is_err() {
            tracing::warn!(run_id = %run.id, state = %run.state, "reconcile transition rejected");
            continue;
        }
        let worktree = state.data_dir.join("worktrees").join(&run.id);
        let _ = state.emit(
            Some(&run.id),
            "run.reconciled",
            json!({
                "run_id": run.id,
                "reason": "daemon_restarted",
                "previous_state": run.state,
                "worktree_preserved": worktree.exists(),
                "detail": "engine process gone after daemon restart; work is preserved in the worktree and must be reviewed or restarted",
            }),
        );
        reconciled += 1;
    }
    reconciled
}
