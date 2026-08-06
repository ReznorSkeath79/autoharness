//! Top cockpit controls for the retained AutoHarness shell.

use gpui::prelude::*;
use gpui::{AnyElement, Context, ElementId, Role, SharedString, Toggled, div, px};

use crate::client::UiState;
use crate::icon::{Icon, IconName, IconSize};
use crate::layout::ShellLayout;
use crate::theme::{self, Colors, Fill, Ink, Metrics, Space, Status, Surface, Tone, Typo};
use crate::{Shell, components};
use autoharness_protocol::params;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolbarAction {
    ToggleSidebar,
    ToggleExecution,
    ToggleInspector,
    /// Pane sizes and open/closed state are window-local, never persisted,
    /// so resetting them is a layout action rather than a setting.
    ResetLayout,
    /// Advance the persisted routing mode. This used to be a control labelled
    /// "Auto" that was drawn permanently active and, when clicked, wrote a
    /// status line and nothing else — while the real setting lived in the
    /// settings sheet and could already be Priority or Parallel. The toolbar
    /// now shows and changes the same value.
    CycleRouteMode,
    UseCodex,
    UseClaude,
    OpenOverview,
    OpenHistory,
    OpenWorktrees,
    /// The read-only file viewer over the selected run's worktree.
    OpenFiles,
    OpenQueue,
    OpenPalette,
    OpenNotifications,
    OpenSettings,
}

/// Every control the toolbar draws, in render order: its accessible label,
/// whether it reads as active in the given state, and the action it runs.
///
/// This is the list `view` walks, not a parallel description of it — a control
/// cannot exist on screen without an action behind it, and a test cannot pass
/// by agreeing with a fiction.
pub fn controls(state: &UiState) -> Vec<(&'static str, bool, ToolbarAction)> {
    vec![
        (
            "Routing mode",
            state.settings.values.default_route_mode != params::RouteMode::Auto,
            ToolbarAction::CycleRouteMode,
        ),
        (
            "Use Codex",
            state.engine == "codex",
            ToolbarAction::UseCodex,
        ),
        (
            "Use Claude",
            state.engine == "claude",
            ToolbarAction::UseClaude,
        ),
        ("Run state", true, ToolbarAction::OpenOverview),
        ("Overview", true, ToolbarAction::OpenOverview),
        ("Worktrees", false, ToolbarAction::OpenWorktrees),
        ("Files", false, ToolbarAction::OpenFiles),
        ("Search", false, ToolbarAction::OpenPalette),
        (
            "Notifications",
            state.attention.unseen() > 0,
            ToolbarAction::OpenNotifications,
        ),
        ("Settings", false, ToolbarAction::OpenSettings),
    ]
}

pub(crate) fn view(
    state: &UiState,
    _layout: ShellLayout,
    files_tab_active: bool,
    cx: &mut Context<Shell>,
) -> AnyElement {
    let status = Status::of_run(&state.run_state);
    let run_label = run_state_label(&state.run_state, state.run_id.is_some(), status);

    div()
        .id("toolbar")
        .role(Role::Toolbar)
        .aria_label("AutoHarness controls")
        .flex()
        .items_center()
        .flex_none()
        .h(px(Metrics::TITLE_BAR))
        .bg(Colors::BACKGROUND)
        .border_b_1()
        .border_color(Colors::stroke())
        .pl(px(16.0))
        .pr(px(14.0))
        .gap(px(Space::ROW_H))
        .child(div().w(px(Metrics::TRAFFIC_LIGHT_LANE)).flex_none())
        .child(
            div()
                .flex_none()
                .text_size(px(Typo::DISPLAY_TITLE.size))
                .font_weight(Typo::DISPLAY_TITLE.weight)
                .child("AutoHarness"),
        )
        .when(
            !state.connected && !state.status.starts_with("preview:"),
            |toolbar| toolbar.child(connection_control(&state.status, cx)),
        )
        .child(div().w(px(30.0)).flex_none())
        .child(group_label("Route"))
        .child(route_control(state.settings.values.default_route_mode, cx))
        .child(div().w(px(24.0)).flex_none())
        .child(group_label("Engine"))
        .child(engine_control(
            "Codex",
            "codex",
            state.engine == "codex",
            cx,
        ))
        .child(engine_control(
            "Claude",
            "claude",
            state.engine == "claude",
            cx,
        ))
        .child(div().flex_1())
        .child(group_label("Run state"))
        .child(run_control(&run_label, status, &state.engine, cx))
        .when(state.queue.pending_objectives() > 0, |toolbar| {
            toolbar.child(queue_control(state.queue.pending_objectives(), cx))
        })
        .child(div().flex_1())
        .child(labelled_control(
            IconName::Grid,
            "Overview",
            true,
            ToolbarAction::OpenOverview,
            cx,
        ))
        .child(labelled_control(
            IconName::Worktree,
            "Worktrees",
            false,
            ToolbarAction::OpenWorktrees,
            cx,
        ))
        .child(labelled_control(
            IconName::Folder,
            "Files",
            files_tab_active,
            ToolbarAction::OpenFiles,
            cx,
        ))
        // One search affordance, not two. The magnifier and the ⌘K chip both
        // opened the same palette from adjacent buttons; the shortcut belongs
        // on the control it triggers, not on a second copy of it.
        .child(shortcut_control(
            IconName::Search,
            "⌘K",
            ToolbarAction::OpenPalette,
            cx,
        ))
        .child(bell_control(state.attention.unseen(), cx))
        .child(icon_control(
            IconName::Settings,
            ToolbarAction::OpenSettings,
            cx,
        ))
        .into_any_element()
}

