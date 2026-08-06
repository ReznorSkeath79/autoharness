//! AutoHarness desktop shell, on GPUI.
//!
//! The previous shell drew everything by hand onto a vello canvas: rows, text
//! clipping, scroll offsets, and a pixel-width estimate for every string. That
//! bought custom rendering nobody needed and cost the things an app actually
//! wants — clicking a row, hovering it, scrolling, focusing a field.
//!
//! Here layout, hit-testing, scrolling, and text measurement belong to the
//! framework, so this file describes the interface rather than drawing it. The
//! daemon client ([`client`]) and the design tokens ([`theme`]) carried over
//! unchanged; only the drawing was thrown away.

pub mod ansi;
pub mod attention;
pub mod client;
pub mod components;
pub mod coordinator;
pub(crate) mod execution_picker;
pub(crate) mod files;
pub mod fuzzy;
pub mod history;
pub mod icon;
pub mod inspector;
pub mod layout;
pub mod menubar;
pub mod motion;
pub mod navigation;
pub mod notify;
pub mod onboarding;
pub mod overview;
pub mod palette;
mod preview;
pub mod query_editor;
pub mod queue;
pub(crate) mod settings;
pub(crate) mod settings_page;
pub mod sidebar;
pub mod switcher;
pub mod theme;
pub mod toolbar;
pub mod update;
pub(crate) mod usage;
pub mod workbench;
pub(crate) mod worktrees;

use client::{Command, DaemonClient, UiState};
use gpui::prelude::*;
use gpui::{
    Animation, AnimationExt, AnyElement, App, Bounds, Context, CursorStyle, DragMoveEvent,
    ElementId, FocusHandle, KeyDownEvent, KeyUpEvent, MouseButton, PathPromptOptions, Role,
    SharedString, StatefulInteractiveElement, Window, WindowBounds, WindowOptions, deferred, div,
    ease_out_quint, px, size,
};
use navigation::{NavigationAction, NavigationSurface, OverlayState, Pane};
use switcher::{RunSwitcher, SwitchDirection, SwitchOutcome};
use theme::{Colors, Fill, Metrics, Radius, Space, Surface, Tone, Typo};

/// The daemon pushes state from another thread, so the shell polls it. GPUI
/// repaints only what changed, so this is cheap.
const REFRESH_MS: u64 = 100;
const RESIZE_HIT_TARGET: f32 = 9.0;
const RESIZE_HIT_OFFSET: f32 = -RESIZE_HIT_TARGET / 2.0;

/// Root view: sidebar, coordinator, review pane, and the palette over them.
pub(crate) struct Shell {
    client: DaemonClient,
    focus: FocusHandle,
    layout: layout::ShellLayout,
    /// The coordinator prompt. A real field: caret, selection, word and line
    /// motion, clipboard — not a String that grows by push and shrinks by pop.
    prompt: query_editor::QueryEditor,
    /// Palette state. `None` when closed — the palette owns the keyboard while
    /// it is open, which is why it is a mode rather than another pane.
    palette: Option<PaletteState>,
    /// Inline provider model and reasoning chooser anchored to the composer.
    model_picker_open: bool,
    /// Typed filter for that chooser, so reaching a model is typing rather
    /// than scrolling a list that grows with every engine.
    model_picker_query: query_editor::QueryEditor,
    /// Which engine tab the chooser is browsing. `None` follows the current
    /// selection; a click on a tab previews that engine without committing.
    pub(crate) model_picker_tab: Option<String>,
    /// The sidebar's new-run picker: which engine to start a thread on.
    new_run_picker_open: bool,
    pub(crate) utility_overlay: Option<OverlayState>,
    /// Settings-page navigation: which section is picked (`None` is all), and
    /// whether the section menu is dropped down.
    pub(crate) settings_section: Option<settings_page::SettingsSection>,
    pub(crate) settings_nav_open: bool,
    /// File-viewer state: which directories are open, which file is shown,
    /// and that file's loaded body (read once per selection, not per frame).
    pub(crate) files_expanded: std::collections::HashSet<std::path::PathBuf>,
    pub(crate) files_selected: Option<std::path::PathBuf>,
    pub(crate) files_body: Option<(std::path::PathBuf, files::FileBody)>,
    /// Which inspector tab is showing: run evidence or the file browser.
    pub(crate) inspector_tab: inspector::InspectorTab,
    /// The settings search filter. Typing anywhere on the page lands here.
    settings_query: query_editor::QueryEditor,
    run_switcher: Option<RunSwitcher>,
    mru_run_ids: Vec<String>,
    execution_mode: workbench::ExecutionMode,
    selected_graph_node: Option<String>,
    execution_transition_generation: u64,
    execution_tab_direction: f32,
    motion_initialized: bool,
    motion_sidebar_open: bool,
    motion_execution_open: bool,
    motion_inspector_open: bool,
    sidebar_seam: f32,
    execution_seam: f32,
    inspector_seam: f32,
    sidebar_slide: Option<motion::SeamSlide>,
    execution_slide: Option<motion::SeamSlide>,
    inspector_slide: Option<motion::SeamSlide>,
    body_available_width: f32,
    workbench_available_height: f32,
    sidebar_resize_origin: Option<(f32, f32)>,
    workbench_resize_origin: Option<(f32, f32)>,
    inspector_resize_origin: Option<(f32, f32)>,
    menu_bar: menubar::MenuBarRollup,
    update_manager: update::UpdateManager,
    quit_for_update: bool,
}

