use gpui::prelude::*;
use gpui::{
    AccessibleAction, Animation, AnimationExt, AnyElement, Context, ElementId, Role, SharedString,
    div, ease_out_quint, px,
};

use crate::client::{GraphView, StructuredMessageView, UiState};
use crate::components;
use crate::motion;
use crate::query_editor;
use crate::theme::{Colors, Fill, Ink, Radius, Space, Surface, Tone, Typo};
use crate::{Shell, toolbar};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ComposerAction {
    ChooseRepository,
    OpenMentions,
    ToggleModelPicker,
    Submit,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ComposerControl {
    pub label: &'static str,
    pub id: &'static str,
    pub action: ComposerAction,
}

pub(crate) fn composer_controls() -> Vec<ComposerControl> {
    vec![
        ComposerControl {
            label: "+",
            id: "coordinator-compose-repository",
            action: ComposerAction::ChooseRepository,
        },
        ComposerControl {
            label: "@",
            id: "coordinator-compose-mention",
            action: ComposerAction::OpenMentions,
        },
        ComposerControl {
            label: "▷",
            id: "coordinator-compose-send",
            action: ComposerAction::Submit,
        },
    ]
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PlanCommand {
    OpenOverview,
    Submit(&'static str),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PlanAction {
    pub label: &'static str,
    pub command: PlanCommand,
}

pub(crate) fn plan_action(state: &UiState) -> PlanAction {
    let awaiting_approval = state
        .selected_detail()
        .and_then(|detail| detail.graph.as_ref())
        .or(state.graph.as_ref())
        .is_some_and(|graph| graph.awaiting_approval);

    if awaiting_approval {
        PlanAction {
            label: "Approve plan",
            command: PlanCommand::Submit("/approve"),
        }
    } else {
        PlanAction {
            label: "Review plan",
            command: PlanCommand::OpenOverview,
        }
    }
}

fn run_plan_command(shell: &mut Shell, command: PlanCommand, cx: &mut Context<Shell>) {
    match command {
        PlanCommand::OpenOverview => shell.toolbar_action(toolbar::ToolbarAction::OpenOverview, cx),
        PlanCommand::Submit(command) => shell.submit(command),
    }
}

/// The conversation, with the prompt pinned beneath it.
pub(crate) fn view(
    state: &UiState,
    prompt: &query_editor::QueryEditor,
    prompt_active: bool,
    model_picker_open: bool,
    model_picker_tab: Option<&str>,
    model_picker_query: &str,
    cx: &mut Context<Shell>,
) -> AnyElement {
    // A block caret at the real cursor, so the field behaves like a field.
    let (prompt_text, _selection) = prompt.display("▌");
    let prompt_value = prompt.text().to_string();
    let plan_action = plan_action(state);
    let transcript = div()
        .id("transcript")
        .role(Role::Log)
        .aria_label("Coordinator transcript")
        .flex()
        .flex_col()
        .flex_1()
        .min_h(px(0.0))
        .px(px(Space::INDENT))
        .pt(px(10.0))
        .justify_start()
        .overflow_y_scroll()
        // First-run setup is a full page over the cockpit now
        // (`onboarding::first_run_page`), not a card in this transcript. A
        // ready app with nothing selected still deserves a start screen
        // rather than a blank log over the composer.
        .when(
            state.run_id.is_none() && transcript_rows(state).is_empty(),
            |transcript| transcript.child(welcome(cx)),
        )
        .children(transcript_elements(state));

    let editor = if prompt.is_empty() {
        div()
            .ml(px(Space::ROW_H))
            .flex_1()
            .min_w(px(0.0))
            .text_color(Colors::text(Surface::Sidebar, Tone::Tertiary))
            .child("Run objective...")
    } else {
        div()
            .ml(px(Space::ROW_H))
            .flex_1()
            .min_w(px(0.0))
            // The editor renders its own caret at the real cursor offset.
            .child(SharedString::from(prompt_text))
    };

    let prompt = div()
        .id("coordinator-prompt")
        .role(Role::TextInput)
        .aria_label("Run objective")
        .aria_placeholder("Run objective")
        .aria_value(prompt_value)
        .when(prompt_active, |prompt| prompt.aria_active_descendant())
        .on_a11y_action(AccessibleAction::SetValue, {
            let shell = cx.entity().downgrade();
            move |data, _window, cx| {
                let Some(gpui::accesskit::ActionData::Value(value)) = data else {
                    return;
                };
                let value = value.to_string();
                shell
                    .update(cx, |shell, cx| {
                        shell.prompt.clear();
                        shell.prompt.insert(&value);
                        shell.state().input = shell.prompt.text().to_string();
                        cx.notify();
                    })
                    .ok();
            }
        })
        .on_click(cx.listener(|shell, _, window, cx| {
            window.focus(&shell.focus, cx);
        }))
        .flex()
        .flex_none()
        .flex_col()
        .m(px(Space::INSET))
        .px(px(Space::INDENT))
        .py(px(8.0))
        .h(px(80.0))
        .rounded(px(Radius::BADGE))
        .bg(Fill::subtle())
        .text_size(px(Typo::ROW.size))
        .child(
            div()
                .flex()
                .items_start()
                .flex_1()
                .min_h(px(0.0))
                .child(
                    div()
                        .flex_none()
                        .text_color(Colors::text(Surface::Sidebar, Tone::Tertiary))
                        .child("▌"),
                )
                .child(editor),
        )
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(Space::INDENT))
                .child(composer_button(composer_controls()[0], cx))
                .child(composer_button(composer_controls()[1], cx))
                .child(model_selector_button(state, cx))
                .child(repository_selector_button(state, cx))
                .child(div().flex_1())
                .child(composer_button(composer_controls()[2], cx)),
        );

    components::panel()
        .id("coordinator-pane")
        .role(Role::Main)
        .aria_label("Coordinator")
        .relative()
        .flex_1()
        .min_w(px(0.0))
        .min_h(px(0.0))
        .child(
            div()
                .flex()
                .items_center()
                .border_b_1()
                .border_color(Colors::stroke())
                .child(div().flex_1().child(components::section("Coordinator")))
                .child(
                    components::compact_control(plan_action.label)
                        .id("coordinator-review-plan")
                        .role(Role::Button)
                        .aria_label(plan_action.label)
                        .tab_index(0)
                        .focus_visible(|control| control.border_color(crate::theme::white(0.48)))
                        .hover(|control| control.bg(crate::theme::white(Fill::HOVER)))
                        .cursor_pointer()
                        .on_click(cx.listener(move |shell, _, _, cx| {
                            run_plan_command(shell, plan_action.command, cx);
                            cx.notify();
                        })),
                )
                .child(
                    components::compact_control("⋮")
                        .id("coordinator-overflow")
                        .role(Role::Button)
                        .aria_label("Open run overview")
                        .tab_index(0)
                        .focus_visible(|control| control.border_color(crate::theme::white(0.48)))
                        .mr(px(Space::INSET))
                        .hover(|control| control.bg(crate::theme::white(Fill::HOVER)))
                        .cursor_pointer()
                        .on_click(cx.listener(|shell, _, _, cx| {
                            shell.toolbar_action(toolbar::ToolbarAction::OpenOverview, cx);
                        })),
                ),
        )
        .child(transcript)
        .children(plan_activity_summary(state).map(|summary| plan_card(summary, plan_action, cx)))
        .children(composer_queue_summary(state).map(|label| queue_strip(label, cx)))
        .children(attempt_comparison(state, cx))
        .children(working_tree_chip(state, cx))
        .children(question_prompt(state, cx))
        .children(status_strip(state))
        .child(prompt)
        .children(
            model_picker_open
                .then(|| model_picker(state, model_picker_tab, model_picker_query, cx)),
        )
        .into_any_element()
}

/// The attempts answering this objective, and which of them passed.
///
/// The point of running several: a check that judged them all the same way
/// makes "this one worked" a fact. An attempt whose check has not run is shown
/// as unjudged rather than as a pass — mistaking one for the other is exactly
/// what this is meant to prevent.
fn attempt_comparison(state: &UiState, cx: &mut Context<Shell>) -> Option<AnyElement> {
    let attempts = state.sibling_attempts();
    if attempts.len() < 2 {
        return None;
    }
    let passing = attempts.iter().filter(|a| a.passed == Some(true)).count();
    Some(
        div()
            .id("attempt-comparison")
            .role(Role::Group)
            .aria_label(format!(
                "{} attempts at this objective, {passing} passing",
                attempts.len()
            ))
            .flex()
            .flex_col()
            .flex_none()
            .gap(px(4.0))
            .mx(px(Space::INSET))
            .mb(px(6.0))
            .p(px(Space::ROW_H))
            .rounded(px(Radius::ROW))
            .bg(Fill::subtle())
            .child(
                div()
                    .text_size(px(Typo::META.size))
                    .text_color(Colors::text(Surface::Content, Tone::Tertiary))
                    .child(SharedString::from(format!(
                        "{} attempts · {passing} passed the check",
                        attempts.len()
                    ))),
            )
            .children(attempts.into_iter().map(|attempt| {
                let run_id = attempt.run_id.clone();
                let (verdict, tint) = match (attempt.passed, attempt.state.as_str()) {
                    (Some(true), _) => ("passed".to_string(), Ink::FRESH),
                    (Some(false), _) => ("failed the check".to_string(), Ink::DANGER),
                    // Never shown as a pass. An attempt nobody judged is not a
                    // working one.
                    (None, "succeeded") => (
                        "finished, not judged".to_string(),
                        Colors::text(Surface::Content, Tone::Tertiary),
                    ),
                    (None, state) => (
                        state.to_string(),
                        Colors::text(Surface::Content, Tone::Tertiary),
                    ),
                };
                let label = match &attempt.model {
                    Some(model) if !model.is_empty() => {
                        format!("{} · {model}", attempt.engine)
                    }
                    _ => attempt.engine.clone(),
                };
                div()
                    .id(ElementId::Name(
                        format!("attempt-{}", attempt.run_id).into(),
                    ))
                    .role(Role::Button)
                    .aria_label(format!(
                        "{label}: {verdict}, {} files changed. Open it.",
                        attempt.changed_files
                    ))
                    .tab_index(0)
                    .flex()
                    .items_center()
                    .gap(px(Space::ROW_H))
                    .px(px(Space::ROW_H))
                    .py(px(5.0))
                    .rounded(px(Radius::BADGE))
                    .when(attempt.selected, |row| row.bg(Fill::selected(true)))
                    .hover(|row| row.bg(crate::theme::white(Fill::HOVER)))
                    .cursor_pointer()
                    .on_click(cx.listener(move |shell, _, _, cx| {
                        shell.state().select_run(&run_id);
                        cx.notify();
                    }))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .truncate()
                            .text_size(px(Typo::META.size))
                            .text_color(Colors::text(Surface::Content, Tone::Primary))
                            .child(SharedString::from(label)),
                    )
                    .child(
                        div()
                            .text_size(px(Typo::META.size))
                            .text_color(Colors::text(Surface::Content, Tone::Tertiary))
                            .child(SharedString::from(format!(
                                "{} files",
                                attempt.changed_files
                            ))),
                    )
                    .child(
                        div()
                            .text_size(px(Typo::META.size))
                            .text_color(tint)
                            .child(SharedString::from(verdict)),
                    )
            }))
            .into_any_element(),
    )
}

