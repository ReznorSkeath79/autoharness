//! What deserves the user's attention, and what may interrupt them for it.
//!
//! The rules live in one pure function so they can be tested without a
//! notification centre, a sound device, or a running daemon. Two of them are
//! not preferences:
//!
//! - A replayed event never interrupts. Reconnecting or relaunching must not
//!   replay yesterday's alerts as today's.
//! - A notification action may only invoke a typed command that already
//!   exists. Nothing here synthesises keystrokes or builds a command string.

use std::collections::VecDeque;

use autoharness_protocol as proto;

/// Most items kept. Attention is a queue of recent asks, not an archive.
const ATTENTION_CAP: usize = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttentionKind {
    /// The engine asked the user something and is waiting.
    Question,
    /// A plan needs approval before workers commit to it.
    Approval,
    /// The run stopped on a sandbox or destructive-action blocker.
    Blocker,
    /// A run the user was not watching finished.
    Succeeded,
    /// A run the user was not watching failed.
    Failed,
    /// The daemon reported pressure on a bounded resource.
    ResourcePressure,
}

impl AttentionKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Question => "Needs you",
            Self::Approval => "Approve plan",
            Self::Blocker => "Blocked",
            Self::Succeeded => "Finished",
            Self::Failed => "Failed",
            Self::ResourcePressure => "Resource pressure",
        }
    }

    /// Whether this kind is worth an operating-system notification at all.
    /// Everything here is; the gate that matters is the user's setting.
    fn interrupts(self) -> bool {
        true
    }
}

/// The only thing a notification action is allowed to do.
///
/// Selecting a run is safe, reversible, and already reachable from the
/// keyboard. Approving a plan or cancelling a run from a notification is
/// deliberately not offered: those decisions need the plan on screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttentionAction {
    SelectRun(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttentionItem {
    pub id: String,
    pub run_id: Option<String>,
    pub kind: AttentionKind,
    pub title: String,
    pub detail: String,
    pub seq: u64,
    pub timestamp_ms: i64,
    pub seen: bool,
    pub action: Option<AttentionAction>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AttentionState {
    pub items: VecDeque<AttentionItem>,
    /// Set while the newest unseen item is shown as a banner.
    pub banner: Option<AttentionItem>,
}

impl AttentionState {
    pub fn unseen(&self) -> usize {
        self.items.iter().filter(|item| !item.seen).count()
    }

    /// Mark everything read. Called when the user opens the panel.
    pub fn mark_all_seen(&mut self) {
        for item in &mut self.items {
            item.seen = true;
        }
        self.banner = None;
    }

    pub fn dismiss_banner(&mut self) {
        self.banner = None;
    }
}

/// What the shell knows when it folds an event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AttentionContext<'a> {
    /// False while the daemon is replaying history from the ledger.
    pub live: bool,
    /// The run currently on screen, if any.
    pub selected_run: Option<&'a str>,
    pub notifications_enabled: bool,
    pub sounds_enabled: bool,
}

/// What the platform should do about one new item.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AttentionEffect {
    /// Post an operating-system notification.
    pub notify: bool,
    /// Play the alert sound.
    pub sound: bool,
    /// Show the in-app banner. Always true for a live item: it is the
    /// fallback when notifications are off, denied, or unavailable.
    pub banner: bool,
}

fn classify(kind: &str) -> Option<AttentionKind> {
    match kind {
        "engine.question" => Some(AttentionKind::Question),
        "run.awaiting_approval" => Some(AttentionKind::Approval),
        "run.blocked" => Some(AttentionKind::Blocker),
        "run.succeeded" => Some(AttentionKind::Succeeded),
        "run.failed" => Some(AttentionKind::Failed),
        "run.resource_pressure" => Some(AttentionKind::ResourcePressure),
        _ => None,
    }
}

