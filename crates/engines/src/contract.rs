//! Shared adapter contract suite. The same lifecycle assertions run against
//! every adapter: FakeEngine always; real adapters only when the CLI is
//! installed and `AUTOHARNESS_LIVE_TESTS=1` is set (live turns cost tokens).

use std::path::Path;
use std::time::Duration;

use crate::adapter::{EngineAdapter, SessionSpec};
use crate::event::EngineEvent;

const EVENT_TIMEOUT: Duration = Duration::from_secs(30);

async fn collect_turn(adapter: &mut dyn EngineAdapter) -> Result<Vec<EngineEvent>, String> {
    let mut events = Vec::new();
    loop {
        let next = tokio::time::timeout(EVENT_TIMEOUT, adapter.next_event())
            .await
            .map_err(|_| "timed out waiting for engine event".to_string())?
            .map_err(|e| format!("next_event failed: {e}"))?;
        match next {
            Some(event) => {
                let terminal = event.is_terminal();
                events.push(event);
                if terminal {
                    return Ok(events);
                }
            }
            None => return Ok(events),
        }
    }
}

/// Exercise the full adapter lifecycle and assert each scripted turn streams
/// exactly its expected normalized events.
///
/// `expected_turns[i]` is the event sequence expected from the i-th
/// `send_turn`. Must end in a terminal event (`Completed`/`Failed`).
pub async fn run_adapter_contract(
    adapter: &mut dyn EngineAdapter,
    working_dir: &Path,
    expected_turns: &[Vec<EngineEvent>],
) -> Result<(), String> {
    let diagnostics = adapter.detect().await;
    if !diagnostics.ready {
        return Err(format!("diagnostics not ready: {:?}", diagnostics.problems));
    }

    let spec = SessionSpec {
        working_dir: working_dir.to_path_buf(),
        data_dir: std::env::temp_dir(),
        // Each contract run is its own conversation; nothing resumes it.
        session_key: autoharness_core::new_id(),
        model: None,
        reasoning_effort: None,
    };
    adapter
        .start_session(&spec)
        .await
        .map_err(|e| format!("start_session failed: {e}"))?;
    let session_id = adapter
        .session_id()
        .map(str::to_string)
        .ok_or("session_id missing after start_session")?;

    // First event of a fresh session is its identity.
    let first = collect_turn(adapter).await?;
    let identity = EngineEvent::SessionIdentity {
        session_id: session_id.clone(),
    };
    if !first.contains(&identity) {
        return Err(format!("expected SessionIdentity first, got {first:?}"));
    }

    for (i, expected) in expected_turns.iter().enumerate() {
        adapter
            .send_turn(&format!("contract turn {i}"))
            .await
            .map_err(|e| format!("send_turn {i} failed: {e}"))?;
        let events = collect_turn(adapter).await?;
        if &events != expected {
            return Err(format!(
                "turn {i} events mismatch:\nexpected: {expected:?}\nactual:   {events:?}"
            ));
        }
    }

    // Pause and interrupt must be safe no-ops after a completed turn.
    adapter.pause().await.map_err(|e| format!("pause: {e}"))?;
    adapter
        .interrupt()
        .await
        .map_err(|e| format!("interrupt: {e}"))?;

    // Session identity persists and resumes when safe.
    adapter
        .resume_session(&session_id.clone(), &spec)
        .await
        .map_err(|e| format!("resume_session failed: {e}"))?;
    if adapter.session_id() != Some(session_id.as_str()) {
        return Err(format!(
            "resume changed session id: {:?} != {session_id}",
            adapter.session_id()
        ));
    }
    let resumed = collect_turn(adapter).await?;
    if !resumed.contains(&identity) {
        return Err(format!("resumed session missing identity: {resumed:?}"));
    }

    adapter.cancel().await.map_err(|e| format!("cancel: {e}"))?;
    Ok(())
}
