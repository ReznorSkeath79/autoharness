//! Native notification and fixed-sound delivery.
//!
//! Ledger events are folded on the daemon client's background thread, while
//! GPUI's macOS notification adapter must be driven from the application
//! thread. [`MacNotificationSink`] bridges that boundary with a bounded
//! queue. [`drain_platform`] is called by the shell on its normal 100 ms
//! refresh and hands the queued notifications to GPUI, which owns permission
//! prompting and `UNUserNotificationCenter` delivery.
//!
//! Every live item still raises an in-app banner. Notification permission can
//! be denied or revoked outside AutoHarness, and an OS notification is never
//! treated as the only copy of an alert.

use std::collections::VecDeque;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use gpui::{App, SystemNotification, SystemNotificationAction};

use crate::attention::{AttentionAction, AttentionItem};

const QUEUE_CAP: usize = 100;
const TITLE_CAP: usize = 80;
const BODY_CAP: usize = 320;
const AFPLAY: &str = "/usr/bin/afplay";
const ALERT_SOUND: &str = "/System/Library/Sounds/Glass.aiff";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Permission {
    /// A test double or platform adapter knows authorization was granted.
    Granted,
    /// The app is bundled and can ask the operating system for permission.
    /// The prompt and the user's decision are owned by macOS.
    Promptable,
    /// The user said no. The in-app banner carries everything instead.
    Denied,
    /// This build cannot ask because it is not running from an app bundle.
    #[default]
    Unavailable,
}

pub trait NotificationSink: Send + Sync {
    fn permission(&self) -> Permission;

    /// Schedule one notification. `true` means it reached the platform queue;
    /// the in-app banner remains visible regardless of this result.
    fn post(&self, item: &AttentionItem) -> bool;

    /// Schedule the fixed alert sound. Returns false when it is unavailable
    /// or a previous alert sound is still playing.
    fn play_sound(&self) -> bool;
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingNotification {
    tag: String,
    title: String,
    body: String,
    run_id: Option<String>,
}

/// The real macOS adapter. It is process-wide in production and independently
/// constructible in tests so queues never leak across test cases.
pub struct MacNotificationSink {
    bundled: bool,
    pending: Mutex<VecDeque<PendingNotification>>,
    routes: Mutex<VecDeque<(String, Option<String>)>>,
    activations: Mutex<VecDeque<AttentionAction>>,
}

impl Default for MacNotificationSink {
    fn default() -> Self {
        Self::new()
    }
}

impl MacNotificationSink {
    pub fn new() -> Self {
        Self::with_bundle_state(running_inside_app_bundle())
    }

    fn with_bundle_state(bundled: bool) -> Self {
        Self {
            bundled,
            pending: Mutex::new(VecDeque::new()),
            routes: Mutex::new(VecDeque::new()),
            activations: Mutex::new(VecDeque::new()),
        }
    }

    fn take_pending(&self) -> Vec<PendingNotification> {
        let Ok(mut pending) = self.pending.lock() else {
            return Vec::new();
        };
        pending.drain(..).collect()
    }

    fn remember_route(&self, tag: String, run_id: Option<String>) {
        let Ok(mut routes) = self.routes.lock() else {
            return;
        };
        routes.retain(|(existing, _)| existing != &tag);
        routes.push_back((tag, run_id));
        while routes.len() > QUEUE_CAP {
            routes.pop_front();
        }
    }

    fn accept_response(&self, tag: &str) {
        let run_id = self.routes.lock().ok().and_then(|mut routes| {
            let index = routes.iter().position(|(candidate, _)| candidate == tag)?;
            routes.remove(index).and_then(|(_, run_id)| run_id)
        });
        let Some(run_id) = run_id else {
            return;
        };
        if let Ok(mut activations) = self.activations.lock() {
            activations.push_back(AttentionAction::SelectRun(run_id));
            while activations.len() > QUEUE_CAP {
                activations.pop_front();
            }
        }
    }

