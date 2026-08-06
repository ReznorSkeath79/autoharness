//! Command palette: actions and fuzzy ranking.
//!
//! Ported from diri's `crates/diri-app/src/palette.rs` (Apache-2.0), adapted
//! to AutoHarness's vocabulary — runs, projects, engines and worktrees rather
//! than sessions and agents. Two ideas carried over because they are what make
//! a palette feel right rather than merely present:
//!
//! 1. **Invisible keywords, penalized.** A row matches on things it does not
//!    print — a run matches its project, engine, and branch — but a title hit
//!    always outranks a keyword hit, so typing what you can see wins.
//! 2. **Curated order is the tiebreak.** An empty query renders the list in the
//!    order a human chose, and equal scores never shuffle between frames.
//!
//! Matching is diri's `fuzzy` module, transplanted alongside this one, so the
//! palette and any future Quick Open rank identically.

use std::ops::Range;

use crate::fuzzy::{FuzzyMatcher, FuzzyQuery, PreparedText};
use crate::navigation::{NavigationSurface, Pane};

/// Higher is better.
pub type Score = i32;

/// A keyword hit is worth less than a title hit, so what you can read wins.
const KEYWORD_PENALTY: Score = 32;

/// What a palette row does when chosen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaletteCommand {
    /// Show an existing run's thread.
    SelectRun(String),
    /// Select a project.
    SelectProject(usize),
    /// Switch the engine for the next run.
    UseEngine(String),
    /// Open one of the cockpit's modal navigation surfaces.
    OpenSurface(NavigationSurface),
    /// Toggle one of the persistent cockpit panes.
    TogglePane(Pane),
    /// Run a known slash command verbatim.
    Run(SlashCommand),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlashCommand {
    NewThread,
    ApprovePlan,
    OpenWorktree,
    RefreshEngines,
    PauseRun,
    ResumeRun,
    CancelRun,
}

impl SlashCommand {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NewThread => "/new",
            Self::ApprovePlan => "/approve",
            Self::OpenWorktree => "/open",
            Self::RefreshEngines => "/engines",
            Self::PauseRun => "/pause",
            Self::ResumeRun => "/resume",
            Self::CancelRun => "/cancel",
        }
    }
}

/// One row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaletteAction {
    pub title: String,
    /// Shown on the right: what this is, or its current state.
    pub detail: String,
    /// Matched against but never displayed.
    pub keywords: String,
    pub command: PaletteCommand,
}

/// A row that survived filtering, with the byte ranges of its title to
/// highlight.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ranked {
    pub action: PaletteAction,
    pub title_matches: Vec<Range<usize>>,
    pub score: Score,
}