#[derive(Default)]
struct PaletteState {
    query: query_editor::QueryEditor,
    selected: usize,
    scope: PaletteScope,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum PaletteScope {
    #[default]
    All,
    RunsAndProjects,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum KeyRoute {
    Global(NavigationAction),
    RunSwitcher,
    UtilityOverlay,
    Palette,
    Composer,
}

pub(crate) type UtilityOverlay = NavigationSurface;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UtilityOverlayRow {
    pub label: String,
    pub value: String,
    pub action: Option<toolbar::ToolbarAction>,
    pub select_run_id: Option<String>,
    pub history_action: Option<HistoryOverlayAction>,
    pub settings_action: Option<settings::SettingsAction>,
    pub update_action: Option<update::UpdateAction>,
    pub worktree_action: Option<worktrees::WorktreeAction>,
    pub queue_action: Option<queue::QueueAction>,
    /// Leading identity mark; only integration rows carry one today.
    pub mark: Option<settings_page::RowMark>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum UtilityOverlayAction {
    Toolbar(toolbar::ToolbarAction),
    SelectRun(String),
    History(HistoryOverlayAction),
    Settings(settings::SettingsAction),
    Update(update::UpdateAction),
    Worktree(worktrees::WorktreeAction),
    Queue(queue::QueueAction),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HistoryOverlayAction {
    Refresh,
    NextPage,
    Adopt { provider: String, source_id: String },
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct ShellBodyMetrics {
    pub available_width: f32,
    pub available_height: f32,
    pub workbench_height: f32,
    pub columns: layout::ColumnWidths,
    pub rows: layout::RowHeights,
    pub center_width: f32,
}

const MAJOR_SEAM: f32 = 1.0;

/// Drag payload for the sidebar seam. It renders nothing; its job is to keep
/// pointer movement routed to the root once a resize starts.
#[derive(Clone, Copy)]
struct DraggedSidebarEdge;

impl Render for DraggedSidebarEdge {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        gpui::Empty
    }
}

/// Drag payload for the coordinator/execution divider.
#[derive(Clone, Copy)]
struct DraggedWorkbenchEdge;

impl Render for DraggedWorkbenchEdge {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        gpui::Empty
    }
}

/// Drag payload for the inspector seam.
#[derive(Clone, Copy)]
struct DraggedInspectorEdge;

impl Render for DraggedInspectorEdge {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        gpui::Empty
    }
}

impl Shell {
    fn with_client(client: DaemonClient, cx: &mut Context<Self>) -> Self {
        // Poll the client's shared state and repaint when it moves.
        cx.spawn(async move |shell, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(REFRESH_MS))
                    .await;
                if shell.update(cx, |_, cx| cx.notify()).is_err() {
                    break;
                }
            }
        })
        .detach();
        Self {
            client,
            focus: cx.focus_handle(),
            layout: layout::ShellLayout::default(),
            prompt: query_editor::QueryEditor::default(),
            palette: None,
            model_picker_open: false,
            model_picker_query: query_editor::QueryEditor::default(),
            model_picker_tab: None,
            new_run_picker_open: false,
            utility_overlay: None,
            settings_section: None,
            settings_nav_open: false,
            files_expanded: std::collections::HashSet::new(),
            files_selected: None,
            files_body: None,
            inspector_tab: inspector::InspectorTab::default(),
            settings_query: query_editor::QueryEditor::default(),
            run_switcher: None,
            mru_run_ids: Vec::new(),
            execution_mode: workbench::ExecutionMode::default(),
            selected_graph_node: None,
            execution_transition_generation: 0,
            execution_tab_direction: 1.0,
            motion_initialized: false,
            motion_sidebar_open: false,
            motion_execution_open: false,
            motion_inspector_open: false,
            sidebar_seam: 0.0,
            execution_seam: 0.0,
            inspector_seam: 0.0,
            sidebar_slide: None,
            execution_slide: None,
            inspector_slide: None,
            body_available_width: 0.0,
            workbench_available_height: 0.0,
            sidebar_resize_origin: None,
            workbench_resize_origin: None,
            inspector_resize_origin: None,
            menu_bar: menubar::MenuBarRollup::new(),
            update_manager: update::UpdateManager::default(),
            quit_for_update: false,
        }
    }

    /// Run a palette choice.
    fn invoke(&mut self, command: palette::PaletteCommand) {
        use palette::PaletteCommand as P;
        match command {
            P::SelectRun(id) => self.select_run(&id),
            P::SelectProject(index) => self.state().selected_project = index,
            P::UseEngine(name) => {
                let mut state = self.state();
                state.status = format!("engine: {name}");
                state.set_engine(name);
            }
            P::OpenSurface(surface) => {
                self.open_utility_surface(surface);
            }
            P::TogglePane(pane) => {
                self.dispatch_navigation_action(NavigationAction::TogglePane(pane));
            }
            P::Run(command) => self.submit(command.as_str()),
        }
        self.palette = None;
    }

    /// Rows for the current query.
    fn palette_rows(&self) -> Vec<palette::Ranked> {
        match &self.palette {
            Some(p) => {
                let actions = palette::actions(&self.state())
                    .into_iter()
                    .filter(|action| match p.scope {
                        PaletteScope::All => true,
                        PaletteScope::RunsAndProjects => matches!(
                            action.command,
                            palette::PaletteCommand::SelectRun(_)
                                | palette::PaletteCommand::SelectProject(_)
                        ),
                    })
                    .collect();
                palette::rank(actions, p.query.text())
            }
            None => Vec::new(),
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, UiState> {
        self.client.state.lock().expect("ui state mutex poisoned")
    }

    fn select_run(&mut self, run_id: &str) {
        self.state().select_run(run_id);
        self.selected_graph_node = None;
        self.remember_run(run_id);
    }

    pub(crate) fn select_graph_node(&mut self, node_id: &str, cx: &mut Context<Self>) {
        self.selected_graph_node = match self.selected_graph_node.as_deref() {
            Some(selected) if selected == node_id => None,
            _ => Some(node_id.to_string()),
        };
        cx.notify();
    }

    fn remember_run(&mut self, run_id: &str) {
        if run_id.is_empty() {
            return;
        }
        self.mru_run_ids.retain(|id| id != run_id);
        self.mru_run_ids.insert(0, run_id.to_string());
        self.mru_run_ids.truncate(24);
    }

    fn current_run_tip_ids(&self) -> Vec<String> {
        let state = self.state();
        current_run_tip_ids_for_state(&state, &self.mru_run_ids)
    }

    fn dispatch_navigation_action(&mut self, action: NavigationAction) {
        match action {
            NavigationAction::OpenSurface(surface) => {
                self.open_utility_surface(surface);
                self.palette = None;
                self.run_switcher = None;
            }
            NavigationAction::OpenPalette => {
                self.open_palette(PaletteScope::All);
                self.utility_overlay = None;
                self.run_switcher = None;
            }
            NavigationAction::TogglePane(pane) => {
                let action = match pane {
                    Pane::Sidebar => toolbar::ToolbarAction::ToggleSidebar,
                    Pane::Execution => toolbar::ToolbarAction::ToggleExecution,
                    Pane::Inspector => toolbar::ToolbarAction::ToggleInspector,
                };
                self.apply_layout_action(action);
            }
            NavigationAction::BeginRunSwitcher(direction) => {
                self.begin_or_cycle_run_switcher(direction);
            }
            NavigationAction::CommitRunSwitcher => {
                self.commit_run_switcher();
            }
            NavigationAction::Cancel => {
                self.cancel_transient_navigation();
            }
            NavigationAction::OverlayNext => self.move_overlay_selection(1),
            NavigationAction::OverlayPrevious => self.move_overlay_selection(-1),
            NavigationAction::OverlayCommit => self.commit_overlay_selection(),
        }
    }

    fn begin_or_cycle_run_switcher(&mut self, direction: navigation::Direction) {
        let direction = match direction {
            navigation::Direction::Forward => SwitchDirection::Forward,
            navigation::Direction::Reverse => SwitchDirection::Reverse,
        };
        if self.run_switcher.is_none() {
            let Some(original) = self.state().run_id.clone() else {
                return;
            };
            let Some(mut switcher) = RunSwitcher::open(original, self.current_run_tip_ids()) else {
                return;
            };
            switcher.cycle(direction);
            self.run_switcher = Some(switcher);
            self.palette = None;
            self.utility_overlay = None;
        } else if let Some(switcher) = &mut self.run_switcher {
            switcher.cycle(direction);
        }
    }

    fn commit_run_switcher(&mut self) {
        let run_id = self
            .run_switcher
            .as_ref()
            .map(RunSwitcher::commit)
            .and_then(|outcome| match outcome {
                SwitchOutcome::Commit(run_id) => Some(run_id.to_string()),
                _ => None,
            });
        self.run_switcher = None;
        if let Some(run_id) = run_id {
            self.select_run(&run_id);
        }
    }

    fn cancel_run_switcher(&mut self) {
        let run_id = self
            .run_switcher
            .as_ref()
            .map(RunSwitcher::cancel)
            .and_then(|outcome| match outcome {
                SwitchOutcome::Cancel(run_id) => Some(run_id.to_string()),
                _ => None,
            });
        self.run_switcher = None;
        if let Some(run_id) = run_id {
            self.state().select_run(&run_id);
        }
    }

    fn cancel_transient_navigation(&mut self) {
        if self.run_switcher.is_some() {
            self.cancel_run_switcher();
        } else if self.model_picker_open {
            self.model_picker_open = false;
        } else if self.new_run_picker_open {
            self.new_run_picker_open = false;
        } else if self.palette.is_some() {
            self.palette = None;
        } else if self.utility_overlay.is_some() {
            self.utility_overlay = None;
        }
    }

    fn overlay_keyboard_actions(&self) -> Vec<UtilityOverlayAction> {
        let Some(overlay) = &self.utility_overlay else {
            return Vec::new();
        };
        let state = self.state();
        // The settings page filters its rows; keyboard traversal must walk
        // exactly what is on screen or selection lands on hidden rows.
        let rows = if overlay.surface == UtilityOverlay::Settings {
            settings_page::visible_rows(
                &state,
                self.layout,
                self.settings_section,
                self.settings_query.text(),
            )
        } else {
            utility_overlay_rows(overlay.surface, &state, self.layout, &self.mru_run_ids)
        };
        utility_overlay_keyboard_actions(&rows)
    }

    fn move_overlay_selection(&mut self, delta: isize) {
        let max = self.overlay_keyboard_actions().len();
        let Some(overlay) = &mut self.utility_overlay else {
            return;
        };
        if max == 0 {
            overlay.selected = 0;
            return;
        }
        let last = max - 1;
        overlay.selected = if delta < 0 {
            overlay.selected.saturating_sub(1)
        } else {
            (overlay.selected + 1).min(last)
        };
    }

    fn commit_overlay_selection(&mut self) {
        let Some(selected) = self
            .utility_overlay
            .as_ref()
            .map(|overlay| overlay.selected)
        else {
            return;
        };
        let action = self.overlay_keyboard_actions().get(selected).cloned();
        if let Some(action) = action {
            self.apply_utility_overlay_action(action);
        }
    }

    /// Left arrow on a numeric settings row. Only settings rows have a
    /// meaningful reverse, so everything else is left alone rather than
    /// firing its forward action by surprise.
    fn reverse_overlay_selection(&mut self) {
        let Some(selected) = self
            .utility_overlay
            .as_ref()
            .map(|overlay| overlay.selected)
        else {
            return;
        };
        if let Some(UtilityOverlayAction::Settings(action)) =
            self.overlay_keyboard_actions().get(selected).cloned()
        {
            self.settings_action(action.reversed());
        }
    }

    pub(crate) fn apply_utility_overlay_action(&mut self, action: UtilityOverlayAction) {
        match action {
            UtilityOverlayAction::SelectRun(run_id) => {
                self.select_run(&run_id);
                self.utility_overlay = None;
            }
            UtilityOverlayAction::Toolbar(action) => {
                self.apply_layout_action(action);
            }
            UtilityOverlayAction::History(action) => self.history_action(action),
            UtilityOverlayAction::Settings(action) => self.settings_action(action),
            UtilityOverlayAction::Update(action) => self.update_action(action),
            UtilityOverlayAction::Worktree(action) => self.worktree_action(action),
            UtilityOverlayAction::Queue(action) => self.queue_action(action),
        }
    }

    fn worktree_action(&mut self, action: worktrees::WorktreeAction) {
        let command = {
            let mut state = self.state();
            worktrees::apply_worktree_action(&mut state, action)
        };
        if let Some(command) = command {
            self.client.send(command);
        }
    }

    pub(crate) fn settings_action(&mut self, action: settings::SettingsAction) {
        let command = {
            let mut state = self.state();
            settings::apply_settings_action(&mut state, action)
        };
        if let Some(command) = command {
            self.client.send(command);
        }
    }

    fn active_update_run_count(&self) -> usize {
        self.state()
            .runs
            .iter()
            .filter(|run| {
                matches!(
                    run.state.to_ascii_lowercase().as_str(),
                    "running" | "paused" | "awaiting_approval"
                )
            })
            .count()
    }

    fn start_update_check(&mut self) {
        let active_runs = self.active_update_run_count();
        let current_bundle = update::running_bundle_path();
        let data_dir = autoharness_daemon::DaemonConfig::default_paths().data_dir;
        if self
            .update_manager
            .start_check(current_bundle, data_dir, active_runs)
        {
            self.state().update_status = update::UpdateStatus::Checking;
        } else if !self.update_manager.checking() {
            self.state().update_status = update::UpdateStatus::Unknown(
                "the update-check worker could not be started".into(),
            );
        }
    }

    fn update_action(&mut self, action: update::UpdateAction) {
        match action {
            update::UpdateAction::CheckNow => self.start_update_check(),
            update::UpdateAction::InstallVerified => {
                let active_runs = self.active_update_run_count();
                let data_dir = autoharness_daemon::DaemonConfig::default_paths().data_dir;
                match self.update_manager.launch_installer(
                    &data_dir,
                    std::process::id(),
                    active_runs,
                ) {
                    Ok(()) => {
                        self.state().status =
                            "Installing verified update; AutoHarness will relaunch".into();
                        self.quit_for_update = true;
                    }
                    Err(blocker) => {
                        self.state().update_status = update::UpdateStatus::Blocked(blocker);
                    }
                }
            }
        }
    }

    fn queue_action(&mut self, action: queue::QueueAction) {
        let command = {
            let mut state = self.state();
            queue::apply_action(&mut state, action)
        };
        self.client.send(command);
    }

    pub(crate) fn toolbar_action(
        &mut self,
        action: toolbar::ToolbarAction,
        cx: &mut Context<Self>,
    ) {
        match action {
            toolbar::ToolbarAction::ToggleSidebar
            | toolbar::ToolbarAction::ToggleExecution
            | toolbar::ToolbarAction::ToggleInspector
            | toolbar::ToolbarAction::ResetLayout => {
                self.apply_layout_action(action);
            }
            toolbar::ToolbarAction::CycleRouteMode => {
                self.settings_action(settings::SettingsAction::CycleRouteMode);
                let mode = self.state().settings.values.default_route_mode;
                self.state().status = format!(
                    "routing {}: {}",
                    toolbar::route_mode_label(mode),
                    toolbar::route_mode_meaning(mode)
                );
            }
            toolbar::ToolbarAction::UseCodex => {
                let mut state = self.state();
                state.set_engine("codex");
                state.status = "engine: codex".into();
            }
            toolbar::ToolbarAction::UseClaude => {
                let mut state = self.state();
                state.set_engine("claude");
                state.status = "engine: claude".into();
            }
            toolbar::ToolbarAction::OpenOverview => {
                self.open_utility_surface(UtilityOverlay::Overview);
            }
            toolbar::ToolbarAction::OpenHistory => {
                self.open_utility_surface(UtilityOverlay::History);
            }
            toolbar::ToolbarAction::OpenWorktrees => {
                self.open_utility_surface(UtilityOverlay::Worktrees);
            }
            toolbar::ToolbarAction::OpenQueue => {
                self.open_utility_surface(UtilityOverlay::Queue);
            }
            toolbar::ToolbarAction::OpenPalette => {
                self.open_palette(PaletteScope::All);
                self.utility_overlay = None;
            }
            toolbar::ToolbarAction::OpenNotifications => {
                self.open_utility_surface(UtilityOverlay::Notifications);
            }
            toolbar::ToolbarAction::OpenSettings => {
                self.open_utility_surface(UtilityOverlay::Settings);
            }
            toolbar::ToolbarAction::OpenFiles => {
                self.select_inspector_tab(inspector::InspectorTab::Files, cx);
            }
        }
        cx.notify();
    }

    pub(crate) fn composer_action(
        &mut self,
        action: coordinator::ComposerAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match action {
            coordinator::ComposerAction::OpenMentions => {
                self.open_palette(PaletteScope::RunsAndProjects);
                self.utility_overlay = None;
            }
            coordinator::ComposerAction::ChooseRepository => {
                self.choose_repository(window, cx);
                return;
            }
            coordinator::ComposerAction::ToggleModelPicker => {
                self.model_picker_open = !self.model_picker_open;
                if self.model_picker_open {
                    // Fresh browse each open: current engine's tab, no filter.
                    self.model_picker_tab = None;
                    self.model_picker_query.clear();
                }
                self.palette = None;
                self.utility_overlay = None;
            }
            coordinator::ComposerAction::Submit => {
                let submitted = self.prompt.text().trim().to_string();
                if !submitted.is_empty() {
                    self.prompt.clear();
                    self.submit(&submitted);
                    self.state().input.clear();
                }
            }
        }
        cx.notify();
    }

    /// Start a new thread on `engine`, the way diri's New Agent row does.
    ///
    /// The draft is created in the daemon immediately, so a row appears in the
    /// sidebar — with a real creation time — as soon as the engine is chosen,
    /// rather than only once an objective has been typed and accepted. The
    /// composer then types into that run.
    /// Run a setup step's action. Each one is an existing, real path — this
    /// surface routes to them, it does not reimplement them.
    pub(crate) fn setup_action(
        &mut self,
        action: onboarding::SetupAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match action {
            onboarding::SetupAction::RecheckEngines => {
                self.client.send(Command::RefreshEngines);
                self.state().status = "checking engines…".into();
            }
            onboarding::SetupAction::ChooseRepository => self.choose_repository(window, cx),
            onboarding::SetupAction::CheckSandbox => {
                self.client.send(Command::CheckSandbox);
                self.state().status = "checking the sandbox…".into();
            }
            onboarding::SetupAction::StartFirstRun => {
                self.new_run_picker_open = true;
                if !self.layout.sidebar_open {
                    self.apply_layout_action(toolbar::ToolbarAction::ToggleSidebar);
                }
            }
        }
        cx.notify();
    }

    /// Reply to the question the selected run is blocked on.
    pub(crate) fn answer_run(&mut self, run_id: &str, answer: autoharness_core::Answer) {
        let label = match &answer {
            autoharness_core::Answer::Approve => "approved",
            autoharness_core::Answer::Deny => "denied",
            autoharness_core::Answer::Text(_) => "answered",
        };
        self.client.send(Command::AnswerRun {
            run_id: run_id.to_string(),
            answer,
        });
        // Cleared optimistically so the prompt does not sit there looking
        // unpressed; the daemon's run.answered is what actually confirms it,
        // and a failure re-reports through run.answer_failed.
        {
            let mut state = self.state();
            if let Some(detail) = state.run_details.get_mut(run_id) {
                detail.pending_question = None;
            }
            state.status = format!("{label} — waiting for the agent");
        }
    }

    /// Take an engine, model and effort together from the chooser.
    ///
    /// One call rather than three, because they are one decision: a model
    /// belongs to an engine and an effort belongs to a model, and applying
    /// them separately means passing through combinations that do not exist.
    pub(crate) fn choose_execution(&mut self, choice: &execution_picker::ExecutionChoice) {
        let update = {
            let mut state = self.state();
            state.set_engine(&choice.engine);
            if choice.model.is_empty() {
                state.model = None;
            } else {
                state.select_model(&choice.model);
            }
            if choice.effort.is_empty() {
                state.reasoning_effort = None;
            } else {
                state.select_reasoning_effort(&choice.effort);
            }
            state.execution_pinned_by_user = true;
            state.status = format!("run with {}", state.execution_selection_label());
            autoharness_protocol::params::SettingsUpdate {
                default_engine: Some(autoharness_core::EngineKind::new(&choice.engine)),
                default_model: Some((choice.engine.clone(), choice.model.clone())),
                default_reasoning_effort: Some((choice.engine.clone(), choice.effort.clone())),
                ..Default::default()
            }
        };
        self.client.send(Command::UpdateSettings(update));
        // The picker stays open (bb's recipe): model and effort are one
        // decision made in two picks, and closing between them forces a
        // reopen. Escape, Done, or Enter-to-commit close it.
        self.model_picker_tab = None;
    }

    pub(crate) fn toggle_new_run_picker(&mut self) {
        self.new_run_picker_open = !self.new_run_picker_open;
        self.model_picker_open = false;
        // Say what happened. A picker that opens off-screen, or does not open
        // at all, is otherwise indistinguishable from a dead button.
        self.state().status = if self.new_run_picker_open {
            "new run — choose an engine".into()
        } else {
            String::new()
        };
    }

    pub(crate) fn begin_new_run(
        &mut self,
        engine: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.new_run_picker_open = false;
        let command = {
            let mut state = self.state();
            state.set_engine(engine);
            // A new thread, not a turn of whichever one happened to be open.
            state.run_id = None;
            state.run_state.clear();
            state.summary.clear();
            state.graph = None;
            state.diff = None;
            match state.selected_project().map(|project| project.id.clone()) {
                Some(project_id) => {
                    // An empty draft on this engine is already exactly what
                    // this click would create. Select it rather than stacking
                    // a second identical row nothing will ever clean up.
                    match state
                        .reusable_draft(&project_id, engine)
                        .map(|run| run.id.clone())
                    {
                        Some(existing) => {
                            state.select_run(&existing);
                            state.status = "new run — describe the objective".into();
                            None
                        }
                        None => Some(Command::CreateDraftRun {
                            project_id,
                            engine: engine.to_string(),
                            model: state.model.clone(),
                            reasoning_effort: state.reasoning_effort.clone(),
                        }),
                    }
                }
                None => {
                    state.status = "choose a Git repository first".into();
                    None
                }
            }
        };
        if let Some(command) = command {
            self.client.send(command);
        }
        self.prompt.clear();
        window.focus(&self.focus, cx);
        cx.notify();
    }

    pub(crate) fn choose_repository(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.model_picker_open = false;
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Choose a Git repository".into()),
        });
        self.state().status = "choose a Git repository…".into();
        cx.spawn_in(window, async move |this, cx| {
            let path = match paths.await {
                Ok(Ok(Some(mut paths))) => paths.pop(),
                _ => None,
            };
            let Some(path) = path else {
                this.update_in(cx, |shell, _window, cx| {
                    shell.state().status = "repository selection cancelled".into();
                    cx.notify();
                })
                .ok();
                return;
            };
            this.update_in(cx, |shell, _window, cx| {
                let name = path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| path.to_string_lossy().into_owned());
                shell.client.send(Command::AddProject {
                    name,
                    path: path.to_string_lossy().into_owned(),
                });
                shell.state().status = "registering repository…".into();
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn open_palette(&mut self, scope: PaletteScope) {
        self.model_picker_open = false;
        self.palette = Some(PaletteState {
            query: query_editor::QueryEditor::default(),
            selected: 0,
            scope,
        });
    }

    pub(crate) fn dismiss_utility_overlay(&mut self, cx: &mut Context<Self>) {
        self.utility_overlay = None;
        cx.notify();
    }

    fn open_utility_surface(&mut self, surface: UtilityOverlay) {
        self.model_picker_open = false;
        self.utility_overlay = Some(OverlayState::new(surface));
        if surface == UtilityOverlay::Settings {
            // The page opens fresh: all sections, no filter, menu closed.
            self.settings_section = None;
            self.settings_nav_open = false;
            self.settings_query.clear();
        }
        if surface == UtilityOverlay::History {
            let should_load = {
                let mut state = self.state();
                if state.history.entries.is_empty() && !state.history.loading {
                    state.history.loading = true;
                    true
                } else {
                    false
                }
            };
            if should_load {
                self.client.send(Command::RefreshHistory { cursor: None });
            }
        }
        if surface == UtilityOverlay::Notifications {
            // Opening the panel is what reading means. The banner goes with it.
            self.state().attention.mark_all_seen();
        }
        if surface == UtilityOverlay::Worktrees {
            let filter = {
                let mut state = self.state();
                state.worktrees.loading = true;
                state.worktrees.error = None;
                state.worktrees.filter
            };
            self.client.send(Command::RefreshWorktrees(filter));
        }
    }

    fn history_action(&mut self, action: HistoryOverlayAction) {
        match action {
            HistoryOverlayAction::Refresh => {
                {
                    let mut state = self.state();
                    state.history.loading = true;
                    state.history.next_cursor = None;
                    state.history.error = None;
                }
                self.client.send(Command::RefreshHistory { cursor: None });
            }
            HistoryOverlayAction::NextPage => {
                let cursor = {
                    let mut state = self.state();
                    let cursor = state.history.next_cursor.clone();
                    if cursor.is_some() {
                        state.history.loading = true;
                    }
                    cursor
                };
                if let Some(cursor) = cursor {
                    self.client.send(Command::RefreshHistory {
                        cursor: Some(cursor),
                    });
                }
            }
            HistoryOverlayAction::Adopt {
                provider,
                source_id,
            } => {
                let command = {
                    let mut state = self.state();
                    let Some(project) = state.selected_project().cloned() else {
                        state.status = "select a project before adopting history".into();
                        return;
                    };
                    state.history.loading = true;
                    state.status = format!("adopting {provider} history…");
                    Command::AdoptHistory {
                        provider,
                        source_id,
                        project_id: project.id,
                        engine: state.engine.clone(),
                    }
                };
                self.client.send(command);
            }
        }
    }

    pub(crate) fn set_execution_mode(
        &mut self,
        mode: workbench::ExecutionMode,
        cx: &mut Context<Self>,
    ) {
        if mode == self.execution_mode {
            return;
        }
        self.execution_tab_direction = if mode == workbench::ExecutionMode::Timeline {
            1.0
        } else {
            -1.0
        };
        self.execution_mode = mode;
        self.execution_transition_generation = self.execution_transition_generation.wrapping_add(1);
        cx.notify();
    }

    pub(crate) fn node_control(
        &mut self,
        run_id: &str,
        node_id: &str,
        retry: bool,
        cx: &mut Context<Self>,
    ) {
        self.client.send(Command::NodeControl {
            run_id: run_id.to_string(),
            node_id: node_id.to_string(),
            retry,
        });
        self.state().status = format!(
            "{} node {node_id}…",
            if retry { "retrying" } else { "cancelling" }
        );
        cx.notify();
    }

    fn layout_transition_active(&self, action: toolbar::ToolbarAction) -> bool {
        match action {
            toolbar::ToolbarAction::ToggleSidebar => {
                self.sidebar_slide.is_some()
                    || (self.motion_initialized
                        && self.layout.sidebar_open != self.motion_sidebar_open)
            }
            toolbar::ToolbarAction::ToggleExecution => {
                self.execution_slide.is_some()
                    || (self.motion_initialized
                        && self.layout.execution_open != self.motion_execution_open)
            }
            toolbar::ToolbarAction::ToggleInspector => {
                self.inspector_slide.is_some()
                    || (self.motion_initialized
                        && self.layout.inspector_open != self.motion_inspector_open)
            }
            _ => false,
        }
    }

    fn apply_layout_action(&mut self, action: toolbar::ToolbarAction) -> bool {
        if self.layout_transition_active(action) {
            return false;
        }
        apply_layout_toolbar_action(&mut self.layout, action)
    }

    /// Text entry. GPUI delivers real key events, so the prompt is a field
    /// rather than a string the window loop happens to append to.
    fn on_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let key = event.keystroke.key.as_str();
        let modifiers = event.keystroke.modifiers;

        // The model chooser is modal to the composer. Tab and control
        // activation stay native to its real focusable rows; Escape closes it
        // even when one of those rows, rather than the root editor, has focus.
        if self.model_picker_open {
            match key {
                "escape" => {
                    self.model_picker_open = false;
                    self.model_picker_query.clear();
                    self.model_picker_tab = None;
                    window.focus(&self.focus, cx);
                }
                // Tab walks the engine tabs, mirroring the pointer.
                "tab" => {
                    let next = {
                        let state = self.state();
                        let view = execution_picker::picker_view(
                            &state,
                            self.model_picker_tab.as_deref(),
                            self.model_picker_query.text(),
                        );
                        let index = view
                            .tabs
                            .iter()
                            .position(|tab| tab.active)
                            .unwrap_or_default();
                        (!view.tabs.is_empty()).then(|| {
                            let step = if modifiers.shift {
                                view.tabs.len().saturating_sub(1)
                            } else {
                                1
                            };
                            view.tabs[(index + step) % view.tabs.len()].engine.clone()
                        })
                    };
                    if let Some(engine) = next {
                        self.model_picker_tab = Some(engine);
                    }
                }
                // The filter is the point of the chooser: typing has to reach
                // it, or it is a search box that cannot be searched.
                "backspace" => {
                    self.model_picker_query
                        .delete_backward(query_editor::Motion::Character);
                }
                "enter" => {
                    // Take the best remaining match on the active tab, commit,
                    // and close: Enter is the "I'm done" gesture, unlike a
                    // click, which keeps the picker open for the effort.
                    let choice = {
                        let state = self.state();
                        execution_picker::picker_view(
                            &state,
                            self.model_picker_tab.as_deref(),
                            self.model_picker_query.text(),
                        )
                        .models
                        .into_iter()
                        .find(|choice| choice.unavailable.is_none())
                    };
                    if let Some(choice) = choice {
                        self.choose_execution(&choice);
                        self.model_picker_open = false;
                        self.model_picker_query.clear();
                    }
                }
                _ => {
                    if let Some(text) = event.keystroke.key_char.as_ref()
                        && !event.keystroke.modifiers.platform
                        && !event.keystroke.modifiers.control
                    {
                        self.model_picker_query.insert(text);
                    }
                }
            }
            window.prevent_default();
            cx.notify();
            return;
        }

        let route = key_route(
            key,
            modifiers,
            self.palette.is_some(),
            self.utility_overlay.is_some(),
            self.run_switcher.is_some(),
        );
        // When a real tab stop owns focus, its on-click handler receives
        // Enter/Space on key-up. The root still owns global shortcuts and
        // modal routing, but must not also type into or submit the composer.
        if !self.focus.is_focused(window) && matches!(route, KeyRoute::Composer) {
            return;
        }

        match route {
            KeyRoute::Global(action) => {
                self.dispatch_navigation_action(action);
                cx.notify();
                return;
            }
            KeyRoute::RunSwitcher => {
                match navigation::shortcut_for_key(key, modifiers) {
                    Some(NavigationAction::BeginRunSwitcher(direction)) => {
                        self.begin_or_cycle_run_switcher(direction);
                    }
                    _ => match key {
                        "escape" => self.dispatch_navigation_action(NavigationAction::Cancel),
                        "tab" => self.begin_or_cycle_run_switcher(if modifiers.shift {
                            navigation::Direction::Reverse
                        } else {
                            navigation::Direction::Forward
                        }),
                        "enter" | "space" => {
                            self.dispatch_navigation_action(NavigationAction::CommitRunSwitcher);
                        }
                        _ => {}
                    },
                }
                window.prevent_default();
                cx.notify();
                return;
            }
            KeyRoute::UtilityOverlay => {
                // The settings page is a search surface: printable keys type
                // into its filter, so it gets its own handler.
                if self
                    .utility_overlay
                    .as_ref()
                    .is_some_and(|overlay| overlay.surface == UtilityOverlay::Settings)
                {
                    self.settings_page_key(event, window, cx);
                    return;
                }
                match key {
                    "escape" => self.dispatch_navigation_action(NavigationAction::Cancel),
                    "up" => self.dispatch_navigation_action(NavigationAction::OverlayPrevious),
                    "down" => self.dispatch_navigation_action(NavigationAction::OverlayNext),
                    "tab" if modifiers.shift => {
                        self.dispatch_navigation_action(NavigationAction::OverlayPrevious)
                    }
                    "tab" => self.dispatch_navigation_action(NavigationAction::OverlayNext),
                    "enter" | "space" | "right" => {
                        self.dispatch_navigation_action(NavigationAction::OverlayCommit);
                    }
                    "left" => self.reverse_overlay_selection(),
                    _ => {}
                }
                window.prevent_default();
                cx.notify();
                return;
            }
            KeyRoute::Palette => {
                self.palette_key(event, window, cx);
                return;
            }
            KeyRoute::Composer => {}
        }

        if key == "enter" && !modifiers.shift {
            let submitted = self.prompt.text().trim().to_string();
            if !submitted.is_empty() {
                self.prompt.clear();
                self.submit(&submitted);
            }
        } else if let Some(edit) = query_editor::edit_for(&event.keystroke) {
            self.apply_edit(edit, false, cx);
        }
        // Mirror into the shared state so the prompt renders from one place.
        let text = self.prompt.text().to_string();
        self.state().input = text;
        cx.notify();
    }

    /// Keys while the settings page owns the window. Escape unwinds one layer
    /// at a time — menu, then filter, then the page — and printable keys type
    /// into the search filter, which is the point of having one.
    fn settings_page_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let key = event.keystroke.key.as_str();
        let modifiers = event.keystroke.modifiers;
        match key {
            "escape" => {
                if self.settings_nav_open {
                    self.settings_nav_open = false;
                } else if !self.settings_query.is_empty() {
                    self.settings_query.clear();
                    self.reset_overlay_selection();
                } else {
                    self.dispatch_navigation_action(NavigationAction::Cancel);
                }
            }
            "up" => self.dispatch_navigation_action(NavigationAction::OverlayPrevious),
            "down" => self.dispatch_navigation_action(NavigationAction::OverlayNext),
            "tab" if modifiers.shift => {
                self.dispatch_navigation_action(NavigationAction::OverlayPrevious)
            }
            "tab" => self.dispatch_navigation_action(NavigationAction::OverlayNext),
            // Space types into the filter ("wall time"), so unlike the other
            // overlays only Enter and Right activate here.
            "enter" | "right" => self.dispatch_navigation_action(NavigationAction::OverlayCommit),
            "left" => self.reverse_overlay_selection(),
            "backspace" => {
                self.settings_query
                    .delete_backward(query_editor::Motion::Character);
                self.reset_overlay_selection();
            }
            _ => {
                if let Some(text) = event.keystroke.key_char.as_ref()
                    && !modifiers.platform
                    && !modifiers.control
                {
                    self.settings_query.insert(text);
                    self.reset_overlay_selection();
                }
            }
        }
        window.prevent_default();
        cx.notify();
    }

    /// Put text into the composer, replacing what was there. The welcome
    /// screen's starters use this: a suggestion that types itself is a
    /// starting point, not a submission.
    pub(crate) fn fill_prompt(&mut self, text: &str) {
        self.prompt.clear();
        self.prompt.insert(text);
        let mirrored = self.prompt.text().to_string();
        self.state().input = mirrored;
    }

    /// Fold a directory open or closed in the file viewer.
    pub(crate) fn files_toggle_dir(&mut self, path: std::path::PathBuf, cx: &mut Context<Self>) {
        if !self.files_expanded.remove(&path) {
            self.files_expanded.insert(path);
        }
        cx.notify();
    }

    /// Show one file. Read once here, not per frame — a render loop that
    /// touches the disk sixty times a second is how a viewer becomes a fan
    /// benchmark.
    pub(crate) fn files_select(
        &mut self,
        root: std::path::PathBuf,
        path: std::path::PathBuf,
        cx: &mut Context<Self>,
    ) {
        let body = files::read_file(&root, &path);
        self.files_selected = Some(path.clone());
        self.files_body = Some((path, body));
        cx.notify();
    }

    /// Make sure the review pane is visible; never close it by surprise.
    /// The working-tree chip uses this — a chip that TOGGLED would sometimes
    /// hide exactly the evidence it advertises.
    pub(crate) fn open_inspector(&mut self, cx: &mut Context<Self>) {
        if !self.layout.inspector_open {
            self.apply_layout_action(toolbar::ToolbarAction::ToggleInspector);
        }
        cx.notify();
    }

    /// Switch the right-hand pane between run evidence and the file browser.
    /// Selecting a tab the user cannot see is a state change without a cause,
    /// so this opens the inspector too.
    pub(crate) fn select_inspector_tab(
        &mut self,
        tab: inspector::InspectorTab,
        cx: &mut Context<Self>,
    ) {
        self.inspector_tab = tab;
        self.open_inspector(cx);
    }

    /// A filter or section change invalidates the old selection index.
    fn reset_overlay_selection(&mut self) {
        if let Some(overlay) = &mut self.utility_overlay {
            overlay.selected = 0;
        }
    }

    fn on_key_up(&mut self, event: &KeyUpEvent, _window: &mut Window, cx: &mut Context<Self>) {
        if self.run_switcher.is_some() && event.keystroke.key.as_str() == "control" {
            self.dispatch_navigation_action(NavigationAction::CommitRunSwitcher);
            cx.notify();
        }
    }

    /// Keys while the palette is open. Navigation first, then the text field,
    /// which handles selection, word/line motion, and the clipboard.
    fn palette_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let rows = self.palette_rows();
        let key = event.keystroke.key.as_str();
        let plain = !event.keystroke.modifiers.platform && !event.keystroke.modifiers.control;

        match key {
            "escape" => {
                self.palette = None;
                window.prevent_default();
                cx.notify();
                return;
            }
            "up" | "down" | "tab" | "enter" if plain => {
                let Some(palette) = &mut self.palette else {
                    return;
                };
                match key {
                    "up" | "tab" if key == "up" || event.keystroke.modifiers.shift => {
                        palette.selected = palette.selected.saturating_sub(1);
                    }
                    "down" | "tab" => {
                        palette.selected = (palette.selected + 1).min(rows.len().saturating_sub(1));
                    }
                    _ => {
                        let chosen = rows.get(palette.selected).map(|r| r.action.command.clone());
                        if let Some(command) = chosen {
                            self.invoke(command);
                        }
                    }
                }
                window.prevent_default();
                cx.notify();
                return;
            }
            _ => {}
        }

        if let Some(edit) = query_editor::edit_for(&event.keystroke) {
            self.apply_edit(edit, true, cx);
        }
        cx.notify();
    }

    /// Route one editor edit to the palette query or the prompt.
    fn apply_edit(&mut self, edit: query_editor::Edit, to_palette: bool, cx: &mut Context<Self>) {
        match edit {
            query_editor::Edit::Local(local) => {
                if to_palette {
                    if let Some(palette) = &mut self.palette
                        && palette.query.apply(local)
                    {
                        palette_query_changed(palette, true);
                    }
                } else {
                    self.prompt.apply(local);
                }
            }
            query_editor::Edit::Clipboard(clip) => {
                let editor: &mut query_editor::QueryEditor = if to_palette {
                    match &mut self.palette {
                        Some(palette) => &mut palette.query,
                        None => return,
                    }
                } else {
                    &mut self.prompt
                };
                match clip {
                    query_editor::ClipboardEdit::Copy => {
                        query_editor::copy_selection(editor, cx);
                    }
                    query_editor::ClipboardEdit::Cut => {
                        let changed = query_editor::cut_selection(editor, cx);
                        if to_palette
                            && changed
                            && let Some(palette) = &mut self.palette
                        {
                            palette_query_changed(palette, true);
                        }
                    }
                    query_editor::ClipboardEdit::Paste => {
                        let changed = cx
                            .read_from_clipboard()
                            .and_then(|item| item.text())
                            .is_some_and(|text| editor.insert(&text));
                        if to_palette
                            && changed
                            && let Some(palette) = &mut self.palette
                        {
                            palette_query_changed(palette, true);
                        }
                    }
                }
            }
        }
    }

    fn sidebar_resize_handle(
        &self,
        left: f32,
        top: f32,
        height: f32,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        deferred(
            div()
                .id("sidebar-resize-handle")
                .absolute()
                .left(px(left + RESIZE_HIT_OFFSET))
                .top(px(top))
                .w(px(RESIZE_HIT_TARGET))
                .h(px(height))
                .cursor(CursorStyle::ResizeLeftRight)
                .occlude()
                .on_drag(DraggedSidebarEdge, |edge, _, _, cx| {
                    cx.stop_propagation();
                    cx.new(|_| *edge)
                })
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, event: &gpui::MouseDownEvent, _, cx| {
                        this.sidebar_resize_origin =
                            Some((f32::from(event.position.x), this.layout.columns().sidebar));
                        cx.stop_propagation();
                    }),
                )
                .on_mouse_up(
                    MouseButton::Left,
                    cx.listener(|this, event: &gpui::MouseUpEvent, _, cx| {
                        if event.click_count == 2 {
                            this.layout.reset_sidebar();
                        }
                        this.finish_sidebar_resize(cx);
                    }),
                )
                .on_mouse_up_out(
                    MouseButton::Left,
                    cx.listener(|this, _, _, cx| this.finish_sidebar_resize(cx)),
                ),
        )
        .into_any_element()
    }

    fn workbench_resize_handle(&self, top: f32, cx: &mut Context<Self>) -> AnyElement {
        deferred(
            div()
                .id("workbench-resize-handle")
                .absolute()
                .top(px(top - 4.0))
                .left(px(0.0))
                .h(px(RESIZE_HIT_TARGET))
                .w_full()
                .cursor(CursorStyle::ResizeUpDown)
                .occlude()
                .on_drag(DraggedWorkbenchEdge, |edge, _, _, cx| {
                    cx.stop_propagation();
                    cx.new(|_| *edge)
                })
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, event: &gpui::MouseDownEvent, _, cx| {
                        let coordinator = this
                            .layout
                            .rows(this.workbench_available_height)
                            .coordinator;
                        this.workbench_resize_origin =
                            Some((f32::from(event.position.y), coordinator));
                        cx.stop_propagation();
                    }),
                )
                .on_mouse_up(
                    MouseButton::Left,
                    cx.listener(|this, event: &gpui::MouseUpEvent, _, cx| {
                        if event.click_count == 2 {
                            this.layout.reset_workbench();
                        }
                        this.finish_workbench_resize(cx);
                    }),
                )
                .on_mouse_up_out(
                    MouseButton::Left,
                    cx.listener(|this, _, _, cx| this.finish_workbench_resize(cx)),
                ),
        )
        .into_any_element()
    }

    fn inspector_resize_handle(
        &self,
        left: f32,
        top: f32,
        height: f32,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        deferred(
            div()
                .id("inspector-resize-handle")
                .absolute()
                .left(px(left + RESIZE_HIT_OFFSET))
                .top(px(top))
                .w(px(RESIZE_HIT_TARGET))
                .h(px(height))
                .cursor(CursorStyle::ResizeLeftRight)
                .occlude()
                .on_drag(DraggedInspectorEdge, |edge, _, _, cx| {
                    cx.stop_propagation();
                    cx.new(|_| *edge)
                })
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, event: &gpui::MouseDownEvent, _, cx| {
                        this.inspector_resize_origin =
                            Some((f32::from(event.position.x), this.layout.columns().inspector));
                        cx.stop_propagation();
                    }),
                )
                .on_mouse_up(
                    MouseButton::Left,
                    cx.listener(|this, event: &gpui::MouseUpEvent, _, cx| {
                        if event.click_count == 2 {
                            this.layout.reset_inspector();
                        }
                        this.finish_inspector_resize(cx);
                    }),
                )
                .on_mouse_up_out(
                    MouseButton::Left,
                    cx.listener(|this, _, _, cx| this.finish_inspector_resize(cx)),
                ),
        )
        .into_any_element()
    }

    fn drag_sidebar_resize(&mut self, pointer_x: f32, cx: &mut Context<Self>) {
        let Some((origin_x, base_width)) = self.sidebar_resize_origin else {
            return;
        };
        self.layout
            .resize_sidebar_for_body(base_width + pointer_x - origin_x, self.body_available_width);
        cx.notify();
    }

    fn drag_workbench_resize(&mut self, pointer_y: f32, cx: &mut Context<Self>) {
        let Some((origin_y, base_height)) = self.workbench_resize_origin else {
            return;
        };
        self.layout.resize_coordinator(
            base_height + pointer_y - origin_y,
            self.workbench_available_height,
        );
        cx.notify();
    }

    fn drag_inspector_resize(&mut self, pointer_x: f32, cx: &mut Context<Self>) {
        let Some((origin_x, base_width)) = self.inspector_resize_origin else {
            return;
        };
        self.layout.resize_inspector_for_body(
            base_width - pointer_x + origin_x,
            self.body_available_width,
        );
        cx.notify();
    }

    fn finish_sidebar_resize(&mut self, cx: &mut Context<Self>) {
        self.sidebar_resize_origin = None;
        cx.notify();
    }

    fn finish_workbench_resize(&mut self, cx: &mut Context<Self>) {
        self.workbench_resize_origin = None;
        cx.notify();
    }

    fn finish_inspector_resize(&mut self, cx: &mut Context<Self>) {
        self.inspector_resize_origin = None;
        cx.notify();
    }

    fn finish_all_resizes(&mut self, cx: &mut Context<Self>) {
        self.sidebar_resize_origin = None;
        self.workbench_resize_origin = None;
        self.inspector_resize_origin = None;
        cx.notify();
    }

    fn resize_shield(&self, cx: &mut Context<Self>) -> AnyElement {
        let vertical = self.workbench_resize_origin.is_some();
        deferred(
            div()
                .id("active-resize-shield")
                .absolute()
                .inset_0()
                .cursor(if vertical {
                    CursorStyle::ResizeUpDown
                } else {
                    CursorStyle::ResizeLeftRight
                })
                .occlude()
                .on_mouse_up(
                    MouseButton::Left,
                    cx.listener(|this, _, _, cx| {
                        this.finish_all_resizes(cx);
                        cx.stop_propagation();
                    }),
                )
                .on_mouse_up_out(
                    MouseButton::Left,
                    cx.listener(|this, _, _, cx| this.finish_all_resizes(cx)),
                ),
        )
        .into_any_element()
    }
}