fn field(payload: &serde_json::Value, key: &str) -> String {
    payload
        .get(key)
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn detail_for(kind: AttentionKind, payload: &serde_json::Value) -> String {
    match kind {
        AttentionKind::Question => field(payload, "prompt"),
        AttentionKind::Approval => "A plan is waiting for your approval".into(),
        AttentionKind::Blocker => {
            let reason = field(payload, "reason");
            let error = field(payload, "error");
            if error.is_empty() {
                reason
            } else {
                format!("{reason}: {error}")
            }
        }
        AttentionKind::Succeeded => "The run finished".into(),
        AttentionKind::Failed => {
            let reason = field(payload, "reason");
            if reason.is_empty() {
                "The run failed".into()
            } else {
                reason
            }
        }
        AttentionKind::ResourcePressure => field(payload, "detail"),
    }
}

/// Fold one event into the attention state and report what it earns.
///
/// Returns `None` when the event is not an attention trigger.
pub fn reduce(
    state: &mut AttentionState,
    event: &proto::Event,
    ctx: AttentionContext<'_>,
) -> Option<AttentionEffect> {
    let kind = classify(&event.kind)?;

    // A finished run the user is already watching is not an interruption:
    // the result is on screen. Only background runs earn an alert.
    let in_foreground = match (&event.run_id, ctx.selected_run) {
        (Some(run), Some(selected)) => run == selected,
        _ => false,
    };
    if matches!(kind, AttentionKind::Succeeded | AttentionKind::Failed) && in_foreground {
        return None;
    }

    let item = AttentionItem {
        id: format!("{}#{}", event.kind, event.sequence),
        run_id: event.run_id.clone(),
        kind,
        title: kind.label().to_string(),
        detail: detail_for(kind, &event.payload),
        seq: event.sequence,
        timestamp_ms: event.timestamp_ms,
        // History arrives already read. This is the rule that stops a
        // reconnect from replaying every old alert as a new one.
        seen: !ctx.live,
        action: event.run_id.clone().map(AttentionAction::SelectRun),
    };

    if state.items.iter().any(|existing| existing.id == item.id) {
        return None;
    }
    state.items.push_back(item.clone());
    while state.items.len() > ATTENTION_CAP {
        state.items.pop_front();
    }

    if !ctx.live {
        return Some(AttentionEffect::default());
    }
    state.banner = Some(item);
    Some(AttentionEffect {
        notify: ctx.notifications_enabled && kind.interrupts(),
        sound: ctx.sounds_enabled && kind.interrupts(),
        banner: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn event(seq: u64, kind: &str, run: Option<&str>) -> proto::Event {
        proto::Event::new(
            seq,
            run.map(str::to_string),
            seq,
            1_775_000_000_000,
            kind,
            json!({ "reason": "sandbox_unavailable", "prompt": "which database?" }),
        )
    }

    fn live_ctx() -> AttentionContext<'static> {
        AttentionContext {
            live: true,
            selected_run: None,
            notifications_enabled: true,
            sounds_enabled: true,
        }
    }

    #[test]
    fn every_trigger_raises_an_item_and_nothing_else_does() {
        let mut state = AttentionState::default();
        for (seq, kind, expected) in [
            (1, "engine.question", AttentionKind::Question),
            (2, "run.awaiting_approval", AttentionKind::Approval),
            (3, "run.blocked", AttentionKind::Blocker),
            (4, "run.succeeded", AttentionKind::Succeeded),
            (5, "run.failed", AttentionKind::Failed),
            (6, "run.resource_pressure", AttentionKind::ResourcePressure),
        ] {
            let effect = reduce(&mut state, &event(seq, kind, Some("run-1")), live_ctx());
            assert_eq!(
                effect,
                Some(AttentionEffect {
                    notify: true,
                    sound: true,
                    banner: true
                })
            );
            assert_eq!(state.items.back().unwrap().kind, expected);
        }
        assert_eq!(state.unseen(), 6);

        for quiet in ["engine.text", "run.commit", "artifact.created"] {
            assert_eq!(
                reduce(&mut state, &event(9, quiet, Some("run-1")), live_ctx()),
                None
            );
        }
    }

    /// Reconnecting replays the ledger. Yesterday's question must not ring
    /// today, and it must not arrive marked unread.
    #[test]
    fn replayed_events_are_recorded_silently_and_already_seen() {
        let mut state = AttentionState::default();
        let ctx = AttentionContext {
            live: false,
            ..live_ctx()
        };
        let effect = reduce(&mut state, &event(1, "engine.question", Some("run-1")), ctx);
        assert_eq!(effect, Some(AttentionEffect::default()));
        assert!(state.items[0].seen);
        assert_eq!(state.unseen(), 0);
        assert_eq!(state.banner, None);
    }

    #[test]
    fn settings_gate_the_interruption_but_never_the_in_app_banner() {
        let mut state = AttentionState::default();
        let effect = reduce(
            &mut state,
            &event(1, "run.blocked", Some("run-1")),
            AttentionContext {
                notifications_enabled: false,
                sounds_enabled: false,
                ..live_ctx()
            },
        );
        assert_eq!(
            effect,
            Some(AttentionEffect {
                notify: false,
                sound: false,
                banner: true
            }),
            "the in-app banner is the fallback and is never suppressed"
        );
        assert!(state.banner.is_some());

        let mut state = AttentionState::default();
        let effect = reduce(
            &mut state,
            &event(2, "run.blocked", Some("run-1")),
            AttentionContext {
                sounds_enabled: false,
                ..live_ctx()
            },
        );
        assert!(effect.unwrap().notify);
        assert!(!effect.unwrap().sound);
    }

    #[test]
    fn a_finished_run_on_screen_is_not_an_interruption() {
        let mut state = AttentionState::default();
        let ctx = AttentionContext {
            selected_run: Some("run-1"),
            ..live_ctx()
        };
        assert_eq!(
            reduce(&mut state, &event(1, "run.succeeded", Some("run-1")), ctx),
            None
        );
        assert_eq!(
            reduce(&mut state, &event(2, "run.failed", Some("run-1")), ctx),
            None
        );
        assert!(state.items.is_empty());

        // A background run still earns one.
        assert!(reduce(&mut state, &event(3, "run.succeeded", Some("run-2")), ctx).is_some());
        // A question is an interruption even for the run on screen: the run
        // is stopped until it is answered.
        assert!(reduce(&mut state, &event(4, "engine.question", Some("run-1")), ctx).is_some());
    }

    #[test]
    fn the_same_event_never_raises_two_items() {
        let mut state = AttentionState::default();
        assert!(
            reduce(
                &mut state,
                &event(7, "run.failed", Some("run-1")),
                live_ctx()
            )
            .is_some()
        );
        assert_eq!(
            reduce(
                &mut state,
                &event(7, "run.failed", Some("run-1")),
                live_ctx()
            ),
            None
        );
        assert_eq!(state.items.len(), 1);
    }

    #[test]
    fn notification_actions_only_select_a_run() {
        let mut state = AttentionState::default();
        reduce(
            &mut state,
            &event(1, "run.blocked", Some("run-9")),
            live_ctx(),
        );
        assert_eq!(
            state.items[0].action,
            Some(AttentionAction::SelectRun("run-9".into()))
        );
        // An event with no run offers no action at all.
        let mut state = AttentionState::default();
        reduce(
            &mut state,
            &event(2, "run.resource_pressure", None),
            live_ctx(),
        );
        assert_eq!(state.items[0].action, None);
    }

    #[test]
    fn opening_the_panel_clears_the_unseen_count_and_the_banner() {
        let mut state = AttentionState::default();
        reduce(
            &mut state,
            &event(1, "engine.question", Some("run-1")),
            live_ctx(),
        );
        reduce(
            &mut state,
            &event(2, "run.failed", Some("run-2")),
            live_ctx(),
        );
        assert_eq!(state.unseen(), 2);
        state.mark_all_seen();
        assert_eq!(state.unseen(), 0);
        assert_eq!(state.banner, None);
    }

    #[test]
    fn the_item_queue_is_bounded() {
        let mut state = AttentionState::default();
        for seq in 0..(ATTENTION_CAP as u64 + 25) {
            reduce(
                &mut state,
                &event(seq, "run.failed", Some("run-1")),
                live_ctx(),
            );
        }
        assert_eq!(state.items.len(), ATTENTION_CAP);
        assert_eq!(state.items.back().unwrap().seq, ATTENTION_CAP as u64 + 24);
    }
}