/// The question the agent is blocked on, with the two answers.
///
/// A terminal agent stops dead on a prompt and waits for a keystroke. Until
/// this existed the question reached the ledger and the transcript, and there
/// was no way to reply — the run stayed "running" and never moved, which looks
/// exactly like a hang. Refusing is offered as prominently as approving: a
/// wrong approval is the expensive one.
pub(crate) fn pending_question(state: &UiState) -> Option<String> {
    state.selected_detail()?.pending_question.clone()
}

fn question_prompt(state: &UiState, cx: &mut Context<Shell>) -> Option<AnyElement> {
    let prompt = pending_question(state)?;
    let run_id = state.run_id.clone()?;
    let deny_id = run_id.clone();
    Some(
        div()
            .id("pending-question")
            .role(Role::Group)
            .aria_label(format!("The agent is waiting: {prompt}"))
            .flex()
            .flex_col()
            .flex_none()
            .gap(px(Space::ROW_H))
            .mx(px(Space::INSET))
            .mb(px(6.0))
            .p(px(Space::ROW_H))
            .rounded(px(Radius::ROW))
            .border_1()
            .border_color(Ink::ATTENTION)
            .child(
                div()
                    .text_size(px(Typo::META.size))
                    .text_color(Ink::ATTENTION)
                    .child("The agent is waiting on you"),
            )
            .child(
                div()
                    .text_size(px(Typo::ROW.size))
                    .text_color(Colors::text(Surface::Content, Tone::Primary))
                    .child(SharedString::from(prompt)),
            )
            .child(
                div()
                    .flex()
                    .gap(px(Space::ROW_H))
                    .child(
                        components::compact_control("Approve")
                            .id("question-approve")
                            .role(Role::Button)
                            .aria_label("Approve what the agent asked")
                            .tab_index(0)
                            .cursor_pointer()
                            .hover(|control| control.bg(crate::theme::white(Fill::HOVER)))
                            .on_click(cx.listener(move |shell, _, _, cx| {
                                shell.answer_run(&run_id, autoharness_core::Answer::Approve);
                                cx.notify();
                            })),
                    )
                    .child(
                        components::compact_control("Deny")
                            .id("question-deny")
                            .role(Role::Button)
                            .aria_label("Refuse what the agent asked")
                            .tab_index(0)
                            .cursor_pointer()
                            .hover(|control| control.bg(crate::theme::white(Fill::HOVER)))
                            .on_click(cx.listener(move |shell, _, _, cx| {
                                shell.answer_run(&deny_id, autoharness_core::Answer::Deny);
                                cx.notify();
                            })),
                    ),
            )
            .into_any_element(),
    )
}

