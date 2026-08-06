use std::collections::HashSet;
use std::path::{Path, PathBuf};

use gpui::prelude::*;
use gpui::{AnyElement, Context, ElementId, Role, SharedString, div, px};

use crate::client::{
    self, ChangedFileStatus, DiffLine, RunDetailView, UiState, UsageView, WorktreeDetail,
};
use crate::theme::{self, Colors, Fill, Ink, Radius, Space, Surface, Tone, Typo};
use crate::{Shell, ansi, components, files};

/// What the right-hand pane is showing. Details is the run evidence stack;
/// Files is the read-only worktree browser. The tab strip is the only way a
/// datum moves out of the scroll — both tabs stay one click apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum InspectorTab {
    #[default]
    Details,
    Files,
}

/// The file viewer's slice of shell state, bundled so `view` stays under
/// the argument ceiling.
pub(crate) struct FilesFocus<'a> {
    pub expanded: &'a HashSet<PathBuf>,
    pub selected: Option<&'a Path>,
    pub body: Option<&'a (PathBuf, files::FileBody)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InspectorSection {
    Changes,
    Diff,
    Checks,
    Artifacts,
    Worktree,
    Engine,
    Budget,
    Usage,
}

#[cfg(test)]
const INSPECTOR_SECTION_HEADER_HEIGHT: f32 = 20.0;
const INSPECTOR_DIFF_HEIGHT: f32 = 100.0;
const COMPACT_DIFF_TEXT_SIZE: f32 = Typo::META_MONO.size;

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct InspectorDensityProjection {
    pub sections: usize,
    pub last_section: InspectorSection,
    pub total_height: f32,
}

#[cfg(test)]
pub(crate) fn inspector_density_projection(state: &UiState) -> InspectorDensityProjection {
    let detail = state.selected_detail();
    let changed_files = detail.map_or(0, |detail| detail.changed_files.len());
    let checks = detail.map_or(0, |detail| detail.checks.len());
    let artifacts = detail.map_or(0, |detail| detail.artifacts.len());
    let worktree_rows = detail
        .and_then(|detail| detail.worktree.as_ref())
        .map_or(1, |_| 4);
    let sections = section_order(state);
    let total_height = sections.len() as f32 * INSPECTOR_SECTION_HEADER_HEIGHT
        + changed_files as f32 * 20.0
        + 4.0
        + INSPECTOR_DIFF_HEIGHT
        + checks as f32 * 22.0
        + 4.0
        + artifacts as f32 * 22.0
        + 4.0
        + worktree_rows as f32 * 17.0
        + 4.0
        + 2.0 * 17.0
        + 4.0
        + 2.0 * 17.0
        + 6.0
        + 2.0 * 17.0
        + 6.0;
    InspectorDensityProjection {
        sections: sections.len(),
        last_section: sections.last().copied().unwrap_or(InspectorSection::Usage),
        total_height,
    }
}

pub(crate) fn section_order(state: &UiState) -> Vec<InspectorSection> {
    let _ = state;
    vec![
        InspectorSection::Changes,
        InspectorSection::Diff,
        InspectorSection::Checks,
        InspectorSection::Artifacts,
        InspectorSection::Worktree,
        InspectorSection::Engine,
        InspectorSection::Budget,
        InspectorSection::Usage,
    ]
}

pub(crate) fn activity_lines(state: &UiState) -> Vec<String> {
    state
        .summary
        .iter()
        .chain(state.events.iter())
        .cloned()
        .collect()
}