    fn take_activations(&self) -> Vec<AttentionAction> {
        let Ok(mut activations) = self.activations.lock() else {
            return Vec::new();
        };
        activations.drain(..).collect()
    }
}

/// Whether this process is running from `Foo.app/Contents/MacOS/...`.
fn running_inside_app_bundle() -> bool {
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    exe.parent()
        .filter(|macos| macos.file_name().is_some_and(|name| name == "MacOS"))
        .and_then(|macos| macos.parent())
        .filter(|contents| contents.file_name().is_some_and(|name| name == "Contents"))
        .and_then(|contents| contents.parent())
        .is_some_and(|bundle| {
            bundle
                .extension()
                .is_some_and(|extension| extension == "app")
        })
}

fn bounded(text: &str, cap: usize) -> String {
    let mut chars = text.chars();
    let mut result: String = chars.by_ref().take(cap).collect();
    if chars.next().is_some() {
        result.push('…');
    }
    result
}

impl NotificationSink for MacNotificationSink {
    fn permission(&self) -> Permission {
        if self.bundled {
            Permission::Promptable
        } else {
            Permission::Unavailable
        }
    }

    fn post(&self, item: &AttentionItem) -> bool {
        if !self.bundled {
            return false;
        }
        let pending = PendingNotification {
            tag: format!("autoharness-attention-{}", item.id),
            title: bounded(&item.title, TITLE_CAP),
            body: bounded(&item.detail, BODY_CAP),
            run_id: item.run_id.clone(),
        };
        let Ok(mut queue) = self.pending.lock() else {
            return false;
        };
        queue.push_back(pending);
        while queue.len() > QUEUE_CAP {
            queue.pop_front();
        }
        true
    }

    fn play_sound(&self) -> bool {
        play_fixed_sound()
    }
}

/// Test double. Records what it was asked to do.
#[derive(Debug, Default)]
pub struct FakeNotificationSink {
    pub permission: Mutex<Permission>,
    pub posted: Mutex<Vec<String>>,
    pub sounds: std::sync::atomic::AtomicUsize,
}

impl FakeNotificationSink {
    pub fn granted() -> Self {
        Self {
            permission: Mutex::new(Permission::Granted),
            ..Self::default()
        }
    }

    pub fn with_permission(permission: Permission) -> Self {
        Self {
            permission: Mutex::new(permission),
            ..Self::default()
        }
    }

    pub fn posted(&self) -> Vec<String> {
        self.posted.lock().unwrap().clone()
    }

    pub fn sound_count(&self) -> usize {
        self.sounds.load(Ordering::Relaxed)
    }
}

impl NotificationSink for FakeNotificationSink {
    fn permission(&self) -> Permission {
        *self.permission.lock().unwrap()
    }

    fn post(&self, item: &AttentionItem) -> bool {
        if self.permission() != Permission::Granted {
            return false;
        }
        self.posted
            .lock()
            .unwrap()
            .push(format!("{}: {}", item.title, item.detail));
        true
    }

    fn play_sound(&self) -> bool {
        if self.permission() == Permission::Unavailable {
            return false;
        }
        self.sounds.fetch_add(1, Ordering::Relaxed);
        true
    }
}

fn platform_sink() -> &'static MacNotificationSink {
    static SINK: OnceLock<MacNotificationSink> = OnceLock::new();
    SINK.get_or_init(MacNotificationSink::new)
}

/// Drain background-thread alerts into GPUI's native notification adapter.
pub fn drain_platform(cx: &App) {
    let sink = platform_sink();
    for pending in sink.take_pending() {
        sink.remember_route(pending.tag.clone(), pending.run_id);
        cx.show_system_notification(SystemNotification {
            tag: pending.tag.into(),
            title: pending.title.into(),
            body: pending.body.into(),
            actions: vec![SystemNotificationAction {
                id: "open".into(),
                label: "Open AutoHarness".into(),
            }],
        });
    }
}

/// Record a click from GPUI's notification response callback.
pub fn accept_system_response(tag: &str) {
    platform_sink().accept_response(tag);
}

/// Typed actions produced by notification clicks, consumed by the shell.
pub fn drain_activation_actions() -> Vec<AttentionAction> {
    platform_sink().take_activations()
}

/// The sink used by the daemon client. Kept behind the trait for reducer tests.
pub fn system_sink() -> &'static dyn NotificationSink {
    platform_sink()
}

/// True only when the fixed executable and fixed system sound are present.
pub fn sound_supported() -> bool {
    cfg!(target_os = "macos") && Path::new(AFPLAY).is_file() && Path::new(ALERT_SOUND).is_file()
}