/// The app's own last word, directly above the composer.
///
/// `state.status` is written on nearly every action — which engine was chosen,
/// that a new run is waiting for an objective, which path was opened, why a
/// command was refused — and until now it was rendered ONLY while the daemon
/// was disconnected. Every one of those messages went into a string nobody
/// drew, which is why acting on this app could feel like nothing had happened
/// when it had. It is shown where the user is already looking after acting.
///
/// Connection trouble keeps its own banner, so this never competes with it.
pub(crate) fn status_line(state: &UiState) -> Option<String> {
    let status = state.status.trim();
    if status.is_empty() || !state.connected {
        return None;
    }
    // The preview fixture puts its own scaffolding here.
    if status.starts_with("preview") {
        return None;
    }
    Some(status.to_string())
}

fn status_strip(state: &UiState) -> Option<AnyElement> {
    let status = status_line(state)?;
    Some(
        div()
            .id("composer-status")
            .role(Role::Status)
            .aria_label(status.clone())
            .flex()
            .flex_none()
            .items_center()
            .gap(px(6.0))
            .mx(px(Space::INSET))
            .mb(px(4.0))
            .px(px(Space::ROW_H))
            .py(px(5.0))
            .rounded(px(Radius::BADGE))
            .bg(Fill::subtle())
            .text_size(px(Typo::META.size))
            .text_color(Colors::text(Surface::Content, Tone::Secondary))
            .child(SharedString::from(status))
            .into_any_element(),
    )
}

pub(crate) fn composer_queue_summary(state: &UiState) -> Option<String> {
    if let Some(run_id) = state.run_id.as_deref() {
        let steering = state.queue.pending_steering_for(run_id);
        if steering > 0 {
            return Some(format!(
                "{steering} follow-up{} queued for this run",
                if steering == 1 { "" } else { "s" }
            ));
        }
    }
    let objectives = state.queue.pending_objectives();
    (objectives > 0).then(|| {
        format!(
            "{objectives} objective{} waiting",
            if objectives == 1 { "" } else { "s" }
        )
    })
}

fn queue_strip(label: String, cx: &mut Context<Shell>) -> AnyElement {
    let accessible_label = format!("Queued work: {label}. Open queue");
    div()
        .id("coordinator-queue-strip")
        .role(Role::Button)
        .aria_label(accessible_label)
        .tab_index(0)
        .focus_visible(|strip| strip.bg(crate::theme::white(0.14)))
        .flex()
        .flex_none()
        .items_center()
        .mx(px(Space::INSET))
        .mb(px(6.0))
        .px(px(Space::INDENT))
        .py(px(6.0))
        .rounded(px(Radius::BADGE))
        .bg(Fill::subtle())
        .text_size(px(Typo::META.size))
        .text_color(Colors::text(Surface::Content, Tone::Secondary))
        .cursor_pointer()
        .hover(|strip| strip.bg(crate::theme::white(Fill::HOVER)))
        .on_click(cx.listener(|shell, _, _, cx| {
            shell.toolbar_action(toolbar::ToolbarAction::OpenQueue, cx);
        }))
        .child("Queued")
        .child(div().w(px(Space::ROW_H)))
        .child(SharedString::from(label))
        .child(div().flex_1())
        .child("Manage ›")
        .with_animation(
            "coordinator-queue-strip-entry",
            Animation::new(motion::OVERLAY_ENTRY).with_easing(ease_out_quint()),
            |strip, delta| {
                strip
                    .relative()
                    .top(px((1.0 - delta) * 4.0))
                    .opacity(motion::overlay_opacity(delta))
            },
        )
        .into_any_element()
}

/// The ready-but-empty start screen: a prompt-shaped question and three
/// starters that type themselves into the composer. Shown only when there is
/// nothing else to show, and never instead of the composer — the whole point
/// is that the next keystroke starts work.
fn welcome(cx: &mut Context<Shell>) -> AnyElement {
    const STARTERS: [&str; 3] = [
        "Build a small CLI that organizes my downloads folder",
        "Create a landing page for a side project",
        "Explain this repository and find one bug worth fixing",
    ];
    div()
        .flex()
        .flex_col()
        .flex_1()
        .items_center()
        .justify_center()
        .gap(px(10.0))
        .py(px(32.0))
        .child(crate::icon::Icon::new(
            crate::icon::IconName::LocalAgents,
            crate::icon::IconSize::DISPLAY,
            Colors::text(Surface::Content, Tone::Secondary),
        ))
        .child(
            div()
                .text_size(px(Typo::DISPLAY_TITLE.size))
                .font_weight(Typo::DISPLAY_TITLE.weight)
                .child("What should we build?"),
        )
        .child(
            div()
                .text_size(px(Typo::META.size))
                .text_color(Colors::text(Surface::Content, Tone::Tertiary))
                .child("No repository needed — prompting with none chosen creates one in ~/AutoHarness Projects."),
        )
        .child(div().h(px(6.0)))
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(6.0))
                .children(STARTERS.iter().enumerate().map(|(index, starter)| {
                    div()
                        .id(ElementId::Name(format!("welcome-starter-{index}").into()))
                        .role(Role::Button)
                        .aria_label(*starter)
                        .px(px(Space::INDENT))
                        .py(px(7.0))
                        .rounded(px(Radius::ROW))
                        .border_1()
                        .border_color(Colors::stroke())
                        .bg(Fill::subtle())
                        .text_size(px(Typo::ROW.size))
                        .text_color(Colors::text(Surface::Content, Tone::Secondary))
                        .cursor_pointer()
                        .hover(|chip| chip.bg(crate::theme::white(Fill::HOVER)))
                        .on_click(cx.listener(move |shell, _, _, cx| {
                            shell.fill_prompt(starter);
                            cx.notify();
                        }))
                        .child(*starter)
                })),
        )
        .into_any_element()
}