#[cfg(test)]
fn toolbar_shortcut(key: &str, modifiers: gpui::Modifiers) -> Option<toolbar::ToolbarAction> {
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

    match key {
        "b" if platform_only => Some(toolbar::ToolbarAction::ToggleSidebar),
        "j" if platform_only => Some(toolbar::ToolbarAction::ToggleExecution),
        "k" if platform_only => Some(toolbar::ToolbarAction::OpenPalette),
        "d" | "D" if platform_shift_only => Some(toolbar::ToolbarAction::ToggleInspector),
        _ => None,
    }
}

fn key_route(
    key: &str,
    modifiers: gpui::Modifiers,
    palette_open: bool,
    utility_open: bool,
    switcher_open: bool,
) -> KeyRoute {
    if switcher_open {
        KeyRoute::RunSwitcher
    } else if utility_open {
        KeyRoute::UtilityOverlay
    } else if palette_open {
        KeyRoute::Palette
    } else if let Some(action) = navigation::shortcut_for_key(key, modifiers) {
        KeyRoute::Global(action)
    } else {
        KeyRoute::Composer
    }
}

fn palette_query_changed(palette: &mut PaletteState, changed: bool) {
    if changed {
        palette.selected = 0;
    }
}

/// Open the shell.
pub fn run() {
    launch_window(DaemonClient::spawn());
}