/// Right-hand review surface. Details is intentionally one scrollable stack;
/// Files swaps the stack for the worktree browser via the tab strip up top.
pub(crate) fn view(
    state: &UiState,
    width: f32,
    tab: InspectorTab,
    files_focus: FilesFocus<'_>,
    cx: &mut Context<Shell>,
) -> AnyElement {
    let detail = state.selected_detail();

    let body: AnyElement = match tab {
        InspectorTab::Details => div()
            .id("inspector-stack")
            .role(Role::Document)
            .aria_label("Changes, checks, artifacts, worktree, budget, and usage")
            .flex()
            .flex_col()
            .flex_1()
            .min_h(px(0.0))
            .overflow_y_scroll()
            .children(section_order(state).into_iter().map(|section| {
                match section {
                    InspectorSection::Changes => changes_section(detail).into_any_element(),
                    InspectorSection::Diff => diff_section(detail, state).into_any_element(),
                    InspectorSection::Checks => checks_section(detail, state).into_any_element(),
                    InspectorSection::Artifacts => artifacts_section(detail).into_any_element(),
                    InspectorSection::Worktree => {
                        worktree_section(detail, state).into_any_element()
                    }
                    InspectorSection::Engine => engine_section(detail, state).into_any_element(),
                    InspectorSection::Budget => budget_section(detail).into_any_element(),
                    InspectorSection::Usage => usage_section(
                        detail
                            .map(|d| &d.usage)
                            .unwrap_or(&state_usage_fallback(state)),
                    )
                    .into_any_element(),
                }
            }))
            .into_any_element(),
        InspectorTab::Files => files::inspector_view(
            files_focus.expanded,
            files_focus.selected,
            files_focus.body,
            state,
            cx,
        ),
    };

    components::panel()
        .id("inspector-pane")
        .role(Role::Complementary)
        .aria_label("Run evidence inspector")
        .flex_none()
        .w(px(width))
        .h_full()
        .border_l_1()
        .border_color(Colors::stroke())
        .child(tab_strip(tab, cx))
        .child(body)
        .into_any_element()
}

fn tab_strip(active: InspectorTab, cx: &mut Context<Shell>) -> AnyElement {
    div()
        .flex()
        .flex_none()
        .items_center()
        .gap(px(4.0))
        .px(px(Space::INDENT))
        .py(px(6.0))
        .border_b_1()
        .border_color(Colors::stroke())
        .child(tab_pill("Details", InspectorTab::Details, active, cx))
        .child(tab_pill("Files", InspectorTab::Files, active, cx))
        .into_any_element()
}

fn tab_pill(
    label: &'static str,
    tab: InspectorTab,
    active: InspectorTab,
    cx: &mut Context<Shell>,
) -> AnyElement {
    let is_active = tab == active;
    div()
        .id(ElementId::Name(format!("inspector-tab-{label}").into()))
        .role(Role::Button)
        .aria_label(format!("Show the {label} tab"))
        .aria_selected(is_active)
        .flex_none()
        .h(px(28.0))
        .px(px(10.0))
        .flex()
        .items_center()
        .rounded(px(Radius::BADGE))
        .text_size(px(Typo::ROW.size))
        .when(is_active, |pill| {
            pill.font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(Colors::text(Surface::Sidebar, Tone::Primary))
                .bg(theme::white(0.09))
        })
        .when(!is_active, |pill| {
            pill.text_color(Colors::text(Surface::Sidebar, Tone::Secondary))
                .hover(|pill| pill.bg(theme::white(Fill::HOVER)))
        })
        .cursor_pointer()
        .on_click(cx.listener(move |shell, _, _, cx| {
            shell.select_inspector_tab(tab, cx);
        }))
        .child(label)
        .into_any_element()
}

fn state_usage_fallback(_state: &UiState) -> UsageView {
    UsageView::default()
}

