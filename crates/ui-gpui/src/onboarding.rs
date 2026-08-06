//! First-run setup.
//!
//! AutoHarness needs three things before it can do anything: a daemon, an
//! engine that is installed and signed in, and a repository to work in. Until
//! this existed, a new install showed an empty cockpit and a composer that
//! answered "add a project first: /add <path>" — a command nobody had been told
//! about, for one of three preconditions, with no way to see the other two.
//!
//! The shape follows diri's readiness surfaces: every step is listed whether or
//! not it passes, a step that cannot pass says *why* in the daemon's own words,
//! and the thing you would do about it is right there. Nothing here reports a
//! state it has not been told; a step the app cannot observe says so rather
//! than showing a reassuring tick.

use gpui::prelude::*;
use gpui::{AnyElement, Context, ElementId, Role, SharedString, div, px};

use crate::client::UiState;
use crate::icon::{Icon, IconName, IconSize};
use crate::theme::{self, Colors, Fill, Ink, Radius, Space, Surface, Tone, Typo};
use crate::{Shell, components};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StepState {
    /// Observed to be satisfied.
    Done,
    /// Observed to be unsatisfied. `detail` says what the daemon reported.
    Blocked,
    /// Not yet observed. Never drawn as either passing or failing.
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SetupAction {
    RecheckEngines,
    ChooseRepository,
    StartFirstRun,
    CheckSandbox,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SetupStep {
    pub title: String,
    pub detail: String,
    pub state: StepState,
    /// `None` when there is nothing the user can usefully press — a step that
    /// is already done, or one that resolves itself.
    pub action: Option<(&'static str, SetupAction)>,
    /// Whether an unfinished step keeps the first run gated. The repository
    /// step does not: prompting with none chosen creates one.
    pub gating: bool,
}

/// The setup checklist for the current daemon state.
pub(crate) fn steps(state: &UiState) -> Vec<SetupStep> {
    let mut steps = Vec::new();

    steps.push(if state.connected {
        SetupStep {
            title: "Daemon running".into(),
            detail: "Connected. Runs, worktrees and the event ledger live here.".into(),
            state: StepState::Done,
            action: None,
            gating: true,
        }
    } else {
        SetupStep {
            title: "Daemon running".into(),
            detail: if state.status.is_empty() {
                "Starting…".into()
            } else {
                state.status.clone()
            },
            // Reconnection is automatic, so there is no button worth pressing.
            state: StepState::Blocked,
            action: None,
            gating: true,
        }
    });

    let ready: Vec<&str> = state
        .engines
        .iter()
        .filter(|engine| engine.ready)
        .map(|engine| engine.name.as_str())
        .collect();
    steps.push(if state.engines.is_empty() {
        SetupStep {
            title: "Sign in to an engine".into(),
            detail: "Not checked yet.".into(),
            state: StepState::Unknown,
            action: Some(("Check again", SetupAction::RecheckEngines)),
            gating: true,
        }
    } else if ready.is_empty() {
        SetupStep {
            title: "Sign in to an engine".into(),
            // The daemon's own words. It asked each CLI through the same
            // sanitized environment a run gets, so this is what a run would hit.
            detail: state
                .engines
                .iter()
                .map(|engine| {
                    let why = engine
                        .problems
                        .first()
                        .cloned()
                        .unwrap_or_else(|| "unavailable".into());
                    format!("{}: {why}", engine.name)
                })
                .collect::<Vec<_>>()
                .join(" · "),
            state: StepState::Blocked,
            action: Some(("Check again", SetupAction::RecheckEngines)),
            gating: true,
        }
    } else {
        SetupStep {
            title: "Sign in to an engine".into(),
            detail: format!("Ready: {}", ready.join(", ")),
            state: StepState::Done,
            action: None,
            gating: true,
        }
    });

    // Optional since `project.create`: prompting with no repository chosen
    // creates a fresh one under ~/AutoHarness, visibly. The step stays listed
    // so choosing an existing repository is still one press away, but it
    // never gates the first run.
    steps.push(match state.selected_project() {
        Some(project) => SetupStep {
            title: "Choose a repository".into(),
            detail: project.path.clone(),
            state: StepState::Done,
            action: Some(("Change", SetupAction::ChooseRepository)),
            gating: false,
        },
        None => SetupStep {
            title: "Choose a repository".into(),
            detail: "Optional — prompting with none chosen creates a fresh repository in \
                     ~/AutoHarness. Runs work in their own git worktree either way."
                .into(),
            state: StepState::Unknown,
            action: Some(("Choose…", SetupAction::ChooseRepository)),
            gating: false,
        },
    });

    // The sandbox is fail-closed: no sandbox, no run. It used to be described
    // here rather than measured, because the UI had no `sandbox.check` state —
    // so a first launch could pass every visible step and then refuse the
    // first objective for a reason never shown.
    steps.push(match &state.sandbox {
        None => SetupStep {
            title: "Sandbox".into(),
            detail: "Not checked yet.".into(),
            state: StepState::Unknown,
            action: Some(("Check now", SetupAction::CheckSandbox)),
            gating: true,
        },
        Some(sandbox) if sandbox.ready => SetupStep {
            title: "Sandbox".into(),
            detail: "Canaries passed. Runs are confined; a failed check blocks a run rather \
                     than running unsandboxed."
                .into(),
            state: StepState::Done,
            action: None,
            gating: true,
        },
        Some(sandbox) => SetupStep {
            title: "Sandbox".into(),
            detail: sandbox
                .problems
                .first()
                .cloned()
                .unwrap_or_else(|| "The sandbox canaries failed.".into()),
            state: StepState::Blocked,
            action: Some(("Check again", SetupAction::CheckSandbox)),
            gating: true,
        },
    });

    let blocked_before = steps
        .iter()
        .any(|step| step.gating && step.state != StepState::Done && step.action.is_some());
    steps.push(SetupStep {
        title: "Start your first run".into(),
        detail: "Runs work in their own git worktree on an ah/run-* branch.".into(),
        state: if state.runs.is_empty() {
            StepState::Unknown
        } else {
            StepState::Done
        },
        action: (!blocked_before).then_some(("New run", SetupAction::StartFirstRun)),
        gating: true,
    });

    steps
}

/// Whether setup is far enough along that the cockpit is worth showing.
///
/// Deliberately not "every step is Done": a user with a repository and a
/// working engine has finished setting up even before their first run, and
/// keeping the panel up after that would be nagging rather than onboarding.
pub(crate) fn is_complete(state: &UiState) -> bool {
    state.connected
        && state.engines.iter().any(|engine| engine.ready)
        // A repository is deliberately NOT required: prompting with none
        // chosen creates one. Requiring it here would park a full-page
        // setup screen over the composer that makes that possible.
        // A sandbox that has failed its canaries means no run can start at
        // all, so setup is not finished no matter what else is in place.
        && state.sandbox.as_ref().is_none_or(|sandbox| sandbox.ready)
}

/// Whether to show the panel at all: incomplete setup, and no run to look at.
pub(crate) fn should_show(state: &UiState) -> bool {
    !is_complete(state) && state.run_id.is_none()
}

/// The whole first-run surface.
///
/// Until the app can take an objective, setup IS the app — not a card
/// competing with an empty transcript above a composer that would refuse the
/// objective anyway. The page fills the window; the palette and settings
/// still layer above it, and it disappears on its own the moment setup is
/// far enough along ([`should_show`]).
pub(crate) fn first_run_page(state: &UiState, cx: &mut Context<Shell>) -> AnyElement {
    div()
        .id("onboarding-page")
        .role(Role::Group)
        .aria_label("Set up AutoHarness")
        .absolute()
        .inset_0()
        .flex()
        .flex_col()
        .bg(Colors::BACKGROUND)
        .child(
            div()
                .id("onboarding-page-scroll")
                .flex_1()
                .min_h(px(0.0))
                .overflow_y_scroll()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap(px(20.0))
                .pb(px(48.0))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .items_center()
                        .gap(px(8.0))
                        .child(Icon::new(
                            IconName::LocalAgents,
                            IconSize::DISPLAY,
                            Colors::text(Surface::Content, Tone::Secondary),
                        ))
                        .child(
                            div()
                                .text_size(px(Typo::DISPLAY_TITLE.size))
                                .font_weight(Typo::DISPLAY_TITLE.weight)
                                .child("AutoHarness"),
                        )
                        .child(
                            div()
                                .text_size(px(Typo::META.size))
                                .text_color(Colors::text(Surface::Content, Tone::Tertiary))
                                .child("One objective, run and judged in its own worktree."),
                        ),
                )
                .child(view(state, cx)),
        )
        .into_any_element()
}

pub(crate) fn view(state: &UiState, cx: &mut Context<Shell>) -> AnyElement {
    let steps = steps(state);
    components::floating_surface()
        .id("onboarding")
        .role(Role::Group)
        .aria_label("Set up AutoHarness")
        .w(px(520.0))
        .p(px(Space::INSET))
        .gap(px(Space::ROW_H))
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(4.0))
                .px(px(Space::ROW_H))
                .pt(px(6.0))
                .child(
                    div()
                        .text_size(px(Typo::DISPLAY_TITLE.size))
                        .font_weight(Typo::DISPLAY_TITLE.weight)
                        .child("Set up AutoHarness"),
                )
                .child(
                    div()
                        .text_size(px(Typo::META.size))
                        .text_color(Colors::text(Surface::Content, Tone::Tertiary))
                        .child("Three things, then you can give it an objective."),
                ),
        )
        .children(
            steps
                .into_iter()
                .enumerate()
                .map(|(index, step)| step_row(index, step, cx)),
        )
        .into_any_element()
}

