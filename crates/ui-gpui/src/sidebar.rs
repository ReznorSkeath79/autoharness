use gpui::prelude::*;
use gpui::{AnyElement, Context, ElementId, Role, SharedString, div, px};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::client::{RunView, UiState};
use crate::icon::{Icon, IconName, IconSize};
use crate::theme::{self, Colors, Fill, Ink, Metrics, Radius, Space, Status, Surface, Tone, Typo};
use crate::{Shell, components};

const PREVIEW_NOW_MS: i64 = 1_746_722_000_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SidebarRunRow {
    pub id: String,
    pub title: String,
    pub time: String,
    pub state: String,
    pub engine: String,
    pub turn_count: usize,
    pub selected: bool,
    recency: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SidebarGroups {
    pub recent: Vec<SidebarRunRow>,
    pub older: Vec<SidebarRunRow>,
}

pub(crate) fn run_groups(state: &UiState) -> SidebarGroups {
    let now_ms = now_for_state(state);
    let selected = state.run_id.as_deref();
    let Some(project) = state.selected_project() else {
        return SidebarGroups {
            recent: Vec::new(),
            older: Vec::new(),
        };
    };

    let mut rows = state
        .threads_of(&project.id)
        .into_iter()
        .enumerate()
        .filter_map(|(order, thread)| row_for_thread(state, thread, selected, order, now_ms))
        .collect::<Vec<_>>();
    rows.sort_by(|left, right| {
        right
            .selected
            .cmp(&left.selected)
            .then_with(|| right.recency.cmp(&left.recency))
            .then_with(|| left.title.cmp(&right.title))
    });

    let split = rows.len().min(4);
    SidebarGroups {
        older: rows.split_off(split),
        recent: rows,
    }
}

fn row_for_thread(
    state: &UiState,
    thread: &RunView,
    selected: Option<&str>,
    fallback_order: usize,
    now_ms: i64,
) -> Option<SidebarRunRow> {
    let members = thread_members(state, &thread.id);
    let tip = state.tip_of(&thread.id).unwrap_or(thread);
    let turn_count = members.len().max(1);
    Some(SidebarRunRow {
        id: thread.id.clone(),
        // A draft created by `+` has no objective yet. Naming it after eight
        // characters of its uuid tells the user nothing; naming it for what it
        // is tells them the row is waiting on them.
        title: match (thread.objective.trim(), thread.state.as_str()) {
            ("", "draft") => "New run".to_string(),
            ("", _) => thread.id.chars().take(8).collect(),
            (objective, _) => objective.to_string(),
        },
        time: run_time_label(state, &members, now_ms),
        state: tip.state.clone(),
        engine: tip.engine.clone(),
        turn_count,
        selected: selected.is_some_and(|selected| {
            selected == thread.id
                || selected == tip.id
                || members.iter().any(|run| run.id == selected)
        }),
        recency: run_recency(state, &members, fallback_order),
    })
}

fn thread_members<'a>(state: &'a UiState, thread_id: &str) -> Vec<&'a RunView> {
    let mut members = state
        .runs
        .iter()
        .filter(|run| {
            run.id == thread_id
                || state
                    .thread_root(&run.id)
                    .is_some_and(|root| root.id == thread_id)
        })
        .collect::<Vec<_>>();
    members.sort_by_key(|run| run_index(state, &run.id));
    members
}

fn run_index(state: &UiState, id: &str) -> usize {
    state
        .runs
        .iter()
        .position(|run| run.id == id)
        .unwrap_or(usize::MAX)
}

/// When a thread last did something, or failing that when it was created.
///
/// A row used to be dated purely from its messages, activity and worktree, so
/// anything that had not spoken yet — a run created a moment ago, one still
/// starting — read "unknown date" in a list where every other row had a time.
/// Creation is a fact the ledger always carries, so there is no state in which
/// a real run has no date to show.
fn run_time_label(state: &UiState, runs: &[&RunView], now_ms: i64) -> String {
    thread_timestamp(state, runs)
        .map(|timestamp| date_label(timestamp, now_ms))
        .unwrap_or_else(|| "no activity yet".into())
}

