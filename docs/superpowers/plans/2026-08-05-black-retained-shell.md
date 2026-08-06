# Black Retained Shell Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace AutoHarness's fixed grey three-pane GPUI shell with the approved true-black, retained, resizable run cockpit while keeping every visible control backed by current daemon behavior.

**Architecture:** Keep `client.rs` as the only JSON-RPC boundary and split the root shell into pure layout state plus focused GPUI view modules. The coordinator remains primary, the existing graph/activity becomes a collapsible lower workbench, and the existing diff/check/activity data moves into a collapsible inspector. A deterministic preview state provides repeatable screenshot validation without starting a provider run.

**Tech Stack:** Rust 2024, GPUI at the workspace-pinned Zed revision, Tokio client channel, existing `autoharness-protocol`, GPUI unit tests, macOS `screencapture`.

**Approved visual reference:** `docs/superpowers/specs/assets/autoharness-diri-black-reference.png`

---

## File map

- Create `crates/ui-gpui/src/layout.rs`: pure sidebar/inspector/workbench sizing and visibility state.
- Create `crates/ui-gpui/src/components.rs`: shared panel, header, separator, status-mark, and compact control recipes.
- Create `crates/ui-gpui/src/toolbar.rs`: route/engine/run state and real pane/palette controls.
- Create `crates/ui-gpui/src/sidebar.rs`: repository and nested run-thread projection/view.
- Create `crates/ui-gpui/src/coordinator.rs`: transcript and objective/steering composer.
- Create `crates/ui-gpui/src/workbench.rs`: graph and activity workbench.
- Create `crates/ui-gpui/src/inspector.rs`: Changes, Checks, and Activity tabs over existing real data.
- Create `crates/ui-gpui/src/preview.rs`: deterministic populated `UiState` used only by the preview executable.
- Create `crates/ui-gpui/examples/shell_preview.rs`: launch the deterministic shell for screenshot inspection.
- Modify `crates/ui-gpui/src/lib.rs`: retain global state/focus/keyboard routing and compose focused modules.
- Modify `crates/ui-gpui/src/client.rs`: add a disconnected preview client constructor; no protocol changes.
- Modify `crates/ui-gpui/src/theme.rs`: true-black elevation tokens and toolbar/inspector metrics.
- Modify `crates/ui-gpui/src/ansi.rs`: remove the baseline warning.
- Modify `crates/ui-gpui/Cargo.toml`: register the preview example only if Cargo cannot infer it.
- Modify `AGENTS.md`: replace stale renderer/headless-render notes with the retained-shell contract and command.
- Modify `NOTICE`: enumerate the newly adapted diri layout/component subsystems.

## Task 1: Lock the true-black visual contract and restore zero warnings

**Files:**
- Modify: `crates/ui-gpui/src/theme.rs:120-175`
- Modify: `crates/ui-gpui/src/theme.rs:369-453`
- Modify: `crates/ui-gpui/src/ansi.rs:48-64`

- [ ] **Step 1: Write the failing token test**

Add to `theme::tests`:

```rust
#[test]
fn the_shell_uses_true_black_with_only_two_elevations() {
    assert_eq!(Colors::BACKGROUND, rgba8(0, 0, 0, 0xff));
    assert_eq!(Colors::SURFACE, rgba8(5, 5, 5, 0xff));
    assert_eq!(Colors::RAISED, rgba8(10, 10, 10, 0xff));
    assert!(Colors::BACKGROUND.r < Colors::SURFACE.r);
    assert!(Colors::SURFACE.r < Colors::RAISED.r);
}
```

- [ ] **Step 2: Run the focused test and verify it fails**

Run: `cargo test -p autoharness-ui-gpui theme::tests::the_shell_uses_true_black_with_only_two_elevations -- --exact`

Expected: compile failure because `Colors::RAISED` does not exist and the existing colors are grey.

- [ ] **Step 3: Implement the black elevation tokens**

Replace the two color constants with:

