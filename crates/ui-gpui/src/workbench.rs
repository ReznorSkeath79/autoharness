use gpui::prelude::*;
use gpui::{
    Animation, AnimationExt, AnyElement, Context, ElementId, Role, SharedString,
    StatefulInteractiveElement, div, ease_out_quint, px,
};

use crate::Shell;
use crate::client::{GraphNodeView, UiState};
use crate::components;
use crate::motion;
use crate::theme::{self, Colors, Fill, Ink, Radius, Space, Status, Surface, Tone, Typo};

const PARALLEL_NODE_HEIGHT: f32 = 78.0;
const ACTIVITY_ROW_VERTICAL_PADDING: f32 = 3.0;
const COMPACT_DAG_TEXT_SIZE: f32 = Typo::META.size;
const ACTIVITY_TIME_WIDTH: f32 = 72.0;
const ACTIVITY_ENGINE_WIDTH: f32 = 64.0;
const ACTIVITY_ACTION_WIDTH: f32 = 84.0;
const ACTIVITY_PATH_WIDTH: f32 = 120.0;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum ExecutionMode {
    #[default]
    Parallel,
    Timeline,
}

/// Whether the execution pane has anything to show.
///
/// A pane whose entire content is "Execution appears when a run is routed"
/// is a permanent apology occupying a third of the window. It collapses to
/// its header until there is a graph or terminal output to put in it.
pub(crate) fn has_anything_to_show(state: &crate::client::UiState) -> bool {
    has_terminal_output(state)
        || state
            .selected_detail()
            .and_then(|detail| detail.graph.as_ref())
            .is_some_and(|graph| !graph.nodes.is_empty())
}

/// Whether the selected run is a terminal-driven one that has painted
/// something.
pub(crate) fn has_terminal_output(state: &crate::client::UiState) -> bool {
    state
        .selected_detail()
        .is_some_and(|detail| !detail.terminal.is_empty())
}

/// What the agent painted, newest at the bottom.
///
/// Deliberately plain lines rather than a cell grid. The screen is already
/// emulated where the PTY is owned, and what reaches here is settled text —
/// so a full VT renderer in the client would re-solve a problem that is
/// already solved, for output that no longer has cursor movement in it.
fn terminal_body(state: &crate::client::UiState) -> impl gpui::IntoElement + use<> {
    let lines: Vec<String> = state
        .selected_detail()
        .map(|detail| detail.terminal.clone())
        .unwrap_or_default();
    div()
        .id("terminal-output")
        .role(Role::Log)
        .aria_label("Terminal output")
        .flex()
        .flex_col()
        .flex_1()
        .min_h(px(0.0))
        .px(px(Space::INDENT))
        .py(px(8.0))
        .gap(px(1.0))
        .overflow_y_scroll()
        .children(lines.into_iter().map(|line| {
            div()
                .font_family("SF Mono")
                .text_size(px(Typo::META.size))
                .text_color(Colors::text(Surface::Content, Tone::Secondary))
                .child(SharedString::from(line))
        }))
}

/// Whether this run's graph has enough in it for the two views to differ.
///
/// A direct run is one node: "Parallel" and "Timeline" render it identically,
/// so offering the toggle asks the user to choose between two identical
/// things and teaches them the control does nothing.
pub(crate) fn has_a_shape_worth_two_views(state: &crate::client::UiState) -> bool {
    state
        .selected_detail()
        .and_then(|detail| detail.graph.as_ref())
        .is_some_and(|graph| graph.nodes.len() > 1)
}

impl ExecutionMode {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Parallel => "Parallel view",
            Self::Timeline => "Timeline",
        }
    }
}

/// Test-only shape check: production renders the graph directly; this proves
/// the two modes still differ materially before the toggle is offered.
#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ExecutionProjection {
    Empty,
    Parallel {
        waves: Vec<Vec<String>>,
        edges: usize,
    },
}

#[cfg(test)]
pub(crate) fn execution_projection(state: &UiState, mode: ExecutionMode) -> ExecutionProjection {
    let graph = state
        .selected_detail()
        .and_then(|detail| detail.graph.as_ref())
        .or(state.graph.as_ref());
    match (mode, graph) {
        (ExecutionMode::Parallel, Some(graph)) if !graph.nodes.is_empty() => {
            let mut waves = vec![Vec::new(); graph.wave_count()];
            for node in &graph.nodes {
                if let Some(wave) = waves.get_mut(node.wave) {
                    wave.push(node.id.clone());
                }
            }
            ExecutionProjection::Parallel {
                waves,
                edges: graph.edges.len(),
            }
        }
        _ => ExecutionProjection::Empty,
    }
}