fn run_recency(state: &UiState, runs: &[&RunView], fallback_order: usize) -> i64 {
    thread_timestamp(state, runs).unwrap_or(fallback_order as i64)
}

fn thread_timestamp(state: &UiState, runs: &[&RunView]) -> Option<i64> {
    detail_latest_timestamp_for_runs(state, runs)
        .or_else(|| created_timestamp_for_runs(state, runs))
}

fn created_timestamp_for_runs(state: &UiState, runs: &[&RunView]) -> Option<i64> {
    runs.iter()
        .filter_map(|run| state.run_details.get(&run.id))
        .filter_map(|detail| detail.created_at_ms)
        .max()
}

fn detail_latest_timestamp_for_runs(state: &UiState, runs: &[&RunView]) -> Option<i64> {
    runs.iter()
        .filter_map(|run| state.run_details.get(&run.id))
        .filter_map(detail_latest_timestamp)
        .max()
}

fn detail_latest_timestamp(detail: &crate::client::RunDetailView) -> Option<i64> {
    detail
        .messages
        .iter()
        .map(|message| message.timestamp_ms)
        .chain(detail.activity.iter().map(|activity| activity.timestamp_ms))
        .chain(
            detail
                .worktree
                .iter()
                .filter_map(|worktree| worktree.created_at_ms),
        )
        .max()
}

pub(crate) fn date_label(timestamp_ms: i64, now_ms: i64) -> String {
    let day = timestamp_ms.div_euclid(86_400_000);
    let today = now_ms.div_euclid(86_400_000);
    let time = time_label(timestamp_ms);
    if day == today {
        return format!("Today, {time}");
    }
    if day == today - 1 {
        return format!("Yesterday, {time}");
    }

    let (month, day_of_month) = month_day_from_unix_day(day);
    format!("{} {day_of_month}, {time}", MONTHS[month - 1])
}

fn time_label(timestamp_ms: i64) -> String {
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

fn month_day_from_unix_day(day: i64) -> (usize, i64) {
    // Howard Hinnant's civil-from-days algorithm, with Unix epoch offset.
    let z = day + 719_468;
    let era = (if z >= 0 { z } else { z - 146_096 }).div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096).div_euclid(365);
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2).div_euclid(153);
    let day_of_month = doy - (153 * mp + 2).div_euclid(5) + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    let _year = y + if month <= 2 { 1 } else { 0 };
    (month as usize, day_of_month)
}

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

fn now_for_state(state: &UiState) -> i64 {
    if state.status.starts_with("preview:") {
        return PREVIEW_NOW_MS;
    }
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(PREVIEW_NOW_MS)
}

/// Repositories, run groups, and a truthful worktree-root footer.
pub(crate) fn view(
    state: &UiState,
    width: f32,
    picker_open: bool,
    cx: &mut Context<Shell>,
) -> AnyElement {
    let groups = run_groups(state);
    let project = state.selected_project();
    let run_count = project
        .map(|project| state.threads_of(&project.id).len())
        .unwrap_or_default();

    components::panel()
        .id("repositories-pane")
        .role(Role::Navigation)
        .aria_label("Repositories and runs")
        // The new-run popover is positioned against THIS panel. Without a
        // positioned ancestor its offsets resolve against the window instead,
        // which puts it a title bar away from the button it belongs to.
        .relative()
        .flex_none()
        .w(px(width))
        .h_full()
        .border_r_1()
        .border_color(Colors::stroke())
        .child(header("Repositories", "⌘"))
        .child(repo_row(state, run_count, cx))
        .child(runs_header(cx))
        .child(
            div()
                .id("sidebar-list")
                .role(Role::ListBox)
                .aria_label("Runs")
                .flex()
                .flex_col()
                .flex_1()
                .min_h(px(0.0))
                .overflow_y_scroll()
                .children(groups.recent.iter().map(|row| run_card(row, false, cx)))
                .when(!groups.older.is_empty(), |list| {
                    list.child(older_header())
                        .children(groups.older.iter().map(|row| run_card(row, true, cx)))
                }),
        )
        .child(worktree_footer(state))
        // Deferred so the popover paints over the run list rather than being
        // clipped by the scroll container it hangs off.
        .children(picker_open.then(|| {
            gpui::deferred(
                div()
                    .absolute()
                    .top(px(Metrics::TITLE_BAR + 92.0))
                    .left(px(Space::INSET))
                    .child(new_run_picker(state, cx)),
            )
        }))
        .into_any_element()
}