```rust
impl Colors {
    pub const BACKGROUND: Rgba = rgba8(0, 0, 0, 0xff);
    pub const SURFACE: Rgba = rgba8(5, 5, 5, 0xff);
    pub const RAISED: Rgba = rgba8(10, 10, 10, 0xff);

    pub fn text(surface: Surface, tone: Tone) -> Rgba {
        let alpha = match (surface, tone) {
            (_, Tone::Primary) => 1.00,
            (Surface::Content, Tone::Secondary) => 0.60,
            (Surface::Content, Tone::Tertiary) => 0.30,
            (Surface::Sidebar, Tone::Secondary) => 0.70,
            (Surface::Sidebar, Tone::Tertiary) => 0.44,
        };
        white(alpha)
    }

    pub const fn stroke() -> Rgba {
        rgba8(0xff, 0xff, 0xff, 20)
    }
}
```

Remove `mut` from the `flush` closure binding in `ansi.rs`:

```rust
let flush = |text: &mut String,
             color: TermColor,
             style: TermStyle,
             out: &mut Vec<Span>| {
    if !text.is_empty() {
        out.push(Span {
            text: std::mem::take(text),
            color,
            style,
        });
    }
};
```

- [ ] **Step 4: Verify the test and warning gate**

Run: `cargo test -p autoharness-ui-gpui theme::tests::the_shell_uses_true_black_with_only_two_elevations -- --exact`

Expected: PASS.

Run: `cargo clippy -p autoharness-ui-gpui --all-targets -- -D warnings`

Expected: PASS with no `unused_mut` warning.

- [ ] **Step 5: Commit the visual contract**

```bash
git add crates/ui-gpui/src/theme.rs crates/ui-gpui/src/ansi.rs
git commit -m "Make black a real shell invariant"
```

## Task 2: Port pure retained layout state

**Files:**
- Create: `crates/ui-gpui/src/layout.rs`
- Modify: `crates/ui-gpui/src/lib.rs:13-18`

- [ ] **Step 1: Write the complete pure-state test module**

Create `layout.rs` with the public types plus tests first:

```rust
pub const DEFAULT_SIDEBAR_WIDTH: f32 = 248.0;
pub const DEFAULT_INSPECTOR_WIDTH: f32 = 360.0;
pub const DEFAULT_COORDINATOR_FRACTION: f32 = 0.62;
const MIN_SIDEBAR_WIDTH: f32 = 200.0;
const MAX_SIDEBAR_WIDTH: f32 = 400.0;
const MIN_INSPECTOR_WIDTH: f32 = 300.0;
const MAX_INSPECTOR_WIDTH: f32 = 560.0;
const MIN_COORDINATOR_HEIGHT: f32 = 260.0;
const MIN_EXECUTION_HEIGHT: f32 = 150.0;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ColumnWidths {
    pub sidebar: f32,
    pub inspector: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RowHeights {
    pub coordinator: f32,
    pub execution: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ShellLayout {
    pub sidebar_open: bool,
    pub inspector_open: bool,
    pub execution_open: bool,
    sidebar_width: f32,
    inspector_width: f32,
    coordinator_fraction: f32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_approved_cockpit() {
        let layout = ShellLayout::default();
        assert_eq!(layout.columns(), ColumnWidths { sidebar: 248.0, inspector: 360.0 });
        assert_eq!(layout.rows(700.0), RowHeights { coordinator: 434.0, execution: 266.0 });
    }

    #[test]
    fn closed_panels_take_no_layout_space() {
        let mut layout = ShellLayout::default();
        layout.sidebar_open = false;
        layout.inspector_open = false;
        layout.execution_open = false;
        assert_eq!(layout.columns(), ColumnWidths { sidebar: 0.0, inspector: 0.0 });
        assert_eq!(layout.rows(700.0), RowHeights { coordinator: 700.0, execution: 0.0 });
    }

    #[test]
    fn pointer_resizes_are_clamped() {
        let mut layout = ShellLayout::default();
        layout.resize_sidebar(900.0);
        layout.resize_inspector(20.0);
        layout.resize_coordinator(690.0, 700.0);
        assert_eq!(layout.columns(), ColumnWidths { sidebar: 400.0, inspector: 300.0 });
        assert_eq!(layout.rows(700.0), RowHeights { coordinator: 550.0, execution: 150.0 });
    }
}
```

