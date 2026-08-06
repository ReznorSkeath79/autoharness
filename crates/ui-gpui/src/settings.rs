//! Interactive settings rows.
//!
//! Every control here maps to one persisted field on
//! [`autoharness_protocol::params::AppSettings`]. There is deliberately no
//! local-only "setting": a row either round-trips through `settings.update`
//! or it is informational and carries no action. Bounds live in the protocol
//! crate so the UI, the daemon, and the store agree on one clamp.

use crate::client::{Command, UiState};
use autoharness_protocol::params;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SettingsNumberField {
    MaxActiveRuns,
    MaxParallelWorkers,
    MaxGraphNodes,
    DefaultWallTimeMinutes,
    RetentionDays,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SettingsBoolField {
    AutomaticHistoryScan,
    NotificationsEnabled,
    SoundsEnabled,
    AutomaticUpdateChecks,
    ConfirmDestructiveActions,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SettingsAction {
    CycleEngine,
    CycleRouteMode,
    Increment(SettingsNumberField),
    Decrement(SettingsNumberField),
    Toggle(SettingsBoolField),
}

impl SettingsAction {
    /// The same row, driven the other way: Left arrow / right click. Cycles
    /// and toggles are their own inverse over enough presses, so only the
    /// numeric fields actually flip.
    pub(crate) const fn reversed(self) -> Self {
        match self {
            Self::Increment(field) => Self::Decrement(field),
            Self::Decrement(field) => Self::Increment(field),
            other => other,
        }
    }
}

impl SettingsNumberField {
    /// Step and bounds per field. Retention moves in months-ish jumps because
    /// nudging 90 days one at a time is not a control, it is a punishment.
    const fn step(self) -> u32 {
        match self {
            Self::MaxActiveRuns | Self::MaxParallelWorkers | Self::MaxGraphNodes => 1,
            Self::DefaultWallTimeMinutes => 5,
            Self::RetentionDays => 30,
        }
    }

    const fn bounds(self) -> (u32, u32) {
        match self {
            Self::MaxActiveRuns => (params::MIN_ACTIVE_RUNS, params::MAX_ACTIVE_RUNS),
            Self::MaxParallelWorkers => {
                (params::MIN_PARALLEL_WORKERS, params::MAX_PARALLEL_WORKERS)
            }
            Self::MaxGraphNodes => (params::MIN_GRAPH_NODES, params::MAX_GRAPH_NODES),
            Self::DefaultWallTimeMinutes => {
                (params::MIN_WALL_TIME_MINUTES, params::MAX_WALL_TIME_MINUTES)
            }
            Self::RetentionDays => (params::MIN_RETENTION_DAYS, params::MAX_RETENTION_DAYS),
        }
    }

    fn read(self, settings: &params::AppSettings) -> u32 {
        match self {
            Self::MaxActiveRuns => settings.max_active_runs,
            Self::MaxParallelWorkers => settings.max_parallel_workers,
            Self::MaxGraphNodes => settings.max_graph_nodes,
            Self::DefaultWallTimeMinutes => settings.default_wall_time_minutes,
            Self::RetentionDays => settings.retention_days,
        }
    }

    fn write(self, settings: &mut params::AppSettings, value: u32) {
        match self {
            Self::MaxActiveRuns => settings.max_active_runs = value,
            Self::MaxParallelWorkers => settings.max_parallel_workers = value,
            Self::MaxGraphNodes => settings.max_graph_nodes = value,
            Self::DefaultWallTimeMinutes => settings.default_wall_time_minutes = value,
            Self::RetentionDays => settings.retention_days = value,
        }
    }

    fn into_update(self, value: u32) -> params::SettingsUpdate {
        let mut update = params::SettingsUpdate::default();
        match self {
            Self::MaxActiveRuns => update.max_active_runs = Some(value),
            Self::MaxParallelWorkers => update.max_parallel_workers = Some(value),
            Self::MaxGraphNodes => update.max_graph_nodes = Some(value),
            Self::DefaultWallTimeMinutes => update.default_wall_time_minutes = Some(value),
            Self::RetentionDays => update.retention_days = Some(value),
        }
        update
    }
}

impl SettingsBoolField {
    fn read(self, settings: &params::AppSettings) -> bool {
        match self {
            Self::AutomaticHistoryScan => settings.automatic_history_scan,
            Self::NotificationsEnabled => settings.notifications_enabled,
            Self::SoundsEnabled => settings.sounds_enabled,
            Self::AutomaticUpdateChecks => settings.automatic_update_checks,
            Self::ConfirmDestructiveActions => settings.confirm_destructive_actions,
        }
    }

    fn write(self, settings: &mut params::AppSettings, value: bool) {
        match self {
            Self::AutomaticHistoryScan => settings.automatic_history_scan = value,
            Self::NotificationsEnabled => settings.notifications_enabled = value,
            Self::SoundsEnabled => settings.sounds_enabled = value,
            Self::AutomaticUpdateChecks => settings.automatic_update_checks = value,
            Self::ConfirmDestructiveActions => settings.confirm_destructive_actions = value,
        }
    }

    fn into_update(self, value: bool) -> params::SettingsUpdate {
        let mut update = params::SettingsUpdate::default();
        match self {
            Self::AutomaticHistoryScan => update.automatic_history_scan = Some(value),
            Self::NotificationsEnabled => update.notifications_enabled = Some(value),
            Self::SoundsEnabled => update.sounds_enabled = Some(value),
            Self::AutomaticUpdateChecks => update.automatic_update_checks = Some(value),
            Self::ConfirmDestructiveActions => update.confirm_destructive_actions = Some(value),
        }
        update
    }
}

/// Apply an action optimistically and return the command that makes it real.
///
/// The optimistic write keeps the row responsive; the daemon's
/// `settings.updated` broadcast is still the authority and overwrites this.
/// The next engine to make default, cycling through what the daemon actually
/// detected.
///
/// This used to be a two-arm swap: Codex became Claude and Claude became
/// Codex. With engines identified by manifest id rather than by enum variant
/// there can be any number of them, and a control that only ever reaches two
/// would hide the rest. Falls back to the built-ins before the first
/// `engine.list` reply, so the setting is never stuck.
fn next_default_engine(state: &UiState) -> autoharness_core::EngineKind {
    let mut known: Vec<autoharness_core::EngineKind> = state
        .engines
        .iter()
        .map(|engine| autoharness_core::EngineKind::new(&engine.name))
        // The default must be something a run can actually start on; gated
        // engines read as coming soon everywhere else, so cycling through
        // them here would be a control that picks the unpickable.
        .filter(autoharness_core::EngineKind::is_generally_available)
        .collect();
    if known.is_empty() {
        known = autoharness_core::EngineKind::builtins().to_vec();
    }
    known.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    let current = &state.settings.values.default_engine;
    let index = known.iter().position(|kind| kind == current);
    match index {
        Some(index) => known[(index + 1) % known.len()].clone(),
        // The persisted default is not among the detected engines; offering
        // the first known one is more useful than cycling within a set that
        // does not contain it.
        None => known[0].clone(),
    }
}

pub(crate) fn apply_settings_action(
    state: &mut UiState,
    action: SettingsAction,
) -> Option<Command> {
    let update = match action {
        SettingsAction::CycleEngine => {
            let next = next_default_engine(state);
            state.settings.values.default_engine = next.clone();
            // The composer's engine follows the persisted default so a new
            // objective goes where the setting says it goes.
            state.set_engine(next.as_str());
            params::SettingsUpdate {
                default_engine: Some(next),
                ..params::SettingsUpdate::default()
            }
        }
        SettingsAction::CycleRouteMode => {
            let next = match state.settings.values.default_route_mode {
                params::RouteMode::Auto => params::RouteMode::Priority,
                params::RouteMode::Priority => params::RouteMode::Parallel,
                params::RouteMode::Parallel => params::RouteMode::Auto,
            };
            state.settings.values.default_route_mode = next;
            params::SettingsUpdate {
                default_route_mode: Some(next),
                ..params::SettingsUpdate::default()
            }
        }
        SettingsAction::Increment(field) | SettingsAction::Decrement(field) => {
            let (min, max) = field.bounds();
            let current = field.read(&state.settings.values);
            let next = if matches!(action, SettingsAction::Increment(_)) {
                current.saturating_add(field.step())
            } else {
                current.saturating_sub(field.step())
            }
            .clamp(min, max);
            field.write(&mut state.settings.values, next);
            field.into_update(next)
        }
        SettingsAction::Toggle(field) => {
            let next = !field.read(&state.settings.values);
            field.write(&mut state.settings.values, next);
            field.into_update(next)
        }
    };
    state.settings.error = None;
    state.settings.pending_update = Some(update.clone());
    Some(Command::UpdateSettings(update))
}

pub(crate) fn on_off(value: bool) -> &'static str {
    if value { "On" } else { "Off" }
}