fn play_fixed_sound() -> bool {
    static PLAYING: AtomicBool = AtomicBool::new(false);
    if !sound_supported()
        || PLAYING
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
    {
        return false;
    }

    let started = std::thread::Builder::new()
        .name("autoharness-alert-sound".into())
        .spawn(|| {
            let _ = Command::new(AFPLAY)
                .arg(ALERT_SOUND)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            PLAYING.store(false, Ordering::Release);
        })
        .is_ok();
    if !started {
        PLAYING.store(false, Ordering::Release);
    }
    started
}

/// Carry out one attention effect. Returns true when the operating-system
/// delivery queue accepted the notification; the banner is independent.
pub fn deliver(
    sink: &dyn NotificationSink,
    effect: crate::attention::AttentionEffect,
    item: &AttentionItem,
) -> bool {
    let posted = effect.notify && sink.post(item);
    if effect.sound {
        sink.play_sound();
    }
    posted
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attention::{AttentionEffect, AttentionKind};

    fn item() -> AttentionItem {
        AttentionItem {
            id: "run.blocked#1".into(),
            run_id: Some("run-1".into()),
            kind: AttentionKind::Blocker,
            title: "Blocked".into(),
            detail: "sandbox_unavailable".into(),
            seq: 1,
            timestamp_ms: 0,
            seen: false,
            action: None,
        }
    }

    #[test]
    fn a_granted_sink_posts_and_plays() {
        let sink = FakeNotificationSink::granted();
        let delivered = deliver(
            &sink,
            AttentionEffect {
                notify: true,
                sound: true,
                banner: true,
            },
            &item(),
        );
        assert!(delivered);
        assert_eq!(sink.posted(), vec!["Blocked: sandbox_unavailable"]);
        assert_eq!(sink.sound_count(), 1);
    }

    #[test]
    fn a_denied_sink_falls_back_rather_than_dropping_the_alert() {
        let sink = FakeNotificationSink::with_permission(Permission::Denied);
        let effect = AttentionEffect {
            notify: true,
            sound: true,
            banner: true,
        };
        assert!(!deliver(&sink, effect, &item()));
        assert!(sink.posted().is_empty());
        assert!(effect.banner, "the banner is what the user still sees");
    }

    #[test]
    fn settings_that_disable_notifications_never_reach_the_sink() {
        let sink = FakeNotificationSink::granted();
        deliver(
            &sink,
            AttentionEffect {
                notify: false,
                sound: false,
                banner: true,
            },
            &item(),
        );
        assert!(sink.posted().is_empty());
        assert_eq!(sink.sound_count(), 0);
    }

    #[test]
    fn an_unbundled_build_refuses_notifications_without_losing_sound_support() {
        let sink = MacNotificationSink::with_bundle_state(false);
        assert_eq!(sink.permission(), Permission::Unavailable);
        assert!(!sink.post(&item()));
        assert!(sink.take_pending().is_empty());
    }

    #[test]
    fn a_bundled_build_queues_one_bounded_clickable_notification() {
        let sink = MacNotificationSink::with_bundle_state(true);
        let mut attention = item();
        attention.title = "x".repeat(TITLE_CAP + 10);
        attention.detail = "🧪".repeat(BODY_CAP + 10);

        assert_eq!(sink.permission(), Permission::Promptable);
        assert!(sink.post(&attention));
        let pending = sink.take_pending();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].title.chars().count(), TITLE_CAP + 1);
        assert_eq!(pending[0].body.chars().count(), BODY_CAP + 1);
        assert_eq!(pending[0].run_id.as_deref(), Some("run-1"));
    }

    #[test]
    fn notification_response_becomes_a_typed_run_selection_once() {
        let sink = MacNotificationSink::with_bundle_state(true);
        sink.remember_route("tag-1".into(), Some("run-7".into()));
        sink.accept_response("tag-1");
        sink.accept_response("tag-1");
        assert_eq!(
            sink.take_activations(),
            vec![AttentionAction::SelectRun("run-7".into())]
        );
    }

    #[test]
    fn notification_queue_is_bounded_and_keeps_the_newest_items() {
        let sink = MacNotificationSink::with_bundle_state(true);
        for sequence in 0..(QUEUE_CAP + 5) {
            let mut attention = item();
            attention.id = sequence.to_string();
            sink.post(&attention);
        }
        let pending = sink.take_pending();
        assert_eq!(pending.len(), QUEUE_CAP);
        assert!(pending.first().unwrap().tag.ends_with('5'));
        assert!(pending.last().unwrap().tag.ends_with("104"));
    }
}