pub fn route_mode_label(mode: params::RouteMode) -> &'static str {
    match mode {
        params::RouteMode::Auto => "Auto",
        params::RouteMode::Priority => "Priority",
        params::RouteMode::Parallel => "Parallel",
    }
}

/// What a routing mode actually does.
///
/// The control showed a word and nothing else, so the three modes were
/// indistinguishable unless you had read the router. A control whose effect
/// you cannot predict is not a control.
pub fn route_mode_meaning(mode: params::RouteMode) -> &'static str {
    match mode {
        params::RouteMode::Auto => {
            "let the router pick the shape from the objective and the repository"
        }
        params::RouteMode::Priority => "one task at a time, in the order you asked",
        params::RouteMode::Parallel => "split independent work across workers where it is safe",
    }
}

fn toolbar_icon(name: IconName, active: bool) -> Icon {
    Icon::new(
        name,
        IconSize::REGULAR,
        Colors::text(
            Surface::Sidebar,
            if active {
                Tone::Primary
            } else {
                Tone::Secondary
            },
        ),
    )
}

/// The routing mode, shown and changed in one control. Three modes cycle, so
/// the label is the current one rather than a fixed word.
fn route_control(mode: params::RouteMode, cx: &mut Context<Shell>) -> AnyElement {
    let label = route_mode_label(mode);
    let active = mode != params::RouteMode::Auto;
    components::compact_control("")
        .id(ElementId::Name(
            ToolbarAction::CycleRouteMode.element_id().into(),
        ))
        .role(Role::Button)
        .aria_label(format!(
            "Routing mode {label}: {}. Activate to change it.",
            route_mode_meaning(mode)
        ))
        .tab_index(0)
        .focus_visible(|control| control.border_color(theme::white(0.48)))
        .gap(px(6.0))
        .when(active, |control| {
            control
                .bg(Fill::selected(true))
                .text_color(Colors::text(Surface::Sidebar, Tone::Primary))
        })
        .child(toolbar_icon(IconName::Merge, active))
        .child(SharedString::from(label))
        .hover(|control| control.bg(theme::white(Fill::HOVER)))
        .cursor_pointer()
        .on_click(cx.listener(|shell, _, _, cx| {
            shell.toolbar_action(ToolbarAction::CycleRouteMode, cx);
        }))
        .into_any_element()
}

fn connection_control(status: &str, cx: &mut Context<Shell>) -> AnyElement {
    components::compact_control("Reconnecting")
        .id("toolbar-connection-recovery")
        .role(Role::Button)
        .aria_label(format!("Daemon disconnected: {status}. Open status"))
        .tab_index(0)
        .focus_visible(|control| control.border_color(Ink::ATTENTION))
        .border_color(Ink::ATTENTION)
        .text_color(Ink::ATTENTION)
        .hover(|control| control.bg(theme::white(Fill::HOVER)))
        .cursor_pointer()
        .on_click(cx.listener(|shell, _, _, cx| {
            shell.toolbar_action(ToolbarAction::OpenNotifications, cx);
        }))
        .into_any_element()
}