/// Open the real shell against a deterministic, in-memory fixture.
pub fn run_preview() {
    launch_window(DaemonClient::preview(preview::populated()));
}

fn launch_window(client: DaemonClient) {
    gpui_platform::application()
        .with_assets(icon::IconAssets)
        .run(|cx: &mut App| {
            cx.set_app_identity("dev.autoharness.app", "AutoHarness");
            cx.on_system_notification_response(|response, cx| {
                notify::accept_system_response(&response.tag);
                cx.activate(true);
            });
            let bounds = Bounds::centered(None, size(px(1280.0), px(820.0)), cx);
            let mut client = Some(client);
            cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    titlebar: Some(gpui::TitlebarOptions {
                        title: Some("AutoHarness".into()),
                        appears_transparent: true,
                        // AppKit's native 8pt origin plus a +12 x / -6 y nudge.
                        traffic_light_position: Some(gpui::point(px(20.0), px(14.0))),
                    }),
                    ..Default::default()
                },
                move |window, cx| {
                    let client = client.take().expect("shell client is consumed once");
                    let shell = cx.new(|cx| Shell::with_client(client, cx));
                    // Take the handle first: `window.focus` needs `cx` mutably.
                    let focus = shell.read(cx).focus.clone();
                    window.focus(&focus, cx);
                    shell
                },
            )
            .expect("open window");
            cx.activate(true);
        });
}

fn advance_seam(
    slide: &mut Option<motion::SeamSlide>,
    seam: &mut f32,
    target: f32,
    now: std::time::Instant,
) {
    match *slide {
        Some(active) if !active.is_done(now) => {
            *seam = active.value_at(target, now);
        }
        Some(_) => {
            *slide = None;
            *seam = target;
        }
        None => *seam = target,
    }
}

impl Render for Shell {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.quit_for_update {
            self.quit_for_update = false;
            cx.quit();
        }
        if let Some(status) = self.update_manager.poll() {
            self.state().update_status = status;
        }
        // Transient surfaces are composites: focus returns to the root while
        // arrows/Tab select an active descendant. This traps modal focus and
        // prevents an underlying button from receiving the matching key-up.
        if (self.palette.is_some() || self.utility_overlay.is_some() || self.run_switcher.is_some())
            && !self.focus.is_focused(window)
        {
            window.focus(&self.focus, cx);
        }
        let should_check_automatically =
            !self.client.is_preview() && !self.update_manager.automatic_attempted() && {
                let state = self.state();
                state.connected
                    && !state.settings.loading
                    && state.settings.values.automatic_update_checks
                    && matches!(state.update_status, update::UpdateStatus::Idle)
            };
        if should_check_automatically {
            self.update_manager.mark_automatic_attempted();
            self.start_update_check();
        }

        notify::drain_platform(cx);
        for action in notify::drain_activation_actions() {
            match action {
                attention::AttentionAction::SelectRun(run_id) => {
                    self.select_run(&run_id);
                    self.utility_overlay = None;
                    self.palette = None;
                    self.run_switcher = None;
                }
            }
        }
        let menu_rollup = {
            let state = self.state();
            menubar::rollup(&state.attention)
        };
        self.menu_bar.update(&menu_rollup);

        let inner = window.inner_window_bounds().get_bounds();
        let layout = self.layout;
        let settled = shell_body_metrics(
            f32::from(inner.size.width),
            f32::from(inner.size.height),
            layout,
        );
        self.body_available_width = settled.available_width;
        self.workbench_available_height = settled.workbench_height;

        let sidebar_target = settled.columns.sidebar;
        let inspector_target = settled.columns.inspector;
        // An execution pane with nothing in it collapses to nothing. Taking
        // this off the layout flag alone is what makes it actually close: the
        // seam animates toward its target, so a target that ignored content
        // just slid the empty pane straight back open.
        let execution_target = if workbench::has_anything_to_show(&self.state()) {
            settled.rows.execution
        } else {
            0.0
        };
        let now = std::time::Instant::now();
        let reduce_motion = cx.reduce_motion();
        if !self.motion_initialized {
            self.motion_initialized = true;
            self.motion_sidebar_open = layout.sidebar_open;
            self.motion_execution_open = layout.execution_open;
            self.motion_inspector_open = layout.inspector_open;
            self.sidebar_seam = sidebar_target;
            self.execution_seam = execution_target;
            self.inspector_seam = inspector_target;
        } else if reduce_motion {
            self.motion_sidebar_open = layout.sidebar_open;
            self.motion_execution_open = layout.execution_open;
            self.motion_inspector_open = layout.inspector_open;
            self.sidebar_slide = None;
            self.execution_slide = None;
            self.inspector_slide = None;
            self.sidebar_seam = sidebar_target;
            self.execution_seam = execution_target;
            self.inspector_seam = inspector_target;
        } else {
            if self.motion_sidebar_open != layout.sidebar_open {
                self.motion_sidebar_open = layout.sidebar_open;
                self.sidebar_slide = Some(motion::SeamSlide::begin(self.sidebar_seam, now));
            }
            if self.motion_execution_open != layout.execution_open {
                self.motion_execution_open = layout.execution_open;
                self.execution_slide = Some(motion::SeamSlide::begin(self.execution_seam, now));
            }
            if self.motion_inspector_open != layout.inspector_open {
                self.motion_inspector_open = layout.inspector_open;
                self.inspector_slide = Some(motion::SeamSlide::begin(self.inspector_seam, now));
            }
            advance_seam(
                &mut self.sidebar_slide,
                &mut self.sidebar_seam,
                sidebar_target,
                now,
            );
            advance_seam(
                &mut self.execution_slide,
                &mut self.execution_seam,
                execution_target,
                now,
            );
            advance_seam(
                &mut self.inspector_slide,
                &mut self.inspector_seam,
                inspector_target,
                now,
            );
            if self.sidebar_slide.is_some()
                || self.execution_slide.is_some()
                || self.inspector_slide.is_some()
            {
                window.request_animation_frame();
            }
        }

        let body_height = settled.available_height;
        let sidebar_visible = layout.sidebar_open || self.sidebar_seam > 0.5;
        // A pane whose whole content is "Execution appears when a run is
        // routed" is a permanent apology taking a third of the window.
        let execution_has_content = workbench::has_anything_to_show(&self.state());
        let execution_visible =
            (layout.execution_open && execution_has_content) || self.execution_seam > 0.5;
        let inspector_visible = layout.inspector_open || self.inspector_seam > 0.5;
        let coordinator_height = if execution_visible {
            (body_height - MAJOR_SEAM - self.execution_seam).max(0.0)
        } else {
            body_height
        };
        let center_width = (settled.available_width - self.sidebar_seam - self.inspector_seam)
            .max(0.0)
            .min(settled.available_width);
        let body_content_top = 0.0;
        let sidebar_handle_x = self.sidebar_seam;
        let inspector_handle_x = self.sidebar_seam + center_width;
        let overlay_kind = self.utility_overlay.clone();
        let execution_mode = self.execution_mode;
        let execution_transition_generation = self.execution_transition_generation;
        let execution_tab_direction = self.execution_tab_direction;
        let selected_graph_node = self.selected_graph_node.clone();
        let prompt_active = self.palette.is_none()
            && self.utility_overlay.is_none()
            && self.run_switcher.is_none()
            && !self.model_picker_open;
        let preview = self.client.is_preview();
        let (title, side, center, right, first_run, utility, banner, recovery) = {
            let state = self.state();
            (
                toolbar::view(
                    &state,
                    layout,
                    layout.inspector_open && self.inspector_tab == inspector::InspectorTab::Files,
                    cx,
                ),
                sidebar_visible.then(|| {
                    sidebar::view(&state, self.sidebar_seam, self.new_run_picker_open, cx)
                }),
                div()
                    .relative()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w(px(layout::MIN_CENTER_WIDTH.min(self.body_available_width)))
                    .min_h(px(0.0))
                    .gap(px(MAJOR_SEAM))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_none()
                            .h(px(coordinator_height))
                            .min_h(px(0.0))
                            .child(coordinator::view(
                                &state,
                                &self.prompt,
                                prompt_active,
                                self.model_picker_open,
                                self.model_picker_tab.as_deref(),
                                self.model_picker_query.text(),
                                cx,
                            )),
                    )
                    .children(execution_visible.then(|| {
                        div()
                            .flex()
                            .flex_col()
                            .flex_none()
                            .h(px(self.execution_seam))
                            .min_h(px(0.0))
                            .child(workbench::view(
                                &state,
                                execution_mode,
                                center_width,
                                execution_transition_generation,
                                execution_tab_direction,
                                selected_graph_node.as_deref(),
                                cx,
                            ))
                    }))
                    .children(
                        (layout.execution_open && self.execution_slide.is_none())
                            .then(|| self.workbench_resize_handle(coordinator_height, cx)),
                    )
                    .into_any_element(),
                inspector_visible.then(|| {
                    inspector::view(
                        &state,
                        self.inspector_seam,
                        self.inspector_tab,
                        inspector::FilesFocus {
                            expanded: &self.files_expanded,
                            selected: self.files_selected.as_deref(),
                            body: self.files_body.as_ref(),
                        },
                        cx,
                    )
                }),
                // Until setup can take an objective, the first-run page IS
                // the app; the palette and utility surfaces still layer over
                // it, so nothing modal is trapped underneath.
                onboarding::should_show(&state).then(|| onboarding::first_run_page(&state, cx)),
                overlay_kind.as_ref().map(|overlay| {
                    if overlay.surface == UtilityOverlay::Settings {
                        let (query_display, _) = self.settings_query.display("▌");
                        settings_page::view(
                            overlay,
                            settings_page::PageNav {
                                section: self.settings_section,
                                nav_open: self.settings_nav_open,
                                query: self.settings_query.text(),
                                query_display: &query_display,
                            },
                            &state,
                            layout,
                            cx,
                        )
                    } else {
                        utility_overlay(overlay.clone(), &state, layout, &self.mru_run_ids, cx)
                    }
                }),
                // The banner is the fallback channel: it appears for every
                // live alert whether or not the operating system took one.
                (overlay_kind.is_none())
                    .then(|| state.attention.banner.clone())
                    .flatten()
                    .map(|item| attention_banner(&item, cx)),
                (!preview && !state.connected)
                    .then(|| connection_recovery_banner(&state.status, self.sidebar_seam, cx)),
            )
        };
        let rows = self.palette_rows();
        let overlay = self
            .palette
            .as_ref()
            // The palette shows its own caret, the same as the prompt.
            .map(|p| (p.query.display("▌").0, p.query.is_empty(), p.selected))
            .map(|(query, empty, selected)| palette_overlay(&query, empty, selected, &rows, cx));
        let switcher = self.run_switcher.as_ref().map(|switcher| {
            let state = self.state();
            run_switcher_overlay(switcher, &state, cx)
        });

        div()
            .id("autoharness-root")
            .role(Role::Application)
            .aria_label("AutoHarness coding agent control center")
            .tab_group()
            .key_context("AutoHarness")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::on_key))
            .on_key_up(cx.listener(Self::on_key_up))
            .on_drag_move(
                cx.listener(|this, event: &DragMoveEvent<DraggedSidebarEdge>, _, cx| {
                    this.drag_sidebar_resize(f32::from(event.event.position.x), cx);
                }),
            )
            .on_drag_move(cx.listener(
                |this, event: &DragMoveEvent<DraggedWorkbenchEdge>, _, cx| {
                    this.drag_workbench_resize(f32::from(event.event.position.y), cx);
                },
            ))
            .on_drag_move(cx.listener(
                |this, event: &DragMoveEvent<DraggedInspectorEdge>, _, cx| {
                    this.drag_inspector_resize(f32::from(event.event.position.x), cx);
                },
            ))
            .flex()
            .flex_col()
            .size_full()
            .bg(Colors::BACKGROUND)
            .text_color(Colors::text(Surface::Content, Tone::Primary))
            .child(title)
            .child(
                div()
                    .relative()
                    .flex()
                    .flex_1()
                    .min_h(px(0.0))
                    .gap(px(0.0))
                    .p(px(0.0))
                    .children(side)
                    .child(center)
                    .children(right)
                    .children(first_run)
                    .children(switcher)
                    .children(
                        (layout.sidebar_open && self.sidebar_slide.is_none()).then(|| {
                            self.sidebar_resize_handle(
                                sidebar_handle_x,
                                body_content_top,
                                body_height,
                                cx,
                            )
                        }),
                    )
                    .children(
                        (layout.inspector_open && self.inspector_slide.is_none()).then(|| {
                            self.inspector_resize_handle(
                                inspector_handle_x,
                                body_content_top,
                                body_height,
                                cx,
                            )
                        }),
                    )
                    .children(overlay)
                    .children(utility)
                    .children(banner)
                    .children(recovery)
                    .children(
                        (self.sidebar_resize_origin.is_some()
                            || self.workbench_resize_origin.is_some()
                            || self.inspector_resize_origin.is_some())
                        .then(|| self.resize_shield(cx)),
                    ),
            )
    }
}