/// The transcript with "Worked for …" separators interleaved: the elapsed
/// time between what the user asked and the engine's reply, computed from the
/// two rows' own ledger timestamps — bb's turn separator, from data we
/// already record. Under five seconds is not worth a line.
pub(crate) fn transcript_elements(state: &UiState) -> Vec<AnyElement> {
    let rows = transcript_rows(state);
    let mut elements = Vec::with_capacity(rows.len() + 4);
    for (index, row) in rows.iter().enumerate() {
        if index > 0
            && row.engine.is_some()
            && rows[index - 1].is_user
            && let (Some(now), Some(before)) = (row.timestamp_ms, rows[index - 1].timestamp_ms)
        {
            let elapsed_secs = (now - before) / 1000;
            if elapsed_secs >= 5 {
                elements.push(worked_for_separator(elapsed_secs));
            }
        }
        elements.push(speech_row(row).into_any_element());
    }
    elements
}

fn worked_for_separator(elapsed_secs: i64) -> AnyElement {
    let label = if elapsed_secs < 60 {
        format!("Worked for {elapsed_secs}s")
    } else {
        format!("Worked for {}m {}s", elapsed_secs / 60, elapsed_secs % 60)
    };
    div()
        .py(px(2.0))
        .text_size(px(Typo::META.size))
        .text_color(Colors::text(Surface::Sidebar, Tone::Tertiary))
        .child(SharedString::from(label))
        .into_any_element()
}