- [ ] **Step 2: Run the new module tests and verify the missing implementation**

Export `pub mod layout;` from `lib.rs`, then run:

`cargo test -p autoharness-ui-gpui layout::tests -- --nocapture`

Expected: compile failures for `Default`, `columns`, `rows`, and resize methods.

- [ ] **Step 3: Implement the minimal retained layout methods**

Add before the test module:

```rust
impl Default for ShellLayout {
    fn default() -> Self {
        Self {
            sidebar_open: true,
            inspector_open: true,
            execution_open: true,
            sidebar_width: DEFAULT_SIDEBAR_WIDTH,
            inspector_width: DEFAULT_INSPECTOR_WIDTH,
            coordinator_fraction: DEFAULT_COORDINATOR_FRACTION,
        }
    }
}

impl ShellLayout {
    pub fn columns(self) -> ColumnWidths {
        ColumnWidths {
            sidebar: if self.sidebar_open { self.sidebar_width } else { 0.0 },
            inspector: if self.inspector_open { self.inspector_width } else { 0.0 },
        }
    }

    pub fn rows(self, available: f32) -> RowHeights {
        let available = available.max(0.0);
        if !self.execution_open {
            return RowHeights { coordinator: available, execution: 0.0 };
        }
        let coordinator = if available > MIN_COORDINATOR_HEIGHT + MIN_EXECUTION_HEIGHT {
            (available * self.coordinator_fraction)
                .clamp(MIN_COORDINATOR_HEIGHT, available - MIN_EXECUTION_HEIGHT)
        } else {
            available * DEFAULT_COORDINATOR_FRACTION
        };
        RowHeights { coordinator, execution: available - coordinator }
    }

    pub fn resize_sidebar(&mut self, width: f32) {
        self.sidebar_width = width.clamp(MIN_SIDEBAR_WIDTH, MAX_SIDEBAR_WIDTH);
    }

    pub fn resize_inspector(&mut self, width: f32) {
        self.inspector_width = width.clamp(MIN_INSPECTOR_WIDTH, MAX_INSPECTOR_WIDTH);
    }

    pub fn resize_coordinator(&mut self, height: f32, available: f32) {
        if available > 0.0 {
            let clamped = height.clamp(
                MIN_COORDINATOR_HEIGHT.min(available),
                (available - MIN_EXECUTION_HEIGHT).max(0.0),
            );
            self.coordinator_fraction = clamped / available;
        }
    }
}
```

- [ ] **Step 4: Verify layout tests**

Run: `cargo test -p autoharness-ui-gpui layout::tests -- --nocapture`

Expected: 3 passed.

- [ ] **Step 5: Commit the retained layout state**

```bash
git add crates/ui-gpui/src/layout.rs crates/ui-gpui/src/lib.rs
git commit -m "Give the cockpit durable pane intent"
```

## Task 3: Extract shared black-shell components

**Files:**
- Create: `crates/ui-gpui/src/components.rs`
- Modify: `crates/ui-gpui/src/lib.rs:13-20,254-288`

- [ ] **Step 1: Create the shared component API**

Create `components.rs` with these complete recipes, moving `section` and `status_mark` out of
`lib.rs` without changing their behavior:

