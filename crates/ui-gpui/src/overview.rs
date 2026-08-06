//! Typed run overview model for the cockpit overlay.

use crate::client::UiState;
use crate::theme::Status;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverviewLane {
    NeedsYou,
    Running,
    Done,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverviewRow {
    pub run_id: String,
    pub lane: OverviewLane,
    pub title: String,
    pub project: String,
    pub engine: String,
    pub status: String,
    pub branch: String,
    pub check: String,
    pub usage: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OverviewCounts {
    pub needs_you: usize,
    pub running: usize,
    pub done: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverviewModel {
    pub connected: bool,
    pub selected_run_id: Option<String>,
    pub empty_reason: Option<String>,
    pub counts: OverviewCounts,
    pub rows: Vec<OverviewRow>,
}

pub fn model(state: &UiState, mru_run_ids: &[String]) -> OverviewModel {
    let mut rows = Vec::new();
    let mut counts = OverviewCounts::default();

    for run in state.runs.iter().filter(|run| run.parent_run_id.is_none()) {
        let tip = state.tip_of(&run.id).unwrap_or(run);
        let lane = lane_for_state(&tip.state);
        match lane {
            OverviewLane::NeedsYou => counts.needs_you += 1,
            OverviewLane::Running => counts.running += 1,
            OverviewLane::Done => counts.done += 1,
        }
        let project = state
            .projects
            .iter()
            .find(|project| project.id == tip.project_id)
            .map(|project| project.name.clone())
            .unwrap_or_else(|| "unknown project".into());
        let detail = state.run_details.get(&tip.id);
        let branch = detail
            .and_then(|detail| detail.worktree.as_ref())
            .and_then(|worktree| worktree.branch.clone())
            .or_else(|| {
                detail
                    .and_then(|detail| detail.worktree.as_ref())
                    .and_then(|worktree| worktree.path.clone())
            })
            .or_else(|| {
                (state.run_id.as_deref() == Some(tip.id.as_str()))
                    .then(|| state.selected_detail())
                    .flatten()
                    .and_then(|detail| detail.worktree.as_ref())
                    .and_then(|worktree| worktree.branch.clone())
            })
            .unwrap_or_else(|| "no worktree".into());
        let check = detail
            .and_then(|detail| detail.checks.last())
            .map(|check| {
                let verdict = match check.passed {
                    Some(true) => "passed",
                    Some(false) => "failed",
                    None => "pending",
                };
                format!("{} {verdict}", check.name)
            })
            .unwrap_or_else(|| {
                state
                    .check_command
                    .as_ref()
                    .map(|check| format!("{check} configured"))
                    .unwrap_or_else(|| "no check".into())
            });
        let usage = detail
            .map(|detail| format_usage(&detail.usage))
            .filter(|usage| !usage.is_empty())
            .unwrap_or_else(|| "no usage".into());

        rows.push(OverviewRow {
            run_id: tip.id.clone(),
            lane,
            title: if tip.objective.is_empty() {
                tip.id.chars().take(8).collect()
            } else {
                tip.objective.clone()
            },
            project,
            engine: tip.engine.clone(),
            status: status_label(&tip.state).into(),
            branch,
            check,
            usage,
        });
    }

    rows.sort_by(|left, right| {
        lane_priority(left.lane)
            .cmp(&lane_priority(right.lane))
            .then_with(|| {
                mru_rank(mru_run_ids, &left.run_id).cmp(&mru_rank(mru_run_ids, &right.run_id))
            })
            .then_with(|| run_recency(state, &right.run_id).cmp(&run_recency(state, &left.run_id)))
    });

    let empty_reason = if rows.is_empty() {
        Some(if state.connected {
            "No runs yet".into()
        } else {
            "Offline — no daemon state yet".into()
        })
    } else {
        None
    };

    OverviewModel {
        connected: state.connected,
        selected_run_id: state.run_id.clone(),
        empty_reason,
        counts,
        rows,
    }
}

pub fn select_row(state: &mut UiState, row: &OverviewRow) {
    state.select_run(&row.run_id);
}

fn lane_for_state(state: &str) -> OverviewLane {
    match state {
        "needs-you" | "blocked" | "paused" | "awaiting_approval" => OverviewLane::NeedsYou,
        "running" | "working" | "pending" => OverviewLane::Running,
        _ => OverviewLane::Done,
    }
}

fn lane_priority(lane: OverviewLane) -> usize {
    match lane {
        OverviewLane::NeedsYou => 0,
        OverviewLane::Running => 1,
        OverviewLane::Done => 2,
    }
}

fn mru_rank(mru_run_ids: &[String], run_id: &str) -> usize {
    mru_run_ids
        .iter()
        .position(|id| id == run_id)
        .unwrap_or(usize::MAX)
}

fn run_recency(state: &UiState, run_id: &str) -> usize {
    state
        .runs
        .iter()
        .position(|run| run.id == run_id)
        .unwrap_or(0)
}

fn status_label(state: &str) -> &'static str {
    Status::of_run(state).label()
}

fn format_usage(usage: &crate::client::UsageView) -> String {
    match (usage.context_tokens, usage.context_limit_tokens) {
        (Some(context), Some(limit)) => {
            format!("{} / {} tokens", comma(context), comma(limit))
        }
        _ if usage.input_tokens > 0 || usage.output_tokens > 0 => {
            format!(
                "{} in / {} out",
                comma(usage.input_tokens),
                comma(usage.output_tokens)
            )
        }
        _ => String::new(),
    }
}

fn comma(value: u64) -> String {
    let s = value.to_string();
    let mut out = String::new();
    for (index, ch) in s.chars().rev().enumerate() {
        if index > 0 && index % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out.chars().rev().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::{
        CheckView, Project, RunDetailView, RunView, UiState, UsageView, WorktreeDetail,
    };

    fn run(id: &str, project_id: &str, state: &str, engine: &str) -> RunView {
        RunView {
            id: id.into(),
            project_id: project_id.into(),
            objective: format!("objective {id}"),
            state: state.into(),
            engine: engine.into(),
            parent_run_id: None,
            attempt_group: None,
        }
    }

    fn state() -> UiState {
        UiState {
            connected: true,
            projects: vec![
                Project {
                    id: "p1".into(),
                    name: "core".into(),
                    path: "/repo/core".into(),
                },
                Project {
                    id: "p2".into(),
                    name: "site".into(),
                    path: "/repo/site".into(),
                },
            ],
            runs: vec![
                run("done-old", "p1", "completed", "codex"),
                run("running", "p1", "running", "claude"),
                run("needs-you", "p2", "needs-you", "codex"),
                run("blocked", "p1", "blocked", "claude"),
                run("failed-new", "p2", "failed", "codex"),
                run("done-new", "p1", "completed", "codex"),
            ],
            run_id: Some("running".into()),
            ..UiState::default()
        }
    }

    #[test]
    fn overview_orders_by_attention_priority_then_mru_then_recency() {
        let mru = vec!["blocked".into(), "needs-you".into(), "done-old".into()];
        let overview = model(&state(), &mru);

        assert_eq!(
            overview
                .rows
                .iter()
                .map(|row| row.run_id.as_str())
                .collect::<Vec<_>>(),
            vec![
                "blocked",
                "needs-you",
                "running",
                "done-old",
                "done-new",
                "failed-new"
            ]
        );
        assert_eq!(overview.counts.needs_you, 2);
        assert_eq!(overview.counts.running, 1);
        assert_eq!(overview.counts.done, 3);
    }

    #[test]
    fn overview_collapses_threads_to_tips_and_summarizes_run_detail() {
        let mut state = state();
        state.runs.push(RunView {
            id: "running-followup".into(),
            project_id: "p1".into(),
            objective: "continue running".into(),
            state: "running".into(),
            engine: "claude".into(),
            parent_run_id: Some("running".into()),
            attempt_group: None,
        });
        state.run_details.insert(
            "running-followup".into(),
            RunDetailView {
                run_id: "running-followup".into(),
                worktree: Some(WorktreeDetail {
                    path: Some("/repo/core/.worktrees/running".into()),
                    branch: Some("nav/overview".into()),
                    ..WorktreeDetail::default()
                }),
                checks: vec![CheckView {
                    name: "cargo test".into(),
                    passed: Some(false),
                    command: Some("cargo test".into()),
                    duration_ms: None,
                    output: None,
                }],
                usage: UsageView {
                    input_tokens: 1200,
                    output_tokens: 345,
                    context_tokens: Some(1545),
                    context_limit_tokens: Some(2000),
                },
                ..RunDetailView::default()
            },
        );

        let overview = model(&state, &[]);
        let running = overview
            .rows
            .iter()
            .find(|row| row.run_id == "running-followup")
            .expect("thread tip should be shown");

        assert!(!overview.rows.iter().any(|row| row.run_id == "running"));
        assert_eq!(running.project, "core");
        assert_eq!(running.engine, "claude");
        assert_eq!(running.branch, "nav/overview");
        assert_eq!(running.check, "cargo test failed");
        assert_eq!(running.usage, "1,545 / 2,000 tokens");
    }

    #[test]
    fn overview_empty_and_offline_states_are_truthful() {
        let offline = UiState {
            connected: false,
            ..UiState::default()
        };
        let overview = model(&offline, &[]);

        assert!(!overview.connected);
        assert!(overview.rows.is_empty());
        assert_eq!(
            overview.empty_reason.as_deref(),
            Some("Offline — no daemon state yet")
        );

        let online = UiState {
            connected: true,
            ..UiState::default()
        };
        let overview = model(&online, &[]);
        assert_eq!(overview.empty_reason.as_deref(), Some("No runs yet"));
    }

    #[test]
    fn selecting_an_overview_row_updates_selected_run_and_structured_detail() {
        let mut state = state();
        state.run_details.insert(
            "blocked".into(),
            RunDetailView {
                run_id: "blocked".into(),
                route_shape: Some("priority".into()),
                graph: Some(crate::client::GraphView::default()),
                ..RunDetailView::default()
            },
        );
        let overview = model(&state, &["blocked".into()]);
        let row = overview
            .rows
            .iter()
            .find(|row| row.run_id == "blocked")
            .expect("blocked row");

        select_row(&mut state, row);

        assert_eq!(state.run_id.as_deref(), Some("blocked"));
        assert_eq!(
            state.selected_detail().unwrap().route_shape.as_deref(),
            Some("priority")
        );
        assert_eq!(state.summary, vec!["routed priority"]);
        assert_eq!(state.graph, Some(crate::client::GraphView::default()));
    }
}
