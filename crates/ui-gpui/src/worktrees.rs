//! The Worktrees panel.
//!
//! Reclaim is destructive, so this surface never offers a one-press delete.
//! A row runs a dry run first; only after the daemon reports the worktree
//! eligible does a confirmation row appear, and only that row sends the real
//! request. The daemon repeats every check anyway — this is the second lock,
//! not the only one.

use crate::client::{Command, UiState, blocker_label};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WorktreeAction {
    Refresh,
    CycleFilter,
    /// Ask the daemon what would happen, and change nothing.
    DryRun(String),
    /// Send the real reclaim for a path the user already confirmed.
    Confirm(String),
    /// Drop a pending confirmation without reclaiming.
    Cancel,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorktreeRow {
    pub label: String,
    pub value: String,
    pub action: Option<WorktreeAction>,
}

fn row(
    label: impl Into<String>,
    value: impl Into<String>,
    action: Option<WorktreeAction>,
) -> WorktreeRow {
    WorktreeRow {
        label: label.into(),
        value: value.into(),
        action,
    }
}

/// Short directory name, so a row stays readable at 11 px.
fn short_path(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

pub(crate) fn worktree_rows(state: &UiState) -> Vec<WorktreeRow> {
    let page = &state.worktrees;
    let mut rows = vec![
        row(
            "Filter",
            page.filter.label(),
            Some(WorktreeAction::CycleFilter),
        ),
        row(
            "Refresh",
            if page.loading {
                "Loading…"
            } else {
                "Re-read the worktree index"
            },
            Some(WorktreeAction::Refresh),
        ),
    ];
    if !page.storage_root.is_empty() {
        rows.push(row("Storage", page.storage_root.clone(), None));
    }
    if let Some(error) = page.error.as_deref() {
        rows.push(row("Error", error.to_string(), None));
    }

    for entry in &page.entries {
        let state_text = if entry.removed_at_ms.is_some() {
            "reclaimed".to_string()
        } else if entry.eligible {
            "eligible".to_string()
        } else {
            entry
                .blockers
                .first()
                .map(|blocker| blocker_label(*blocker).to_string())
                .unwrap_or_else(|| "blocked".into())
        };
        let missing = if entry.exists { "" } else { " · missing" };
        rows.push(row(
            short_path(&entry.path),
            format!(
                "{} · {} · run {}{}",
                state_text, entry.branch, entry.run_state, missing
            ),
            // A blocked or already reclaimed row still offers the dry run,
            // because seeing why is the useful thing to do with it.
            (entry.removed_at_ms.is_none()).then(|| WorktreeAction::DryRun(entry.path.clone())),
        ));
    }

    if page.entries.is_empty() && !page.loading {
        rows.push(row("Worktrees", "No worktrees indexed", None));
    }

    for line in &page.diagnostics {
        rows.push(row("Diagnostic", line.clone(), None));
    }

    if let Some(path) = page.pending_confirm.as_deref() {
        rows.push(row(
            "Confirm reclaim",
            path.to_string(),
            Some(WorktreeAction::Confirm(path.to_string())),
        ));
        rows.push(row(
            "Cancel",
            "Keep this worktree",
            Some(WorktreeAction::Cancel),
        ));
    }
    rows
}

/// Apply an action to local state and return the command it needs, if any.
pub(crate) fn apply_worktree_action(
    state: &mut UiState,
    action: WorktreeAction,
) -> Option<Command> {
    match action {
        WorktreeAction::Refresh => {
            state.worktrees.loading = true;
            state.worktrees.error = None;
            Some(Command::RefreshWorktrees(state.worktrees.filter))
        }
        WorktreeAction::CycleFilter => {
            state.worktrees.filter = state.worktrees.filter.next();
            state.worktrees.loading = true;
            Some(Command::RefreshWorktrees(state.worktrees.filter))
        }
        WorktreeAction::DryRun(path) => {
            state.worktrees.pending_confirm = None;
            state.worktrees.diagnostics.clear();
            state.worktrees.loading = true;
            Some(Command::ReclaimWorktree {
                path,
                dry_run: true,
            })
        }
        WorktreeAction::Confirm(path) => {
            // Only a path the daemon just called eligible may be confirmed.
            if state.worktrees.pending_confirm.as_deref() != Some(path.as_str()) {
                state.status = "confirmation no longer matches; run the check again".into();
                return None;
            }
            state.worktrees.pending_confirm = None;
            state.worktrees.loading = true;
            Some(Command::ReclaimWorktree {
                path,
                dry_run: false,
            })
        }
        WorktreeAction::Cancel => {
            state.worktrees.pending_confirm = None;
            state.worktrees.diagnostics.clear();
            None
        }
    }
}