/// Every action available right now, in curated order: what the user is most
/// likely to want first, and the destructive controls last.
pub fn actions(state: &crate::client::UiState) -> Vec<PaletteAction> {
    let mut actions = Vec::new();

    for (index, project) in state.projects.iter().enumerate() {
        actions.push(PaletteAction {
            title: project.name.clone(),
            detail: "project".into(),
            keywords: format!("project open {} {}", project.name, project.path),
            command: PaletteCommand::SelectProject(index),
        });
    }

    // Threads, not runs: a follow-up is another turn of a conversation the
    // user already has, and listing every turn separately buries the thread.
    for thread in state.runs.iter().filter(|r| r.parent_run_id.is_none()) {
        let tip = state.tip_of(&thread.id).unwrap_or(thread);
        let project = state
            .projects
            .iter()
            .find(|p| p.id == thread.project_id)
            .map(|p| p.name.as_str())
            .unwrap_or("");
        let title = if thread.objective.is_empty() {
            thread.id.chars().take(8).collect::<String>()
        } else {
            thread.objective.clone()
        };
        actions.push(PaletteAction {
            title,
            detail: crate::theme::Status::of_run(&tip.state).label().to_string(),
            // Everything true about a thread that its row does not print,
            // including every turn's objective.
            keywords: format!("run thread {project} {} {}", tip.engine, tip.state),
            // Select the TIP, so a reply continues the thread.
            command: PaletteCommand::SelectRun(tip.id.clone()),
        });
    }

    for engine in &state.engines {
        // A gated engine gets no "Use …" action: the palette runs what it
        // offers, and offering a coming-soon engine is a dead command.
        if !crate::client::engine_generally_available(&engine.name) {
            continue;
        }
        actions.push(PaletteAction {
            title: format!("Use {}", engine.name),
            detail: if engine.ready { "ready" } else { "not ready" }.into(),
            keywords: format!("engine switch {}", engine.name),
            command: PaletteCommand::UseEngine(engine.name.clone()),
        });
    }

    for (title, detail, keywords, command) in [
        (
            "Open Overview",
            "runs by attention",
            "overview running needs blocked cockpit",
            PaletteCommand::OpenSurface(NavigationSurface::Overview),
        ),
        (
            "Open History",
            "recent runs",
            "history recent previous runs",
            PaletteCommand::OpenSurface(NavigationSurface::History),
        ),
        (
            "Open Worktrees",
            "selected checkout",
            "worktrees checkout branch finder",
            PaletteCommand::OpenSurface(NavigationSurface::Worktrees),
        ),
        (
            "Open Queue",
            "waiting objectives and follow-ups",
            "queue waiting reorder cancel objectives steering",
            PaletteCommand::OpenSurface(NavigationSurface::Queue),
        ),
        (
            "Open Notifications",
            "attention queue",
            "notifications bell blocked needs attention",
            PaletteCommand::OpenSurface(NavigationSurface::Notifications),
        ),
        (
            "Open Settings",
            "cockpit panes",
            "settings preferences panes layout",
            PaletteCommand::OpenSurface(NavigationSurface::Settings),
        ),
        (
            "Toggle Sidebar",
            "show or hide",
            "sidebar pane left",
            PaletteCommand::TogglePane(Pane::Sidebar),
        ),
        (
            "Toggle Execution",
            "show or hide",
            "execution workbench graph pane",
            PaletteCommand::TogglePane(Pane::Execution),
        ),
        (
            "Toggle Inspector",
            "show or hide",
            "inspector review right pane",
            PaletteCommand::TogglePane(Pane::Inspector),
        ),
        (
            "New thread",
            "start fresh",
            "new reset clear",
            PaletteCommand::Run(SlashCommand::NewThread),
        ),
        (
            "Approve plan",
            "run the proposed graph",
            "approve accept plan",
            PaletteCommand::Run(SlashCommand::ApprovePlan),
        ),
        (
            "Open worktree",
            "reveal in Finder",
            "open finder reveal worktree",
            PaletteCommand::Run(SlashCommand::OpenWorktree),
        ),
        (
            "Refresh engines",
            "re-detect setup",
            "engines detect refresh",
            PaletteCommand::Run(SlashCommand::RefreshEngines),
        ),
        (
            "Pause run",
            "hold at the next boundary",
            "pause hold",
            PaletteCommand::Run(SlashCommand::PauseRun),
        ),
        (
            "Resume run",
            "continue",
            "resume continue",
            PaletteCommand::Run(SlashCommand::ResumeRun),
        ),
        (
            "Cancel run",
            "stop and clean up",
            "cancel stop kill abort",
            PaletteCommand::Run(SlashCommand::CancelRun),
        ),
    ] {
        actions.push(PaletteAction {
            title: title.into(),
            detail: detail.into(),
            keywords: keywords.into(),
            command,
        });
    }

    actions
}

/// Score a title (highlighted) against keywords (invisible, penalized) and keep
/// whichever wins. `None` filters the row out.
fn rank_one(
    query: &FuzzyQuery,
    matcher: &mut FuzzyMatcher,
    action: &PaletteAction,
) -> Option<(Score, Vec<Range<usize>>)> {
    if query.is_empty() {
        return Some((0, Vec::new()));
    }
    let title = query
        .highlights(&PreparedText::new(&action.title), &action.title, matcher)
        .map(|(score, ranges)| (score as Score, ranges));
    let keyword = (!action.keywords.is_empty())
        .then(|| query.score(&PreparedText::new(&action.keywords), matcher))
        .flatten()
        .map(|score| (score as Score).saturating_sub(KEYWORD_PENALTY));

    match (title, keyword) {
        (Some((score, ranges)), Some(keyword)) if keyword > score => Some((keyword, ranges)),
        (Some((score, ranges)), _) => Some((score, ranges)),
        (None, Some(keyword)) => Some((keyword, Vec::new())),
        (None, None) => None,
    }
}