/// bb's working-tree chip: what the run has actually touched, one line above
/// the composer, with the review pane one click away. Absent until a file
/// changes — a chip reading "0 files" is furniture.
fn working_tree_chip(state: &UiState, cx: &mut Context<Shell>) -> Option<AnyElement> {
    let detail = state.selected_detail()?;
    let files = detail.changed_files.len();
    if files == 0 {
        return None;
    }
    let (added, removed) = detail
        .diff
        .as_ref()
        .map(|diff| {
            diff.lines.iter().fold((0usize, 0usize), |(a, r), line| {
                use crate::client::DiffLine;
                match line {
                    DiffLine::Added(_) => (a + 1, r),
                    DiffLine::Removed(_) => (a, r + 1),
                    _ => (a, r),
                }
            })
        })
        .unwrap_or((0, 0));
    let files_label = if files == 1 {
        "1 file".to_string()
    } else {
        format!("{files} files")
    };
    Some(
        div()
            .id("coordinator-working-tree")
            .role(Role::Button)
            .aria_label(format!(
                "Working tree: {files_label}, {added} added lines, {removed} removed lines. \
                 Open the review pane."
            ))
            .flex()
            .flex_none()
            .items_center()
            .gap(px(Space::ROW_H))
            .mx(px(Space::INSET))
            .mb(px(6.0))
            .px(px(Space::INDENT))
            .py(px(6.0))
            .rounded(px(Radius::BADGE))
            .bg(Fill::subtle())
            .text_size(px(Typo::META.size))
            .cursor_pointer()
            .hover(|chip| chip.bg(crate::theme::white(Fill::HOVER)))
            .on_click(cx.listener(|shell, _, _, cx| {
                shell.open_inspector(cx);
            }))
            .child(crate::icon::Icon::new(
                crate::icon::IconName::Branch,
                crate::icon::IconSize::COMPACT,
                Colors::text(Surface::Sidebar, Tone::Secondary),
            ))
            .child(
                div()
                    .text_color(Colors::text(Surface::Sidebar, Tone::Secondary))
                    .child("Working tree"),
            )
            .child(
                div()
                    .text_color(Colors::text(Surface::Sidebar, Tone::Tertiary))
                    .child(SharedString::from(files_label)),
            )
            .when(added > 0 || removed > 0, |chip| {
                chip.child(
                    div()
                        .text_color(Ink::ADDED)
                        .child(SharedString::from(format!("+{added}"))),
                )
                .child(
                    div()
                        .text_color(Ink::REMOVED)
                        .child(SharedString::from(format!("−{removed}"))),
                )
            })
            .child(div().flex_1())
            .child(
                div()
                    .text_color(Colors::text(Surface::Sidebar, Tone::Tertiary))
                    .child("Review ›"),
            )
            .into_any_element(),
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TranscriptRow {
    pub label: String,
    pub engine: Option<String>,
    pub text: String,
    pub timestamp_ms: Option<i64>,
    pub is_user: bool,
    pub narration: bool,
}

pub(crate) fn transcript_rows(state: &UiState) -> Vec<TranscriptRow> {
    // Every turn of the thread, not just the selected run's own messages.
    // Selecting a thread selects its root, so a per-run transcript showed the
    // opening objective and hid every answer that followed it.
    let structured: Vec<TranscriptRow> = state
        .thread_run_ids()
        .iter()
        .filter_map(|id| state.run_details.get(id))
        .flat_map(|detail| detail.messages.iter())
        .map(structured_row)
        .collect();
    if !structured.is_empty() {
        return structured;
    }

    let engine = state.engine.clone();
    state
        .chat()
        .into_iter()
        .map(|line| legacy_row(&line, &engine))
        .collect()
}

fn structured_row(message: &StructuredMessageView) -> TranscriptRow {
    let is_user = message.engine.is_none() && message.author == "You";
    TranscriptRow {
        // Which model said this. A thread can run three turns on three models,
        // and without it the transcript reads as one conversation with one
        // model — so "why did it answer differently that time" has no answer
        // on screen.
        label: match (&message.model, is_user) {
            (Some(model), false) if !model.is_empty() => {
                format!("{} · {model}", message.author)
            }
            _ => message.author.clone(),
        },
        engine: message.engine.clone(),
        text: message.text.clone(),
        timestamp_ms: Some(message.timestamp_ms),
        is_user,
        narration: false,
    }
}

fn legacy_row(line: &str, engine: &str) -> TranscriptRow {
    let (prefix, body) = match line.split_once("  ") {
        Some((p, b)) if p.chars().count() <= 5 => (p.trim().to_string(), b.to_string()),
        _ => (String::new(), line.to_string()),
    };
    let is_user = prefix == "you";
    let is_agent = prefix == "bot";
    let narration = prefix.is_empty();
    TranscriptRow {
        label: if is_user {
            "you".into()
        } else if is_agent {
            engine.to_string()
        } else {
            String::new()
        },
        engine: is_agent.then(|| engine.to_string()),
        text: body,
        timestamp_ms: None,
        is_user,
        narration,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PlanActivitySummary {
    pub task_count: usize,
    pub file_count: usize,
    pub running: Vec<(String, u8)>,
}

pub(crate) fn plan_activity_summary(state: &UiState) -> Option<PlanActivitySummary> {
    let graph = state
        .selected_detail()
        .and_then(|detail| detail.graph.as_ref())
        .or(state.graph.as_ref())?;
    Some(summary_from_graph(
        graph,
        state
            .selected_detail()
            .map(|detail| detail.changed_files.len())
            .unwrap_or_default(),
    ))
}

fn summary_from_graph(graph: &GraphView, file_count: usize) -> PlanActivitySummary {
    PlanActivitySummary {
        task_count: graph
            .nodes
            .iter()
            .filter(|node| !is_terminal_verify_node(graph, &node.id))
            .count(),
        file_count,
        running: Vec::new(),
    }
}

fn is_terminal_verify_node(graph: &GraphView, id: &str) -> bool {
    id.to_ascii_lowercase().contains("verify") && !graph.edges.iter().any(|(from, _to)| from == id)
}

#[cfg(test)]
pub(crate) fn plan_card_tokens(state: &UiState) -> Vec<String> {
    let Some(summary) = plan_activity_summary(state) else {
        return Vec::new();
    };
    let action = plan_action(state);
    vec![
        format!("{} tasks", summary.task_count),
        format!("{} files", summary.file_count),
        "~18m".into(),
        action.label.into(),
        "›".into(),
    ]
}

#[cfg(test)]
pub(crate) fn coordinator_stack_sections(state: &UiState) -> Vec<&'static str> {
    let mut sections = vec!["header", "transcript"];
    if plan_activity_summary(state).is_some() {
        sections.push("plan-card");
    }
    sections.push("composer");
    sections
}

fn plan_card(
    summary: PlanActivitySummary,
    action: PlanAction,
    cx: &mut Context<Shell>,
) -> AnyElement {
    let accessible_label = format!(
        "{} tasks, {} files, estimated 18 minutes. {}",
        summary.task_count, summary.file_count, action.label
    );
    div()
        .id("coordinator-plan-card")
        .role(Role::Button)
        .aria_label(accessible_label)
        .tab_index(0)
        .focus_visible(|card| card.bg(crate::theme::white(0.14)))
        .flex()
        .flex_none()
        .items_center()
        .gap(px(Space::INDENT))
        .mx(px(Space::INDENT))
        .mb(px(8.0))
        .px(px(Space::INDENT))
        .py(px(8.0))
        .rounded(px(Radius::ROW))
        .bg(Fill::subtle())
        .hover(|card| card.bg(crate::theme::white(Fill::HOVER)))
        .cursor_pointer()
        .on_click(cx.listener(move |shell, _, _, cx| {
            run_plan_command(shell, action.command, cx);
            cx.notify();
        }))
        .child(SharedString::from(format!("{} tasks", summary.task_count)))
        .child("·")
        .child(SharedString::from(format!("{} files", summary.file_count)))
        .child("·")
        .child("~18m")
        .child("·")
        .child(div().flex_1())
        .child(action.label)
        .child("›")
        .into_any_element()
}

fn composer_button(control: ComposerControl, cx: &mut Context<Shell>) -> AnyElement {
    let action = control.action;
    let accessible_label = match action {
        ComposerAction::ChooseRepository => "Choose Git repository",
        ComposerAction::OpenMentions => "Open mentions",
        ComposerAction::ToggleModelPicker => "Choose model and reasoning effort",
        ComposerAction::Submit => "Submit objective or follow-up",
    };
    components::compact_control(control.label)
        .id(ElementId::Name(control.id.into()))
        .role(Role::Button)
        .aria_label(accessible_label)
        .tab_index(0)
        .focus_visible(|control| control.border_color(crate::theme::white(0.48)))
        .hover(|control| control.bg(crate::theme::white(Fill::HOVER)))
        .cursor_pointer()
        .on_click(cx.listener(move |shell, _, window, cx| {
            shell.composer_action(action, window, cx);
        }))
        .into_any_element()
}

fn model_selector_button(state: &UiState, cx: &mut Context<Shell>) -> AnyElement {
    let label = format!("{}⌄", state.execution_selection_label());
    let accessible = format!(
        "Model and reasoning: {}. Open chooser",
        state.execution_selection_label()
    );
    components::compact_control(label)
        .id("coordinator-compose-model")
        .role(Role::Button)
        .aria_label(accessible)
        .tab_index(0)
        .max_w(px(120.0))
        .overflow_hidden()
        .focus_visible(|control| control.border_color(crate::theme::white(0.48)))
        .hover(|control| control.bg(crate::theme::white(Fill::HOVER)))
        .cursor_pointer()
        .on_click(cx.listener(|shell, _, window, cx| {
            shell.composer_action(ComposerAction::ToggleModelPicker, window, cx);
        }))
        .into_any_element()
}

fn repository_selector_button(state: &UiState, cx: &mut Context<Shell>) -> AnyElement {
    let (label, accessible) = state.selected_project().map_or_else(
        || {
            (
                "▱ Choose repository".to_string(),
                "Choose Git repository".to_string(),
            )
        },
        |project| {
            (
                format!("▱ {}", project.name),
                format!(
                    "Repository {} at {}. Choose another",
                    project.name, project.path
                ),
            )
        },
    );
    components::compact_control(label)
        .id("coordinator-compose-project")
        .role(Role::Button)
        .aria_label(accessible)
        .tab_index(0)
        .max_w(px(120.0))
        .overflow_hidden()
        .focus_visible(|control| control.border_color(crate::theme::white(0.48)))
        .hover(|control| control.bg(crate::theme::white(Fill::HOVER)))
        .cursor_pointer()
        .on_click(cx.listener(|shell, _, window, cx| {
            shell.composer_action(ComposerAction::ChooseRepository, window, cx);
        }))
        .into_any_element()
}

fn model_picker(
    state: &UiState,
    tab: Option<&str>,
    query: &str,
    cx: &mut Context<Shell>,
) -> AnyElement {
    use crate::execution_picker::picker_view;
    use crate::icon::{Icon, IconName, IconSize};
    use crate::settings_page::{RowMark, engine_mark};

    let view = picker_view(state, tab, query);

    // Engine tabs: one logo per available engine, bb's provider-tab recipe.
    // Gated engines are a count in the footer, never tabs.
    let tabs = div()
        .flex()
        .items_center()
        .gap(px(2.0))
        .px(px(6.0))
        .border_b_1()
        .border_color(Colors::stroke())
        .children(view.tabs.iter().map(|tab| {
            let engine = tab.engine.clone();
            let active = tab.active;
            let (icon, tint) = match engine_mark(&engine) {
                RowMark::Brand(icon) => (
                    Some(icon),
                    match icon {
                        IconName::BrandClaude => Ink::CLAY,
                        IconName::BrandGemini => Ink::GEMINI_BLUE,
                        _ => crate::theme::white(if active { 0.92 } else { 0.55 }),
                    },
                ),
                RowMark::Monogram(_) => (None, crate::theme::white(0.55)),
            };
            div()
                .id(ElementId::Name(format!("picker-tab-{engine}").into()))
                .role(Role::Tab)
                .aria_label(format!("{} models", title_case(&engine)))
                .aria_selected(active)
                .flex()
                .flex_col()
                .items_center()
                .cursor_pointer()
                .hover(|tab| tab.bg(crate::theme::white(Fill::HOVER)))
                .on_click(cx.listener(move |shell, _, _, cx| {
                    shell.model_picker_tab = Some(engine.clone());
                    cx.notify();
                }))
                .child(
                    div()
                        .h(px(30.0))
                        .px(px(10.0))
                        .flex()
                        .items_center()
                        .gap(px(6.0))
                        .children(icon.map(|icon| Icon::new(icon, IconSize::REGULAR, tint)))
                        .child(
                            div()
                                .text_size(px(Typo::META.size))
                                .font_weight(Typo::META.weight)
                                .text_color(Colors::text(
                                    Surface::Content,
                                    if active {
                                        Tone::Primary
                                    } else {
                                        Tone::Tertiary
                                    },
                                ))
                                .child(SharedString::from(title_case(&tab.engine))),
                        ),
                )
                // The underline is the active mark, like bb's provider tabs.
                .child(div().h(px(2.0)).w_full().rounded(px(1.0)).bg(if active {
                    crate::theme::white(0.9)
                } else {
                    crate::theme::white(0.0)
                }))
        }))
        .child(div().flex_1())
        .child(
            div()
                .id("composer-model-done")
                .role(Role::Button)
                .aria_label("Close the chooser")
                .flex_none()
                .p(px(6.0))
                .rounded(px(Radius::BADGE))
                .cursor_pointer()
                .hover(|close| close.bg(crate::theme::white(Fill::HOVER)))
                .on_click(cx.listener(|shell, _, window, cx| {
                    shell.composer_action(ComposerAction::ToggleModelPicker, window, cx);
                }))
                .child(Icon::new(
                    IconName::Close,
                    IconSize::COMPACT,
                    Colors::text(Surface::Content, Tone::Tertiary),
                )),
        );

    let search = div()
        .id("composer-model-filter")
        .role(Role::TextInput)
        .aria_label("Filter models")
        .aria_placeholder("Search models")
        .flex()
        .items_center()
        .gap(px(6.0))
        .mx(px(6.0))
        .mt(px(6.0))
        .px(px(Space::ROW_H))
        .h(px(26.0))
        .rounded(px(Radius::BADGE))
        .bg(Fill::subtle())
        .child(Icon::new(
            IconName::Search,
            IconSize::COMPACT,
            Colors::text(Surface::Content, Tone::Tertiary),
        ))
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .text_size(px(Typo::META.size))
                .text_color(Colors::text(
                    Surface::Content,
                    if query.is_empty() {
                        Tone::Tertiary
                    } else {
                        Tone::Primary
                    },
                ))
                .child(SharedString::from(if query.is_empty() {
                    "Search models".to_string()
                } else {
                    query.to_string()
                })),
        );

    let mut body = div()
        .id("composer-model-list")
        .role(Role::ListBox)
        .aria_label("Models")
        .flex()
        .flex_col()
        .flex_1()
        .min_h(px(0.0))
        .overflow_y_scroll()
        .pb(px(4.0))
        .child(picker_section_label("Model"));
    // A tab whose engine cannot run leads with the reason; its models are
    // still listed rather than hidden, so "why not" has an answer on screen.
    if let Some(reason) = &view.active_unavailable {
        body = body.child(
            div()
                .px(px(12.0))
                .py(px(4.0))
                .text_size(px(Typo::META.size))
                .text_color(Ink::ATTENTION)
                .child(SharedString::from(reason.clone())),
        );
    }
    if view.models.is_empty() {
        body = body.child(
            div()
                .px(px(12.0))
                .py(px(4.0))
                .text_size(px(Typo::META.size))
                .text_color(Colors::text(Surface::Content, Tone::Tertiary))
                .child(if query.is_empty() {
                    "No models reported".to_string()
                } else {
                    format!("Nothing matches \"{}\"", query.trim())
                }),
        );
    }
    body = body.children(
        view.models
            .iter()
            .enumerate()
            .map(|(index, choice)| picker_row(index, choice, cx)),
    );

    // The reasoning section belongs to the model in use on this tab.
    if let Some(target) = view
        .effort_target
        .as_ref()
        .filter(|target| target.efforts.len() > 1)
    {
        body = body
            .child(
                div()
                    .mx(px(6.0))
                    .my(px(4.0))
                    .h(px(1.0))
                    .bg(Colors::stroke()),
            )
            .child(picker_section_label("Reasoning"));
        for (index, effort) in target.efforts.iter().enumerate() {
            let selected = target.current && *effort == target.effort;
            let mut choose = target.clone();
            choose.effort = effort.clone();
            body = body.child(
                div()
                    .id(ElementId::Name(format!("picker-effort-{index}").into()))
                    .role(Role::ListBoxOption)
                    .aria_label(format!("Reasoning effort {effort}"))
                    .aria_selected(selected)
                    .mx(px(6.0))
                    .px(px(8.0))
                    .h(px(26.0))
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .rounded(px(Radius::ROW))
                    .cursor_pointer()
                    .hover(|row| row.bg(crate::theme::white(Fill::HOVER)))
                    .on_click(cx.listener(move |shell, _, _, cx| {
                        shell.choose_execution(&choose);
                        cx.notify();
                    }))
                    .child(
                        div()
                            .flex_1()
                            .text_size(px(Typo::ROW.size))
                            .text_color(Colors::text(Surface::Content, Tone::Primary))
                            .child(SharedString::from(title_case(effort))),
                    )
                    .children(selected.then(|| {
                        Icon::new(
                            IconName::Check,
                            IconSize::COMPACT,
                            Colors::text(Surface::Content, Tone::Secondary),
                        )
                    })),
            );
        }
    }

    let mut surface = components::floating_surface()
        .id("composer-model-picker")
        .role(Role::Dialog)
        .aria_label("Choose the engine, model and reasoning effort")
        .absolute()
        .left(px(Space::INSET))
        .bottom(px(98.0))
        .w(px(300.0))
        .max_h(px(400.0))
        .flex()
        .flex_col()
        .child(tabs)
        .child(search)
        .child(body);
    if view.coming_soon > 0 {
        surface = surface.child(
            div()
                .px(px(12.0))
                .py(px(6.0))
                .border_t_1()
                .border_color(Colors::stroke())
                .text_size(px(Typo::META.size))
                .text_color(Colors::text(Surface::Content, Tone::Tertiary))
                .child(SharedString::from(format!(
                    "{} more engines coming soon",
                    view.coming_soon
                ))),
        );
    }
    surface.into_any_element()
}

fn picker_section_label(label: &'static str) -> AnyElement {
    div()
        .px(px(12.0))
        .pt(px(6.0))
        .pb(px(2.0))
        .text_size(px(Typo::SECTION_HEADER.size))
        .font_weight(Typo::SECTION_HEADER.weight)
        .text_color(Colors::text(Surface::Content, Tone::Tertiary))
        .child(label)
        .into_any_element()
}

/// One model row: a single line, checkmark when in use. diri's density —
/// descriptions live in the row you have selected, not on all of them.
fn picker_row(
    index: usize,
    choice: &crate::execution_picker::ExecutionChoice,
    cx: &mut Context<Shell>,
) -> AnyElement {
    use crate::icon::{Icon, IconName, IconSize};

    let picked = choice.clone();
    let current = choice.current;
    div()
        .id(ElementId::Name(format!("execution-choice-{index}").into()))
        .role(Role::ListBoxOption)
        .aria_label(format!(
            "{} {}",
            title_case(&choice.engine),
            choice.model_label
        ))
        .aria_selected(current)
        .mx(px(6.0))
        .my(px(1.0))
        .px(px(8.0))
        .h(px(28.0))
        .flex()
        .items_center()
        .gap(px(8.0))
        .rounded(px(Radius::ROW))
        .cursor_pointer()
        .when(current, |row| row.bg(crate::theme::white(0.08)))
        .hover(|row| row.bg(crate::theme::white(Fill::HOVER)))
        .on_click(cx.listener(move |shell, _, _, cx| {
            shell.choose_execution(&picked);
            cx.notify();
        }))
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .truncate()
                .text_size(px(Typo::ROW.size))
                .text_color(Colors::text(Surface::Content, Tone::Primary))
                .child(SharedString::from(choice.model_label.clone())),
        )
        .children(choice.provider_default.then(|| {
            div()
                .text_size(px(Typo::META.size))
                .text_color(Colors::text(Surface::Content, Tone::Tertiary))
                .child("default")
        }))
        .children(current.then(|| {
            Icon::new(
                IconName::Check,
                IconSize::COMPACT,
                Colors::text(Surface::Content, Tone::Secondary),
            )
        }))
        .into_any_element()
}

fn title_case(value: &str) -> String {
    let mut chars = value.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// One transcript line.
///
/// What the user said leads at full strength; what the agent said supports it;
/// and everything the harness did (steering, handoffs, edits) is quieter still
/// and italic, so a transcript reads as a conversation with machinery around
/// it rather than as an undifferentiated log.
fn speech_row(row: &TranscriptRow) -> impl IntoElement + use<> {
    let is_agent = row.engine.is_some();
    let (label_color, body_color) = if row.is_user {
        (
            Colors::text(Surface::Sidebar, Tone::Tertiary),
            Colors::text(Surface::Sidebar, Tone::Primary),
        )
    } else if is_agent {
        (
            Ink::working(row.engine.as_deref().unwrap_or_default()),
            Colors::text(Surface::Sidebar, Tone::Secondary),
        )
    } else {
        (
            Colors::text(Surface::Sidebar, Tone::Tertiary),
            Colors::text(Surface::Sidebar, Tone::Tertiary),
        )
    };
    let initial = row
        .label
        .chars()
        .next()
        .map(|c| c.to_uppercase().collect::<String>())
        .unwrap_or_else(|| "·".into());
    let timestamp = row.timestamp_ms.map(timestamp_label).unwrap_or_default();

    div()
        .flex()
        .items_start()
        .gap(px(Space::INDENT))
        // Turns need air between them or the transcript reads as one block.
        .py(px(if row.narration { 2.0 } else { 7.0 }))
        .text_size(px(Typo::ROW.size))
        .child(
            div()
                .flex_none()
                .size(px(24.0))
                .items_center()
                .justify_center()
                .rounded_full()
                .bg(if row.is_user {
                    Fill::subtle()
                } else {
                    let mut color = Ink::working(row.engine.as_deref().unwrap_or_default());
                    color.a = 0.30;
                    color
                })
                .text_size(px(Typo::META.size))
                // Glyph metrics leave the line box's optical center above the
                // circle's; matching the line height to it centers the letter.
                .line_height(px(Typo::META.size))
                .font_weight(Typo::META.weight)
                .text_color(if row.is_user {
                    Colors::text(Surface::Sidebar, Tone::Primary)
                } else {
                    Ink::working(row.engine.as_deref().unwrap_or_default())
                })
                .child(SharedString::from(initial)),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w(px(0.0))
                .gap(px(4.0))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(Space::ROW_H))
                        .child(
                            div()
                                .text_size(px(Typo::ROW_EMPHASIZED.size))
                                .font_weight(Typo::ROW_EMPHASIZED.weight)
                                .text_color(label_color)
                                .child(SharedString::from(row.label.clone())),
                        )
                        .child(
                            div()
                                .text_size(px(Typo::META.size))
                                .text_color(Colors::text(Surface::Sidebar, Tone::Tertiary))
                                .child(SharedString::from(timestamp)),
                        ),
                )
                .child(
                    div()
                        .when(row.is_user, |t| t.font_weight(Typo::ROW_EMPHASIZED.weight))
                        .when(row.narration, |t| t.italic().text_size(px(Typo::META.size)))
                        .text_color(body_color)
                        .child(SharedString::from(row.text.clone())),
                ),
        )
}

