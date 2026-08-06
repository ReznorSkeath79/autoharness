//! Fully in-process deterministic adapter for contract tests and daemon
//! integration tests. Scripted with per-turn canned event sequences.

use std::collections::VecDeque;

use autoharness_core::EngineKind;

use crate::adapter::{EngineAdapter, EngineError, SessionSpec};
use crate::diagnostics::EngineDiagnostics;
use crate::event::EngineEvent;

/// Which engine the fake presents itself as.
#[derive(Debug, Clone)]
pub struct FakeIdentity(pub EngineKind);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FakeState {
    Idle,
    InTurn,
    Cancelled,
}

/// Deterministic, scriptable [`EngineAdapter`].
///
/// - `start_session` queues a `SessionIdentity` event with a fixed ID.
/// - `send_turn` pops the next scripted turn into the pending queue.
/// - `next_event` drains the queue; `Ok(None)` when the turn is exhausted.
/// - `interrupt` drops the rest of the current turn; `cancel` ends everything.
/// - `pause` is a soft no-op state marker, matching the real adapters'
///   soft-pause semantics.
pub struct FakeEngine {
    kind: EngineKind,
    diagnostics: EngineDiagnostics,
    script: VecDeque<Vec<EngineEvent>>,
    pending: VecDeque<EngineEvent>,
    session_id: Option<String>,
    state: FakeState,
    /// When true, turns never produce events and `next_event` blocks.
    hang_turns: bool,
    /// When set, each turn's events are held until the gate is released.
    gate: Option<std::sync::Arc<tokio::sync::Notify>>,
    gate_armed: bool,
    /// Session ID the fake reports; fixed so tests can assert on it.
    pub fixed_session_id: String,
}

impl FakeEngine {
    /// Script one event sequence per turn, in order.
    pub fn scripted(kind: EngineKind, turns: Vec<Vec<EngineEvent>>) -> Self {
        Self {
            kind: kind.clone(),
            diagnostics: EngineDiagnostics::ready(
                kind,
                std::path::PathBuf::from("/fake/engine"),
                "fake-1.0".into(),
            ),
            script: turns.into(),
            pending: VecDeque::new(),
            session_id: None,
            state: FakeState::Idle,
            hang_turns: false,
            gate: None,
            gate_armed: false,
            fixed_session_id: "fake-session-0001".into(),
        }
    }

    /// A scripted fake whose turn events are held until `notify` fires.
    /// Returns the fake and the gate. Tests use it to interleave external
    /// actions (e.g. writing files) mid-run deterministically.
    pub fn gated(
        kind: EngineKind,
        turns: Vec<Vec<EngineEvent>>,
    ) -> (Self, std::sync::Arc<tokio::sync::Notify>) {
        let gate = std::sync::Arc::new(tokio::sync::Notify::new());
        let mut fake = Self::scripted(kind, turns);
        fake.gate = Some(std::sync::Arc::clone(&gate));
        (fake, gate)
    }

    /// A fake whose detection reports setup problems (unavailable engine).
    pub fn unavailable(kind: EngineKind, problems: Vec<String>) -> Self {
        let mut fake = Self::scripted(kind.clone(), vec![]);
        fake.diagnostics = EngineDiagnostics::not_installed(kind)
            .with_problem("fake engine configured unavailable");
        fake.diagnostics.problems.extend(problems);
        fake
    }

    /// A fake whose turns never complete: `send_turn` accepts work and
    /// `next_event` blocks indefinitely. For pause/resume/cancel tests.
    pub fn hanging(kind: EngineKind) -> Self {
        let mut fake = Self::scripted(kind, vec![]);
        fake.hang_turns = true;
        fake
    }

    /// Override the diagnostics (e.g. unauthenticated scenarios).
    pub fn with_diagnostics(mut self, diagnostics: EngineDiagnostics) -> Self {
        self.diagnostics = diagnostics;
        self
    }

    /// Events queued but not yet drained (test introspection).
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }
}

#[async_trait::async_trait]
impl EngineAdapter for FakeEngine {
    fn kind(&self) -> EngineKind {
        self.kind.clone()
    }

    async fn detect(&self) -> EngineDiagnostics {
        self.diagnostics.clone()
    }

    async fn start_session(&mut self, _spec: &SessionSpec) -> Result<(), EngineError> {
        if !self.diagnostics.ready {
            return Err(EngineError::NotInstalled("fake engine unavailable".into()));
        }
        self.session_id = Some(self.fixed_session_id.clone());
        self.state = FakeState::Idle;
        self.pending.push_back(EngineEvent::SessionIdentity {
            session_id: self.fixed_session_id.clone(),
        });
        Ok(())
    }

    async fn resume_session(
        &mut self,
        session_id: &str,
        spec: &SessionSpec,
    ) -> Result<(), EngineError> {
        self.start_session(spec).await?;
        // The resumed session keeps the caller's ID, not a fresh one.
        self.session_id = Some(session_id.to_string());
        self.pending = VecDeque::from([EngineEvent::SessionIdentity {
            session_id: session_id.to_string(),
        }]);
        Ok(())
    }

    async fn send_turn(&mut self, _prompt: &str) -> Result<(), EngineError> {
        if self.session_id.is_none() {
            return Err(EngineError::Protocol(
                "send_turn before start_session".into(),
            ));
        }
        if self.state == FakeState::Cancelled {
            return Err(EngineError::ProcessExited("session cancelled".into()));
        }
        if self.hang_turns {
            self.state = FakeState::InTurn;
            return Ok(());
        }
        if self.gate.is_some() {
            self.gate_armed = true;
        }
        let events = self.script.pop_front().unwrap_or_else(|| {
            vec![
                EngineEvent::Text {
                    text: "fake acknowledgment".into(),
                },
                EngineEvent::Completed { summary: None },
            ]
        });
        self.pending.extend(events);
        self.state = FakeState::InTurn;
        Ok(())
    }

    async fn next_event(&mut self) -> Result<Option<EngineEvent>, EngineError> {
        if self.state == FakeState::Cancelled {
            return Ok(None);
        }
        if self.hang_turns && self.pending.is_empty() && self.state == FakeState::InTurn {
            // Block until cancelled/interrupted (dropped by the caller's
            // select), like a long-running provider turn.
            tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
            return Ok(None);
        }
        if self.gate_armed && !self.pending.is_empty() {
            if let Some(gate) = &self.gate {
                gate.notified().await;
            }
            self.gate_armed = false;
        }
        let event = self.pending.pop_front();
        if self.pending.is_empty() && self.state == FakeState::InTurn && !self.hang_turns {
            self.state = FakeState::Idle;
        }
        Ok(event)
    }

    async fn pause(&mut self) -> Result<(), EngineError> {
        // Soft pause: no-op for the deterministic fake.
        Ok(())
    }

    async fn interrupt(&mut self) -> Result<(), EngineError> {
        self.pending.clear();
        if self.state == FakeState::InTurn {
            self.state = FakeState::Idle;
        }
        Ok(())
    }

    async fn cancel(&mut self) -> Result<(), EngineError> {
        self.pending.clear();
        self.state = FakeState::Cancelled;
        Ok(())
    }

    fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }
}