fn header(label: &'static str, action: &'static str) -> AnyElement {
    div()
        .flex()
        .items_center()
        .px(px(Space::INDENT))
        .pt(px(Space::INDENT))
        .pb(px(Space::ROW_H))
        .border_b_1()
        .border_color(Colors::stroke())
        .child(
            div()
                .flex_1()
                .text_size(px(Typo::SECTION_HEADER.size))
                .font_weight(Typo::SECTION_HEADER.weight)
                .child(label),
        )
        .child(
            div()
                .text_size(px(Typo::META.size))
                .text_color(Colors::text(Surface::Sidebar, Tone::Tertiary))
                .child(action),
        )
        .into_any_element()
}

fn repo_row(state: &UiState, run_count: usize, cx: &mut Context<Shell>) -> AnyElement {
    match state.selected_project() {
        Some(project) => {
            let accessible_label = format!(
                "Repository {}, {}, {run_count} runs. Refresh repositories",
                project.name, project.path
            );
            div()
                .id("selected-repository-row")
                .role(Role::Button)
                .aria_label(accessible_label)
                .tab_index(0)
                .focus_visible(|row| row.border_color(theme::white(0.48)))
                .flex()
                .items_center()
                .gap(px(Space::ROW_H))
                .px(px(Space::INDENT))
                .py(px(12.0))
                .border_b_1()
                .border_color(Colors::stroke())
                .hover(|row| row.bg(theme::white(Fill::HOVER)))
                .cursor_pointer()
                .on_click(cx.listener(|shell, _, window, cx| {
                    shell.choose_repository(window, cx);
                }))
                .child(
                    div()
                        .flex_none()
                        .text_size(px(Typo::ROW.size))
                        .text_color(Colors::text(Surface::Sidebar, Tone::Secondary))
                        .child("▣"),
                )
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_w(px(0.0))
                        .gap(px(3.0))
                        .child(
                            div()
                                .truncate()
                                .text_size(px(Typo::ROW_EMPHASIZED.size))
                                .font_weight(Typo::ROW_EMPHASIZED.weight)
                                .child(SharedString::from(project.name.clone())),
                        )
                        .child(
                            div()
                                .truncate()
                                .text_size(px(Typo::META.size))
                                .text_color(Colors::text(Surface::Sidebar, Tone::Tertiary))
                                .child(SharedString::from(project.path.clone())),
                        ),
                )
                .child(count_badge(run_count))
                .into_any_element()
        }
        None => div()
            .id("projects-empty-refresh")
            .role(Role::Button)
            .aria_label("No repositories. Refresh repositories or add a path")
            .tab_index(0)
            .focus_visible(|row| row.bg(theme::white(0.14)))
            .mx(px(Space::INSET))
            .px(px(Space::ROW_H))
            .py(px(Space::ROW_H))
            .rounded(px(Radius::ROW))
            .text_size(px(Typo::ROW.size))
            .text_color(Colors::text(Surface::Sidebar, Tone::Tertiary))
            .hover(|row| row.bg(theme::white(Fill::HOVER)))
            .cursor_pointer()
            .on_click(cx.listener(|shell, _, window, cx| {
                shell.choose_repository(window, cx);
            }))
            .child("none yet — choose a Git repository")
            .into_any_element(),
    }
}

/// One engine offered by the new-run picker.
///
/// diri's new-session popover lists every agent it knows and dims the ones that
/// cannot run, with the reason next to them, instead of hiding them or letting
/// you pick one that will fail. Same rule here: an engine that is not installed
/// or not signed in is shown, explained, and not clickable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NewRunChoice {
    pub engine: String,
    pub label: String,
    pub detail: String,
    pub ready: bool,
}