fn queue_control(count: usize, cx: &mut Context<Shell>) -> AnyElement {
    let label = format!("{count} queued objectives");
    components::compact_control(SharedString::from(format!("{count} queued")))
        .id(ElementId::Name(
            ToolbarAction::OpenQueue.element_id().into(),
        ))
        .role(Role::Button)
        .aria_label(label)
        .tab_index(0)
        .focus_visible(|control| control.border_color(theme::white(0.48)))
        .hover(|control| control.bg(theme::white(Fill::HOVER)))
        .cursor_pointer()
        .on_click(cx.listener(|shell, _, _, cx| {
            shell.toolbar_action(ToolbarAction::OpenQueue, cx);
        }))
        .into_any_element()
}

fn group_label(label: &'static str) -> AnyElement {
    div()
        .flex_none()
        .text_size(px(Typo::META.size))
        .text_color(Colors::text(Surface::Content, Tone::Tertiary))
        .child(label)
        .into_any_element()
}

fn engine_control(
    label: &'static str,
    engine: &'static str,
    active: bool,
    cx: &mut Context<Shell>,
) -> AnyElement {
    let action = match engine {
        "codex" => ToolbarAction::UseCodex,
        _ => ToolbarAction::UseClaude,
    };
    components::compact_control(label)
        .id(ElementId::Name(action.element_id().into()))
        .role(Role::RadioButton)
        .aria_label(format!("Use {label}"))
        .aria_toggled(if active {
            Toggled::True
        } else {
            Toggled::False
        })
        .tab_index(0)
        .focus_visible(|control| control.border_color(theme::white(0.48)))
        .gap(px(6.0))
        .when(active, |control| {
            control
                .bg(Fill::selected(true))
                .text_color(Colors::text(Surface::Sidebar, Tone::Primary))
        })
        .hover(|control| control.bg(theme::white(Fill::HOVER)))
        .cursor_pointer()
        .on_click(cx.listener(move |shell, _, _, cx| {
            shell.toolbar_action(action, cx);
        }))
        .into_any_element()
}

fn run_control(label: &str, status: Status, engine: &str, cx: &mut Context<Shell>) -> AnyElement {
    let label = label.to_string();
    components::compact_control("")
        .id(ElementId::Name("toolbar-action-run-state".into()))
        .role(Role::Button)
        .aria_label(format!("Run state: {label}. Open overview"))
        .tab_index(0)
        .focus_visible(|control| control.border_color(theme::white(0.48)))
        .gap(px(6.0))
        .bg(Fill::selected(true))
        .child(components::status_mark(status, engine))
        .child(SharedString::from(label))
        .hover(|control| control.bg(theme::white(Fill::HOVER)))
        .cursor_pointer()
        .on_click(cx.listener(|shell, _, _, cx| {
            shell.toolbar_action(ToolbarAction::OpenOverview, cx);
        }))
        .into_any_element()
}

/// The attention rollup: one bell in the toolbar carrying the unseen count.
///
/// This is a toolbar rollup, not a macOS menu-bar extra. A real menu-bar item
/// is an `NSStatusItem`, which needs an application bundle, so it is blocked
/// by the same thing that blocks notifications (see `crate::notify`). The
/// count is the whole rollup: a number beside the bell reads at a glance and
/// needs no colour, which matters on an all-black surface.
fn bell_control(unseen: usize, cx: &mut Context<Shell>) -> AnyElement {
    let count: Option<SharedString> = match unseen {
        0 => None,
        1..=99 => Some(unseen.to_string().into()),
        _ => Some("99+".into()),
    };
    components::compact_control("")
        .id(ElementId::Name(
            ToolbarAction::OpenNotifications.element_id().into(),
        ))
        .gap(px(5.0))
        .child(toolbar_icon(IconName::Activity, unseen > 0))
        .children(count.map(|count| div().child(count)))
        .role(Role::Button)
        .aria_label(if unseen == 0 {
            "Notifications, none unseen".to_string()
        } else {
            format!("Notifications, {unseen} unseen")
        })
        .tab_index(0)
        .focus_visible(|control| control.border_color(theme::white(0.48)))
        .when(unseen == 0, |control| {
            control.w(px(Metrics::TOOLBAR_CONTROL_SIZE)).px(px(0.0))
        })
        .hover(|control| control.bg(theme::white(Fill::HOVER)))
        .cursor_pointer()
        .on_click(cx.listener(move |shell, _, _, cx| {
            shell.toolbar_action(ToolbarAction::OpenNotifications, cx);
        }))
        .into_any_element()
}