/// The lower center pane: real routed execution graph or its activity timeline.
pub(crate) fn view(
    state: &UiState,
    mode: ExecutionMode,
    available_width: f32,
    transition_generation: u64,
    tab_direction: f32,
    selected_graph_node: Option<&str>,
    cx: &mut Context<Shell>,
) -> AnyElement {
    let transition_id =
        ElementId::Name(format!("execution-tab-transition-{transition_generation}").into());
    // An agent driven on a terminal has no graph to draw; what it painted IS
    // the execution, and without this the pane says "Execution appears when a
    // run is routed" while the agent works.
    let body = if has_terminal_output(state) {
        terminal_body(state).into_any_element()
    } else {
        match mode {
            ExecutionMode::Parallel => {
                parallel_body(state, available_width, selected_graph_node, cx).into_any_element()
            }
            ExecutionMode::Timeline => timeline_body(state).into_any_element(),
        }
    };
    components::panel()
        .id("execution-pane")
        .role(Role::TabPanel)
        .aria_label("Execution")
        .flex_1()
        .min_w(px(0.0))
        .min_h(px(0.0))
        .border_t_1()
        .border_color(Colors::stroke())
        .child(
            div()
                .id("execution-tabs")
                .role(Role::TabList)
                .aria_label("Execution view")
                .flex()
                .items_center()
                .px(px(Space::INDENT))
                .py(px(5.0))
                .child(
                    div()
                        .flex_1()
                        .text_size(px(Typo::SECTION_HEADER.size))
                        .font_weight(Typo::SECTION_HEADER.weight)
                        .child("Execution"),
                )
                // Two renderings of the same graph, which for a direct run —
                // most runs — are the same single node drawn twice. The choice
                // only means something once there is a shape to choose between,
                // so it appears then.
                .when(has_a_shape_worth_two_views(state), |tabs| {
                    tabs.child(mode_button(ExecutionMode::Parallel, mode, cx))
                        .child(mode_button(ExecutionMode::Timeline, mode, cx))
                }),
        )
        .children(route_strip(state))
        .child(
            div()
                .id("execution-activity-table")
                .role(Role::Region)
                .aria_label(mode.label())
                .flex()
                .flex_col()
                .flex_1()
                .min_h(px(0.0))
                .overflow_hidden()
                .child(div().relative().size_full().child(body).with_animation(
                    transition_id,
                    Animation::new(motion::TAB_TRANSITION).with_easing(ease_out_quint()),
                    move |body, delta| {
                        body.left(px(motion::tab_offset(tab_direction, delta, false)))
                            .opacity(motion::tab_opacity(delta))
                    },
                )),
        )
        .into_any_element()
}