pub(crate) fn new_run_choices(state: &UiState) -> Vec<NewRunChoice> {
    let mut choices: Vec<NewRunChoice> = ["codex", "claude"]
        .into_iter()
        .map(|engine| {
            let status = state.engines.iter().find(|e| e.name == engine);
            let (ready, detail) =
                match status {
                    None => (false, "not detected yet".to_string()),
                    Some(status) if status.ready => (
                        true,
                        status
                            .version
                            .clone()
                            .map(|version| format!("ready · {version}"))
                            .unwrap_or_else(|| "ready".into()),
                    ),
                    Some(status) => (
                        false,
                        status.problems.first().cloned().unwrap_or_else(|| {
                            match status.installed {
                                false => "not installed".into(),
                                true => "not signed in".into(),
                            }
                        }),
                    ),
                };
            NewRunChoice {
                engine: engine.to_string(),
                label: title_case(engine),
                detail,
                ready,
            }
        })
        .collect();
    // Everything else the daemon detected is visible but gated: a "coming
    // soon" row answers "where is Cursor?" without offering a dead start.
    let mut gated: Vec<NewRunChoice> = state
        .engines
        .iter()
        .filter(|engine| !crate::client::engine_generally_available(&engine.name))
        .map(|engine| NewRunChoice {
            engine: engine.name.clone(),
            label: title_case(&engine.name),
            detail: "coming soon".into(),
            ready: false,
        })
        .collect();
    gated.sort_by(|a, b| a.label.cmp(&b.label));
    choices.extend(gated);
    choices
}

fn title_case(value: &str) -> String {
    let mut chars = value.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// The new-run picker, anchored under the Runs header.
pub(crate) fn new_run_picker(state: &UiState, cx: &mut Context<Shell>) -> AnyElement {
    let choices = new_run_choices(state);
    components::floating_surface()
        .id("sidebar-new-run-picker")
        .role(Role::ListBox)
        .aria_label("Start a new run")
        .w(px(244.0))
        .child(
            div()
                .px(px(Space::INDENT))
                .pt(px(10.0))
                .pb(px(6.0))
                .text_size(px(Typo::META.size))
                .text_color(Colors::text(Surface::Content, Tone::Tertiary))
                .child("New run on"),
        )
        .children(choices.into_iter().enumerate().map(|(index, choice)| {
            let engine = choice.engine.clone();
            let ready = choice.ready;
            let row = div()
                .id(ElementId::Name(
                    format!("sidebar-new-run-{}", choice.engine).into(),
                ))
                .role(Role::ListBoxOption)
                .aria_label(format!("{} — {}", choice.label, choice.detail))
                .flex()
                .items_center()
                .gap(px(Space::ROW_H))
                .mx(px(6.0))
                .px(px(Space::ROW_H))
                .py(px(8.0))
                .rounded(px(Radius::ROW))
                .child(
                    Icon::new(
                        IconName::Sparkle,
                        IconSize::REGULAR,
                        if ready {
                            Ink::working(&choice.engine)
                        } else {
                            Colors::text(Surface::Content, Tone::Tertiary)
                        },
                    )
                    .into_any_element(),
                )
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_w(px(0.0))
                        .gap(px(2.0))
                        .child(
                            div()
                                .text_size(px(Typo::ROW.size))
                                .text_color(Colors::text(
                                    Surface::Content,
                                    if ready { Tone::Primary } else { Tone::Tertiary },
                                ))
                                .child(SharedString::from(choice.label)),
                        )
                        .child(
                            div()
                                .truncate()
                                .text_size(px(Typo::META.size))
                                .text_color(Colors::text(Surface::Content, Tone::Tertiary))
                                .child(SharedString::from(choice.detail)),
                        ),
                );
            if ready {
                row.tab_index(index as isize)
                    .focus_visible(|row| row.bg(theme::white(0.14)))
                    .hover(|row| row.bg(theme::white(Fill::HOVER)))
                    .cursor_pointer()
                    .on_click(cx.listener(move |shell, _, window, cx| {
                        shell.begin_new_run(&engine, window, cx);
                    }))
                    .into_any_element()
            } else {
                // Not a dead click: it is visibly unavailable and says why.
                row.cursor(gpui::CursorStyle::OperationNotAllowed)
                    .into_any_element()
            }
        }))
        .child(div().h(px(6.0)))
        .into_any_element()
}