pub(crate) fn shell_body_metrics(
    window_width: f32,
    window_height: f32,
    layout: layout::ShellLayout,
) -> ShellBodyMetrics {
    let available_width = window_width.max(0.0);
    let available_height = (window_height - Metrics::TITLE_BAR).max(0.0);
    let workbench_height = if layout.execution_open {
        (available_height - MAJOR_SEAM).max(0.0)
    } else {
        available_height
    };
    let columns = layout.columns_for_body(available_width);
    let rows = layout.rows(workbench_height);
    let center_width = (available_width - columns.sidebar - columns.inspector)
        .max(0.0)
        .min(available_width);
    ShellBodyMetrics {
        available_width,
        available_height,
        workbench_height,
        columns,
        rows,
        center_width,
    }
}

pub(crate) fn current_run_tip_ids_for_state(
    state: &UiState,
    mru_run_ids: &[String],
) -> Vec<String> {
    let mut ids = Vec::new();
    for run_id in mru_run_ids {
        let Some(root) = state.thread_root(run_id) else {
            continue;
        };
        let Some(tip) = state.tip_of(&root.id) else {
            continue;
        };
        if !ids.contains(&tip.id) {
            ids.push(tip.id.clone());
        }
    }
    for run in state
        .runs
        .iter()
        .filter(|run| run.parent_run_id.is_none())
        .rev()
    {
        if let Some(tip) = state.tip_of(&run.id)
            && !ids.contains(&tip.id)
        {
            ids.push(tip.id.clone());
        }
    }
    ids
}

pub(crate) fn apply_layout_toolbar_action(
    layout: &mut layout::ShellLayout,
    action: toolbar::ToolbarAction,
) -> bool {
    match action {
        toolbar::ToolbarAction::ToggleSidebar => {
            layout.sidebar_open = !layout.sidebar_open;
            true
        }
        toolbar::ToolbarAction::ToggleExecution => {
            layout.execution_open = !layout.execution_open;
            true
        }
        toolbar::ToolbarAction::ToggleInspector => {
            layout.inspector_open = !layout.inspector_open;
            true
        }
        toolbar::ToolbarAction::ResetLayout => {
            *layout = layout::ShellLayout::default();
            true
        }
        _ => false,
    }
}

/// A repository name from an objective: its first four words, slugged. The
/// daemon sanitizes again and picks a free directory; this only has to be
/// predictable enough that the status line and the folder agree.
pub(crate) fn repository_name_for(objective: &str) -> String {
    let words: Vec<&str> = objective
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .filter(|word| !word.is_empty())
        .take(4)
        .collect();
    if words.is_empty() {
        "new-project".into()
    } else {
        words.join("-").to_ascii_lowercase()
    }
}

/// The engines `/attempts` races: ready AND generally available, with each
/// one's persisted model and effort defaults. A gated engine never enters a
/// race — a comparison including an engine the product does not offer is a
/// comparison the user cannot act on.
pub(crate) fn attempt_engines(state: &UiState) -> Vec<(String, Option<String>, Option<String>)> {
    state
        .engines
        .iter()
        .filter(|engine| engine.ready && client::engine_generally_available(&engine.name))
        .map(|engine| {
            (
                engine.name.clone(),
                state
                    .settings
                    .values
                    .default_models
                    .get(&engine.name)
                    .cloned(),
                state
                    .settings
                    .values
                    .default_reasoning_efforts
                    .get(&engine.name)
                    .cloned(),
            )
        })
        .collect()
}

pub(crate) fn utility_overlay_rows(
    kind: UtilityOverlay,
    state: &UiState,
    layout: layout::ShellLayout,
    mru_run_ids: &[String],
) -> Vec<UtilityOverlayRow> {
    let detail = state.selected_detail();
    match kind {
        UtilityOverlay::Overview => overview_rows(state, mru_run_ids),
        UtilityOverlay::History => history_overlay_rows(state),
        UtilityOverlay::Worktrees => {
            let mut rows = vec![
                row(
                    "Selected",
                    detail
                        .and_then(|detail| detail.worktree.as_ref())
                        .and_then(|worktree| worktree.path.clone())
                        .or_else(|| state.worktree.clone())
                        .unwrap_or_else(|| "No worktree for this run".into()),
                    None,
                ),
                row(
                    "Branch",
                    detail
                        .and_then(|detail| detail.worktree.as_ref())
                        .and_then(|worktree| worktree.branch.clone())
                        .unwrap_or_else(|| "unavailable".into()),
                    None,
                ),
            ];
            rows.extend(
                worktrees::worktree_rows(state)
                    .into_iter()
                    .map(|entry| worktree_row(entry.label, entry.value, entry.action)),
            );
            rows
        }
        UtilityOverlay::Queue => queue_overlay_rows(state),
        UtilityOverlay::Notifications => notification_rows(state),
        UtilityOverlay::Settings => settings_page::visible_rows(state, layout, None, ""),
        // The file viewer is pointer-driven; it has no generic action rows.
        UtilityOverlay::Files => Vec::new(),
    }
}

pub(crate) fn row(
    label: impl Into<String>,
    value: impl Into<String>,
    action: Option<toolbar::ToolbarAction>,
) -> UtilityOverlayRow {
    UtilityOverlayRow {
        label: label.into(),
        value: value.into(),
        action,
        select_run_id: None,
        history_action: None,
        settings_action: None,
        update_action: None,
        worktree_action: None,
        queue_action: None,
        mark: None,
    }
}

pub(crate) fn settings_row(
    label: impl Into<String>,
    value: impl Into<String>,
    action: settings::SettingsAction,
) -> UtilityOverlayRow {
    UtilityOverlayRow {
        label: label.into(),
        value: value.into(),
        action: None,
        select_run_id: None,
        history_action: None,
        settings_action: Some(action),
        update_action: None,
        worktree_action: None,
        queue_action: None,
        mark: None,
    }
}

fn worktree_row(
    label: impl Into<String>,
    value: impl Into<String>,
    action: Option<worktrees::WorktreeAction>,
) -> UtilityOverlayRow {
    UtilityOverlayRow {
        label: label.into(),
        value: value.into(),
        action: None,
        select_run_id: None,
        history_action: None,
        settings_action: None,
        update_action: None,
        worktree_action: action,
        queue_action: None,
        mark: None,
    }
}

fn select_row(
    label: impl Into<String>,
    value: impl Into<String>,
    run_id: impl Into<String>,
) -> UtilityOverlayRow {
    UtilityOverlayRow {
        label: label.into(),
        value: value.into(),
        action: None,
        select_run_id: Some(run_id.into()),
        history_action: None,
        settings_action: None,
        update_action: None,
        worktree_action: None,
        queue_action: None,
        mark: None,
    }
}

fn history_row(
    label: impl Into<String>,
    value: impl Into<String>,
    action: Option<HistoryOverlayAction>,
) -> UtilityOverlayRow {
    UtilityOverlayRow {
        label: label.into(),
        value: value.into(),
        action: None,
        select_run_id: None,
        history_action: action,
        settings_action: None,
        update_action: None,
        worktree_action: None,
        queue_action: None,
        mark: None,
    }
}

fn queue_row(
    label: impl Into<String>,
    value: impl Into<String>,
    action: queue::QueueAction,
) -> UtilityOverlayRow {
    UtilityOverlayRow {
        label: label.into(),
        value: value.into(),
        action: None,
        select_run_id: None,
        history_action: None,
        settings_action: None,
        update_action: None,
        worktree_action: None,
        queue_action: Some(action),
        mark: None,
    }
}

pub(crate) fn update_row(
    label: impl Into<String>,
    value: impl Into<String>,
    action: update::UpdateAction,
) -> UtilityOverlayRow {
    UtilityOverlayRow {
        label: label.into(),
        value: value.into(),
        action: None,
        select_run_id: None,
        history_action: None,
        settings_action: None,
        update_action: Some(action),
        worktree_action: None,
        queue_action: None,
        mark: None,
    }
}

fn queue_overlay_rows(state: &UiState) -> Vec<UtilityOverlayRow> {
    use autoharness_protocol::params::{QueueKind, QueueState};

    let waiting: Vec<_> = state.queue.waiting().collect();
    let mut rows = vec![
        row(
            "Waiting",
            format!(
                "{} objectives · {} total",
                state.queue.pending_objectives(),
                waiting.len()
            ),
            None,
        ),
        queue_row(
            "Refresh",
            "Reload daemon order",
            queue::QueueAction::Refresh,
        ),
    ];
    let mut previous_objective: Option<String> = None;
    for (index, item) in waiting.into_iter().enumerate() {
        let kind = match item.kind {
            QueueKind::Objective => "Objective",
            QueueKind::Steering => "Follow-up",
        };
        rows.push(select_row(
            format!("{kind} {}", index + 1),
            item.content.clone(),
            item.run_id.clone(),
        ));
        if item.kind == QueueKind::Objective {
            if let Some(before_item_id) = previous_objective.clone() {
                rows.push(queue_row(
                    "Move up",
                    item.content.clone(),
                    queue::QueueAction::MoveBefore {
                        item_id: item.id.clone(),
                        before_item_id,
                    },
                ));
            }
            previous_objective = Some(item.id.clone());
        }
        if item.state == QueueState::Pending {
            rows.push(queue_row(
                "Cancel",
                item.content.clone(),
                queue::QueueAction::Cancel(item.id.clone()),
            ));
        }
    }
    if state.queue.loading {
        rows.push(row("Queue", "Loading authoritative order…", None));
    }
    if let Some(error) = state.queue.error.as_deref() {
        rows.push(row("Queue error", error, None));
    }
    if state.queue.items.is_empty() && !state.queue.loading {
        rows.push(row("Queue", "Nothing is waiting", None));
    }
    rows
}

pub(crate) fn utility_overlay_keyboard_actions(
    rows: &[UtilityOverlayRow],
) -> Vec<UtilityOverlayAction> {
    rows.iter().filter_map(row_action).collect()
}

/// One place decides whether a row is actionable, so keyboard traversal and
/// pointer hit targets can never disagree about which rows are live.
pub(crate) fn row_action(row: &UtilityOverlayRow) -> Option<UtilityOverlayAction> {
    if let Some(action) = row.action {
        return Some(UtilityOverlayAction::Toolbar(action));
    }
    if let Some(run_id) = row.select_run_id.as_ref() {
        return Some(UtilityOverlayAction::SelectRun(run_id.clone()));
    }
    if let Some(action) = row.history_action.clone() {
        return Some(UtilityOverlayAction::History(action));
    }
    if let Some(action) = row.settings_action {
        return Some(UtilityOverlayAction::Settings(action));
    }
    if let Some(action) = row.update_action {
        return Some(UtilityOverlayAction::Update(action));
    }
    if let Some(action) = row.worktree_action.clone() {
        return Some(UtilityOverlayAction::Worktree(action));
    }
    row.queue_action.clone().map(UtilityOverlayAction::Queue)
}

#[cfg(test)]
pub(crate) fn dispatch_utility_overlay_keyboard_action(
    state: &mut UiState,
    layout: &mut layout::ShellLayout,
    action: UtilityOverlayAction,
) -> bool {
    match action {
        UtilityOverlayAction::Toolbar(action) => apply_layout_toolbar_action(layout, action),
        UtilityOverlayAction::SelectRun(run_id) => {
            state.select_run(&run_id);
            true
        }
        UtilityOverlayAction::History(_) => true,
        UtilityOverlayAction::Settings(action) => {
            settings::apply_settings_action(state, action).is_some()
        }
        UtilityOverlayAction::Update(_) => true,
        UtilityOverlayAction::Worktree(action) => {
            worktrees::apply_worktree_action(state, action);
            true
        }
        UtilityOverlayAction::Queue(action) => {
            queue::apply_action(state, action);
            true
        }
    }
}

fn overview_rows(state: &UiState, mru_run_ids: &[String]) -> Vec<UtilityOverlayRow> {
    let model = overview::model(state, mru_run_ids);
    if let Some(reason) = model.empty_reason {
        return vec![row("State", reason, None)];
    }
    let mut rows = vec![row(
        "Counts",
        format!(
            "{} needs you · {} running · {} done",
            model.counts.needs_you, model.counts.running, model.counts.done
        ),
        None,
    )];
    rows.extend(model.rows.into_iter().map(|run| {
        select_row(
            run.status,
            format!(
                "{} · {} · {} · {} · {}",
                run.project, run.engine, run.branch, run.check, run.usage
            ),
            run.run_id,
        )
    }));
    rows
}

fn history_overlay_rows(state: &UiState) -> Vec<UtilityOverlayRow> {
    let mut rows = crate::history::grouped_rows(state)
        .into_iter()
        .map(|row| {
            let action = match (
                row.select_run_id.clone(),
                row.adopt_provider,
                row.adopt_source_id,
            ) {
                (Some(run_id), _, _) => return select_row(row.label, row.value, run_id),
                (None, Some(provider), Some(source_id)) => Some(HistoryOverlayAction::Adopt {
                    provider,
                    source_id,
                }),
                _ => None,
            };
            history_row(row.label, row.value, action)
        })
        .collect::<Vec<_>>();
    if state.history.loading {
        rows.push(history_row("Loading", "Scanning provider history…", None));
    }
    if let Some(error) = state.history.error.as_deref() {
        rows.push(history_row("Error", error, None));
    }
    for diagnostic in &state.history.diagnostics {
        rows.push(history_row("Diagnostic", diagnostic.clone(), None));
    }
    if state.history.next_cursor.is_some() {
        rows.push(history_row(
            "Load more",
            "Fetch the next page",
            Some(HistoryOverlayAction::NextPage),
        ));
    }
    rows.push(history_row(
        "Refresh history",
        "Rescan Codex and Claude metadata",
        Some(HistoryOverlayAction::Refresh),
    ));
    rows
}

/// The Notifications panel: what has asked for the user, newest first.
///
/// Selecting an item is the only action, and it is the same typed command a
/// notification may carry. Nothing here approves, cancels, or retries.
fn notification_rows(state: &UiState) -> Vec<UtilityOverlayRow> {
    let attention = &state.attention;
    let mut rows = vec![
        row("Unseen", attention.unseen().to_string(), None),
        row(
            "Notifications",
            format!(
                "{} · sounds {}",
                on_off_label(state.settings.values.notifications_enabled),
                on_off_label(state.settings.values.sounds_enabled)
            ),
            None,
        ),
        row("Status", state.status.clone(), None),
    ];
    for item in attention.items.iter().rev() {
        let marker = if item.seen { "" } else { " ·  unseen" };
        let label = format!("{}{marker}", item.kind.label());
        match &item.action {
            Some(attention::AttentionAction::SelectRun(run_id)) => {
                rows.push(select_row(label, item.detail.clone(), run_id.clone()));
            }
            None => rows.push(row(label, item.detail.clone(), None)),
        }
    }
    if attention.items.is_empty() {
        rows.push(row("Attention", "Nothing needs you", None));
    }
    rows
}

fn on_off_label(value: bool) -> &'static str {
    if value { "on" } else { "off" }
}

/// How much room a utility surface needs.
///
/// Every one of these was a 360x420 popover pinned to the top-right corner,
/// including Settings, which has four tabs of preferences, and Worktrees,
/// which lists checkouts with paths. Content that is a page was being read
/// through a letterbox. The small, glanceable ones stay where they are.
struct SurfaceGeometry {
    width: f32,
    max_height: f32,
    /// Centred surfaces read as the thing you are doing; corner ones read as
    /// something you glanced at.
    centered: bool,
}

fn surface_geometry(kind: UtilityOverlay) -> SurfaceGeometry {
    match kind {
        UtilityOverlay::Settings | UtilityOverlay::Worktrees | UtilityOverlay::History => {
            SurfaceGeometry {
                width: 720.0,
                max_height: 640.0,
                centered: true,
            }
        }
        UtilityOverlay::Files => SurfaceGeometry {
            width: 860.0,
            max_height: 640.0,
            centered: true,
        },
        UtilityOverlay::Overview | UtilityOverlay::Queue | UtilityOverlay::Notifications => {
            SurfaceGeometry {
                width: 360.0,
                max_height: 420.0,
                centered: false,
            }
        }
    }
}