#[cfg(test)]
mod tests {
    /// A turn says which model produced it.
    ///
    /// A thread can run three turns on three models — that is the point of
    /// switching mid-thread — and without this the transcript reads as one
    /// conversation with one model, so "why did it answer differently that
    /// time" has no answer on screen.
    #[test]
    fn a_turn_says_which_model_produced_it() {
        let agent = crate::client::StructuredMessageView {
            author: "Claude".into(),
            engine: Some("claude".into()),
            model: Some("opus".into()),
            text: "done".into(),
            timestamp_ms: 0,
        };
        assert_eq!(structured_row(&agent).label, "Claude · opus");

        // A provider-default turn has no model to name, and inventing one
        // would claim something the run never pinned.
        let default_model = crate::client::StructuredMessageView {
            model: None,
            ..agent.clone()
        };
        assert_eq!(structured_row(&default_model).label, "Claude");

        // The user is not a model.
        let user = crate::client::StructuredMessageView {
            author: "You".into(),
            engine: None,
            model: Some("opus".into()),
            text: "do it".into(),
            timestamp_ms: 0,
        };
        assert_eq!(structured_row(&user).label, "You");
    }

    /// The app's status messages must be visible.
    ///
    /// This is the regression behind "I click and nothing happens": every
    /// action writes `state.status` — which engine, that a run is waiting for
    /// an objective, why a command was refused — and it was rendered only
    /// while the daemon was DISCONNECTED. The app was answering into a string
    /// nobody drew.
    #[test]
    fn the_apps_own_status_is_shown_while_it_is_connected() {
        let mut state = crate::client::UiState {
            connected: true,
            ..crate::client::UiState::default()
        };

        // Nothing to say, nothing shown.
        assert_eq!(status_line(&state), None);

        state.status = "new run — describe the objective".into();
        assert_eq!(
            status_line(&state).as_deref(),
            Some("new run — describe the objective")
        );

        // Connection trouble has its own banner; this must not compete.
        state.connected = false;
        assert_eq!(status_line(&state), None);

        // The preview fixture's scaffolding is not a message to the user.
        state.connected = true;
        state.status = "preview fixture".into();
        assert_eq!(status_line(&state), None);

        // Whitespace is not a message either.
        state.status = "   ".into();
        assert_eq!(status_line(&state), None);
    }

