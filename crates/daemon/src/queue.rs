//! Durable objective scheduling.
//!
//! SQLite owns the order and claim state. This module only applies the
//! persisted capacity setting and invokes the normal `run.start` path, so a
//! queued run has exactly the same sandbox, routing, worktree, and engine
//! behavior as a direct RPC start.

use std::sync::Arc;

use autoharness_core::RunState;
use autoharness_protocol::params::{QueueItem, QueueState};
use serde_json::{Value, json};

use crate::AppState;

/// Schedule a capacity check without blocking the caller that just persisted
/// an event or finished a runner.
pub(crate) fn kick(state: Arc<AppState>) {
    tokio::spawn(async move {
        dispatch_pending(state).await;
    });
}

async fn current_load(state: &AppState) -> Result<usize, autoharness_store::StoreError> {
    let dispatching = state.store.dispatching_objective_count()?;
    let active_ids: Vec<String> = state.active_runs.lock().await.keys().cloned().collect();
    let mut legacy = 0usize;
    for run_id in active_ids {
        if !state.store.run_has_dispatching_objective(&run_id)? {
            legacy += 1;
        }
    }
    Ok(dispatching + legacy)
}

async fn dispatch_pending(state: Arc<AppState>) {
    // A claim is already transactional in SQLite. This process-local lock
    // also makes the capacity observation and the claim one serialized
    // scheduler decision across concurrent client connections.
    let _dispatch = state.queue_dispatch.lock().await;
    loop {
        let limit = match state.store.app_settings() {
            Ok(settings) => settings.max_active_runs as usize,
            Err(error) => {
                tracing::error!(%error, "queue settings could not be loaded");
                return;
            }
        };
        let load = match current_load(&state).await {
            Ok(load) => load,
            Err(error) => {
                tracing::error!(%error, "queue load could not be computed");
                return;
            }
        };
        if load >= limit {
            return;
        }
        let item = match state.store.claim_next_objective() {
            Ok(Some(item)) => item,
            Ok(None) => return,
            Err(error) => {
                tracing::error!(%error, "queue claim failed");
                return;
            }
        };
        let _ = state.emit(
            Some(&item.run_id),
            "queue.dispatching",
            queue_payload(&item),
        );

        let options = state
            .store
            .queue_start_options(&item.id)
            .unwrap_or_else(|_| json!({}));
        let params = json!({
            "run_id": item.run_id,
            "route_mode": options.get("route_mode").cloned().unwrap_or(Value::Null),
            "budget": options.get("budget").cloned().unwrap_or(Value::Null),
        });
        match super::handle_run_start(&state, &params).await {
            Ok(result)
                if result.get("started").and_then(Value::as_bool) == Some(true)
                    || result.get("awaiting_approval").and_then(Value::as_bool) == Some(true) =>
            {
                let _ = state.emit(
                    Some(&item.run_id),
                    "queue.dispatched",
                    json!({
                        "queue_id": item.id,
                        "run_id": item.run_id,
                        "started": result.get("started").cloned().unwrap_or(Value::Bool(false)),
                        "awaiting_approval": result
                            .get("awaiting_approval")
                            .cloned()
                            .unwrap_or(Value::Bool(false)),
                    }),
                );
            }
            Ok(result) => {
                let reason = result
                    .get("reason")
                    .and_then(Value::as_str)
                    .unwrap_or("run did not start")
                    .to_string();
                fail_item(&state, &item, &reason);
            }
            Err(response) => {
                let reason = response
                    .error
                    .as_ref()
                    .map(|error| error.message.as_str())
                    .unwrap_or("run.start failed")
                    .to_string();
                fail_item(&state, &item, &reason);
            }
        }
    }
}

fn fail_item(state: &AppState, item: &QueueItem, reason: &str) {
    if let Ok(item) = state
        .store
        .finish_queue_item(&item.id, QueueState::Failed, Some(reason))
    {
        let _ = state.emit(
            Some(&item.run_id),
            "queue.failed",
            json!({
                "queue_id": item.id,
                "run_id": item.run_id,
                "kind": "objective",
                "error": reason,
            }),
        );
    }
}

pub(crate) fn queue_payload(item: &QueueItem) -> Value {
    serde_json::to_value(item).unwrap_or_else(|_| {
        json!({
            "queue_id": item.id,
            "run_id": item.run_id,
        })
    })
}

/// Called after the run task is no longer present in `active_runs`. This
/// ordering matters: the freed slot must be visible before the next kick.
pub(crate) async fn runner_stopped(state: &Arc<AppState>, run_id: &str) {
    state.active_runs.lock().await.remove(run_id);
    let Ok(run) = state.store.get_run(run_id) else {
        kick(Arc::clone(state));
        return;
    };
    let Ok(run_state) = run.state.parse::<RunState>() else {
        kick(Arc::clone(state));
        return;
    };
    let terminal = match run_state {
        RunState::Succeeded => Some((QueueState::Completed, "queue.completed", None)),
        RunState::Failed => Some((QueueState::Failed, "queue.failed", Some("run failed"))),
        RunState::Cancelled => Some((
            QueueState::Cancelled,
            "queue.cancelled",
            Some("run cancelled"),
        )),
        _ => None,
    };
    if let Some((queue_state, event_kind, error)) = terminal
        && let Ok(Some(item)) = state
            .store
            .finish_objective_for_run(run_id, queue_state, error)
    {
        let _ = state.emit(
            Some(run_id),
            event_kind,
            json!({
                "queue_id": item.id,
                "run_id": run_id,
                "kind": "objective",
                "state": queue_state,
                "error": error,
            }),
        );
    }
    kick(Arc::clone(state));
}