fn utility_overlay(
    overlay: OverlayState,
    state: &UiState,
    layout: layout::ShellLayout,
    mru_run_ids: &[String],
    cx: &mut Context<Shell>,
) -> AnyElement {
    let kind = overlay.surface;
    let geometry = surface_geometry(kind);
    let rows = utility_overlay_rows(kind, state, layout, mru_run_ids);
    let mut selectable_index = 0usize;
    div()
        .id(ElementId::Name(
            format!("utility-overlay-{}", kind.title()).into(),
        ))
        .role(Role::Dialog)
        .aria_label(kind.title())
        .aria_description(
            "Use Tab or arrow keys to move, Enter or Space to activate, Escape to close",
        )
        .absolute()
        .when(geometry.centered, |surface| {
            // A page, not a note pinned to the corner: centred, so it reads as
            // the thing you are doing rather than something hovering beside it.
            surface
                .top(px(56.0))
                .left(px(0.0))
                .right(px(0.0))
                .bottom(px(56.0))
                .flex()
                .justify_center()
        })
        .when(!geometry.centered, |surface| {
            surface.top(px(12.0)).right(px(12.0))
        })
        .child(
            div()
                .w(px(geometry.width))
                .max_h(px(geometry.max_height))
                .flex()
                .flex_col()
                .bg(Colors::RAISED)
                .border_1()
                .border_color(Colors::stroke())
                .rounded(px(Radius::CARD))
                .shadow_lg()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .h(px(42.0))
                        .px(px(Space::INDENT))
                        .border_b_1()
                        .border_color(Colors::stroke())
                        .child(
                            div()
                                .flex_1()
                                .text_size(px(Typo::TITLE.size))
                                .font_weight(Typo::TITLE.weight)
                                .child(kind.title()),
                        )
                        .child(
                            components::compact_control("Close")
                                .id("utility-overlay-close")
                                .role(Role::Button)
                                .aria_label(format!("Close {}", kind.title()))
                                .hover(|control| control.bg(theme::white(Fill::HOVER)))
                                .cursor_pointer()
                                .on_click(cx.listener(|shell, _, _, cx| {
                                    shell.dismiss_utility_overlay(cx);
                                })),
                        ),
                )
                .child(
                    div()
                        .id("utility-overlay-rows")
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_h(px(0.0))
                        .size_full()
                        .overflow_y_scroll()
                        .children(rows.into_iter().enumerate().map(move |(row_index, row)| {
                            let action = row_action(&row);
                            let settings_action = row.settings_action;
                            let actionable = action.is_some();
                            let selected = actionable && {
                                let is_selected = selectable_index == overlay.selected;
                                selectable_index += 1;
                                is_selected
                            };
                            let accessible_label = format!("{}: {}", row.label, row.value);
                            div()
                                .id(ElementId::Name(
                                    format!("utility-overlay-row-{row_index}").into(),
                                ))
                                .role(if actionable {
                                    Role::Button
                                } else {
                                    Role::Label
                                })
                                .aria_label(accessible_label)
                                .when(selected, |row| row.aria_active_descendant())
                                .flex()
                                .gap(px(Space::ROW_H))
                                .px(px(Space::INDENT))
                                .py(px(7.0))
                                .border_b_1()
                                .border_color(theme::white(0.05))
                                .when(selected, |row| row.bg(Fill::selected(true)))
                                .when_some(action, |row, action| {
                                    row.cursor_pointer()
                                        .hover(|row| row.bg(theme::white(Fill::HOVER)))
                                        .on_click(cx.listener(move |shell, _, _, cx| {
                                            shell.apply_utility_overlay_action(action.clone());
                                            cx.notify();
                                        }))
                                })
                                .when_some(settings_action, |row, action| {
                                    // Right click walks a numeric setting back down,
                                    // mirroring Left arrow. Primary activation above
                                    // is shared by mouse, keyboard, and VoiceOver.
                                    row.on_mouse_down(
                                        MouseButton::Right,
                                        cx.listener(move |shell, _, _, cx| {
                                            shell.settings_action(action.reversed());
                                            cx.notify();
                                        }),
                                    )
                                })
                                .child(
                                    div()
                                        .w(px(104.0))
                                        .flex_none()
                                        .text_size(px(Typo::META.size))
                                        .text_color(Colors::text(Surface::Content, Tone::Tertiary))
                                        .child(SharedString::from(row.label)),
                                )
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w(px(0.0))
                                        .text_size(px(Typo::ROW.size))
                                        .text_color(Colors::text(Surface::Content, Tone::Secondary))
                                        .child(SharedString::from(row.value)),
                                )
                        })),
                ),
        )
        .with_animation(
            ElementId::Name(format!("utility-overlay-entry-{}", kind.title()).into()),
            Animation::new(motion::OVERLAY_ENTRY).with_easing(ease_out_quint()),
            |surface, delta| surface.opacity(motion::overlay_opacity(delta)),
        )
        .into_any_element()
}

/// A disconnected window keeps its last durable projection visible, but it
/// must never look live. The client retries automatically; this banner makes
/// the cached-state boundary explicit and opens the full status on activation.
fn connection_recovery_banner(
    status: &str,
    sidebar_width: f32,
    cx: &mut Context<Shell>,
) -> AnyElement {
    let detail = format!("{status} · showing last synced state");
    div()
        .id("connection-recovery-banner")
        .role(Role::Alert)
        .aria_label(format!("Reconnecting to daemon. {detail}"))
        .tab_index(0)
        .focus_visible(|banner| banner.border_color(theme::Ink::ATTENTION))
        .absolute()
        .top(px(12.0))
        .left(px(sidebar_width + 12.0))
        .w(px(440.0))
        .flex()
        .flex_col()
        .gap(px(2.0))
        .p(px(Space::INDENT))
        .bg(Colors::RAISED)
        .border_1()
        .border_color(theme::Ink::ATTENTION)
        .rounded(px(Radius::CARD))
        .shadow_lg()
        .cursor_pointer()
        .hover(|banner| banner.bg(theme::white(Fill::HOVER)))
        .on_click(cx.listener(|shell, _, _, cx| {
            shell.toolbar_action(toolbar::ToolbarAction::OpenNotifications, cx);
        }))
        .child(
            div()
                .text_size(px(Typo::META.size))
                .font_weight(Typo::META.weight)
                .text_color(theme::Ink::ATTENTION)
                .child("Reconnecting automatically"),
        )
        .child(
            div()
                .text_size(px(Typo::ROW.size))
                .text_color(Colors::text(Surface::Content, Tone::Secondary))
                .child(SharedString::from(detail)),
        )
        .with_animation(
            "connection-recovery-entry",
            Animation::new(motion::OVERLAY_ENTRY).with_easing(ease_out_quint()),
            |banner, delta| banner.opacity(motion::overlay_opacity(delta)),
        )
        .into_any_element()
}

/// One in-app banner for the newest live alert.
///
/// Clicking it selects the run — the same typed command a notification action
/// may carry. Dismissing it clears the banner but not the unseen count, so
/// nothing is lost by waving it away.
fn attention_banner(item: &attention::AttentionItem, cx: &mut Context<Shell>) -> AnyElement {
    let run_id = item
        .action
        .as_ref()
        .map(|attention::AttentionAction::SelectRun(run_id)| run_id.clone());
    let title = SharedString::from(item.title.clone());
    let detail = SharedString::from(item.detail.clone());
    let accessible_label = format!("{}: {}", item.title, item.detail);
    div()
        .absolute()
        .top(px(12.0))
        .right(px(12.0))
        .w(px(320.0))
        .flex()
        .flex_col()
        .gap(px(2.0))
        .p(px(Space::INDENT))
        .bg(Colors::RAISED)
        .border_1()
        .border_color(Colors::stroke())
        .rounded(px(Radius::CARD))
        .shadow_lg()
        .id("attention-banner")
        .role(Role::Alert)
        .aria_label(accessible_label)
        .tab_index(0)
        .focus_visible(|banner| banner.border_color(theme::white(0.48)))
        .cursor_pointer()
        .hover(|banner| banner.bg(theme::white(Fill::HOVER)))
        .on_click(cx.listener(move |shell, _, _, cx| {
            if let Some(run_id) = run_id.clone() {
                shell.select_run(&run_id);
            }
            shell.state().attention.dismiss_banner();
            cx.notify();
        }))
        .child(
            div()
                .text_size(px(Typo::META.size))
                .text_color(Colors::text(Surface::Content, Tone::Tertiary))
                .child(title),
        )
        .child(
            div()
                .text_size(px(Typo::ROW.size))
                .text_color(Colors::text(Surface::Content, Tone::Secondary))
                .child(detail),
        )
        .with_animation(
            ElementId::Name(format!("attention-banner-entry-{}", item.id).into()),
            Animation::new(motion::OVERLAY_ENTRY).with_easing(ease_out_quint()),
            |banner, delta| banner.opacity(motion::overlay_opacity(delta)),
        )
        .into_any_element()
}

/// The palette: a floating card over the workbench. Denser material than a
/// sidebar, because transient UI layered over live content needs stronger
/// separation than something persistent.
fn palette_overlay(
    query: &str,
    query_is_empty: bool,
    selected: usize,
    rows: &[palette::Ranked],
    cx: &mut Context<Shell>,
) -> impl IntoElement + use<> {
    let card = div()
        .id("palette-dialog")
        .role(Role::Dialog)
        .aria_label("Search runs, projects, engines, and commands")
        .w(px(560.0))
        .max_h(px(420.0))
        .flex()
        .flex_col()
        .bg(Colors::SURFACE)
        .rounded(px(Radius::PANEL))
        .border_1()
        .border_color(Colors::stroke())
        .shadow_lg()
        .child(
            div()
                .id("palette-query")
                .role(Role::SearchInput)
                .aria_label("Search")
                .aria_placeholder("Type to search runs, projects, engines, commands")
                .aria_value(query.to_string())
                .flex()
                .items_center()
                .px(px(Space::INDENT))
                .h(px(Metrics::ROW_HEIGHT + Space::INSET))
                .border_b_1()
                .border_color(Colors::stroke())
                .text_size(px(Typo::ROW.size))
                .child(SharedString::from(if query_is_empty {
                    "Type to search runs, projects, engines, commands".to_string()
                } else {
                    query.to_string()
                }))
                .when(query_is_empty, |row| {
                    row.text_color(Colors::text(Surface::Sidebar, Tone::Tertiary))
                }),
        )
        .child(
            div()
                .id("palette-rows")
                .role(Role::ListBox)
                .aria_label("Search results")
                .flex()
                .flex_col()
                .flex_1()
                .min_h(px(0.0))
                .py(px(4.0))
                .overflow_y_scroll()
                .children(rows.iter().enumerate().map(|(index, ranked)| {
                    let command = ranked.action.command.clone();
                    let accessible_label =
                        format!("{}: {}", ranked.action.title, ranked.action.detail);
                    div()
                        .id(ElementId::Name(format!("palette-{index}").into()))
                        .role(Role::ListBoxOption)
                        .aria_label(accessible_label)
                        .aria_selected(index == selected)
                        .when(index == selected, |row| row.aria_active_descendant())
                        .flex()
                        .items_center()
                        .gap(px(Space::ROW_H))
                        .mx(px(Space::ROW_H))
                        .px(px(Space::ROW_H))
                        .h(px(Metrics::ROW_HEIGHT))
                        .rounded(px(Radius::ROW))
                        .when(index == selected, |row| row.bg(Fill::selected(true)))
                        .hover(|row| row.bg(theme::white(Fill::HOVER)))
                        .cursor_pointer()
                        .on_click(cx.listener(move |shell, _, _, cx| {
                            shell.invoke(command.clone());
                            cx.notify();
                        }))
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.0))
                                .truncate()
                                .text_size(px(Typo::ROW.size))
                                .child(SharedString::from(ranked.action.title.clone())),
                        )
                        .child(
                            div()
                                .flex_none()
                                .text_size(px(Typo::META.size))
                                .text_color(Colors::text(Surface::Sidebar, Tone::Tertiary))
                                .child(SharedString::from(ranked.action.detail.clone())),
                        )
                })),
        )
        .with_animation(
            "palette-entry",
            Animation::new(motion::OVERLAY_ENTRY).with_easing(ease_out_quint()),
            |card, delta| card.opacity(motion::overlay_opacity(delta)),
        );

    div()
        .absolute()
        .top_0()
        .left_0()
        .size_full()
        .flex()
        .justify_center()
        .pt(px(80.0))
        // Dim what is beneath, so the palette reads as a mode.
        .bg(gpui::Rgba {
            r: 0.0,
            g: 0.0,
            b: 0.0,
            a: 0.35,
        })
        .child(card)
}

fn run_switcher_overlay(
    switcher: &RunSwitcher,
    state: &UiState,
    cx: &mut Context<Shell>,
) -> AnyElement {
    let highlighted = switcher.highlighted().to_string();
    let rows = switcher.run_ids().iter().take(8).map(|run_id| {
        let run = state.runs.iter().find(|run| run.id == *run_id);
        let title = run
            .map(|run| {
                if run.objective.is_empty() {
                    run.id.chars().take(8).collect::<String>()
                } else {
                    run.objective.clone()
                }
            })
            .unwrap_or_else(|| run_id.clone());
        let detail = run
            .map(|run| format!("{} · {}", run.engine, run.state))
            .unwrap_or_else(|| "unavailable".into());
        let selected = *run_id == highlighted;
        let selected_run_id = run_id.clone();
        let accessible_label = format!("{title}: {detail}");
        div()
            .id(ElementId::Name(format!("run-switcher-{run_id}").into()))
            .role(Role::ListBoxOption)
            .aria_label(accessible_label)
            .aria_selected(selected)
            .when(selected, |row| row.aria_active_descendant())
            .flex()
            .items_center()
            .gap(px(Space::ROW_H))
            .mx(px(Space::ROW_H))
            .px(px(Space::ROW_H))
            .h(px(Metrics::ROW_HEIGHT))
            .rounded(px(Radius::ROW))
            .when(selected, |row| row.bg(Fill::selected(true)))
            .cursor_pointer()
            .on_click(cx.listener(move |shell, _, _, cx| {
                shell.select_run(&selected_run_id);
                shell.run_switcher = None;
                cx.notify();
            }))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .truncate()
                    .text_size(px(Typo::ROW.size))
                    .child(SharedString::from(title)),
            )
            .child(
                div()
                    .flex_none()
                    .text_size(px(Typo::META.size))
                    .text_color(Colors::text(Surface::Sidebar, Tone::Tertiary))
                    .child(SharedString::from(detail)),
            )
    });

    div()
        .id("run-switcher-dialog")
        .role(Role::Dialog)
        .aria_label("Run switcher")
        .absolute()
        .top(px(68.0))
        .left_0()
        .right_0()
        .flex()
        .justify_center()
        .child(
            div()
                .id("run-switcher-list")
                .role(Role::ListBox)
                .aria_label("Recent runs")
                .w(px(460.0))
                .max_h(px(320.0))
                .flex()
                .flex_col()
                .bg(Colors::RAISED)
                .rounded(px(Radius::PANEL))
                .border_1()
                .border_color(Colors::stroke())
                .shadow_lg()
                .child(
                    div()
                        .h(px(34.0))
                        .flex()
                        .items_center()
                        .px(px(Space::INDENT))
                        .border_b_1()
                        .border_color(Colors::stroke())
                        .text_size(px(Typo::META.size))
                        .text_color(Colors::text(Surface::Content, Tone::Tertiary))
                        .child("Ctrl-Tab run switcher · release Control or press Enter to commit"),
                )
                .children(rows),
        )
        .into_any_element()
}

