//! Typed navigation actions shared by keyboard, palette, and overlays.

use crate::toolbar;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NavigationSurface {
    Overview,
    History,
    Worktrees,
    Queue,
    Notifications,
    Settings,
    Files,
}

impl NavigationSurface {
    pub const fn title(self) -> &'static str {
        match self {
            Self::Overview => "Overview",
            Self::History => "History",
            Self::Worktrees => "Worktrees",
            Self::Queue => "Queue",
            Self::Notifications => "Notifications",
            Self::Settings => "Settings",
            Self::Files => "Files",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pane {
    Sidebar,
    Execution,
    Inspector,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Forward,
    Reverse,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NavigationAction {
    OpenSurface(NavigationSurface),
    OpenPalette,
    TogglePane(Pane),
    BeginRunSwitcher(Direction),
    CommitRunSwitcher,
    Cancel,
    OverlayNext,
    OverlayPrevious,
    OverlayCommit,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OverlayState {
    pub surface: NavigationSurface,
    pub selected: usize,
}

impl OverlayState {
    pub const fn new(surface: NavigationSurface) -> Self {
        Self {
            surface,
            selected: 0,
        }
    }
}

pub fn toolbar_action_for(action: NavigationAction) -> Option<toolbar::ToolbarAction> {
    match action {
        NavigationAction::OpenSurface(NavigationSurface::Overview) => {
            Some(toolbar::ToolbarAction::OpenOverview)
        }
        NavigationAction::OpenSurface(NavigationSurface::History) => {
            Some(toolbar::ToolbarAction::OpenHistory)
        }
        NavigationAction::OpenSurface(NavigationSurface::Worktrees) => {
            Some(toolbar::ToolbarAction::OpenWorktrees)
        }
        NavigationAction::OpenSurface(NavigationSurface::Queue) => {
            Some(toolbar::ToolbarAction::OpenQueue)
        }
        NavigationAction::OpenSurface(NavigationSurface::Notifications) => {
            Some(toolbar::ToolbarAction::OpenNotifications)
        }
        NavigationAction::OpenSurface(NavigationSurface::Settings) => {
            Some(toolbar::ToolbarAction::OpenSettings)
        }
        NavigationAction::OpenSurface(NavigationSurface::Files) => {
            Some(toolbar::ToolbarAction::OpenFiles)
        }
        NavigationAction::OpenPalette => Some(toolbar::ToolbarAction::OpenPalette),
        NavigationAction::TogglePane(Pane::Sidebar) => Some(toolbar::ToolbarAction::ToggleSidebar),
        NavigationAction::TogglePane(Pane::Execution) => {
            Some(toolbar::ToolbarAction::ToggleExecution)
        }
        NavigationAction::TogglePane(Pane::Inspector) => {
            Some(toolbar::ToolbarAction::ToggleInspector)
        }
        _ => None,
    }
}

pub fn shortcut_for_key(key: &str, modifiers: gpui::Modifiers) -> Option<NavigationAction> {
    let platform_only = modifiers.platform
        && !modifiers.shift
        && !modifiers.control
        && !modifiers.alt
        && !modifiers.function;
    let platform_shift_only = modifiers.platform
        && modifiers.shift
        && !modifiers.control
        && !modifiers.alt
        && !modifiers.function;
    let control_only = modifiers.control
        && !modifiers.platform
        && !modifiers.shift
        && !modifiers.alt
        && !modifiers.function;
    let control_shift_only = modifiers.control
        && modifiers.shift
        && !modifiers.platform
        && !modifiers.alt
        && !modifiers.function;

    match key {
        "k" if platform_only => Some(NavigationAction::OpenPalette),
        "b" if platform_only => Some(NavigationAction::TogglePane(Pane::Sidebar)),
        "j" if platform_only => Some(NavigationAction::TogglePane(Pane::Execution)),
        "d" | "D" if platform_shift_only => Some(NavigationAction::TogglePane(Pane::Inspector)),
        "tab" if control_only => Some(NavigationAction::BeginRunSwitcher(Direction::Forward)),
        "tab" if control_shift_only => Some(NavigationAction::BeginRunSwitcher(Direction::Reverse)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn modifiers(keys: &str) -> gpui::Modifiers {
        gpui::Modifiers {
            platform: keys.contains('@'),
            control: keys.contains('^'),
            alt: keys.contains('~'),
            shift: keys.contains('$'),
            function: keys.contains('!'),
        }
    }

    #[test]
    fn keyboard_shortcuts_map_to_typed_navigation_actions() {
        assert_eq!(
            shortcut_for_key("k", modifiers("@")),
            Some(NavigationAction::OpenPalette)
        );
        assert_eq!(
            shortcut_for_key("tab", modifiers("^")),
            Some(NavigationAction::BeginRunSwitcher(Direction::Forward))
        );
        assert_eq!(
            shortcut_for_key("tab", modifiers("^$")),
            Some(NavigationAction::BeginRunSwitcher(Direction::Reverse))
        );
        assert_eq!(
            shortcut_for_key("b", modifiers("@")),
            Some(NavigationAction::TogglePane(Pane::Sidebar))
        );
        assert_eq!(
            shortcut_for_key("j", modifiers("@")),
            Some(NavigationAction::TogglePane(Pane::Execution))
        );
        assert_eq!(
            shortcut_for_key("d", modifiers("@$")),
            Some(NavigationAction::TogglePane(Pane::Inspector))
        );
    }

    #[test]
    fn every_typed_navigation_action_is_dispatchable_to_toolbar_or_shell() {
        for action in [
            NavigationAction::OpenSurface(NavigationSurface::Overview),
            NavigationAction::OpenSurface(NavigationSurface::History),
            NavigationAction::OpenSurface(NavigationSurface::Worktrees),
            NavigationAction::OpenSurface(NavigationSurface::Queue),
            NavigationAction::OpenSurface(NavigationSurface::Notifications),
            NavigationAction::OpenSurface(NavigationSurface::Settings),
            NavigationAction::OpenSurface(NavigationSurface::Files),
            NavigationAction::OpenPalette,
            NavigationAction::TogglePane(Pane::Sidebar),
            NavigationAction::TogglePane(Pane::Execution),
            NavigationAction::TogglePane(Pane::Inspector),
            NavigationAction::BeginRunSwitcher(Direction::Forward),
            NavigationAction::BeginRunSwitcher(Direction::Reverse),
            NavigationAction::CommitRunSwitcher,
            NavigationAction::Cancel,
            NavigationAction::OverlayNext,
            NavigationAction::OverlayPrevious,
            NavigationAction::OverlayCommit,
        ] {
            assert!(
                toolbar_action_for(action.clone()).is_some()
                    || matches!(
                        action,
                        NavigationAction::OpenSurface(_)
                            | NavigationAction::TogglePane(_)
                            | NavigationAction::BeginRunSwitcher(_)
                            | NavigationAction::CommitRunSwitcher
                            | NavigationAction::Cancel
                            | NavigationAction::OverlayNext
                            | NavigationAction::OverlayPrevious
                            | NavigationAction::OverlayCommit
                    ),
                "{action:?} should be reachable"
            );
        }
    }
}