fn icon_control(name: IconName, action: ToolbarAction, cx: &mut Context<Shell>) -> AnyElement {
    components::compact_control("")
        .id(ElementId::Name(action.element_id().into()))
        .role(Role::Button)
        .aria_label(action.accessibility_label())
        .tab_index(0)
        .focus_visible(|control| control.border_color(theme::white(0.48)))
        .w(px(Metrics::TOOLBAR_CONTROL_SIZE))
        .px(px(0.0))
        .child(toolbar_icon(name, false))
        .hover(|control| control.bg(theme::white(Fill::HOVER)))
        .cursor_pointer()
        .on_click(cx.listener(move |shell, _, _, cx| {
            shell.toolbar_action(action, cx);
        }))
        .into_any_element()
}

/// An icon control that also carries the keyboard shortcut that reaches it, so
/// the shortcut never needs a button of its own.
fn shortcut_control(
    name: IconName,
    shortcut: &'static str,
    action: ToolbarAction,
    cx: &mut Context<Shell>,
) -> AnyElement {
    components::compact_control("")
        .id(ElementId::Name(action.element_id().into()))
        .role(Role::Button)
        .aria_label(format!("{} ({shortcut})", action.accessibility_label()))
        .tab_index(0)
        .focus_visible(|control| control.border_color(theme::white(0.48)))
        .gap(px(6.0))
        .child(toolbar_icon(name, false))
        .child(
            div()
                .text_color(Colors::text(Surface::Sidebar, Tone::Tertiary))
                .child(shortcut),
        )
        .hover(|control| control.bg(theme::white(Fill::HOVER)))
        .cursor_pointer()
        .on_click(cx.listener(move |shell, _, _, cx| {
            shell.toolbar_action(action, cx);
        }))
        .into_any_element()
}

fn labelled_control(
    name: IconName,
    label: &'static str,
    active: bool,
    action: ToolbarAction,
    cx: &mut Context<Shell>,
) -> AnyElement {
    components::compact_control("")
        .id(ElementId::Name(action.element_id().into()))
        .role(Role::Button)
        .aria_label(action.accessibility_label())
        .tab_index(0)
        .focus_visible(|control| control.border_color(theme::white(0.48)))
        .gap(px(6.0))
        .when(active, |control| {
            control
                .bg(Fill::selected(true))
                .text_color(Colors::text(Surface::Sidebar, Tone::Primary))
        })
        .child(toolbar_icon(name, active))
        .child(label)
        .hover(|control| control.bg(theme::white(Fill::HOVER)))
        .cursor_pointer()
        .on_click(cx.listener(move |shell, _, _, cx| {
            shell.toolbar_action(action, cx);
        }))
        .into_any_element()
}

impl ToolbarAction {
    pub(crate) fn accessibility_label(self) -> &'static str {
        match self {
            Self::ToggleSidebar => "Toggle repositories pane",
            Self::ToggleExecution => "Toggle execution pane",
            Self::ToggleInspector => "Toggle inspector pane",
            Self::ResetLayout => "Reset pane layout",
            Self::CycleRouteMode => "Change routing mode",
            Self::UseCodex => "Use Codex",
            Self::UseClaude => "Use Claude",
            Self::OpenOverview => "Open overview",
            Self::OpenHistory => "Open provider history",
            Self::OpenWorktrees => "Open worktrees",
            Self::OpenFiles => "Open the file viewer",
            Self::OpenQueue => "Open queued work",
            Self::OpenPalette => "Search runs, projects, and commands",
            Self::OpenNotifications => "Open notifications",
            Self::OpenSettings => "Open settings",
        }
    }

    pub(crate) fn element_id(self) -> &'static str {
        match self {
            Self::ToggleSidebar => "toolbar-action-toggle-sidebar",
            Self::ToggleExecution => "toolbar-action-toggle-execution",
            Self::ToggleInspector => "toolbar-action-toggle-inspector",
            Self::ResetLayout => "toolbar-action-reset-layout",
            Self::CycleRouteMode => "toolbar-action-cycle-route-mode",
            Self::UseCodex => "toolbar-action-use-codex",
            Self::UseClaude => "toolbar-action-use-claude",
            Self::OpenOverview => "toolbar-action-open-overview",
            Self::OpenHistory => "toolbar-action-open-history",
            Self::OpenWorktrees => "toolbar-action-open-worktrees",
            Self::OpenFiles => "toolbar-action-open-files",
            Self::OpenQueue => "toolbar-action-open-queue",
            Self::OpenPalette => "toolbar-action-open-palette",
            Self::OpenNotifications => "toolbar-action-open-notifications",
            Self::OpenSettings => "toolbar-action-open-settings",
        }
    }
}