/// Filter and sort. Curated order is the tiebreak, so an empty query renders
/// unchanged and equal scores never shuffle between frames.
pub fn rank(actions: Vec<PaletteAction>, query: &str) -> Vec<Ranked> {
    let query = FuzzyQuery::new(query);
    let mut matcher = FuzzyMatcher::text();
    let mut ranked: Vec<(usize, Ranked)> = actions
        .into_iter()
        .enumerate()
        .filter_map(|(index, action)| {
            rank_one(&query, &mut matcher, &action).map(|(score, title_matches)| {
                (
                    index,
                    Ranked {
                        action,
                        title_matches,
                        score,
                    },
                )
            })
        })
        .collect();
    ranked.sort_by(|left, right| {
        right
            .1
            .score
            .cmp(&left.1.score)
            .then_with(|| left.0.cmp(&right.0))
    });
    ranked.into_iter().map(|(_, ranked)| ranked).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::{EngineStatus, Project, RunView, UiState};

    fn state() -> UiState {
        UiState {
            projects: vec![
                Project {
                    id: "p1".into(),
                    name: "autoharness".into(),
                    path: "/Users/you/autoharness".into(),
                },
                Project {
                    id: "p2".into(),
                    name: "website".into(),
                    path: "/Users/you/website".into(),
                },
            ],
            runs: vec![RunView {
                id: "r1".into(),
                project_id: "p1".into(),
                objective: "Fix the typo in the README".into(),
                state: "succeeded".into(),
                engine: "codex".into(),
                parent_run_id: None,
                attempt_group: None,
            }],
            engines: vec![EngineStatus {
                name: "claude".into(),
                ready: true,
                installed: true,
                authenticated: Some(true),
                version: Some("2.1.221".into()),
                problems: vec![],
                models: vec![],
                model_load_error: None,
            }],
            ..UiState::default()
        }
    }

    fn titles(query: &str) -> Vec<String> {
        rank(actions(&state()), query)
            .into_iter()
            .map(|r| r.action.title)
            .collect()
    }

    /// An empty query must render the curated order untouched — a palette that
    /// reshuffles when you open it is unusable.
    #[test]
    fn an_empty_query_keeps_every_action_in_curated_order() {
        let all = titles("");
        assert_eq!(all.len(), actions(&state()).len());
        assert_eq!(all[0], "autoharness");
        assert_eq!(all[1], "website");
        assert!(all.contains(&"Cancel run".to_string()));
    }

    #[test]
    fn rows_are_found_by_acronym_and_by_substring() {
        // Acronym across word starts.
        assert_eq!(titles("nt").first().map(String::as_str), Some("New thread"));
        // Plain substring.
        assert!(titles("cancel").first().unwrap().contains("Cancel"));
    }

    /// The rule that makes ranking feel right: what you can read beats what
    /// you cannot.
    #[test]
    fn a_title_match_outranks_a_keyword_match() {
        // "website" is a project title; it is also nobody's keyword.
        let ranked = rank(actions(&state()), "website");
        assert_eq!(ranked[0].action.title, "website");
        assert!(
            !ranked[0].title_matches.is_empty(),
            "a title hit highlights"
        );

        // "codex" appears only in a run's invisible keywords.
        let ranked = rank(actions(&state()), "codex");
        assert!(ranked[0].action.title.contains("typo"));
        assert!(
            ranked[0].title_matches.is_empty(),
            "a keyword-only hit highlights nothing in the title"
        );
    }

    /// A run is findable by things its row never prints.
    #[test]
    fn runs_match_their_project_engine_and_state() {
        for query in ["autoharness", "codex", "succeeded"] {
            let ranked = rank(actions(&state()), query);
            assert!(
                ranked.iter().any(|r| r.action.title.contains("typo")),
                "{query} should find the run"
            );
        }
    }

    #[test]
    fn a_query_that_matches_nothing_returns_nothing() {
        assert!(titles("zzzzqqq").is_empty());
    }

    #[test]
    fn highlight_ranges_land_on_the_matched_bytes() {
        let ranked = rank(actions(&state()), "web");
        let hit = &ranked[0];
        let matched: String = hit
            .title_matches
            .iter()
            .map(|r| &hit.action.title[r.clone()])
            .collect();
        assert_eq!(matched.to_lowercase(), "web");
    }

    /// Consecutive letters must beat scattered ones, or ranking is arbitrary.
    #[test]
    fn consecutive_matches_score_higher_than_scattered_ones() {
        let tight = crate::fuzzy::score("can", "Cancel run").unwrap();
        let loose = crate::fuzzy::score("can", "Create a new thing").unwrap();
        assert!(tight > loose, "tight {tight} should beat loose {loose}");
    }

    #[test]
    fn ranking_is_stable_across_calls() {
        let first = titles("run");
        let second = titles("run");
        assert_eq!(first, second);
    }

    #[test]
    fn navigation_surfaces_and_pane_toggles_are_real_palette_actions() {
        let titles = titles("");
        for expected in [
            "Open Overview",
            "Open History",
            "Open Worktrees",
            "Open Notifications",
            "Open Settings",
            "Toggle Sidebar",
            "Toggle Execution",
            "Toggle Inspector",
        ] {
            assert!(
                titles.contains(&expected.to_string()),
                "{expected} should be visible"
            );
        }
    }

    #[test]
    fn every_static_slash_command_row_is_known_to_shell_dispatch() {
        let known = [
            "/new", "/approve", "/open", "/engines", "/pause", "/resume", "/cancel",
        ];
        for action in actions(&state()) {
            if let PaletteCommand::Run(command) = action.command {
                assert!(
                    known.contains(&command.as_str()),
                    "{} is visible but has no shell handler",
                    command.as_str()
                );
            }
        }
    }
}