fn runs_header(cx: &mut Context<Shell>) -> AnyElement {
    div()
        .flex()
        .items_center()
        .px(px(Space::INDENT))
        .pt(px(16.0))
        .pb(px(8.0))
        .child(
            div()
                .flex_1()
                .text_size(px(Typo::SECTION_HEADER.size))
                .font_weight(Typo::SECTION_HEADER.weight)
                .child("Runs"),
        )
        .child(
            div()
                .id("sidebar-new-run")
                .role(Role::Button)
                .aria_label("New run")
                .tab_index(0)
                .focus_visible(|button| button.bg(theme::white(0.14)))
                .flex()
                .flex_none()
                .size(px(24.0))
                .items_center()
                .justify_center()
                .rounded(px(Radius::BADGE))
                .hover(|button| button.bg(theme::white(Fill::HOVER)))
                .cursor_pointer()
                .on_click(cx.listener(|shell, _, _, cx| {
                    shell.toggle_new_run_picker();
                    cx.notify();
                }))
                .child(Icon::new(
                    IconName::Plus,
                    IconSize::REGULAR,
                    Colors::text(Surface::Sidebar, Tone::Secondary),
                )),
        )
        .into_any_element()
}

fn older_header() -> AnyElement {
    div()
        .flex()
        .items_center()
        .gap(px(5.0))
        .mx(px(Space::INDENT))
        .mt(px(14.0))
        .pt(px(10.0))
        .border_t_1()
        .border_color(Colors::stroke())
        .text_size(px(Typo::META.size))
        .text_color(Colors::text(Surface::Sidebar, Tone::Tertiary))
        .child("Older runs")
        .child("⌄")
        .into_any_element()
}

fn run_card(row: &SidebarRunRow, compact: bool, cx: &mut Context<Shell>) -> AnyElement {
    let status = Status::of_run(&row.state);
    let id = row.id.clone();
    let accessible_label = format!(
        "{}, {}, {}, {}, {} turns",
        row.title, row.state, row.engine, row.time, row.turn_count
    );
    div()
        .id(ElementId::Name(format!("run-card-{}", row.id).into()))
        .role(Role::ListBoxOption)
        .aria_label(accessible_label)
        .aria_selected(row.selected)
        .tab_index(0)
        .focus_visible(|card| card.bg(theme::white(0.14)))
        .flex()
        .items_center()
        .gap(px(8.0))
        .mx(px(Space::INSET))
        .my(px(if compact { 3.0 } else { 5.0 }))
        .px(px(Space::ROW_H))
        .py(px(if compact { 6.0 } else { 9.0 }))
        .rounded(px(Radius::ROW))
        .when(row.selected, |card| card.bg(Fill::selected(true)))
        .hover(|card| card.bg(theme::white(Fill::HOVER)))
        .cursor_pointer()
        .on_click(cx.listener(move |shell, _, _, cx| {
            shell.state().select_run(&id);
            cx.notify();
        }))
        .child(components::status_mark(status, &row.engine))
        .child(
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w(px(0.0))
                .gap(px(4.0))
                .child(
                    div()
                        .truncate()
                        .text_size(px(Typo::ROW.size))
                        .font_weight(Typo::ROW_EMPHASIZED.weight)
                        .text_color(Colors::text(Surface::Sidebar, Tone::Primary))
                        .child(SharedString::from(row.title.clone())),
                )
                .child(
                    div()
                        .truncate()
                        .text_size(px(Typo::META.size))
                        .text_color(Colors::text(Surface::Sidebar, Tone::Tertiary))
                        .child(SharedString::from(row.time.clone())),
                ),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .items_end()
                .gap(px(6.0))
                .child(
                    div()
                        .text_size(px(Typo::META.size))
                        .font_weight(Typo::META.weight)
                        .text_color(status.color(&row.engine))
                        .child(status_label(status, &row.state)),
                )
                .child(count_badge(row.turn_count)),
        )
        .into_any_element()
}