fn run_state_label(state: &str, has_run: bool, status: Status) -> String {
    if !has_run {
        return "Idle".into();
    }
    match state {
        "working" | "running" => "Running".into(),
        "awaiting_approval" => "Needs you".into(),
        "succeeded" => "Completed".into(),
        "failed" => "Failed".into(),
        "cancelled" => "Cancelled".into(),
        _ => status.label().into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// No dead clicks, and no two controls that do the same thing.
    ///
    /// The regression this covers is two-sided. The toolbar used to carry a
    /// magnifier and a `⌘K` chip that both opened the same palette, and a
    /// control labelled "Auto" that was drawn permanently active and wrote a
    /// status string when pressed. The old test asserted on a hand-written
    /// list of labels that did not match what `view` rendered at all, so it
    /// agreed with the toolbar's description of itself rather than the toolbar.
    #[test]
    fn every_toolbar_control_runs_a_distinct_action() {
        let state = crate::preview::populated();
        let controls = controls(&state);
        assert_eq!(controls.len(), 10);

        // Overview is reachable from the run-state chip and from its own
        // button, which is deliberate: one reports, one navigates. Everything
        // else must be the only way to its action.
        let mut actions: Vec<ToolbarAction> = controls
            .iter()
            .map(|(_, _, action)| *action)
            .filter(|action| *action != ToolbarAction::OpenOverview)
            .collect();
        let before = actions.len();
        actions.sort_by_key(|action| action.element_id());
        actions.dedup();
        assert_eq!(before, actions.len(), "a toolbar action is duplicated");

        // Every control's element id is unique, so clicks cannot be routed to
        // the wrong one.
        let mut ids: Vec<&str> = controls
            .iter()
            .map(|(_, _, action)| action.element_id())
            .collect();
        ids.sort_unstable();
        let total = ids.len();
        ids.dedup();
        assert_eq!(ids.len(), total - 1, "only Overview may appear twice");

        assert!(
            controls
                .iter()
                .all(|(label, _, _)| !label.is_empty() && *label != "Auto"),
            "controls describe themselves by what they do"
        );
    }

    /// Transcript adoption is a rare setup action and sat in the toolbar as a
    /// peer of Overview, which is where you live. It stays reachable from the
    /// palette; it does not need a permanent seat.
    #[test]
    fn history_is_not_a_toolbar_peer_of_overview() {
        let state = crate::preview::populated();
        assert!(
            !controls(&state)
                .iter()
                .any(|(_, _, action)| *action == ToolbarAction::OpenHistory),
            "history left the toolbar"
        );
    }

    /// A control whose effect you cannot predict is not a control. The routing
    /// chip showed one word and nothing else, so the three modes were
    /// indistinguishable unless you had read the router.
    #[test]
    fn every_routing_mode_explains_what_it_does() {
        for mode in [
            params::RouteMode::Auto,
            params::RouteMode::Priority,
            params::RouteMode::Parallel,
        ] {
            let meaning = route_mode_meaning(mode);
            assert!(!meaning.is_empty());
            assert_ne!(
                meaning,
                route_mode_label(mode),
                "the meaning must say more than the name"
            );
        }
        assert!(route_mode_meaning(params::RouteMode::Parallel).contains("split"));
        assert!(route_mode_meaning(params::RouteMode::Priority).contains("one task"));
    }

    /// The routing control reports the mode that is actually set, rather than
    /// claiming Auto forever.
    #[test]
    fn the_routing_control_reflects_the_persisted_mode() {
        let mut state = crate::preview::populated();

        state.settings.values.default_route_mode = params::RouteMode::Auto;
        let (_, active, action) = controls(&state)[0];
        assert_eq!(action, ToolbarAction::CycleRouteMode);
        assert!(!active, "Auto is the resting mode, not a selected one");

        state.settings.values.default_route_mode = params::RouteMode::Parallel;
        let (_, active, _) = controls(&state)[0];
        assert!(active, "a non-default routing mode must read as set");
    }

    #[test]
    fn run_state_label_projects_working_preview_as_running() {
        assert_eq!(run_state_label("working", true, Status::Working), "Running");
        assert_eq!(run_state_label("", false, Status::None), "Idle");
    }
}