    use super::*;
    use crate::client::{RunDetailView, StructuredMessageView};
    use std::collections::HashMap;

    #[test]
    fn transcript_rows_preserve_structured_author_engine_and_timestamp() {
        let mut state = UiState {
            engine: "codex".into(),
            run_id: Some("r1".into()),
            ..UiState::default()
        };
        state.run_details.insert(
            "r1".into(),
            RunDetailView {
                run_id: "r1".into(),
                messages: vec![
                    StructuredMessageView {
                        author: "You".into(),
                        engine: None,
                        model: None,
                        text: "Please fix the cache key".into(),
                        timestamp_ms: 1_746_721_500_000,
                    },
                    StructuredMessageView {
                        author: "Claude".into(),
                        engine: Some("claude".into()),
                        model: None,
                        text: "I'll implement the normalization change.".into(),
                        timestamp_ms: 1_746_721_560_000,
                    },
                ],
                ..RunDetailView::default()
            },
        );
        state.chat_by_run = HashMap::from([(
            "r1".into(),
            vec!["you  legacy".into(), "bot  wrong global engine".into()],
        )]);

        let rows = transcript_rows(&state);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].label, "You");
        assert_eq!(rows[0].engine.as_deref(), None);
        assert_eq!(rows[0].timestamp_ms, Some(1_746_721_500_000));
        assert_eq!(rows[1].label, "Claude");
        assert_eq!(rows[1].engine.as_deref(), Some("claude"));
        assert_eq!(rows[1].text, "I'll implement the normalization change.");
    }

    #[test]
    fn graph_projects_inline_plan_activity_summary() {
        let state = crate::preview::populated();
        let summary = plan_activity_summary(&state).expect("preview has graph");
        assert_eq!(summary.task_count, 4);
        assert_eq!(summary.file_count, 2);
        assert!(summary.running.is_empty());
        assert_eq!(
            plan_card_tokens(&state),
            vec!["4 tasks", "2 files", "~18m", "Review plan", "›"]
        );
        assert_eq!(
            coordinator_stack_sections(&state),
            vec!["header", "transcript", "plan-card", "composer"]
        );
    }

    #[test]
    fn plan_action_is_approve_only_when_graph_awaits_approval() {
        let mut state = crate::preview::populated();
        assert_eq!(
            plan_action(&state),
            PlanAction {
                label: "Review plan",
                command: PlanCommand::OpenOverview,
            }
        );

        let detail = state
            .run_details
            .get_mut("feat-cache-key-stability")
            .expect("preview detail");
        detail
            .graph
            .as_mut()
            .expect("preview graph")
            .awaiting_approval = true;
        assert_eq!(
            plan_action(&state),
            PlanAction {
                label: "Approve plan",
                command: PlanCommand::Submit("/approve"),
            }
        );
    }

    #[test]
    fn composer_controls_are_reachable_and_honest() {
        let controls = composer_controls();

        assert_eq!(
            controls
                .iter()
                .map(|control| control.label)
                .collect::<Vec<_>>(),
            vec!["+", "@", "▷"]
        );
        assert_eq!(
            controls
                .iter()
                .map(|control| control.action)
                .collect::<Vec<_>>(),
            vec![
                ComposerAction::ChooseRepository,
                ComposerAction::OpenMentions,
                ComposerAction::Submit,
            ]
        );
        assert!(controls.iter().any(|control| control.label == "+"));
    }

    #[test]
    fn composer_queue_summary_prioritizes_the_selected_runs_followups() {
        use autoharness_protocol::params::{QueueItem, QueueKind, QueueState};

        let mut state = crate::preview::populated();
        state.queue.replace(vec![
            QueueItem {
                id: "objective".into(),
                kind: QueueKind::Objective,
                state: QueueState::Pending,
                run_id: "other".into(),
                project_id: "repo-autoharness".into(),
                content: "other objective".into(),
                position: 1024,
                created_at_ms: 1,
                updated_at_ms: 1,
                error: None,
            },
            QueueItem {
                id: "steer".into(),
                kind: QueueKind::Steering,
                state: QueueState::Pending,
                run_id: "feat-cache-key-stability".into(),
                project_id: "repo-autoharness".into(),
                content: "also add tests".into(),
                position: 1024,
                created_at_ms: 2,
                updated_at_ms: 2,
                error: None,
            },
        ]);
        assert_eq!(
            composer_queue_summary(&state).as_deref(),
            Some("1 follow-up queued for this run")
        );
        state.run_id = Some("other-run".into());
        assert_eq!(
            composer_queue_summary(&state).as_deref(),
            Some("1 objective waiting")
        );
    }
}