fn section_shell(title: &'static str, right: Option<String>, body: AnyElement) -> AnyElement {
    div()
        .id(ElementId::Name(format!("inspector-section-{title}").into()))
        .role(Role::Region)
        .aria_label(title)
        .flex()
        .flex_col()
        .border_b_1()
        .border_color(Colors::stroke())
        .child(
            div()
                .flex()
                .items_center()
                .px(px(Space::INDENT))
                .pt(px(4.0))
                .pb(px(3.0))
                .child(
                    div()
                        .flex_1()
                        .text_size(px(Typo::SECTION_HEADER.size))
                        .font_weight(Typo::SECTION_HEADER.weight)
                        .child(title),
                )
                .children(right.map(|right| {
                    div()
                        .text_size(px(Typo::META.size))
                        .text_color(Colors::text(Surface::Sidebar, Tone::Secondary))
                        .child(SharedString::from(right))
                })),
        )
        .child(body)
        .into_any_element()
}

fn empty_row(message: &'static str) -> AnyElement {
    div()
        .px(px(Space::INDENT))
        .pb(px(12.0))
        .text_size(px(Typo::ROW.size))
        .text_color(Colors::text(Surface::Sidebar, Tone::Tertiary))
        .child(message)
        .into_any_element()
}

fn changes_section(detail: Option<&RunDetailView>) -> AnyElement {
    let files = detail
        .map(|detail| detail.changed_files.as_slice())
        .unwrap_or(&[]);
    let body = if files.is_empty() {
        empty_row("No changed files yet")
    } else {
        div()
            .flex()
            .flex_col()
            .pb(px(4.0))
            .children(files.iter().map(|file| {
                div()
                    .flex()
                    .items_center()
                    .gap(px(Space::ROW_H))
                    .px(px(Space::INDENT))
                    .py(px(3.0))
                    .bg(Fill::subtle())
                    .child(
                        div()
                            .text_size(px(Typo::ROW.size))
                            .text_color(Colors::text(Surface::Sidebar, Tone::Secondary))
                            .child("▱"),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .truncate()
                            .text_size(px(Typo::ROW.size))
                            .child(SharedString::from(file.path.clone())),
                    )
                    .child(
                        div()
                            .text_size(px(Typo::META.size))
                            .text_color(file_status_color(file.status))
                            .child(file_status_label(file.status)),
                    )
            }))
            .into_any_element()
    };
    section_shell(
        "Changes",
        Some(format!("{} files changed", files.len())),
        body,
    )
}

fn diff_section(detail: Option<&RunDetailView>, state: &UiState) -> AnyElement {
    let diff = detail
        .and_then(|detail| detail.diff.as_ref())
        .or(state.diff.as_ref());
    let body = match diff {
        Some(diff) => diff_view(diff).into_any_element(),
        None => empty_row("No diff captured yet"),
    };
    section_shell("Diff", None, body)
}