impl Shell {
    /// Interpret one submitted line. Same vocabulary as before; the difference
    /// is that most of it is now also reachable by clicking.
    fn submit(&mut self, line: &str) {
        let (head, rest) = match line.split_once(char::is_whitespace) {
            Some((head, rest)) => (head, rest.trim()),
            None => (line, ""),
        };
        let mut state = self.state();
        let run_id = state.run_id.clone();
        let live = matches!(state.run_state.as_str(), "running" | "paused");

        let command = match head {
            "/add" if !rest.is_empty() => {
                let path = std::path::PathBuf::from(rest);
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| rest.to_string());
                Some(Command::AddProject {
                    name,
                    path: rest.to_string(),
                })
            }
            // Answer this objective every ready way at once, and let the
            // check decide. The engines are whatever is actually ready, so
            // this never queues an attempt that cannot run.
            "/attempts" if !rest.is_empty() => {
                match state.selected_project().map(|project| project.id.clone()) {
                    Some(project_id) => {
                        let attempts = attempt_engines(&state);
                        if attempts.len() < 2 {
                            state.status =
                                "at least two ready engines are needed to compare attempts".into();
                            None
                        } else {
                            state.status =
                                format!("racing {} engines on this objective", attempts.len());
                            Some(Command::StartAttempts {
                                project_id,
                                objective: rest.to_string(),
                                attempts,
                                check_command: state.check_command.clone(),
                            })
                        }
                    }
                    None => {
                        state.status = "add a project first: /add <path>".into();
                        None
                    }
                }
            }
            "/projects" => Some(Command::RefreshProjects),
            "/engines" => Some(Command::RefreshEngines),
            "/engine" => {
                match rest {
                    "codex" | "claude" => {
                        // The thread continues; the new engine cannot resume
                        // the old provider's session, so it is handed a brief
                        // of what happened instead. Say so, or switching looks
                        // like it silently dropped the conversation.
                        let continuing = state.thread_tip().is_some_and(|tip| tip.engine != rest);
                        state.set_engine(rest);
                        state.status = if continuing {
                            format!("engine: {rest} — this thread will be handed over to it")
                        } else {
                            format!("engine: {rest}")
                        };
                    }
                    other => state.status = format!("unknown engine '{other}' (codex|claude)"),
                }
                None
            }
            "/check" => {
                state.check_command = (!rest.is_empty()).then(|| rest.to_string());
                state.status = match &state.check_command {
                    Some(command) => format!("check: {command}"),
                    None => "check cleared".into(),
                };
                None
            }
            "/new" => {
                state.run_id = None;
                state.run_state.clear();
                state.summary.clear();
                state.graph = None;
                state.diff = None;
                state.status = "ready for a new objective".into();
                None
            }
            "/approve" => run_id.clone().map(|run_id| Command::Approve { run_id }),
            "/pause" | "/resume" | "/cancel" => run_id.clone().map(|run_id| Command::Control {
                run_id,
                method: match head {
                    "/pause" => autoharness_protocol::methods::RUN_PAUSE,
                    "/resume" => autoharness_protocol::methods::RUN_RESUME,
                    _ => autoharness_protocol::methods::RUN_CANCEL,
                },
            }),
            "/i" | "/interrupt" if !rest.is_empty() => {
                run_id.clone().map(|run_id| Command::Interrupt {
                    run_id,
                    message: rest.to_string(),
                })
            }
            "/open" => {
                match state.worktree.clone() {
                    Some(path) => {
                        let _ = std::process::Command::new("/usr/bin/open")
                            .arg(&path)
                            .spawn();
                        state.status = format!("opened {path}");
                    }
                    None => state.status = "this run has no worktree yet".into(),
                }
                None
            }
            other if other.starts_with('/') => {
                state.status = format!("unknown command {other}");
                None
            }
            // A live run is steered. A finished one is CONTINUED: the reply
            // becomes the next turn of the same thread, resuming the engine
            // session, rather than a brand new run that has never met the user.
            _ if live && run_id.is_some() => Some(Command::Chat {
                run_id: run_id.clone().unwrap(),
                message: line.to_string(),
            }),
            // Starting anything on a gated engine is refused up front with
            // the fix, not sent to the daemon to fail later. A stale persisted
            // pick is the only way to get here — every chooser hides gated
            // engines behind "Coming soon".
            _ if !client::engine_generally_available(&state.engine) => {
                state.status = format!(
                    "{} is coming soon — pick Claude or Codex in the model chooser",
                    state.engine
                );
                None
            }
            // A draft with no objective yet is the row the sidebar's `+` just
            // created. Submitting fills THAT run in and queues it; creating
            // another here would leave two rows for one piece of work.
            _ if state.selected_draft_awaiting_objective().is_some() => {
                let run_id = state
                    .selected_draft_awaiting_objective()
                    .expect("guard checked")
                    .id
                    .clone();
                match state.selected_project() {
                    Some(project) => Some(Command::StartDraft {
                        run_id,
                        project_id: project.id.clone(),
                        objective: line.to_string(),
                        check_command: state.check_command.clone(),
                    }),
                    None => {
                        state.status = "add a project first: /add <path>".into();
                        None
                    }
                }
            }
            _ => match state.selected_project() {
                Some(project) => Some(Command::StartRun {
                    project_id: project.id.clone(),
                    engine: state.engine.clone(),
                    model: state.model.clone(),
                    reasoning_effort: state.reasoning_effort.clone(),
                    objective: line.to_string(),
                    check_command: state.check_command.clone(),
                    parent_run_id: state.thread_tip().map(|r| r.id.clone()),
                }),
                None => {
                    // No repository chosen: make one, visibly. The daemon
                    // creates a fresh repo in ~/AutoHarness/<name>, the
                    // status line says exactly where it went, and the
                    // objective starts there — never a silent git init in a
                    // directory the user did not expect.
                    let name = repository_name_for(line);
                    state.status =
                        format!("no repository chosen — creating ~/AutoHarness Projects/{name}");
                    Some(Command::CreateProjectAndStart {
                        name,
                        objective: line.to_string(),
                        engine: state.engine.clone(),
                        model: state.model.clone(),
                        reasoning_effort: state.reasoning_effort.clone(),
                        check_command: state.check_command.clone(),
                    })
                }
            },
        };
        drop(state);
        if let Some(command) = command {
            self.client.send(command);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `/attempts` fans an objective across the engines that can actually run
    /// it, and refuses when there is nothing to compare.
    ///
    /// One attempt is not a comparison — it is an ordinary run with extra
    /// ceremony — and queueing an engine that is not ready would spend a slot
    /// on something that blocks immediately.
    #[test]
    fn attempts_race_only_engines_that_are_ready() {
        use crate::client::EngineStatus;

        fn engine(name: &str, ready: bool) -> EngineStatus {
            EngineStatus {
                name: name.into(),
                ready,
                installed: true,
                authenticated: Some(ready),
                version: None,
                problems: Vec::new(),
                models: Vec::new(),
                model_load_error: None,
            }
        }

        let mut state = crate::preview::populated();
        state.engines = vec![engine("codex", true), engine("claude", false)];
        assert_eq!(attempt_engines(&state).len(), 1, "only one engine can run");

        state.engines = vec![engine("codex", true), engine("claude", true)];
        assert_eq!(
            attempt_engines(&state).len(),
            2,
            "now there is something to compare"
        );

        // A gated engine never enters the race, even when its CLI is ready
        // on this machine: the product does not offer it yet.
        state.engines = vec![
            engine("codex", true),
            engine("claude", true),
            engine("opencode", true),
            engine("grok", true),
        ];
        let racing = attempt_engines(&state);
        assert_eq!(racing.len(), 2);
        assert!(racing.iter().all(|(name, _, _)| name != "opencode"));
    }

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
    fn cmd_shift_arrows_are_not_consumed_by_dead_inspector_tabs() {
        assert_eq!(toolbar_shortcut("right", modifiers("@$")), None);
        assert_eq!(toolbar_shortcut("left", modifiers("@$")), None);
    }

    fn run(id: &str, project_id: &str, parent_run_id: Option<&str>) -> client::RunView {
        client::RunView {
            id: id.into(),
            project_id: project_id.into(),
            objective: format!("objective {id}"),
            state: "running".into(),
            engine: "codex".into(),
            parent_run_id: parent_run_id.map(str::to_string),
            attempt_group: None,
        }
    }

    #[test]
    fn current_run_tip_ids_normalizes_older_mru_entries_to_current_thread_tips() {
        let state = UiState {
            runs: vec![
                run("root", "p1", None),
                run("followup", "p1", Some("root")),
                run("other", "p1", None),
            ],
            ..UiState::default()
        };

        assert_eq!(
            current_run_tip_ids_for_state(&state, &["root".into(), "followup".into()]),
            vec!["followup", "other"]
        );
    }

    #[test]
    fn current_run_tip_ids_filters_deleted_mru_entries_and_dedupes_live_tips() {
        let state = UiState {
            runs: vec![run("root", "p1", None), run("followup", "p1", Some("root"))],
            ..UiState::default()
        };

        assert_eq!(
            current_run_tip_ids_for_state(
                &state,
                &[
                    "missing".into(),
                    "root".into(),
                    "followup".into(),
                    "missing".into()
                ]
            ),
            vec!["followup"]
        );
    }

    #[test]
    fn transient_surfaces_own_global_navigation_shortcuts_until_dismissed() {
        for key in ["b", "j", "k"] {
            assert_eq!(
                key_route(key, modifiers("@"), true, false, false),
                KeyRoute::Palette
            );
            assert_eq!(
                key_route(key, modifiers("@"), false, true, false),
                KeyRoute::UtilityOverlay
            );
            assert_eq!(
                key_route(key, modifiers("@"), false, false, true),
                KeyRoute::RunSwitcher
            );
        }

        assert_eq!(
            key_route("d", modifiers("@$"), true, false, false),
            KeyRoute::Palette
        );
        assert_eq!(
            key_route("tab", modifiers("^"), false, true, false),
            KeyRoute::UtilityOverlay
        );
        assert_eq!(
            key_route("tab", modifiers("^"), false, false, true),
            KeyRoute::RunSwitcher
        );
    }

    #[test]
    fn palette_query_changes_from_clipboard_style_edits_reset_deep_selection() {
        let mut palette = PaletteState {
            selected: 8,
            ..PaletteState::default()
        };

        palette_query_changed(&mut palette, true);

        assert_eq!(palette.selected, 0);
    }

    #[test]
    fn repository_names_come_from_the_objective_and_never_surprise() {
        assert_eq!(
            repository_name_for("Fix the auth bug in login"),
            "fix-the-auth-bug"
        );
        assert_eq!(repository_name_for("yoo"), "yoo");
        assert_eq!(repository_name_for("  !!  "), "new-project");
        assert_eq!(repository_name_for(""), "new-project");
    }

    #[test]
    fn settings_overlay_exposes_mouse_reachable_pane_toggles() {
        let state = crate::preview::populated();
        let layout = layout::ShellLayout::default();
        let rows = utility_overlay_rows(UtilityOverlay::Settings, &state, layout, &[]);

        assert!(rows.iter().any(|row| {
            row.label == "Sidebar pane"
                && row.value == "Open"
                && row.action == Some(toolbar::ToolbarAction::ToggleSidebar)
        }));
        assert!(rows.iter().any(|row| {
            row.label == "Execution pane"
                && row.value == "Open"
                && row.action == Some(toolbar::ToolbarAction::ToggleExecution)
        }));
        assert!(rows.iter().any(|row| {
            row.label == "Inspector pane"
                && row.value == "Open"
                && row.action == Some(toolbar::ToolbarAction::ToggleInspector)
        }));
    }

    #[test]
    fn settings_overlay_keyboard_traversal_reaches_action_rows_after_info_rows() {
        let state = crate::preview::populated();
        let layout = layout::ShellLayout::default();
        let rows = utility_overlay_rows(UtilityOverlay::Settings, &state, layout, &[]);

        let actions = utility_overlay_keyboard_actions(&rows);
        // Page order: General's persisted settings first, then Limits,
        // Notifications, Updates, and the local Layout toggles last.
        // Integration and Usage rows are informational and skipped.
        assert_eq!(
            actions,
            [
                UtilityOverlayAction::Settings(settings::SettingsAction::CycleEngine),
                UtilityOverlayAction::Settings(settings::SettingsAction::CycleRouteMode),
                UtilityOverlayAction::Settings(settings::SettingsAction::Toggle(
                    settings::SettingsBoolField::AutomaticHistoryScan
                )),
                UtilityOverlayAction::Settings(settings::SettingsAction::Toggle(
                    settings::SettingsBoolField::ConfirmDestructiveActions
                )),
                UtilityOverlayAction::Settings(settings::SettingsAction::Increment(
                    settings::SettingsNumberField::MaxActiveRuns
                )),
                UtilityOverlayAction::Settings(settings::SettingsAction::Increment(
                    settings::SettingsNumberField::MaxParallelWorkers
                )),
                UtilityOverlayAction::Settings(settings::SettingsAction::Increment(
                    settings::SettingsNumberField::MaxGraphNodes
                )),
                UtilityOverlayAction::Settings(settings::SettingsAction::Increment(
                    settings::SettingsNumberField::DefaultWallTimeMinutes
                )),
                UtilityOverlayAction::Settings(settings::SettingsAction::Increment(
                    settings::SettingsNumberField::RetentionDays
                )),
                UtilityOverlayAction::Settings(settings::SettingsAction::Toggle(
                    settings::SettingsBoolField::NotificationsEnabled
                )),
                UtilityOverlayAction::Settings(settings::SettingsAction::Toggle(
                    settings::SettingsBoolField::SoundsEnabled
                )),
                UtilityOverlayAction::Settings(settings::SettingsAction::Toggle(
                    settings::SettingsBoolField::AutomaticUpdateChecks
                )),
                UtilityOverlayAction::Update(update::UpdateAction::CheckNow),
                UtilityOverlayAction::Toolbar(toolbar::ToolbarAction::ToggleSidebar),
                UtilityOverlayAction::Toolbar(toolbar::ToolbarAction::ToggleExecution),
                UtilityOverlayAction::Toolbar(toolbar::ToolbarAction::ToggleInspector),
                UtilityOverlayAction::Toolbar(toolbar::ToolbarAction::ResetLayout),
            ]
        );
    }

    #[test]
    fn settings_overlay_exposes_real_persisted_rows_and_no_unsupported_controls() {
        let mut state = crate::preview::populated();
        state.settings.values = serde_json::from_value(serde_json::json!({
            "version": 1,
            "default_engine": "claude",
            "default_route_mode": "parallel",
            "max_parallel_workers": 3,
            "max_graph_nodes": 11,
            "default_wall_time_minutes": 45,
            "retention_days": 30,
            "automatic_history_scan": false,
            "notifications_enabled": true,
            "sounds_enabled": false,
            "automatic_update_checks": true,
            "confirm_destructive_actions": true,
        }))
        .unwrap();
        state.settings.loading = false;
        let rows = utility_overlay_rows(
            UtilityOverlay::Settings,
            &state,
            layout::ShellLayout::default(),
            &[],
        );

        for (label, value) in [
            ("Default engine", "claude"),
            ("Route mode", "parallel"),
            ("Active runs", "2"),
            ("Max workers", "3"),
            ("Max graph nodes", "11"),
            ("Wall time", "45 min"),
            ("Retention", "30 days"),
            ("History scan", "Off"),
            ("Notifications", "On"),
            ("Update checks", "On"),
            ("Confirm destructive actions", "On"),
        ] {
            assert!(
                rows.iter().any(|row| row.label == label
                    && row.value == value
                    && row.settings_action.is_some()),
                "{label} row should be actionable and show {value}"
            );
        }
        assert!(
            rows.iter()
                .any(|row| row.label == "Reset layout" && row.value == "Local only")
        );
        // The Sounds switch persists, but it must not claim to make a noise
        // this build cannot make.
        let sounds = rows.iter().find(|row| row.label == "Sounds").unwrap();
        assert!(
            sounds.settings_action.is_some(),
            "the setting still persists"
        );
        assert!(sounds.value.starts_with("Off"));
        assert_eq!(
            sounds.value.contains("unavailable on this build"),
            !crate::notify::sound_supported(),
            "the row says so exactly when no sound can play"
        );
        assert!(!rows.iter().any(|row| {
            row.label.contains("Launch")
                || row.value.contains("Launch")
                || row.label.contains("login")
        }));
    }

    #[test]
    fn settings_actions_mutate_state_and_return_typed_update_commands() {
        let mut state = UiState::default();
        state.settings.values = autoharness_protocol::params::AppSettings::default();

        let command = crate::settings::apply_settings_action(
            &mut state,
            crate::settings::SettingsAction::CycleEngine,
        )
        .expect("engine change should persist");
        assert_eq!(
            command,
            Command::UpdateSettings(
                serde_json::from_value(serde_json::json!({
                    "default_engine": "claude"
                }))
                .unwrap()
            )
        );
        assert_eq!(state.settings.values.default_engine.to_string(), "claude");
        assert_eq!(state.engine, "claude");

        state.settings.values.max_parallel_workers =
            autoharness_protocol::params::MAX_PARALLEL_WORKERS;
        let command = crate::settings::apply_settings_action(
            &mut state,
            crate::settings::SettingsAction::Increment(
                crate::settings::SettingsNumberField::MaxParallelWorkers,
            ),
        )
        .expect("bounded increment still sends current value");
        assert_eq!(
            state.settings.values.max_parallel_workers,
            autoharness_protocol::params::MAX_PARALLEL_WORKERS
        );
        assert_eq!(
            command,
            Command::UpdateSettings(autoharness_protocol::params::SettingsUpdate {
                max_parallel_workers: Some(autoharness_protocol::params::MAX_PARALLEL_WORKERS),
                ..autoharness_protocol::params::SettingsUpdate::default()
            })
        );

        let command = crate::settings::apply_settings_action(
            &mut state,
            crate::settings::SettingsAction::Toggle(
                crate::settings::SettingsBoolField::NotificationsEnabled,
            ),
        )
        .expect("toggle should persist");
        assert_eq!(
            command,
            Command::UpdateSettings(autoharness_protocol::params::SettingsUpdate {
                notifications_enabled: Some(false),
                ..autoharness_protocol::params::SettingsUpdate::default()
            })
        );
        assert!(!state.settings.values.notifications_enabled);
    }

    fn worktree_entry(path: &str, eligible: bool) -> autoharness_protocol::params::WorktreeEntry {
        autoharness_protocol::params::WorktreeEntry {
            path: path.into(),
            kind: "run".into(),
            run_id: "run-1".into(),
            node_id: None,
            repo_path: "/repo".into(),
            branch: "ah/run-1".into(),
            base_commit: "abc123".into(),
            created_at_ms: 1_775_000_000_000,
            removed_at_ms: None,
            run_state: "succeeded".into(),
            exists: true,
            eligible,
            blockers: if eligible {
                Vec::new()
            } else {
                vec![autoharness_protocol::params::WorktreeBlocker::DirtyWorktree]
            },
        }
    }

    #[test]
    fn worktree_panel_lists_entries_and_explains_why_a_row_is_blocked() {
        let mut state = UiState::default();
        state.worktrees.storage_root = "/data/worktrees".into();
        state.worktrees.entries = vec![
            worktree_entry("/data/worktrees/run-1", true),
            worktree_entry("/data/worktrees/run-2", false),
        ];
        let rows = utility_overlay_rows(
            UtilityOverlay::Worktrees,
            &state,
            layout::ShellLayout::default(),
            &[],
        );

        assert!(
            rows.iter()
                .any(|row| row.label == "Filter" && row.value == "Live")
        );
        assert!(rows.iter().any(|row| row.label == "Storage"));
        let eligible = rows.iter().find(|row| row.label == "run-1").unwrap();
        assert!(eligible.value.contains("eligible"));
        assert!(eligible.worktree_action.is_some());
        let blocked = rows.iter().find(|row| row.label == "run-2").unwrap();
        assert!(
            blocked.value.contains("uncommitted changes"),
            "a blocked row states the reason: {}",
            blocked.value
        );
        // No row offers a direct reclaim before a check has run.
        assert!(!rows.iter().any(|row| row.label == "Confirm reclaim"));
    }

    #[test]
    fn worktree_reclaim_needs_a_dry_run_and_then_an_explicit_confirmation() {
        let mut state = UiState::default();
        state.worktrees.entries = vec![worktree_entry("/data/worktrees/run-1", true)];

        let command = crate::worktrees::apply_worktree_action(
            &mut state,
            crate::worktrees::WorktreeAction::DryRun("/data/worktrees/run-1".into()),
        );
        assert_eq!(
            command,
            Some(Command::ReclaimWorktree {
                path: "/data/worktrees/run-1".into(),
                dry_run: true,
            })
        );

        // Confirming a path the daemon has not cleared sends nothing.
        let refused = crate::worktrees::apply_worktree_action(
            &mut state,
            crate::worktrees::WorktreeAction::Confirm("/data/worktrees/run-1".into()),
        );
        assert_eq!(refused, None);

        // The dry-run answer is what unlocks the confirmation row.
        state.worktrees.pending_confirm = Some("/data/worktrees/run-1".into());
        let rows = utility_overlay_rows(
            UtilityOverlay::Worktrees,
            &state,
            layout::ShellLayout::default(),
            &[],
        );
        assert!(
            rows.iter().any(|row| {
                row.label == "Confirm reclaim" && row.value == "/data/worktrees/run-1"
            })
        );
        assert!(rows.iter().any(|row| row.label == "Cancel"));

        let command = crate::worktrees::apply_worktree_action(
            &mut state,
            crate::worktrees::WorktreeAction::Confirm("/data/worktrees/run-1".into()),
        );
        assert_eq!(
            command,
            Some(Command::ReclaimWorktree {
                path: "/data/worktrees/run-1".into(),
                dry_run: false,
            })
        );
        assert_eq!(state.worktrees.pending_confirm, None);
    }

    #[test]
    fn worktree_cancel_drops_the_pending_confirmation() {
        let mut state = UiState::default();
        state.worktrees.pending_confirm = Some("/data/worktrees/run-1".into());
        state.worktrees.diagnostics = vec!["eligible".into()];
        let command = crate::worktrees::apply_worktree_action(
            &mut state,
            crate::worktrees::WorktreeAction::Cancel,
        );
        assert_eq!(command, None);
        assert_eq!(state.worktrees.pending_confirm, None);
        assert!(state.worktrees.diagnostics.is_empty());
    }

    #[test]
    fn worktree_filter_cycles_and_asks_the_daemon_again() {
        let mut state = UiState::default();
        for expected in [
            client::WorktreeFilter::OnlyEligible,
            client::WorktreeFilter::IncludeReclaimed,
            client::WorktreeFilter::Live,
        ] {
            let command = crate::worktrees::apply_worktree_action(
                &mut state,
                crate::worktrees::WorktreeAction::CycleFilter,
            );
            assert_eq!(state.worktrees.filter, expected);
            assert_eq!(command, Some(Command::RefreshWorktrees(expected)));
        }
    }

    #[test]
    fn every_worktree_row_action_is_reachable_by_keyboard() {
        let mut state = UiState::default();
        state.worktrees.entries = vec![worktree_entry("/data/worktrees/run-1", true)];
        state.worktrees.pending_confirm = Some("/data/worktrees/run-1".into());
        let rows = utility_overlay_rows(
            UtilityOverlay::Worktrees,
            &state,
            layout::ShellLayout::default(),
            &[],
        );
        let visible = rows
            .iter()
            .filter(|row| row.worktree_action.is_some())
            .count();
        let reachable = utility_overlay_keyboard_actions(&rows)
            .into_iter()
            .filter(|action| matches!(action, UtilityOverlayAction::Worktree(_)))
            .count();
        assert_eq!(visible, reachable);
        assert!(visible >= 4, "filter, refresh, row, confirm, cancel");
    }

    #[test]
    fn usage_overlay_shows_ledger_tokens_without_fake_cost() {
        let mut state = UiState::default();
        state.usage_summary.providers = vec![autoharness_protocol::params::UsageProviderSummary {
            provider: "codex".into(),
            today: autoharness_protocol::params::UsageBucket {
                input_tokens: 100,
                output_tokens: 40,
                total_tokens: 140,
                event_count: 2,
                run_count: 1,
            },
            month: autoharness_protocol::params::UsageBucket {
                input_tokens: 200,
                output_tokens: 75,
                total_tokens: 275,
                event_count: 3,
                run_count: 2,
            },
            all_time: autoharness_protocol::params::UsageBucket {
                input_tokens: 1_200,
                output_tokens: 300,
                total_tokens: 1_500,
                event_count: 9,
                run_count: 4,
            },
        }];
        state.usage_summary.runs = vec![autoharness_protocol::params::UsageRunSummary {
            run_id: "run-1".into(),
            provider: "codex".into(),
            input_tokens: 100,
            output_tokens: 40,
            total_tokens: 140,
            event_count: 2,
        }];
        let rows = crate::usage::usage_rows(&state);

        assert!(
            rows.iter()
                .any(|row| { row.label == "codex today" && row.value.contains("140 tokens") })
        );
        assert!(
            rows.iter()
                .any(|row| { row.label == "run-1" && row.value.contains("100 in / 40 out") })
        );
        assert!(!rows.iter().any(|row| {
            let text = format!("{} {}", row.label, row.value);
            text.contains('$') || text.to_ascii_lowercase().contains("cost")
        }));
    }

    #[test]
    fn mixed_informational_toolbar_and_run_rows_are_traversed_without_dead_rows() {
        let rows = vec![
            row("Info", "not actionable", None),
            row(
                "Execution pane",
                "Open",
                Some(toolbar::ToolbarAction::ToggleExecution),
            ),
            select_row("Working", "core · codex", "run-1"),
            row("Footer", "also informational", None),
        ];

        assert_eq!(
            utility_overlay_keyboard_actions(&rows),
            vec![
                UtilityOverlayAction::Toolbar(toolbar::ToolbarAction::ToggleExecution),
                UtilityOverlayAction::SelectRun("run-1".into()),
            ]
        );
    }

    #[test]
    fn settings_keyboard_enter_dispatches_the_selected_pane_toggle() {
        let mut state = crate::preview::populated();
        let mut layout = layout::ShellLayout::default();
        let rows = utility_overlay_rows(UtilityOverlay::Settings, &state, layout, &[]);
        let actions = utility_overlay_keyboard_actions(&rows);

        // The Layout section sits at the end of the page; its toggles must
        // still dispatch through the shared keyboard path.
        let sidebar_index = actions
            .iter()
            .position(|action| {
                action == &UtilityOverlayAction::Toolbar(toolbar::ToolbarAction::ToggleSidebar)
            })
            .expect("sidebar toggle is keyboard reachable");

        assert!(layout.sidebar_open);
        assert!(dispatch_utility_overlay_keyboard_action(
            &mut state,
            &mut layout,
            actions[sidebar_index].clone()
        ));
        assert!(!layout.sidebar_open);

        assert!(layout.execution_open);
        assert!(dispatch_utility_overlay_keyboard_action(
            &mut state,
            &mut layout,
            actions[sidebar_index + 1].clone()
        ));
        assert!(!layout.execution_open);
    }

    #[test]
    fn every_visible_utility_action_has_keyboard_dispatch() {
        let state = crate::preview::populated();
        let layout = layout::ShellLayout::default();

        for surface in [
            UtilityOverlay::Overview,
            UtilityOverlay::History,
            UtilityOverlay::Worktrees,
            UtilityOverlay::Queue,
            UtilityOverlay::Notifications,
            UtilityOverlay::Settings,
        ] {
            let rows = utility_overlay_rows(surface, &state, layout, &[]);
            let visible_actions = rows
                .iter()
                .filter(|row| {
                    row.action.is_some()
                        || row.select_run_id.is_some()
                        || row.history_action.is_some()
                        || row.settings_action.is_some()
                        || row.update_action.is_some()
                        || row.worktree_action.is_some()
                        || row.queue_action.is_some()
                })
                .count();
            assert_eq!(
                utility_overlay_keyboard_actions(&rows).len(),
                visible_actions,
                "{surface:?} has a visible action row that keyboard traversal skips"
            );
        }
    }

    #[test]
    fn queue_overlay_exposes_select_reorder_cancel_and_keyboard_parity() {
        use autoharness_protocol::params::{QueueItem, QueueKind, QueueState};

        let mut state = crate::preview::populated();
        state.queue.replace(vec![
            QueueItem {
                id: "queue-1".into(),
                kind: QueueKind::Objective,
                state: QueueState::Pending,
                run_id: "feat-cache-key-stability".into(),
                project_id: "repo-autoharness".into(),
                content: "first".into(),
                position: 1024,
                created_at_ms: 1,
                updated_at_ms: 1,
                error: None,
            },
            QueueItem {
                id: "queue-2".into(),
                kind: QueueKind::Objective,
                state: QueueState::Pending,
                run_id: "fix-ui-regression".into(),
                project_id: "repo-autoharness".into(),
                content: "second".into(),
                position: 2048,
                created_at_ms: 2,
                updated_at_ms: 2,
                error: None,
            },
        ]);
        let rows = utility_overlay_rows(
            UtilityOverlay::Queue,
            &state,
            layout::ShellLayout::default(),
            &[],
        );
        assert!(
            rows.iter()
                .any(|row| row.select_run_id.as_deref() == Some("feat-cache-key-stability"))
        );
        assert!(rows.iter().any(|row| matches!(
            row.queue_action,
            Some(queue::QueueAction::MoveBefore { ref item_id, .. }) if item_id == "queue-2"
        )));
        assert!(rows.iter().any(|row| matches!(
            row.queue_action,
            Some(queue::QueueAction::Cancel(ref id)) if id == "queue-2"
        )));
        let visible = rows.iter().filter(|row| row_action(row).is_some()).count();
        assert_eq!(utility_overlay_keyboard_actions(&rows).len(), visible);
    }

    #[test]
    fn layout_toolbar_actions_toggle_and_restore_each_pane() {
        let mut layout = layout::ShellLayout::default();

        assert!(apply_layout_toolbar_action(
            &mut layout,
            toolbar::ToolbarAction::ToggleSidebar
        ));
        assert!(!layout.sidebar_open);
        assert!(apply_layout_toolbar_action(
            &mut layout,
            toolbar::ToolbarAction::ToggleSidebar
        ));
        assert!(layout.sidebar_open);

        assert!(apply_layout_toolbar_action(
            &mut layout,
            toolbar::ToolbarAction::ToggleExecution
        ));
        assert!(!layout.execution_open);
        assert!(apply_layout_toolbar_action(
            &mut layout,
            toolbar::ToolbarAction::ToggleExecution
        ));
        assert!(layout.execution_open);

        assert!(apply_layout_toolbar_action(
            &mut layout,
            toolbar::ToolbarAction::ToggleInspector
        ));
        assert!(!layout.inspector_open);
        assert!(apply_layout_toolbar_action(
            &mut layout,
            toolbar::ToolbarAction::ToggleInspector
        ));
        assert!(layout.inspector_open);
        assert!(!apply_layout_toolbar_action(
            &mut layout,
            toolbar::ToolbarAction::OpenSettings
        ));
    }

    #[test]
    fn shell_body_metrics_are_flush_to_available_body() {
        let metrics = shell_body_metrics(1280.0, 820.0, layout::ShellLayout::default());

        assert_eq!(metrics.available_width, 1280.0);
        assert_eq!(metrics.available_height, 820.0 - Metrics::TITLE_BAR);
        assert_eq!(
            metrics.columns,
            layout::ColumnWidths {
                sidebar: 300.0,
                inspector: 379.0,
            }
        );
        assert_eq!(metrics.center_width, 601.0);
    }

    #[test]
    fn utility_overlays_project_real_preview_state() {
        let state = crate::preview::populated();
        let layout = layout::ShellLayout::default();

        assert_eq!(UtilityOverlay::Overview.title(), "Overview");
        assert!(
            utility_overlay_rows(UtilityOverlay::Overview, &state, layout, &[])
                .iter()
                .any(|row| row.label == "Counts" && row.value.contains("needs you"))
        );
        assert!(
            utility_overlay_rows(UtilityOverlay::Worktrees, &state, layout, &[])
                .iter()
                .any(|row| row.label == "Selected" && row.value == ".worktrees/run-20250508-1525")
        );
        assert!(
            utility_overlay_rows(UtilityOverlay::Settings, &state, layout, &[])
                .iter()
                .any(|row| row.label == "Selected engine" && row.value == "codex")
        );
    }
}

#[cfg(test)]
mod surface_geometry_tests {
    use super::*;

    /// Content that is a page gets a page.
    ///
    /// Every utility surface was a 360x420 popover pinned to the top-right
    /// corner — including Settings, which has four tabs of preferences, and
    /// Worktrees, which lists checkouts with full paths. That is reading a
    /// page through a letterbox.
    #[test]
    fn page_sized_content_is_not_shown_in_a_corner_popover() {
        for page in [
            UtilityOverlay::Settings,
            UtilityOverlay::Worktrees,
            UtilityOverlay::History,
        ] {
            let geometry = surface_geometry(page);
            assert!(geometry.centered, "{page:?} is a page");
            assert!(geometry.width >= 600.0, "{page:?} is {}", geometry.width);
        }
    }

    /// The glanceable ones stay where a glance expects them. Centring a
    /// notification list would make checking it feel like a detour.
    #[test]
    fn glanceable_surfaces_stay_in_the_corner() {
        for glance in [
            UtilityOverlay::Overview,
            UtilityOverlay::Queue,
            UtilityOverlay::Notifications,
        ] {
            let geometry = surface_geometry(glance);
            assert!(!geometry.centered, "{glance:?} is a glance");
        }
    }
}
