//! Compact, replay-safe attention rollup for the macOS menu bar.
//!
//! The pure [`rollup`] function only considers unseen items. Replayed ledger
//! history is inserted as seen by [`crate::attention`], so reopening the app
//! cannot turn old events into a fresh menu-bar badge.

use crate::attention::{AttentionKind, AttentionState};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rollup {
    pub unseen: usize,
    pub priority: Option<AttentionKind>,
    pub title: String,
    pub tooltip: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MenuModel {
    headline: String,
    open_label: String,
}

fn menu_model(rollup: &Rollup) -> MenuModel {
    let headline = if rollup.unseen == 0 {
        "Nothing needs you".into()
    } else {
        rollup
            .tooltip
            .rsplit_once(" — ")
            .map_or_else(|| rollup.tooltip.clone(), |(_, detail)| detail.to_string())
    };
    let open_label = if rollup.unseen == 0 {
        "Open AutoHarness".into()
    } else {
        format!("Open AutoHarness ({} unseen)", rollup.unseen)
    };
    MenuModel {
        headline,
        open_label,
    }
}

fn priority(kind: AttentionKind) -> u8 {
    match kind {
        AttentionKind::Question | AttentionKind::Approval => 0,
        AttentionKind::Blocker | AttentionKind::Failed => 1,
        AttentionKind::ResourcePressure => 2,
        AttentionKind::Succeeded => 3,
    }
}

fn compact(text: &str) -> String {
    const CAP: usize = 120;
    let mut chars = text.chars();
    let mut result: String = chars.by_ref().take(CAP).collect();
    if chars.next().is_some() {
        result.push('…');
    }
    result
}

pub fn rollup(state: &AttentionState) -> Rollup {
    let unseen: Vec<_> = state.items.iter().rev().filter(|item| !item.seen).collect();
    let selected = unseen
        .iter()
        .copied()
        .min_by_key(|item| priority(item.kind));
    let count = unseen.len();
    match selected {
        Some(item) => Rollup {
            unseen: count,
            priority: Some(item.kind),
            title: format!("AH {count}"),
            tooltip: format!(
                "AutoHarness — {count} unseen — {}: {}",
                item.kind.label(),
                compact(&item.detail)
            ),
        },
        None => Rollup {
            unseen: 0,
            priority: None,
            title: "AH".into(),
            tooltip: "AutoHarness — nothing needs you".into(),
        },
    }
}

#[cfg(target_os = "macos")]
mod native {
    use objc2::rc::Retained;
    use objc2::{MainThreadMarker, sel};
    use objc2_app_kit::{
        NSApplication, NSMenu, NSMenuItem, NSStatusBar, NSStatusItem, NSVariableStatusItemLength,
    };
    use objc2_foundation::NSString;

    use super::{Rollup, menu_model};

    /// A retained native status item. It is created and updated only by the
    /// GPUI shell on AppKit's main thread.
    pub struct MenuBarRollup {
        bar: Retained<NSStatusBar>,
        item: Retained<NSStatusItem>,
        headline: Retained<NSMenuItem>,
        open_item: Retained<NSMenuItem>,
        current: Option<Rollup>,
    }

    impl MenuBarRollup {
        pub fn new() -> Self {
            let mtm = MainThreadMarker::new()
                .expect("the native status item must be created on AppKit's main thread");
            let bar = NSStatusBar::systemStatusBar();
            let item = bar.statusItemWithLength(NSVariableStatusItemLength);
            let menu = NSMenu::new(mtm);
            let headline =
                NSMenuItem::sectionHeaderWithTitle(&NSString::from_str("Nothing needs you"), mtm);
            let open_item = NSMenuItem::new(mtm);
            open_item.setTitle(&NSString::from_str("Open AutoHarness"));
            let application = NSApplication::sharedApplication(mtm);
            // SAFETY: Both values are compile-time constants. NSApplication
            // is the exact target type for `unhide:` and Cocoa sends the menu
            // item as that method's optional sender. No user/model value can
            // become a selector, target, pointer, or executable input.
            unsafe {
                open_item.setTarget(Some(&application));
                open_item.setAction(Some(sel!(unhide:)));
            }
            menu.addItem(&headline);
            menu.addItem(&NSMenuItem::separatorItem(mtm));
            menu.addItem(&open_item);
            item.setMenu(Some(&menu));
            Self {
                bar,
                item,
                headline,
                open_item,
                current: None,
            }
        }

        pub fn update(&mut self, next: &Rollup) {
            if self.current.as_ref() == Some(next) {
                return;
            }
            // These two non-action properties are safe generated bindings.
            // No selector or user-provided executable input is involved.
            #[allow(deprecated)]
            self.item
                .setTitle(Some(&NSString::from_str(next.title.as_str())));
            #[allow(deprecated)]
            self.item
                .setToolTip(Some(&NSString::from_str(next.tooltip.as_str())));
            let menu = menu_model(next);
            self.headline
                .setTitle(&NSString::from_str(menu.headline.as_str()));
            self.open_item
                .setTitle(&NSString::from_str(menu.open_label.as_str()));
            self.current = Some(next.clone());
        }
    }

    impl Drop for MenuBarRollup {
        fn drop(&mut self) {
            self.bar.removeStatusItem(&self.item);
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod native {
    use super::Rollup;

    pub struct MenuBarRollup;

    impl MenuBarRollup {
        pub fn new() -> Self {
            Self
        }

        pub fn update(&mut self, _next: &Rollup) {}
    }
}

pub use native::MenuBarRollup;

impl Default for MenuBarRollup {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use super::*;
    use crate::attention::AttentionItem;

    fn item(id: &str, kind: AttentionKind, detail: &str, seen: bool) -> AttentionItem {
        AttentionItem {
            id: id.into(),
            run_id: Some(format!("run-{id}")),
            kind,
            title: kind.label().into(),
            detail: detail.into(),
            seq: 0,
            timestamp_ms: 0,
            seen,
            action: None,
        }
    }

    #[test]
    fn empty_and_replayed_attention_do_not_badge_the_menu_bar() {
        let state = AttentionState {
            items: VecDeque::from([item("old", AttentionKind::Blocker, "old blocker", true)]),
            banner: None,
        };
        assert_eq!(
            rollup(&state),
            Rollup {
                unseen: 0,
                priority: None,
                title: "AH".into(),
                tooltip: "AutoHarness — nothing needs you".into(),
            }
        );
    }

    #[test]
    fn rollup_prioritizes_a_human_question_over_newer_completion_noise() {
        let state = AttentionState {
            items: VecDeque::from([
                item(
                    "question",
                    AttentionKind::Question,
                    "Which database?",
                    false,
                ),
                item(
                    "finished",
                    AttentionKind::Succeeded,
                    "The run finished",
                    false,
                ),
            ]),
            banner: None,
        };
        let result = rollup(&state);
        assert_eq!(result.unseen, 2);
        assert_eq!(result.priority, Some(AttentionKind::Question));
        assert_eq!(result.title, "AH 2");
        assert!(result.tooltip.contains("Needs you: Which database?"));
    }

    #[test]
    fn newest_item_wins_within_the_same_priority_band() {
        let state = AttentionState {
            items: VecDeque::from([
                item("first", AttentionKind::Blocker, "first", false),
                item("second", AttentionKind::Failed, "second", false),
            ]),
            banner: None,
        };
        assert!(rollup(&state).tooltip.ends_with("Failed: second"));
    }

    #[test]
    fn native_menu_keeps_the_alert_visible_and_offers_a_real_open_action() {
        let state = AttentionState {
            items: VecDeque::from([item(
                "question",
                AttentionKind::Question,
                "Which database?",
                false,
            )]),
            banner: None,
        };
        let result = rollup(&state);
        let model = menu_model(&result);
        assert_eq!(model.headline, "Needs you: Which database?");
        assert_eq!(model.open_label, "Open AutoHarness (1 unseen)");
    }
}