fn checks_section(detail: Option<&RunDetailView>, state: &UiState) -> AnyElement {
    let checks = detail.map(|detail| detail.checks.as_slice()).unwrap_or(&[]);
    let passed = checks
        .iter()
        .filter(|check| check.passed == Some(true))
        .count();
    let body = if !checks.is_empty() {
        div()
            .flex()
            .flex_col()
            .pb(px(4.0))
            .children(checks.iter().map(|check| {
                div()
                    .flex()
                    .items_center()
                    .gap(px(Space::ROW_H))
                    .mx(px(Space::INSET))
                    .px(px(Space::ROW_H))
                    .py(px(3.0))
                    .rounded(px(Radius::ROW))
                    .border_1()
                    .border_color(Colors::stroke())
                    .child(
                        div()
                            .w(px(16.0))
                            .text_color(check_color(check.passed))
                            .child(check_symbol(check.passed)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .truncate()
                            .text_size(px(Typo::ROW.size))
                            .child(SharedString::from(check.name.clone())),
                    )
                    .child(
                        div()
                            .text_size(px(Typo::META.size))
                            .text_color(Colors::text(Surface::Sidebar, Tone::Secondary))
                            .child(SharedString::from(
                                check.duration_ms.map(duration_label).unwrap_or_default(),
                            )),
                    )
            }))
            .into_any_element()
    } else if state.check_output.is_empty() {
        empty_row("No checks have run yet")
    } else {
        output_view(&state.check_output).into_any_element()
    };
    section_shell(
        "Checks",
        Some(format!("{passed} / {} passed", checks.len())),
        body,
    )
}

fn artifacts_section(detail: Option<&RunDetailView>) -> AnyElement {
    let artifacts = detail
        .map(|detail| detail.artifacts.as_slice())
        .unwrap_or(&[]);
    let body = if artifacts.is_empty() {
        empty_row("No artifacts yet")
    } else {
        div()
            .flex()
            .flex_col()
            .pb(px(4.0))
            .children(artifacts.iter().map(|artifact| {
                div()
                    .flex()
                    .items_center()
                    .gap(px(Space::ROW_H))
                    .mx(px(Space::INSET))
                    .px(px(Space::ROW_H))
                    .py(px(3.0))
                    .border_1()
                    .border_color(Colors::stroke())
                    .rounded(px(Radius::ROW))
                    .child("▧")
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .truncate()
                            .text_size(px(Typo::ROW.size))
                            .child(SharedString::from(artifact.name.clone())),
                    )
                    .child(
                        div()
                            .text_size(px(Typo::META.size))
                            .text_color(Colors::text(Surface::Sidebar, Tone::Tertiary))
                            .child(SharedString::from(
                                artifact
                                    .size_bytes
                                    .map(bytes_label)
                                    .unwrap_or_else(|| "size unavailable".into()),
                            )),
                    )
            }))
            .into_any_element()
    };
    section_shell("Artifacts", Some(artifacts.len().to_string()), body)
}

fn worktree_section(detail: Option<&RunDetailView>, state: &UiState) -> AnyElement {
    let worktree = detail.and_then(|detail| detail.worktree.as_ref());
    let rows = match worktree {
        Some(worktree) => worktree_rows(worktree),
        None => vec![(
            "Path".into(),
            state
                .worktree
                .clone()
                .unwrap_or_else(|| "No worktree for this run".into()),
        )],
    };
    section_shell("Worktree", None, key_values(rows))
}

fn engine_section(detail: Option<&RunDetailView>, state: &UiState) -> AnyElement {
    let engine = detail
        .and_then(|detail| detail.engine.clone())
        .unwrap_or_else(|| state.engine.clone());
    let readiness = state
        .engines
        .iter()
        .find(|status| status.name == engine)
        .map(|status| status.summary())
        .unwrap_or_else(|| "engine status unavailable".into());
    let model_id = detail
        .map(|detail| detail.model.clone())
        .unwrap_or_else(|| state.model.clone());
    let model = model_id
        .as_deref()
        .and_then(|id| {
            state
                .engines
                .iter()
                .find(|status| status.name == engine)
                .and_then(|status| status.models.iter().find(|model| model.id == id))
                .map(|model| model.display_name.clone())
        })
        .or(model_id)
        .unwrap_or_else(|| "Provider default".into());
    let reasoning = detail
        .map(|detail| detail.reasoning_effort.clone())
        .unwrap_or_else(|| state.reasoning_effort.clone())
        .map(|effort| {
            let mut chars = effort.chars();
            chars
                .next()
                .map(|first| first.to_uppercase().collect::<String>() + chars.as_str())
                .unwrap_or_default()
        })
        .unwrap_or_else(|| "Provider default".into());
    section_shell(
        "Engine",
        Some("Auto".into()),
        key_values(vec![
            ("Selected".into(), engine),
            ("Model".into(), model),
            ("Reasoning".into(), reasoning),
            ("Status".into(), readiness),
        ]),
    )
}