fn mode_button(
    mode: ExecutionMode,
    selected: ExecutionMode,
    cx: &mut Context<Shell>,
) -> AnyElement {
    components::compact_control(mode.label())
        .id(ElementId::Name(
            format!("execution-mode-{}", mode.label()).into(),
        ))
        .role(Role::Tab)
        .aria_label(mode.label())
        .aria_selected(mode == selected)
        .tab_index(0)
        .focus_visible(|button| button.border_color(theme::white(0.48)))
        .when(mode == selected, |button| button.bg(Fill::selected(true)))
        .hover(|button| button.bg(theme::white(Fill::HOVER)))
        .cursor_pointer()
        .on_click(cx.listener(move |shell, _, _, cx| {
            shell.set_execution_mode(mode, cx);
        }))
        .into_any_element()
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct ParallelGraphMetrics {
    pub node_width: f32,
    pub arrow_width: f32,
    pub wave_gap: f32,
    pub row_gap: f32,
    pub total_width: f32,
    pub total_height: f32,
    pub visible_node_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ParallelGraphNodeLabel {
    pub full: String,
    pub lines: Vec<String>,
    pub truncated: bool,
    pub accessible_label: String,
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct ExecutionDensityProjection {
    pub graph_nodes: usize,
    pub activity_rows: usize,
    pub total_height: f32,
}

#[cfg(test)]
pub(crate) fn execution_density_projection(
    state: &UiState,
    available_width: f32,
) -> ExecutionDensityProjection {
    let graph = parallel_graph_metrics(state, available_width);
    let graph_nodes = graph.map_or(0, |metrics| metrics.visible_node_count);
    let graph_height = graph.map_or(0.0, |metrics| metrics.total_height);
    let activity_rows = state
        .selected_detail()
        .map_or(0, |detail| detail.activity.len());
    let total_height = 31.0 + 15.0 + graph_height + 20.0 + activity_rows as f32 * 22.0 + 13.0;
    ExecutionDensityProjection {
        graph_nodes,
        activity_rows,
        total_height,
    }
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct ActivityTableColumns {
    pub detail_width: f32,
    pub path_width: f32,
}

#[cfg(test)]
pub(crate) fn activity_table_columns(available_width: f32) -> ActivityTableColumns {
    let fixed_width = ACTIVITY_TIME_WIDTH
        + ACTIVITY_ENGINE_WIDTH
        + ACTIVITY_ACTION_WIDTH
        + ACTIVITY_PATH_WIDTH
        + Space::INDENT * 4.0;
    ActivityTableColumns {
        detail_width: (available_width - fixed_width).max(0.0),
        path_width: ACTIVITY_PATH_WIDTH,
    }
}

pub(crate) fn parallel_graph_metrics(
    state: &UiState,
    available_width: f32,
) -> Option<ParallelGraphMetrics> {
    let graph = state
        .selected_detail()
        .and_then(|detail| detail.graph.as_ref())
        .or(state.graph.as_ref())?;
    if graph.nodes.is_empty() {
        return None;
    }

    let wave_count = graph.wave_count().max(1);
    let max_stack = (0..wave_count)
        .map(|wave| graph.nodes.iter().filter(|node| node.wave == wave).count())
        .max()
        .unwrap_or(1)
        .max(1);
    let arrow_width = 16.0;
    let wave_gap = 6.0;
    let row_gap = 4.0;
    let overhead = wave_count.saturating_sub(1) as f32 * (arrow_width + wave_gap * 2.0);
    let node_width = ((available_width - overhead) / wave_count as f32)
        .floor()
        .clamp(70.0, 128.0);
    let total_width = node_width * wave_count as f32 + overhead;
    let total_height =
        max_stack as f32 * PARALLEL_NODE_HEIGHT + max_stack.saturating_sub(1) as f32 * row_gap;

    Some(ParallelGraphMetrics {
        node_width,
        arrow_width,
        wave_gap,
        row_gap,
        total_width,
        total_height,
        visible_node_count: graph.nodes.len(),
    })
}

#[cfg(test)]
pub(crate) fn parallel_graph_node_labels(
    state: &UiState,
    available_width: f32,
) -> Option<Vec<ParallelGraphNodeLabel>> {
    let graph = state
        .selected_detail()
        .and_then(|detail| detail.graph.as_ref())
        .or(state.graph.as_ref())?;
    let metrics = parallel_graph_metrics(state, available_width)?;
    Some(
        graph
            .nodes
            .iter()
            .map(|node| node_label_projection(&node.id, metrics.node_width))
            .collect(),
    )
}

fn parallel_body(
    state: &UiState,
    available_width: f32,
    selected_graph_node: Option<&str>,
    cx: &mut Context<Shell>,
) -> AnyElement {
    let graph = state
        .selected_detail()
        .and_then(|detail| detail.graph.as_ref())
        .or(state.graph.as_ref());
    let Some(graph) = graph.filter(|graph| !graph.nodes.is_empty()) else {
        return empty_state("Execution appears when a run is routed");
    };
    let metrics = parallel_graph_metrics(state, available_width).unwrap_or(ParallelGraphMetrics {
        node_width: 92.0,
        arrow_width: 16.0,
        wave_gap: 6.0,
        row_gap: 6.0,
        total_width: available_width,
        total_height: 134.0,
        visible_node_count: graph.nodes.len(),
    });
    let engine = state.engine.clone();

    div()
        .flex()
        .flex_col()
        .flex_1()
        .min_h(px(0.0))
        .px(px(10.0))
        .pb(px(4.0))
        .gap(px(4.0))
        .child(
            div()
                .text_size(px(Typo::META.size))
                .text_color(Colors::text(Surface::Sidebar, Tone::Secondary))
                .child(SharedString::from(format!(
                    "DAG · {} nodes · {} dependencies",
                    graph.nodes.len(),
                    graph.edges.len()
                ))),
        )
        .child(
            div()
                .flex()
                .items_center()
                .flex_none()
                .w(px(metrics.total_width))
                .max_w_full()
                .children((0..graph.wave_count()).map(|wave| {
                    div()
                        .flex()
                        .items_center()
                        .gap(px(metrics.wave_gap))
                        .flex_none()
                        .children(Some(
                            div()
                                .flex()
                                .flex_col()
                                .flex_none()
                                .w(px(metrics.node_width))
                                .gap(px(metrics.row_gap))
                                .children(
                                    graph
                                        .nodes
                                        .iter()
                                        .filter(move |node| node.wave == wave)
                                        .map(|node| {
                                            node_box(
                                                node,
                                                &engine,
                                                metrics.node_width,
                                                state.run_id.as_deref(),
                                                selected_graph_node == Some(node.id.as_str()),
                                                cx,
                                            )
                                        }),
                                ),
                        ))
                        .when(wave + 1 < graph.wave_count(), |column| {
                            column.child(
                                div()
                                    .flex_none()
                                    .w(px(metrics.arrow_width))
                                    .text_size(px(18.0))
                                    .text_color(Colors::text(Surface::Sidebar, Tone::Tertiary))
                                    .child("→"),
                            )
                        })
                })),
        )
        .child(dependency_strip(graph))
        .children(
            selected_graph_node
                .and_then(|id| graph.nodes.iter().find(|node| node.id == id))
                .map(node_detail_strip),
        )
        .child(activity_table(state))
        .into_any_element()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NodeControl {
    Retry,
    Cancel,
}

pub(crate) fn node_control(node: &GraphNodeView) -> Option<NodeControl> {
    if node.can_cancel {
        Some(NodeControl::Cancel)
    } else if node.can_retry {
        Some(NodeControl::Retry)
    } else {
        None
    }
}

fn node_box(
    node: &GraphNodeView,
    fallback_engine: &str,
    node_width: f32,
    run_id: Option<&str>,
    selected: bool,
    cx: &mut Context<Shell>,
) -> AnyElement {
    let declared_role = matches!(
        node.role.as_str(),
        "reader" | "editor" | "verifier" | "integration"
    );
    let engine = if node.role.is_empty() || declared_role {
        fallback_engine
    } else {
        &node.role
    };
    let task_label = if node.objective.is_empty() {
        &node.id
    } else {
        &node.objective
    };
    let label = node_label_projection(task_label, node_width);
    let status = Status::of_node(&node.state);
    let control = node_control(node).and_then(|control| {
        let run_id = run_id?.to_string();
        let node_id = node.id.clone();
        let (label, retry) = match control {
            NodeControl::Retry => ("Retry", true),
            NodeControl::Cancel => ("Cancel", false),
        };
        let accessible_label = format!("{label} execution node {node_id}");
        Some(
            components::compact_control(label)
                .id(ElementId::Name(
                    format!("node-control-{run_id}-{node_id}").into(),
                ))
                .role(Role::Button)
                .aria_label(accessible_label)
                .tab_index(0)
                .focus_visible(|control| control.border_color(theme::white(0.48)))
                .hover(|control| control.bg(theme::white(Fill::HOVER)))
                .cursor_pointer()
                .on_click(cx.listener(move |shell, _, _, cx| {
                    cx.stop_propagation();
                    shell.node_control(&run_id, &node_id, retry, cx);
                })),
        )
    });
    let dependencies = if node.depends_on.is_empty() {
        "no dependencies".to_string()
    } else {
        format!("after {}", node.depends_on.join(", "))
    };
    let node_accessible_label = format!(
        "Execution node {}, id {}, {}, engine {}, {}. Select for details",
        label.accessible_label, node.id, node.state, engine, dependencies
    );
    let selected_id = node.id.clone();
    div()
        .id(ElementId::Name(
            format!("execution-node-{}", node.id).into(),
        ))
        .role(Role::Button)
        .aria_label(node_accessible_label)
        .tab_index(0)
        .focus_visible(|node| node.border_color(theme::white(0.48)))
        .cursor_pointer()
        .hover(|node| node.bg(theme::white(Fill::HOVER)))
        .on_click(cx.listener(move |shell, _, _, cx| {
            shell.select_graph_node(&selected_id, cx);
        }))
        .flex()
        .flex_col()
        .gap(px(1.0))
        .flex_none()
        .w(px(node_width))
        .h(px(PARALLEL_NODE_HEIGHT))
        .min_w(px(0.0))
        .px(px(7.0))
        .py(px(2.0))
        .rounded(px(Radius::ROW))
        .border_1()
        .when(selected, |node| node.bg(Fill::selected(true)))
        .border_color(if selected || status.filled() {
            status.color(engine)
        } else {
            Colors::stroke()
        })
        .child(
            div()
                .flex()
                .flex_none()
                .flex_col()
                .gap(px(0.0))
                .text_size(px(COMPACT_DAG_TEXT_SIZE))
                .font_weight(Typo::ROW_EMPHASIZED.weight)
                .children(label.lines.into_iter().map(|line| {
                    div()
                        .flex_none()
                        .min_w(px(0.0))
                        .child(SharedString::from(line))
                })),
        )
        .child(
            div()
                .flex_none()
                .truncate()
                .text_size(px(COMPACT_DAG_TEXT_SIZE))
                .text_color(Ink::working(engine))
                .child(SharedString::from(if declared_role {
                    format!("{engine} · {}", node.role)
                } else {
                    engine.to_string()
                })),
        )
        .child(
            div()
                .flex()
                .flex_none()
                .items_center()
                .gap(px(6.0))
                .min_w(px(0.0))
                .truncate()
                .text_size(px(COMPACT_DAG_TEXT_SIZE))
                .text_color(Colors::text(Surface::Sidebar, Tone::Secondary))
                .child(status_detail(node))
                .children(control),
        )
        .children(node.progress_percent.map(progress_bar))
        .into_any_element()
}

fn dependency_strip(graph: &crate::client::GraphView) -> AnyElement {
    let flow = if graph.edges.is_empty() {
        "No dependencies".to_string()
    } else {
        graph
            .edges
            .iter()
            .map(|(from, to)| format!("{from} → {to}"))
            .collect::<Vec<_>>()
            .join("  ·  ")
    };
    div()
        .id("execution-dependency-flow")
        .role(Role::Region)
        .aria_label(format!("Execution dependencies: {flow}"))
        .flex_none()
        .truncate()
        .py(px(3.0))
        .text_size(px(Typo::META.size))
        .text_color(Colors::text(Surface::Sidebar, Tone::Tertiary))
        .child(SharedString::from(format!("Flow  {flow}")))
        .into_any_element()
}

fn node_detail_strip(node: &GraphNodeView) -> AnyElement {
    let dependencies = if node.depends_on.is_empty() {
        "root".to_string()
    } else {
        format!("after {}", node.depends_on.join(", "))
    };
    let scope = if node.file_scope.is_empty() {
        "read-only or repository-wide".to_string()
    } else {
        node.file_scope.join(", ")
    };
    let checks = if node.acceptance_checks.is_empty() {
        "no node-local check".to_string()
    } else {
        node.acceptance_checks.join(" && ")
    };
    div()
        .id("execution-node-detail")
        .role(Role::Region)
        .aria_label(format!(
            "Selected node {}. {}. Scope {}. Checks {}",
            node.objective, dependencies, scope, checks
        ))
        .flex()
        .flex_none()
        .items_center()
        .gap(px(Space::INDENT))
        .px(px(Space::INDENT))
        .py(px(5.0))
        .rounded(px(Radius::ROW))
        .bg(Fill::subtle())
        .text_size(px(Typo::META.size))
        .child(
            div()
                .min_w(px(120.0))
                .font_weight(Typo::ROW_EMPHASIZED.weight)
                .child(SharedString::from(node.objective.clone())),
        )
        .child(
            div()
                .flex_none()
                .text_color(Colors::text(Surface::Sidebar, Tone::Tertiary))
                .child(SharedString::from(dependencies)),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .truncate()
                .text_color(Colors::text(Surface::Sidebar, Tone::Secondary))
                .child(SharedString::from(format!("Scope  {scope}"))),
        )
        .child(
            div()
                .max_w(px(220.0))
                .truncate()
                .text_color(Colors::text(Surface::Sidebar, Tone::Secondary))
                .child(SharedString::from(format!("Checks  {checks}"))),
        )
        .into_any_element()
}

fn node_label_projection(label: &str, _node_width: f32) -> ParallelGraphNodeLabel {
    let words = label.split_whitespace().collect::<Vec<_>>();
    let lines = match words.len() {
        0 => vec![label.to_string()],
        1 | 2 => vec![label.to_string()],
        3 => vec![words[..2].join(" "), words[2].to_string()],
        _ => {
            let total_chars = label.chars().count();
            let mut best_split = 1;
            let mut best_score = usize::MAX;
            for split in 1..words.len() {
                let left = words[..split].join(" ");
                let right = words[split..].join(" ");
                let max_line = left.chars().count().max(right.chars().count());
                let balance = max_line.abs_diff(total_chars / 2);
                if balance < best_score {
                    best_score = balance;
                    best_split = split;
                }
            }
            vec![words[..best_split].join(" "), words[best_split..].join(" ")]
        }
    };
    ParallelGraphNodeLabel {
        full: label.into(),
        lines,
        truncated: false,
        accessible_label: label.into(),
    }
}

fn status_detail(node: &GraphNodeView) -> SharedString {
    let duration = node
        .duration_ms
        .map(duration_label)
        .unwrap_or_else(|| node.detail.clone());
    SharedString::from(match node.state.as_str() {
        "succeeded" => format!("✓ {duration}"),
        "running" => duration,
        "pending" => "Waiting".into(),
        other => other.into(),
    })
}

/// What the router decided, pinned over the execution it produced: shape,
/// confidence, why, and the implication. Without it the pane shows work with
/// no stated cause — the transcript carries the same words on arrival.
fn route_strip(state: &UiState) -> Option<AnyElement> {
    let route = state
        .selected_detail()
        .and_then(|detail| detail.route.as_ref())
        .filter(|route| !route.shape.is_empty())?;
    let mut label = route.shape.replace('_', " ");
    if let Some(confidence) = route.confidence {
        label.push_str(&format!(" · {:.0}%", confidence * 100.0));
    }
    let mut detail = Vec::new();
    if let Some(reason) = route.reasons.first() {
        detail.push(reason.clone());
    }
    if let Some(turns) = route.max_turns {
        detail.push(format!("up to {turns} turns"));
    }
    Some(
        div()
            .id("execution-route-strip")
            .flex()
            .items_center()
            .gap(px(Space::ROW_H))
            .mx(px(Space::INDENT))
            .mb(px(2.0))
            .px(px(Space::INDENT))
            .py(px(4.0))
            .rounded(px(Radius::BADGE))
            .bg(Fill::subtle())
            .text_size(px(Typo::META.size))
            .child(
                div()
                    .font_weight(Typo::ROW_EMPHASIZED.weight)
                    .text_color(Colors::text(Surface::Sidebar, Tone::Secondary))
                    .child(SharedString::from(label)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .truncate()
                    .text_color(Colors::text(Surface::Sidebar, Tone::Tertiary))
                    .child(SharedString::from(detail.join(" · "))),
            )
            .into_any_element(),
    )
}

fn timeline_body(state: &UiState) -> AnyElement {
    let rows: Vec<(String, String, String, String)> = state
        .selected_detail()
        .map(|detail| {
            detail
                .activity
                .iter()
                .map(|row| {
                    (
                        timestamp_label(row.timestamp_ms),
                        row.engine.clone().unwrap_or_else(|| "Harness".into()),
                        row.action.clone(),
                        row.detail.clone(),
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    // A legacy transcript carries no structured activity; its summary lines
    // still belong on a timeline, one row per line.
    let rows: Vec<(String, String, String, String)> = if rows.is_empty() {
        crate::inspector::activity_lines(state)
            .into_iter()
            .map(|line| (String::new(), String::new(), String::new(), line))
            .collect()
    } else {
        rows
    };
    if rows.is_empty() {
        return empty_state("Activity appears as execution emits events");
    }
    div()
        .id("execution-timeline")
        .flex()
        .flex_col()
        .flex_1()
        .min_h(px(0.0))
        .px(px(Space::INDENT))
        .overflow_y_scroll()
        .children(rows.into_iter().map(|(time, engine, action, detail)| {
            div()
                .flex()
                .items_baseline()
                .gap(px(Space::INDENT))
                .py(px(5.0))
                .border_b_1()
                .border_color(theme::white(0.05))
                .text_size(px(Typo::META.size))
                .when(!time.is_empty(), |row| {
                    row.child(
                        div()
                            .flex_none()
                            .w(px(ACTIVITY_TIME_WIDTH))
                            .text_color(Colors::text(Surface::Sidebar, Tone::Tertiary))
                            .child(SharedString::from(time)),
                    )
                    .child(
                        div()
                            .flex_none()
                            .w(px(ACTIVITY_ENGINE_WIDTH))
                            .text_color(Ink::working(&engine.to_lowercase()))
                            .child(SharedString::from(engine)),
                    )
                    .child(
                        div()
                            .flex_none()
                            .w(px(ACTIVITY_ACTION_WIDTH))
                            .text_color(Colors::text(Surface::Sidebar, Tone::Secondary))
                            .child(SharedString::from(action)),
                    )
                })
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .truncate()
                        .text_color(Colors::text(Surface::Sidebar, Tone::Secondary))
                        .child(SharedString::from(detail)),
                )
        }))
        .into_any_element()
}

fn activity_table(state: &UiState) -> AnyElement {
    let rows = state
        .selected_detail()
        .map(|detail| {
            detail
                .activity
                .iter()
                .map(|row| {
                    (
                        timestamp_label(row.timestamp_ms),
                        row.engine.clone().unwrap_or_else(|| "Harness".into()),
                        row.action.clone(),
                        row.detail.clone(),
                        row.path.clone().unwrap_or_default(),
                    )
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    div()
        .flex()
        .flex_col()
        .flex_1()
        .min_h(px(0.0))
        .child(
            div()
                .flex()
                .items_center()
                .pt(px(4.0))
                .pb(px(2.0))
                .text_size(px(Typo::ROW.size))
                .text_color(Colors::text(Surface::Sidebar, Tone::Secondary))
                .child(div().flex_1().child("Activity")),
        )
        .child(
            div()
                .id("execution-activity-rows")
                .flex()
                .flex_col()
                .flex_1()
                .min_h(px(0.0))
                .overflow_y_scroll()
                .children(
                    rows.into_iter()
                        .map(|(time, engine, action, detail, path)| {
                            div()
                                .flex()
                                .items_center()
                                .gap(px(Space::INDENT))
                                .py(px(ACTIVITY_ROW_VERTICAL_PADDING))
                                .border_b_1()
                                .border_color(theme::white(0.05))
                                .text_size(px(Typo::META.size))
                                .child(
                                    div()
                                        .w(px(ACTIVITY_TIME_WIDTH))
                                        .text_color(Colors::text(Surface::Sidebar, Tone::Tertiary))
                                        .child(SharedString::from(time)),
                                )
                                .child(
                                    div()
                                        .w(px(ACTIVITY_ENGINE_WIDTH))
                                        .text_color(Ink::working(&engine.to_lowercase()))
                                        .child(SharedString::from(engine)),
                                )
                                .child(
                                    div()
                                        .w(px(ACTIVITY_ACTION_WIDTH))
                                        .text_color(Colors::text(Surface::Sidebar, Tone::Secondary))
                                        .child(SharedString::from(action)),
                                )
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w(px(0.0))
                                        .truncate()
                                        .text_color(Colors::text(Surface::Sidebar, Tone::Secondary))
                                        .child(SharedString::from(detail)),
                                )
                                .child(
                                    div()
                                        .flex_none()
                                        .max_w(px(ACTIVITY_PATH_WIDTH))
                                        .truncate()
                                        .text_color(Colors::text(Surface::Sidebar, Tone::Tertiary))
                                        .child(SharedString::from(path)),
                                )
                        }),
                ),
        )
        .into_any_element()
}

fn progress_bar(progress: u8) -> AnyElement {
    let progress = progress.min(100);
    div()
        .h(px(2.0))
        .rounded_full()
        .bg(theme::white(0.12))
        .child(
            div()
                .h_full()
                .w(px(progress as f32))
                .rounded_full()
                .bg(Ink::FRESH),
        )
        .into_any_element()
}

fn empty_state(message: &'static str) -> AnyElement {
    div()
        .flex()
        .flex_1()
        .items_center()
        .justify_center()
        .text_size(px(Typo::ROW.size))
        .text_color(Colors::text(Surface::Sidebar, Tone::Tertiary))
        .child(message)
        .into_any_element()
}

#[cfg(test)]
fn activity_header_actions() -> Vec<&'static str> {
    Vec::new()
}

fn timestamp_label(timestamp_ms: i64) -> String {
    let total_minutes = timestamp_ms.div_euclid(60_000).rem_euclid(24 * 60);
    let hour24 = total_minutes / 60;
    let minute = total_minutes % 60;
    let suffix = if hour24 >= 12 { "PM" } else { "AM" };
    let hour12 = match hour24 % 12 {
        0 => 12,
        hour => hour,
    };
    format!("{hour12}:{minute:02}:{:02} {suffix}", 0)
}

fn duration_label(ms: u64) -> String {
    let seconds = ms.div_ceil(1000);
    format!("{seconds}s")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::preview;

    #[test]
    fn node_controls_are_exposed_only_when_the_daemon_marks_them_controllable() {
        let mut node = preview::populated().graph.unwrap().nodes.remove(0);
        assert_eq!(node_control(&node), None);
        node.can_retry = true;
        assert_eq!(node_control(&node), Some(NodeControl::Retry));
        node.can_cancel = true;
        assert_eq!(node_control(&node), Some(NodeControl::Cancel));
        node.can_retry = false;
        assert_eq!(node_control(&node), Some(NodeControl::Cancel));
    }

    #[test]
    fn execution_mode_projection_materially_changes_surface() {
        let state = preview::populated();

        assert_eq!(
            execution_projection(&state, ExecutionMode::Parallel),
            ExecutionProjection::Parallel {
                waves: vec![
                    vec!["Analyze".into()],
                    vec!["Plan".into()],
                    vec![
                        "Implement key normalization".into(),
                        "Add cross-version tests".into()
                    ],
                    vec!["Verify".into()],
                ],
                edges: 5,
            }
        );

        assert!(matches!(
            execution_projection(&state, ExecutionMode::Timeline),
            ExecutionProjection::Empty
        ));
    }

    #[test]
    fn activity_header_does_not_advertise_fake_clear_action() {
        assert_eq!(activity_header_actions(), Vec::<&'static str>::new());
    }

    #[test]
    fn parallel_graph_fits_default_and_compact_center_widths() {
        let state = preview::populated();

        let default = parallel_graph_metrics(&state, 601.0).expect("preview has graph");
        assert!(default.total_width <= 601.0, "{default:?}");
        assert!(default.total_height <= 168.0, "{default:?}");
        assert_eq!(default.visible_node_count, 5);

        let compact = parallel_graph_metrics(&state, 420.0).expect("preview has graph");
        assert!(compact.total_width <= 420.0, "{compact:?}");
        assert!(compact.node_width < default.node_width);
        assert_eq!(compact.visible_node_count, 5);
    }

    #[test]
    fn default_parallel_graph_shows_full_task_labels_without_ellipsis() {
        let state = preview::populated();
        let labels = parallel_graph_node_labels(&state, 601.0).expect("preview has graph labels");

        assert!(labels.iter().any(|label| {
            label.full == "Implement key normalization"
                && label.lines == vec!["Implement key", "normalization"]
                && !label.truncated
                && label.accessible_label == "Implement key normalization"
        }));
        assert!(labels.iter().any(|label| {
            label.full == "Add cross-version tests"
                && label.lines == vec!["Add cross-version", "tests"]
                && !label.truncated
                && label.accessible_label == "Add cross-version tests"
        }));
        assert!(
            !labels
                .iter()
                .flat_map(|label| label.lines.iter())
                .any(|line| line.contains('…'))
        );
    }

    #[test]
    fn activity_table_preserves_full_task_detail_before_path() {
        let columns = activity_table_columns(601.0);

        assert!(columns.detail_width >= 180.0, "{columns:?}");
        assert!(columns.path_width <= 120.0, "{columns:?}");
    }

    #[test]
    fn default_execution_density_fits_full_graph_and_four_activity_rows() {
        let state = preview::populated();
        let available_height = crate::layout::ShellLayout::default().rows(771.0).execution;
        let projection = execution_density_projection(&state, 601.0);

        assert_eq!(projection.activity_rows, 4, "{projection:?}");
        assert_eq!(projection.graph_nodes, 5, "{projection:?}");
        assert!(
            projection.total_height <= available_height,
            "{projection:?}"
        );
    }

    #[test]
    fn compact_dag_text_uses_the_minimum_typography_token() {
        assert_eq!(COMPACT_DAG_TEXT_SIZE, Typo::META.size);
        const { assert!(COMPACT_DAG_TEXT_SIZE >= 11.0) };
    }
}

#[cfg(test)]
mod view_choice_tests {
    use super::*;

    fn node(id: &str) -> crate::client::GraphNodeView {
        crate::client::GraphNodeView {
            id: id.into(),
            role: "worker".into(),
            objective: "do it".into(),
            file_scope: Vec::new(),
            acceptance_checks: Vec::new(),
            depends_on: Vec::new(),
            wave: 0,
            state: "pending".into(),
            detail: String::new(),
            progress_percent: None,
            duration_ms: None,
            can_retry: false,
            can_cancel: false,
        }
    }

    /// The pane collapses when it has nothing in it.
    ///
    /// A third of the window reading "Execution appears when a run is routed"
    /// is a permanent apology, and it is what most runs show — a direct run
    /// has no graph to draw at all.
    #[test]
    fn an_empty_execution_pane_collapses() {
        let mut state = crate::preview::populated();
        for detail in state.run_details.values_mut() {
            detail.graph = None;
            detail.terminal.clear();
        }
        assert!(!has_anything_to_show(&state));

        // Terminal output alone is enough: for a PTY agent it IS the
        // execution.
        let run_id = state.run_id.clone().expect("fixture selects a run");
        state
            .run_details
            .entry(run_id.clone())
            .or_default()
            .terminal
            .push("$ cargo test".into());
        assert!(has_anything_to_show(&state));

        // So is a graph.
        state
            .run_details
            .entry(run_id)
            .or_default()
            .terminal
            .clear();
        let run_id = state.run_id.clone().unwrap();
        state.run_details.entry(run_id).or_default().graph = Some(crate::client::GraphView {
            nodes: vec![node("only")],
            ..Default::default()
        });
        assert!(has_anything_to_show(&state));
    }

    /// The Parallel/Timeline toggle appears only when the two views can
    /// differ.
    ///
    /// A direct run is one node, and both views draw it identically — so the
    /// control asked the user to choose between two identical things, which is
    /// how someone learns a control does nothing.
    #[test]
    fn the_view_toggle_is_offered_only_when_there_is_a_shape_to_choose() {
        let mut state = crate::preview::populated();

        // No graph at all: nothing to view two ways.
        for detail in state.run_details.values_mut() {
            detail.graph = None;
        }
        assert!(!has_a_shape_worth_two_views(&state));

        let Some(run_id) = state.run_id.clone() else {
            panic!("the fixture selects a run");
        };
        let detail = state.run_details.entry(run_id).or_default();

        // One node is a direct run: both views are the same picture.
        detail.graph = Some(crate::client::GraphView {
            nodes: vec![node("only")],
            ..Default::default()
        });
        assert!(!has_a_shape_worth_two_views(&state));

        // More than one node is a real shape.
        let Some(run_id) = state.run_id.clone() else {
            unreachable!()
        };
        let detail = state.run_details.entry(run_id).or_default();
        detail.graph = Some(crate::client::GraphView {
            nodes: vec![node("first"), node("second")],
            ..Default::default()
        });
        assert!(has_a_shape_worth_two_views(&state));
    }
}

#[cfg(test)]
mod terminal_tests {
    use super::*;

    /// A terminal-driven run shows what it painted.
    ///
    /// The regression this guards: `engine.text_delta` is filtered as noise —
    /// correct for a model typing, since twenty of them say only "it is
    /// typing" — but for an agent that speaks no structured protocol the
    /// painted lines are the ENTIRE account of what happened. Routing them
    /// through the same event would have left a working agent looking idle.
    #[test]
    fn a_terminal_run_shows_its_output_instead_of_an_empty_graph() {
        let mut state = crate::preview::populated();
        for detail in state.run_details.values_mut() {
            detail.terminal.clear();
        }
        assert!(!has_terminal_output(&state));

        let run_id = state.run_id.clone().expect("the fixture selects a run");
        state
            .run_details
            .entry(run_id)
            .or_default()
            .terminal
            .push("$ cargo test".into());
        assert!(has_terminal_output(&state));
    }
}