fn status_label(status: Status, raw: &str) -> &'static str {
    match raw {
        "working" | "running" => "Running",
        "succeeded" | "completed" => "Completed",
        "failed" => "Failed",
        "needs-you" => "Needs you",
        // A draft is not idle: it is waiting on the user for an objective, and
        // saying "Idle" reads as "nothing to do here" for the one row that
        // does need something done.
        "draft" => "Draft",
        _ => status.label(),
    }
}

fn count_badge(count: usize) -> AnyElement {
    div()
        .flex_none()
        .min_w(px(22.0))
        .h(px(20.0))
        .items_center()
        .justify_center()
        .rounded(px(Radius::BADGE))
        .bg(Fill::subtle())
        .px(px(6.0))
        .text_size(px(Typo::META.size))
        .text_color(Colors::text(Surface::Sidebar, Tone::Secondary))
        .child(SharedString::from(count.to_string()))
        .into_any_element()
}

fn worktree_footer(state: &UiState) -> AnyElement {
    let value = state
        .selected_project()
        .map(|project| format!("{}/worktrees", project.path.trim_end_matches('/')))
        .or_else(|| state.worktree.clone())
        .unwrap_or_else(|| "No repository selected".into());

    div()
        .flex()
        .flex_col()
        .flex_none()
        .gap(px(4.0))
        .px(px(Space::INDENT))
        .py(px(14.0))
        .border_t_1()
        .border_color(Colors::stroke())
        .child(
            div()
                .text_size(px(Typo::META.size))
                .text_color(Colors::text(Surface::Sidebar, Tone::Tertiary))
                .child("Worktree root"),
        )
        .child(
            div()
                .truncate()
                .text_size(px(Typo::META.size))
                .text_color(Colors::text(Surface::Sidebar, Tone::Secondary))
                .child(SharedString::from(value)),
        )
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::preview;

    #[test]
    fn sidebar_groups_selected_and_recent_before_older_runs() {
        let state = preview::populated();
        let groups = run_groups(&state);

        assert_eq!(groups.recent.len(), 4);
        assert_eq!(groups.older.len(), 4);
        assert!(groups.recent[0].selected);
        assert_eq!(groups.recent[0].id, "feat-cache-key-stability");
        assert_eq!(
            groups
                .recent
                .iter()
                .map(|row| row.title.as_str())
                .collect::<Vec<_>>(),
            vec![
                "feat/cache-key-stability",
                "fix/ui-regression",
                "refactor/worker-pool",
                "chore/deps-bump",
            ]
        );
        assert_eq!(
            groups
                .recent
                .iter()
                .map(|row| row.turn_count)
                .collect::<Vec<_>>(),
            vec![4, 3, 5, 2]
        );
        assert_eq!(
            groups
                .older
                .iter()
                .map(|row| row.turn_count)
                .collect::<Vec<_>>(),
            vec![1, 3, 2, 1]
        );
        assert_eq!(
            groups
                .recent
                .iter()
                .map(|row| row.time.as_str())
                .collect::<Vec<_>>(),
            vec![
                "Today, 4:27 PM",
                "Today, 4:26 PM",
                "Yesterday, 4:23 PM",
                "May 6, 4:22 PM"
            ]
        );
        assert!(
            groups
                .recent
                .iter()
                .chain(groups.older.iter())
                .all(|row| !row.time.is_empty()
                    && row.time != "time unavailable"
                    && row.turn_count >= 1)
        );
    }

    /// A run the daemon has created but that has not spoken yet still has a
    /// date and a name.
    ///
    /// The regression: the sidebar dated a row only from its messages,
    /// activity and worktree, so a draft — the row `+` creates — rendered as
    /// "unknown date" with eight characters of its uuid for a title, in a list
    /// where every other row read "Today, 10:28 PM".
    #[test]
    fn a_fresh_draft_row_shows_a_real_time_and_a_readable_name() {
        let mut state = preview::populated();
        state.runs.push(RunView {
            id: "draft-run-id-0001".into(),
            project_id: "autoharness".into(),
            objective: String::new(),
            state: "draft".into(),
            engine: "claude".into(),
            parent_run_id: None,
            attempt_group: None,
        });
        state.run_details.insert(
            "draft-run-id-0001".into(),
            crate::client::RunDetailView {
                run_id: "draft-run-id-0001".into(),
                created_at_ms: Some(1_746_721_620_000),
                ..Default::default()
            },
        );
        state.run_id = Some("draft-run-id-0001".into());

        let groups = run_groups(&state);
        let row = groups
            .recent
            .iter()
            .find(|row| row.id == "draft-run-id-0001")
            .expect("the draft is listed");
        assert_eq!(row.title, "New run");
        assert_eq!(row.time, "Today, 4:27 PM");
        assert!(!row.time.contains("unknown"));
        assert!(row.selected, "a just-created draft is the selected row");
        // "Idle" would read as "nothing to do"; this row is the one waiting.
        assert_eq!(
            status_label(Status::of_run(&row.state), &row.state),
            "Draft"
        );
    }

    /// The picker shows every engine and says why an unusable one cannot be
    /// picked, rather than hiding it or offering a click that fails.
    #[test]
    fn the_new_run_picker_dims_engines_that_cannot_run_and_says_why() {
        let mut state = preview::populated();
        state.engines = vec![
            crate::client::EngineStatus {
                name: "codex".into(),
                ready: true,
                installed: true,
                authenticated: Some(true),
                version: Some("0.146.0".into()),
                problems: Vec::new(),
                models: Vec::new(),
                model_load_error: None,
            },
            crate::client::EngineStatus {
                name: "claude".into(),
                ready: false,
                installed: true,
                authenticated: Some(false),
                version: Some("2.1.222".into()),
                problems: vec!["not signed in: run `claude auth login`".into()],
                models: Vec::new(),
                model_load_error: None,
            },
        ];

        let choices = new_run_choices(&state);
        assert_eq!(choices.len(), 2, "every engine is offered or explained");

        let codex = &choices[0];
        assert_eq!(codex.label, "Codex");
        assert!(codex.ready);
        assert!(codex.detail.contains("0.146.0"));

        let claude = &choices[1];
        assert_eq!(claude.label, "Claude");
        assert!(!claude.ready, "an engine that cannot run is not offered");
        assert!(
            claude.detail.contains("not signed in"),
            "and it says why: {}",
            claude.detail
        );
    }

    /// An engine the daemon has not reported on yet is not silently presented
    /// as ready.
    #[test]
    fn an_undetected_engine_is_not_offered_as_ready() {
        let mut state = preview::populated();
        state.engines.clear();
        assert!(new_run_choices(&state).iter().all(|choice| !choice.ready));
    }

    #[test]
    fn date_label_is_day_aware_and_deterministic() {
        let now = 1_746_722_000_000;

        assert_eq!(date_label(1_746_721_640_000, now), "Today, 4:27 PM");
        assert_eq!(date_label(1_746_635_000_000, now), "Yesterday, 4:23 PM");
        assert_eq!(date_label(1_746_548_540_000, now), "May 6, 4:22 PM");
    }

    #[test]
    fn selected_thread_stays_first_but_remaining_rows_use_newest_first_order() {
        let mut state = preview::populated();
        state.run_id = Some("docs-readme-update".into());

        let groups = run_groups(&state);

        assert_eq!(groups.recent[0].id, "docs-readme-update");
        assert!(groups.recent[0].selected);
        assert_eq!(
            groups
                .recent
                .iter()
                .skip(1)
                .map(|row| row.id.as_str())
                .collect::<Vec<_>>(),
            vec![
                "feat-cache-key-stability",
                "fix-ui-regression",
                "refactor-worker-pool",
            ]
        );
    }
}