fn budget_section(detail: Option<&RunDetailView>) -> AnyElement {
    let Some(budget) = detail.and_then(|detail| detail.budget.as_ref()) else {
        return section_shell("Budget", None, empty_row("No budget emitted yet"));
    };
    section_shell(
        "Budget",
        None,
        div()
            .flex()
            .flex_col()
            .px(px(Space::INDENT))
            .pb(px(6.0))
            .gap(px(4.0))
            .children(
                [
                    progress_row(
                        "Time",
                        budget.spent_wall_time_secs,
                        budget.wall_time_secs,
                        |secs| format!("{}m", secs / 60),
                    ),
                    progress_row(
                        "Tool calls",
                        budget.spent_tool_calls.map(u64::from),
                        budget.max_tool_calls.map(u64::from),
                        |calls| calls.to_string(),
                    ),
                ]
                .into_iter()
                .flatten(),
            )
            .into_any_element(),
    )
}

fn usage_section(usage: &UsageView) -> AnyElement {
    section_shell(
        "Usage",
        None,
        div()
            .flex()
            .flex_col()
            .px(px(Space::INDENT))
            .pb(px(6.0))
            .gap(px(4.0))
            .children(
                [
                    progress_row(
                        "Context",
                        usage.context_tokens,
                        usage.context_limit_tokens,
                        compact_number,
                    ),
                    progress_row(
                        "Tokens",
                        Some(usage.input_tokens + usage.output_tokens),
                        usage.context_limit_tokens,
                        compact_number,
                    ),
                ]
                .into_iter()
                .flatten(),
            )
            .into_any_element(),
    )
}

fn key_values(rows: Vec<(String, String)>) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .pb(px(4.0))
        .children(rows.into_iter().map(|(label, value)| {
            div()
                .flex()
                .gap(px(Space::ROW_H))
                .px(px(Space::INDENT))
                .py(px(2.0))
                .child(
                    div()
                        .w(px(110.0))
                        .text_size(px(Typo::META.size))
                        .text_color(Colors::text(Surface::Sidebar, Tone::Tertiary))
                        .child(SharedString::from(label)),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .truncate()
                        .text_size(px(Typo::META.size))
                        .text_color(Colors::text(Surface::Sidebar, Tone::Secondary))
                        .child(SharedString::from(value)),
                )
        }))
        .into_any_element()
}

fn progress_row(
    label: &'static str,
    spent: Option<u64>,
    limit: Option<u64>,
    format: fn(u64) -> String,
) -> Option<AnyElement> {
    // Nothing measured, nothing claimed — and nothing rendered. A column of
    // "unavailable" rows is noise wearing the costume of honesty; absence
    // says the same thing quietly.
    let spent_value = spent?;
    let fraction = match limit {
        Some(limit) if limit > 0 => (spent_value as f32 / limit as f32).clamp(0.0, 1.0),
        _ => 0.0,
    };
    let value = match limit {
        Some(limit) => format!("{} / {}", format(spent_value), format(limit)),
        None => format(spent_value),
    };
    let row = div()
        .flex()
        .items_center()
        .gap(px(Space::ROW_H))
        .child(
            div()
                .w(px(72.0))
                .text_size(px(Typo::META.size))
                .text_color(Colors::text(Surface::Sidebar, Tone::Tertiary))
                .child(label),
        )
        .child(
            div()
                .w(px(86.0))
                .text_size(px(Typo::META.size))
                .text_color(Colors::text(Surface::Sidebar, Tone::Secondary))
                .child(SharedString::from(value)),
        )
        .child(
            div()
                .flex_1()
                .h(px(5.0))
                .rounded_full()
                .bg(theme::white(0.11))
                .child(
                    div()
                        .h_full()
                        .w(px(120.0 * fraction))
                        .rounded_full()
                        .bg(Ink::FRESH),
                ),
        )
        .into_any_element();
    Some(row)
}