fn step_row(index: usize, step: SetupStep, cx: &mut Context<Shell>) -> AnyElement {
    let (icon, tint) = match step.state {
        StepState::Done => (IconName::CheckCircle, Ink::FRESH),
        StepState::Blocked => (IconName::Warning, Ink::ATTENTION),
        StepState::Unknown => (
            IconName::Clock,
            Colors::text(Surface::Content, Tone::Tertiary),
        ),
    };
    div()
        .flex()
        .items_start()
        .gap(px(Space::ROW_H))
        .px(px(Space::ROW_H))
        .py(px(9.0))
        .rounded(px(Radius::ROW))
        .child(
            div()
                .pt(px(1.0))
                .child(Icon::new(icon, IconSize::REGULAR, tint)),
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
                        .font_weight(Typo::ROW_EMPHASIZED.weight)
                        .child(SharedString::from(step.title)),
                )
                .child(
                    div()
                        .text_size(px(Typo::META.size))
                        .text_color(Colors::text(Surface::Content, Tone::Tertiary))
                        .child(SharedString::from(step.detail)),
                ),
        )
        .children(step.action.map(|(label, action)| {
            components::compact_control(label)
                .id(ElementId::Name(format!("onboarding-action-{index}").into()))
                .role(Role::Button)
                .aria_label(label)
                .tab_index(0)
                .focus_visible(|control| control.border_color(theme::white(0.48)))
                .hover(|control| control.bg(theme::white(Fill::HOVER)))
                .cursor_pointer()
                .on_click(cx.listener(move |shell, _, window, cx| {
                    shell.setup_action(action, window, cx);
                }))
        }))
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::EngineStatus;

    fn engine(name: &str, ready: bool, problem: Option<&str>) -> EngineStatus {
        EngineStatus {
            name: name.into(),
            ready,
            installed: true,
            authenticated: Some(ready),
            version: Some("1.0".into()),
            problems: problem.into_iter().map(str::to_string).collect(),
            models: Vec::new(),
            model_load_error: None,
        }
    }

    /// A brand-new install must be told what is missing, in order, with
    /// something to press for each thing it can act on.
    #[test]
    fn a_fresh_install_lists_every_precondition_with_an_action() {
        let state = UiState::default();
        assert!(should_show(&state), "an unset-up app shows setup");

        let steps = steps(&state);
        assert_eq!(steps.len(), 5);
        assert_eq!(steps[0].title, "Daemon running");
        assert_eq!(steps[1].title, "Sign in to an engine");
        assert_eq!(steps[2].title, "Choose a repository");
        assert_eq!(steps[3].title, "Sandbox");
        assert_eq!(steps[4].title, "Start your first run");

        // Nothing observed yet is Unknown, never a tick.
        assert_eq!(steps[1].state, StepState::Unknown);
        assert!(steps.iter().all(|step| step.state != StepState::Done));

        // The last step is not offered while an earlier one still blocks it —
        // a button that cannot work is worse than no button.
        assert!(steps[4].action.is_none());
    }

    /// An engine that cannot run is reported in the daemon's own words, not as
    /// a generic failure the user cannot act on.
    #[test]
    fn a_blocked_engine_step_repeats_the_daemons_reason() {
        let state = UiState {
            connected: true,
            engines: vec![
                engine("codex", false, Some("not installed")),
                engine(
                    "claude",
                    false,
                    Some("not signed in: run `claude auth login`"),
                ),
            ],
            ..UiState::default()
        };

        let steps = steps(&state);
        assert_eq!(steps[1].state, StepState::Blocked);
        assert!(steps[1].detail.contains("not installed"), "{:?}", steps[1]);
        assert!(
            steps[1].detail.contains("claude auth login"),
            "{:?}",
            steps[1]
        );
        assert_eq!(
            steps[1].action,
            Some(("Check again", SetupAction::RecheckEngines))
        );
    }

    /// Setup is done once the app can actually take an objective, and the
    /// panel gets out of the way at that point rather than after a first run.
    /// A repository is deliberately not required: prompting with none chosen
    /// creates one, so the setup page must not stand between the user and
    /// the composer that makes that possible.
    #[test]
    fn setup_completes_when_the_app_can_take_an_objective() {
        let mut state = UiState {
            connected: true,
            engines: vec![engine("codex", true, None)],
            ..UiState::default()
        };
        state.sandbox = Some(crate::client::SandboxView {
            ready: true,
            problems: Vec::new(),
        });
        assert!(
            is_complete(&state),
            "no repository chosen is not incomplete — prompting creates one"
        );
        assert!(!should_show(&state));

        // Without a repository the step says it is optional and what happens,
        // and the first run is still offered.
        let open = steps(&state);
        assert_eq!(open[2].state, StepState::Unknown);
        assert!(open[2].detail.contains("Optional"), "{:?}", open[2].detail);
        assert!(!open[2].gating);
        assert_eq!(
            open[4].action,
            Some(("New run", SetupAction::StartFirstRun)),
            "a missing repository never gates the first run"
        );

        state.projects.push(crate::client::Project {
            id: "p1".into(),
            name: "demo".into(),
            path: "/tmp/demo".into(),
        });
        let chosen = steps(&state);
        assert_eq!(chosen[2].state, StepState::Done);
        assert_eq!(
            chosen[4].action,
            Some(("New run", SetupAction::StartFirstRun))
        );
    }

    /// A failed sandbox is reported as measured, in the daemon's words, and
    /// keeps setup incomplete.
    ///
    /// The regression: this step used to DESCRIBE the sandbox rather than
    /// check it, because the UI carried no `sandbox.check` state. A first
    /// launch could pass every visible step and then have its first objective
    /// refused for a reason it had never been shown.
    #[test]
    fn a_failing_sandbox_blocks_setup_and_says_what_failed() {
        let mut state = UiState {
            connected: true,
            engines: vec![engine("codex", true, None)],
            ..UiState::default()
        };
        state.projects.push(crate::client::Project {
            id: "p1".into(),
            name: "demo".into(),
            path: "/tmp/demo".into(),
        });

        // Not measured yet is a third state, never a tick.
        assert_eq!(steps(&state)[3].state, StepState::Unknown);
        assert_eq!(
            steps(&state)[3].action,
            Some(("Check now", SetupAction::CheckSandbox))
        );
        // And nothing downstream is offered while it is unknown.
        assert!(steps(&state)[4].action.is_none());

        state.sandbox = Some(crate::client::SandboxView {
            ready: false,
            problems: vec!["canary network-egress failed: reached example.com".into()],
        });
        let failing = steps(&state);
        assert_eq!(failing[3].state, StepState::Blocked);
        assert!(
            failing[3].detail.contains("network-egress"),
            "{:?}",
            failing[3]
        );
        assert!(
            !is_complete(&state),
            "no run can start, so setup is not finished"
        );

        state.sandbox = Some(crate::client::SandboxView {
            ready: true,
            problems: Vec::new(),
        });
        assert_eq!(steps(&state)[3].state, StepState::Done);
        assert!(is_complete(&state));
    }

    /// A selected run means the user is working, so setup never covers it.
    #[test]
    fn an_open_run_hides_setup_even_when_a_step_is_incomplete() {
        let state = UiState {
            run_id: Some("r1".into()),
            ..UiState::default()
        };
        assert!(!should_show(&state));
    }
}