```rust
use gpui::prelude::*;
use gpui::{AnyElement, SharedString, div, px};

use crate::theme::{Colors, Fill, Metrics, Radius, Space, Status, Surface, Tone, Typo};

pub fn section(label: impl Into<SharedString>) -> AnyElement {
    div()
        .px(px(Space::INDENT))
        .pt(px(Space::INSET))
        .pb(px(Space::ROW_H))
        .text_size(px(Typo::SECTION_HEADER.size))
        .font_weight(Typo::SECTION_HEADER.weight)
        .text_color(Colors::text(Surface::Sidebar, Tone::Tertiary))
        .child(label.into())
        .into_any_element()
}

pub fn panel(child: impl IntoElement) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .min_w(px(0.0))
        .min_h(px(0.0))
        .bg(Colors::SURFACE)
        .border_1()
        .border_color(Colors::stroke())
        .rounded(px(Radius::PANEL))
        .child(child)
        .into_any_element()
}

pub fn compact_control(label: impl Into<SharedString>, selected: bool) -> AnyElement {
    div()
        .flex()
        .items_center()
        .h(px(Metrics::TOOLBAR_CHIP_HEIGHT))
        .px(px(Space::INSET))
        .rounded(px(Radius::CHIP))
        .bg(if selected { Fill::selected(true) } else { Fill::subtle() })
        .text_size(px(Typo::META.size))
        .text_color(Colors::text(Surface::Content, Tone::Secondary))
        .child(label.into())
        .into_any_element()
}

pub fn status_mark(status: Status, engine: &str) -> AnyElement {
    let phase = crate::theme::AnimationPhase::now();
    let mut color = status.color(engine);
    color.a *= status.opacity(phase);
    let size = 8.0 * status.scale(phase);
    let dot = div().size(px(size)).rounded_full();
    div()
        .flex()
        .flex_none()
        .w(px(14.0))
        .justify_center()
        .items_center()
        .child(if status.filled() { dot.bg(color) } else { dot.border_1().border_color(color) })
        .into_any_element()
}
```

- [ ] **Step 2: Add missing toolbar metrics**

Add to `Metrics` in `theme.rs`:

```rust
pub const TOOLBAR_CONTROL_SIZE: f32 = 26.0;
pub const TOOLBAR_CHIP_HEIGHT: f32 = 24.0;
pub const INSPECTOR_WIDTH: f32 = 360.0;
```

- [ ] **Step 3: Export the module and replace root helpers**

Add `pub mod components;` to `lib.rs`, replace internal calls with
`components::section(...)` and `components::status_mark(...)`, then delete the old helper
definitions and unused imports.

- [ ] **Step 4: Verify compile and focused tests**

Run: `cargo test -p autoharness-ui-gpui`

Expected: all selected tests pass.

Run: `cargo clippy -p autoharness-ui-gpui --all-targets -- -D warnings`

Expected: PASS.

- [ ] **Step 5: Commit the component boundary**

```bash
git add crates/ui-gpui/src/components.rs crates/ui-gpui/src/lib.rs crates/ui-gpui/src/theme.rs
git commit -m "Make shell chrome one coherent language"
```

## Task 4: Build the functional toolbar and pane controls

**Files:**
- Create: `crates/ui-gpui/src/toolbar.rs`
- Modify: `crates/ui-gpui/src/lib.rs:32-137,290-332,439-486`

- [ ] **Step 1: Add an exhaustively tested toolbar action model**

Create `toolbar.rs` with:

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolbarAction {
    ToggleSidebar,
    ToggleExecution,
    ToggleInspector,
    OpenPalette,
}