/// Only the facts the ledger actually carries. A worktree replayed from an
/// older run may know its path and nothing else; four rows of "unavailable"
/// under it said nothing and took four lines to say it.
fn worktree_rows(worktree: &WorktreeDetail) -> Vec<(String, String)> {
    [
        ("Path", worktree.path.clone()),
        ("Base", worktree.base.clone()),
        ("Created", worktree.created_at_ms.map(timestamp_label)),
        ("Isolation", worktree.isolation.clone()),
    ]
    .into_iter()
    .filter_map(|(label, value)| value.map(|value| (label.to_string(), value)))
    .collect()
}

fn diff_view(diff: &client::DiffView) -> impl IntoElement + use<> {
    div()
        .id("diff")
        .flex()
        .flex_col()
        .max_h(px(INSPECTOR_DIFF_HEIGHT))
        .overflow_y_scroll()
        .text_size(px(COMPACT_DIFF_TEXT_SIZE))
        .children(diff.lines.iter().map(|line| {
            let (text, color, tint) = match line {
                DiffLine::File(path) => (
                    path.clone(),
                    Colors::text(Surface::Sidebar, Tone::Primary),
                    Some(Fill::subtle()),
                ),
                DiffLine::Hunk(header) => (
                    header.clone(),
                    Colors::text(Surface::Sidebar, Tone::Tertiary),
                    None,
                ),
                DiffLine::Added(t) => (format!("+{t}"), Ink::ADDED, Some(Ink::ADDED_BG)),
                DiffLine::Removed(t) => (format!("-{t}"), Ink::REMOVED, Some(Ink::REMOVED_BG)),
                DiffLine::Context(t) => (
                    format!(" {t}"),
                    Colors::text(Surface::Sidebar, Tone::Secondary),
                    None,
                ),
                DiffLine::Note(t) => (t.clone(), Ink::ATTENTION, None),
            };
            div()
                .px(px(Space::INDENT))
                .py(px(0.0))
                .when_some(tint, |row, tint| row.bg(tint))
                .text_color(color)
                .child(SharedString::from(text))
        }))
}

fn output_view(lines: &[String]) -> impl IntoElement + use<> {
    div()
        .id("check-output")
        .flex()
        .flex_col()
        .max_h(px(140.0))
        .overflow_y_scroll()
        .text_size(px(Typo::META_MONO.size))
        .children(lines.iter().map(|line| {
            div()
                .px(px(Space::INDENT))
                .children(ansi::parse_line(line).into_iter().map(|span| {
                    let color = match span.color {
                        ansi::TermColor::Default => Colors::text(Surface::Sidebar, Tone::Secondary),
                        ansi::TermColor::Indexed(i) => terminal_palette(i),
                        ansi::TermColor::Rgb(r, g, b) => theme::rgba8(r, g, b, 0xff),
                    };
                    let mut color = color;
                    if span.style.dim {
                        color.a *= 0.6;
                    }
                    div()
                        .when(span.style.bold, |t| t.font_weight(gpui::FontWeight::BOLD))
                        .when(span.style.italic, |t| t.italic())
                        .when(span.style.underline, |t| t.underline())
                        .text_color(color)
                        .child(SharedString::from(span.text))
                }))
        }))
}

fn terminal_palette(index: u8) -> gpui::Rgba {
    match index % 16 {
        0 => theme::white(0.35),
        1 => Ink::REMOVED,
        2 => Ink::ADDED,
        3 => Ink::ATTENTION,
        4 => theme::rgba8(0x7a, 0xa2, 0xf7, 0xff),
        5 => theme::rgba8(0xbb, 0x9a, 0xf7, 0xff),
        6 => Ink::TEAL,
        7 => Colors::text(Surface::Sidebar, Tone::Secondary),
        8 => theme::white(0.45),
        9 => Ink::DANGER,
        10 => Ink::FRESH,
        11 => theme::rgba8(0xff, 0xc7, 0x77, 0xff),
        12 => theme::rgba8(0x9d, 0xb8, 0xff, 0xff),
        13 => theme::rgba8(0xd0, 0xb0, 0xff, 0xff),
        14 => theme::rgba8(0x6d, 0xe8, 0xdc, 0xff),
        _ => Colors::text(Surface::Sidebar, Tone::Primary),
    }
}