pub fn labels(layout: crate::layout::ShellLayout) -> [(&'static str, bool, ToolbarAction); 4] {
    [
        ("Repositories", layout.sidebar_open, ToolbarAction::ToggleSidebar),
        ("Execution", layout.execution_open, ToolbarAction::ToggleExecution),
        ("Changes", layout.inspector_open, ToolbarAction::ToggleInspector),
        ("Search", false, ToolbarAction::OpenPalette),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_visible_toolbar_control_has_a_real_action() {
        let labels = labels(crate::layout::ShellLayout::default());
        assert_eq!(labels.len(), 4);
        assert_eq!(labels[0].2, ToolbarAction::ToggleSidebar);
        assert_eq!(labels[3].2, ToolbarAction::OpenPalette);
    }
}
```

- [ ] **Step 2: Verify the action test passes before rendering code**

Export `pub mod toolbar;` and run:

`cargo test -p autoharness-ui-gpui toolbar::tests -- --nocapture`

Expected: 1 passed.

- [ ] **Step 3: Retain layout and dispatch actions in `Shell`**

Add `layout: layout::ShellLayout` to `Shell`, initialize it with `Default::default()`, and add:

```rust
fn toolbar_action(&mut self, action: toolbar::ToolbarAction, cx: &mut Context<Self>) {
    match action {
        toolbar::ToolbarAction::ToggleSidebar => self.layout.sidebar_open ^= true,
        toolbar::ToolbarAction::ToggleExecution => self.layout.execution_open ^= true,
        toolbar::ToolbarAction::ToggleInspector => self.layout.inspector_open ^= true,
        toolbar::ToolbarAction::OpenPalette => {
            self.palette = Some(PaletteState {
                query: query_editor::QueryEditor::default(),
                selected: 0,
            });
        }
    }
    cx.notify();
}
```

Add shortcuts in `on_key`: Command-B toggles the sidebar, Command-J toggles execution,
Command-Shift-D toggles the inspector, and Command-K keeps opening search.

- [ ] **Step 4: Render a real toolbar from current state**

Move `title_bar` to `toolbar.rs` as `pub(crate) fn view(...) -> AnyElement`. Its left side must
render AutoHarness and the current status; its center must render route mode `"Auto"` (the current
router is always automatic), current engine, and run state; its trailing controls must call `Shell::toolbar_action`
through pointer handlers. Do not render History, Worktrees, notifications, or Settings yet—those
controls would be dead until later slices.

- [ ] **Step 5: Verify keyboard and pointer compilation**

Run: `cargo test -p autoharness-ui-gpui toolbar::tests`

Expected: PASS.

Run: `cargo clippy -p autoharness-ui-gpui --all-targets -- -D warnings`

Expected: PASS.

- [ ] **Step 6: Commit the functional toolbar**

```bash
git add crates/ui-gpui/src/toolbar.rs crates/ui-gpui/src/lib.rs
git commit -m "Put real cockpit controls in reach"
```

## Task 5: Split the shell into sidebar, coordinator, workbench, and inspector

**Files:**
- Create: `crates/ui-gpui/src/sidebar.rs`
- Create: `crates/ui-gpui/src/coordinator.rs`
- Create: `crates/ui-gpui/src/workbench.rs`
- Create: `crates/ui-gpui/src/inspector.rs`
- Modify: `crates/ui-gpui/src/lib.rs:488-985`

- [ ] **Step 1: Write pure inspector selection tests**

Start `inspector.rs` with:

```rust
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum InspectorTab {
    #[default]
    Changes,
    Checks,
    Activity,
}

impl InspectorTab {
    pub const ALL: [Self; 3] = [Self::Changes, Self::Checks, Self::Activity];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Changes => "Changes",
            Self::Checks => "Checks",
            Self::Activity => "Activity",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_foundation_tab_is_backed_by_existing_state() {
        assert_eq!(InspectorTab::ALL.map(InspectorTab::label), ["Changes", "Checks", "Activity"]);
    }
}
```

- [ ] **Step 2: Export the modules and verify the test**

Add module declarations for all four files and run:

`cargo test -p autoharness-ui-gpui inspector::tests -- --nocapture`

Expected: 1 passed.

- [ ] **Step 3: Move sidebar projection and view intact**

Move the current `sidebar` function into `sidebar.rs` as:

```rust
pub(crate) fn view(state: &UiState, width: f32, cx: &mut Context<Shell>) -> AnyElement
```

Keep current repository/thread selection handlers, status priority, engine rows, and scroll IDs.
Change visible headings from `Projects` to `Repositories` and from `Engines` to `Engines`. Replace
the empty-state command string with `"Add a repository to begin"`; clicking the empty state sends
`Command::RefreshProjects` so it is not decorative.

- [ ] **Step 4: Move coordinator view intact and rename the placeholder**

Move `coordinator` and `speech` into `coordinator.rs`. Its public entry point is:

```rust
pub(crate) fn view(state: &UiState, prompt: &QueryEditor) -> AnyElement
```

Keep real transcript and composer behavior. Display `"Run objective..."` only when there is no
input; otherwise display the editor's measured text and caret.

- [ ] **Step 5: Put graph and activity into the lower workbench**

Move only graph rendering and the activity timeline out of `review` into:

```rust
pub(crate) fn view(state: &UiState) -> AnyElement
```

in `workbench.rs`. It must render `Execution` as its header, the current `GraphView` in wave order,
and the existing `summary + events` activity rows. Empty state is `"Execution appears when a run
is routed"` and has no fake progress.

- [ ] **Step 6: Put real diff/check/activity data into the inspector**

Move `diff_view`, `output_view`, and `terminal_palette` into `inspector.rs`. Add:

```rust
pub(crate) fn view(
    state: &UiState,
    selected: InspectorTab,
    width: f32,
    cx: &mut Context<Shell>,
) -> AnyElement
```

The tabs map exactly: Changes → parsed `state.diff`, Checks → `state.check_output`, Activity →
`state.summary + state.events`. An unavailable tab renders a quiet, truthful empty state and no
action button.

- [ ] **Step 7: Compose the approved geometry in `Shell::render`**

Add `inspector_tab: InspectorTab` to `Shell`. Replace the fixed row with this structure:

```rust
let columns = self.layout.columns();
let content = div()
    .flex()
    .flex_1()
    .min_h(px(0.0))
    .gap(px(Space::ROW_H))
    .p(px(Space::ROW_H))
    .when(columns.sidebar > 0.0, |row| {
        row.child(sidebar::view(&state, columns.sidebar, cx))
    })
    .child(
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w(px(0.0))
            .min_h(px(0.0))
            .gap(px(Space::ROW_H))
            .child(coordinator::view(&state, &self.prompt))
            .when(self.layout.execution_open, |center| center.child(workbench::view(&state))),
    )
    .when(columns.inspector > 0.0, |row| {
        row.child(inspector::view(&state, self.inspector_tab, columns.inspector, cx))
    });
```

Use flex weights for the first pass; Task 6 adds pointer resizing after the modules compile.

- [ ] **Step 8: Verify all UI state tests and compile**

Run: `cargo test -p autoharness-ui-gpui`

Expected: all existing 57 tests plus new theme/layout/toolbar/inspector tests pass.

Run: `cargo clippy -p autoharness-ui-gpui --all-targets -- -D warnings`

Expected: PASS.

- [ ] **Step 9: Commit the focused view boundaries**

```bash
git add crates/ui-gpui/src/lib.rs crates/ui-gpui/src/sidebar.rs crates/ui-gpui/src/coordinator.rs crates/ui-gpui/src/workbench.rs crates/ui-gpui/src/inspector.rs
git commit -m "Let each cockpit surface own one job"
```

## Task 6: Add retained resizing and inspector tab interactions

**Files:**
- Modify: `crates/ui-gpui/src/layout.rs`
- Modify: `crates/ui-gpui/src/lib.rs`
- Modify: `crates/ui-gpui/src/inspector.rs`

- [ ] **Step 1: Add drag state to the root**

Add these private marker types and fields:

```rust
#[derive(Clone, Copy)]
enum ResizeTarget { Sidebar, Workbench, Inspector }

struct ResizeDrag {
    target: ResizeTarget,
    origin_x: f32,
    origin_y: f32,
    initial: layout::ShellLayout,
}
```

Add `resize_drag: Option<ResizeDrag>` to `Shell` and initialize it to `None`.

- [ ] **Step 2: Add 9-point resize hit areas**

At the sidebar's trailing edge, workbench's top edge, and inspector's leading edge, render absolute
9-point hit targets with `CursorStyle::ResizeLeftRight` or `ResizeUpDown`. On mouse down, snapshot
the pointer and `ShellLayout`; on mouse move, call the matching `resize_*` method; on mouse up or
mouse-up-out, clear `resize_drag`. Double-click resets the relevant dimension to its default.

- [ ] **Step 3: Make inspector tabs clickable**

Pass `&mut Context<Shell>` into `inspector::view`. Each tab row uses an element ID of
`inspector-tab-{label}` and sets `shell.inspector_tab` on left mouse down. Selected tabs use
`Fill::selected(true)`; unselected tabs use alpha-only text hierarchy.

- [ ] **Step 4: Add keyboard tab cycling**

In `on_key`, when Command-Shift-Right or Command-Shift-Left is pressed and the inspector is open,
cycle through `InspectorTab::ALL` with wrapping. This must not consume those keys when the
inspector is closed.

- [ ] **Step 5: Verify layout and clippy gates**

Run: `cargo test -p autoharness-ui-gpui layout::tests inspector::tests`

Expected: PASS.

Run: `cargo clippy -p autoharness-ui-gpui --all-targets -- -D warnings`

Expected: PASS.

- [ ] **Step 6: Commit retained interactions**

```bash
git add crates/ui-gpui/src/layout.rs crates/ui-gpui/src/lib.rs crates/ui-gpui/src/inspector.rs
git commit -m "Keep the workbench shaped around the task"
```

## Task 7: Add deterministic preview and visual inspection

**Files:**
- Create: `crates/ui-gpui/src/preview.rs`
- Create: `crates/ui-gpui/examples/shell_preview.rs`
- Modify: `crates/ui-gpui/src/client.rs:417-451`
- Modify: `crates/ui-gpui/src/lib.rs:49-69,226-252`

- [ ] **Step 1: Add a disconnected preview client test**

Add to `client::tests`:

```rust
#[test]
fn a_preview_client_preserves_its_fixture_without_connecting() {
    let fixture = UiState { status: "preview".into(), connected: true, ..UiState::default() };
    let client = DaemonClient::preview(fixture);
    assert_eq!(client.state.lock().unwrap().status, "preview");
}
```

- [ ] **Step 2: Verify the preview constructor is missing**

Run: `cargo test -p autoharness-ui-gpui client::tests::a_preview_client_preserves_its_fixture_without_connecting -- --exact`

Expected: compile failure because `DaemonClient::preview` does not exist.

- [ ] **Step 3: Implement the disconnected constructor**

Add:

```rust
pub fn preview(state: UiState) -> Self {
    let (commands, _rx) = mpsc::unbounded_channel();
    Self { commands, state: Arc::new(Mutex::new(state)) }
}
```

- [ ] **Step 4: Create a deterministic populated state**

Create `preview.rs` with `pub fn populated() -> UiState`. It must contain one repository, four run
threads spanning running/awaiting-approval/succeeded/blocked, both engines, a four-node two-wave
graph, real transcript lines, summary/activity rows, ANSI check output, and a small unified diff.
Use only the existing public `UiState`, `Project`, `RunView`, `EngineStatus`, `GraphView`, and
`parse_diff` types; do not add preview-only fields to production models.

- [ ] **Step 5: Allow the shell to receive an injected client**

Refactor construction to:

```rust
fn new(cx: &mut Context<Self>) -> Self {
    Self::with_client(DaemonClient::spawn(), cx)
}

fn with_client(client: DaemonClient, cx: &mut Context<Self>) -> Self {
    // retain the existing repaint task
    Self {
        client,
        focus: cx.focus_handle(),
        prompt: QueryEditor::default(),
        palette: None,
        layout: ShellLayout::default(),
        inspector_tab: InspectorTab::Changes,
        resize_drag: None,
    }
}
```

Extract the window-opening body into `fn launch(client: Option<DaemonClient>)` and expose:

```rust
pub fn run() { launch(None); }

pub fn run_preview() {
    launch(Some(DaemonClient::preview(preview::populated())));
}
```

- [ ] **Step 6: Add the preview executable**

Create `examples/shell_preview.rs`:

```rust
fn main() {
    autoharness_ui_gpui::run_preview();
}
```

- [ ] **Step 7: Run and capture the approved states**

Run: `cargo run -p autoharness-ui-gpui --example shell_preview`

Expected: a populated true-black AutoHarness cockpit opens without starting `autoharnessd` or a
provider process.

Capture: `screencapture -o -x /tmp/autoharness-black-shell.png`

Inspect the image for traffic-light clearance, text collisions, clipping, dead-looking controls,
wrong elevation colors, unusable narrow panes, and mismatch with the approved reference.

- [ ] **Step 8: Run the preview test and commit**

Run: `cargo test -p autoharness-ui-gpui client::tests::a_preview_client_preserves_its_fixture_without_connecting -- --exact`

Expected: PASS.

```bash
git add crates/ui-gpui/src/client.rs crates/ui-gpui/src/lib.rs crates/ui-gpui/src/preview.rs crates/ui-gpui/examples/shell_preview.rs
git commit -m "Make the cockpit visually testable without tokens"
```

## Task 8: Document attribution and verify the slice

**Files:**
- Modify: `NOTICE`
- Modify: `AGENTS.md`
- Modify: `docs/superpowers/specs/2026-08-05-diri-experience-transplant-design.md`
- Add: `docs/superpowers/specs/assets/autoharness-diri-black-reference.png`

- [ ] **Step 1: Update Apache attribution**

Append the adapted modules to the existing diri paragraph in `NOTICE`: workbench layout,
retained pane seams, compact control components, inspector structure, and navigation geometry.

- [ ] **Step 2: Replace stale UI guidance**

In `AGENTS.md`, document the new focused module map, true-black token rule, functional-control
rule, and preview command:

```text
cargo run -p autoharness-ui-gpui --example shell_preview
```

Delete claims about missing or removed preview examples that are no longer true.

- [ ] **Step 3: Run formatting**

Run: `cargo fmt --all -- --check`

Expected: PASS. If it fails, run `cargo fmt --all`, inspect the formatting-only diff, and rerun the
check.

- [ ] **Step 4: Run strict linting**

Run: `cargo clippy --workspace --all-targets -- -D warnings`

Expected: PASS with zero warnings.

- [ ] **Step 5: Run build and full tests**

Run: `cargo build --workspace`

Expected: PASS with zero project warnings.

Run: `cargo test --workspace`

Expected: at least the baseline 233 tests plus every new shell test pass, with 0 failures.

- [ ] **Step 6: Run a real debug smoke**

Run: `cargo run -p autoharness`

Expected: the app connects to or starts the daemon, existing repositories/runs replay, project and
run rows remain clickable, Command-K opens the palette, Command-B/J/Shift-D toggle real panes, and
submitting text still creates/continues through typed `Command` values. Do not start a live
provider turn; use existing replayed state or FakeEngine-backed fixtures.

- [ ] **Step 7: Inspect final repository state**

Run: `git status --short`

Expected: only the plan/spec visual documentation changes remain if they were not committed with
the implementation tasks; no temporary screenshots or build products are tracked.

- [ ] **Step 8: Commit documentation and acceptance evidence**

```bash
git add NOTICE AGENTS.md docs/superpowers/specs/2026-08-05-diri-experience-transplant-design.md docs/superpowers/specs/assets/autoharness-diri-black-reference.png
git commit -m "Keep the transplanted shell accountable"
```

## Program continuation

This plan deliberately ends with working software. After it is green and visually approved,
write separate detailed plans for: navigation and overview; history adoption; worktree list and
safe reclaim RPCs; notifications/sounds/menu bar; settings and usage; richer artifact review; and
the signed app-owned updater. Those plans must use the retained boundaries created here and may
not reintroduce a monolithic `lib.rs` or dead preview controls.