fn file_status_label(status: ChangedFileStatus) -> &'static str {
    match status {
        ChangedFileStatus::Added => "A",
        ChangedFileStatus::Modified => "M",
        ChangedFileStatus::Deleted => "D",
        ChangedFileStatus::Renamed => "R",
        ChangedFileStatus::Unknown => "?",
    }
}

fn file_status_color(status: ChangedFileStatus) -> gpui::Rgba {
    match status {
        ChangedFileStatus::Added => Ink::FRESH,
        ChangedFileStatus::Modified => Ink::ATTENTION,
        ChangedFileStatus::Deleted => Ink::DANGER,
        ChangedFileStatus::Renamed => Ink::TEAL,
        ChangedFileStatus::Unknown => Colors::text(Surface::Sidebar, Tone::Tertiary),
    }
}

fn check_symbol(passed: Option<bool>) -> &'static str {
    match passed {
        Some(true) => "✓",
        Some(false) => "×",
        None => "○",
    }
}

fn check_color(passed: Option<bool>) -> gpui::Rgba {
    match passed {
        Some(true) => Ink::FRESH,
        Some(false) => Ink::DANGER,
        None => Colors::text(Surface::Sidebar, Tone::Tertiary),
    }
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
    format!("{hour12}:{minute:02} {suffix}")
}

fn duration_label(ms: u64) -> String {
    let seconds = ms.div_ceil(1000);
    format!("{seconds}s")
}

fn bytes_label(bytes: u64) -> String {
    if bytes >= 1024 * 1024 {
        format!("{} MB", bytes / (1024 * 1024))
    } else if bytes >= 1024 {
        format!("{} KB", bytes / 1024)
    } else {
        format!("{bytes} B")
    }
}

fn compact_number(value: u64) -> String {
    if value >= 1000 {
        format!("{}K", value / 1000)
    } else {
        value.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_inspector_opens_on_run_evidence_not_the_file_browser() {
        // The file viewer is a sub-option of the right-hand pane, not what a
        // user opening the inspector for a failing run should land on.
        assert_eq!(InspectorTab::default(), InspectorTab::Details);
    }

    #[test]
    fn inspector_section_order_matches_single_stacked_reference() {
        assert_eq!(
            section_order(&UiState::default()),
            vec![
                InspectorSection::Changes,
                InspectorSection::Diff,
                InspectorSection::Checks,
                InspectorSection::Artifacts,
                InspectorSection::Worktree,
                InspectorSection::Engine,
                InspectorSection::Budget,
                InspectorSection::Usage,
            ]
        );
    }

    #[test]
    fn activity_lines_are_summary_then_events_without_fake_rows() {
        let state = UiState {
            summary: vec!["summary one".into(), "summary two".into()],
            events: vec!["event one".into(), "event two".into()],
            ..UiState::default()
        };

        assert_eq!(
            activity_lines(&state),
            vec![
                "summary one".to_string(),
                "summary two".to_string(),
                "event one".to_string(),
                "event two".to_string(),
            ]
        );
        assert!(activity_lines(&UiState::default()).is_empty());
    }

    #[test]
    fn default_inspector_density_reaches_usage_without_initial_scroll() {
        let state = crate::preview::populated();
        let projection = inspector_density_projection(&state);

        assert_eq!(projection.sections, 8, "{projection:?}");
        assert_eq!(projection.last_section, InspectorSection::Usage);
        assert!(projection.total_height <= 772.0, "{projection:?}");
    }

    #[test]
    fn compact_diff_text_uses_the_minimum_monospace_typography_token() {
        assert_eq!(COMPACT_DIFF_TEXT_SIZE, Typo::META_MONO.size);
        const { assert!(COMPACT_DIFF_TEXT_SIZE >= 11.0) };
    }
}
