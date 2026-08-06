//! Deterministic shell fixture for visual and interaction smoke tests.
//!
//! This state never represents daemon truth. It is deliberately populated in
//! memory so the real shell can be launched without a daemon, provider account,
//! or client token.

use std::collections::HashMap;

use crate::client::{
    ActivityRowView, ArtifactView, BudgetView, ChangedFileStatus, ChangedFileView, CheckView,
    EngineModelView, EngineStatus, GraphNodeView, GraphView, HistoryPageState, Project,
    RunDetailView, RunView, SettingsView, StructuredMessageView, UiState, UsageSummaryView,
    UsageView, WorktreeDetail, WorktreePageState, parse_diff,
};

const PREVIEW_NOW_MS: i64 = 1_746_722_000_000;

/// A populated cockpit state that is stable across machines and test runs.
pub fn populated() -> UiState {
    let project = Project {
        id: "autoharness".into(),
        name: "autoharness".into(),
        path: "~/dev/autoharness".into(),
    };

    let mut runs = vec![
        RunView {
            id: "ci-lint-fixes".into(),
            project_id: project.id.clone(),
            objective: "ci/lint-fixes".into(),
            state: "completed".into(),
            engine: "claude".into(),
            parent_run_id: None,
            attempt_group: None,
        },
        RunView {
            id: "perf-memoization".into(),
            project_id: project.id.clone(),
            objective: "perf/memoization".into(),
            state: "needs-you".into(),
            engine: "codex".into(),
            parent_run_id: None,
            attempt_group: None,
        },
        RunView {
            id: "test-e2e-stability".into(),
            project_id: project.id.clone(),
            objective: "test/e2e-stability".into(),
            state: "completed".into(),
            engine: "claude".into(),
            parent_run_id: None,
            attempt_group: None,
        },
        RunView {
            id: "docs-readme-update".into(),
            project_id: project.id.clone(),
            objective: "docs/readme-update".into(),
            state: "completed".into(),
            engine: "codex".into(),
            parent_run_id: None,
            attempt_group: None,
        },
        RunView {
            id: "chore-deps-bump".into(),
            project_id: project.id.clone(),
            objective: "chore/deps-bump".into(),
            state: "failed".into(),
            engine: "claude".into(),
            parent_run_id: None,
            attempt_group: None,
        },
        RunView {
            id: "refactor-worker-pool".into(),
            project_id: project.id.clone(),
            objective: "refactor/worker-pool".into(),
            state: "completed".into(),
            engine: "codex".into(),
            parent_run_id: None,
            attempt_group: None,
        },
        RunView {
            id: "fix-ui-regression".into(),
            project_id: project.id.clone(),
            objective: "fix/ui-regression".into(),
            state: "needs-you".into(),
            engine: "claude".into(),
            parent_run_id: None,
            attempt_group: None,
        },
        RunView {
            id: "feat-cache-key-stability".into(),
            project_id: project.id.clone(),
            objective: "feat/cache-key-stability".into(),
            state: "working".into(),
            engine: "codex".into(),
            parent_run_id: None,
            attempt_group: None,
        },
    ];
    add_preview_followups(
        &mut runs,
        &project.id,
        "feat-cache-key-stability",
        "feat/cache-key-stability",
        "codex",
        "working",
        3,
    );
    add_preview_followups(
        &mut runs,
        &project.id,
        "fix-ui-regression",
        "fix/ui-regression",
        "claude",
        "needs-you",
        2,
    );
    add_preview_followups(
        &mut runs,
        &project.id,
        "refactor-worker-pool",
        "refactor/worker-pool",
        "codex",
        "completed",
        4,
    );
    add_preview_followups(
        &mut runs,
        &project.id,
        "chore-deps-bump",
        "chore/deps-bump",
        "claude",
        "failed",
        1,
    );
    add_preview_followups(
        &mut runs,
        &project.id,
        "test-e2e-stability",
        "test/e2e-stability",
        "claude",
        "completed",
        2,
    );
    add_preview_followups(
        &mut runs,
        &project.id,
        "perf-memoization",
        "perf/memoization",
        "codex",
        "needs-you",
        1,
    );

    let graph = GraphView {
        nodes: vec![
            GraphNodeView {
                id: "Analyze".into(),
                role: "codex".into(),
                objective: "Analyze cache-key behavior".into(),
                file_scope: vec!["src/cache/**".into(), "test/cache/**".into()],
                acceptance_checks: vec![],
                depends_on: vec![],
                wave: 0,
                state: "succeeded".into(),
                detail: "✓ 20s".into(),
                progress_percent: None,
                duration_ms: Some(20_000),
                can_retry: false,
                can_cancel: false,
            },
            GraphNodeView {
                id: "Plan".into(),
                role: "codex".into(),
                objective: "Plan stable normalization".into(),
                file_scope: vec!["src/cache/**".into(), "test/cache/**".into()],
                acceptance_checks: vec![],
                depends_on: vec!["Analyze".into()],
                wave: 1,
                state: "succeeded".into(),
                detail: "✓ 32s".into(),
                progress_percent: None,
                duration_ms: Some(32_000),
                can_retry: false,
                can_cancel: false,
            },
            GraphNodeView {
                id: "Implement key normalization".into(),
                role: "claude".into(),
                objective: "Implement key normalization".into(),
                file_scope: vec!["src/cache/key.ts".into()],
                acceptance_checks: vec!["npm test -- cache".into()],
                depends_on: vec!["Plan".into()],
                wave: 2,
                state: "running".into(),
                detail: "editing src/cache/key.ts".into(),
                progress_percent: Some(60),
                duration_ms: None,
                can_retry: false,
                can_cancel: false,
            },
            GraphNodeView {
                id: "Add cross-version tests".into(),
                role: "codex".into(),
                objective: "Add cross-version tests".into(),
                file_scope: vec!["test/cache/**".into()],
                acceptance_checks: vec!["npm test -- cache".into()],
                depends_on: vec!["Plan".into()],
                wave: 2,
                state: "running".into(),
                detail: "editing test/cache/cross-version.test.ts".into(),
                progress_percent: Some(40),
                duration_ms: None,
                can_retry: false,
                can_cancel: false,
            },
            GraphNodeView {
                id: "Verify".into(),
                role: "codex".into(),
                objective: "Verify both branches together".into(),
                file_scope: vec!["src/cache/**".into(), "test/cache/**".into()],
                acceptance_checks: vec!["npm test -- cache".into(), "npm run typecheck".into()],
                depends_on: vec![
                    "Implement key normalization".into(),
                    "Add cross-version tests".into(),
                ],
                wave: 3,
                state: "pending".into(),
                detail: "Waiting".into(),
                progress_percent: None,
                duration_ms: None,
                can_retry: false,
                can_cancel: false,
            },
        ],
        edges: vec![
            ("Analyze".into(), "Plan".into()),
            ("Plan".into(), "Implement key normalization".into()),
            ("Plan".into(), "Add cross-version tests".into()),
            ("Implement key normalization".into(), "Verify".into()),
            ("Add cross-version tests".into(), "Verify".into()),
        ],
        awaiting_approval: false,
    };

    let patch = r#"diff --git a/src/cache/key.ts b/src/cache/key.ts
index 2f5c771..4b72e0d 100644
--- a/src/cache/key.ts
+++ b/src/cache/key.ts
@@ -42,7 +42,11 @@ export function stableKey(input: Input): string {
- const normalized = JSON.stringify(input)
-   .replace(/\s+/g, ' ')
-   .trim()
+ const normalized = canonicalize(input, {
+   sortKeys: true,
+   dropUndefined: true,
+ })
  return createHash('sha256').update(normalized).digest('hex')
 }
diff --git a/test/cache/cross-version.test.ts b/test/cache/cross-version.test.ts
new file mode 100644
index 0000000..c88ac21
--- /dev/null
+++ b/test/cache/cross-version.test.ts
@@ -0,0 +1,4 @@
+import { stableKey } from '../../src/cache/key'
+
+test('cache key is stable across Node 18 and 20', () => {
+  expect(stableKey({ b: 2, a: 1 })).toEqual(stableKey({ a: 1, b: 2 }))
+})
"#;
    let diff = parse_diff(patch);

    let messages = vec![
        StructuredMessageView {
            author: "You".into(),
            engine: None,
            model: None,
            text: "Make the cache key stable across Node 18 and 20. Update tests and docs.".into(),
            timestamp_ms: 1_746_721_500_000,
        },
        StructuredMessageView {
            author: "Codex".into(),
            engine: Some("codex".into()),
            model: None,
            text: "I'll analyze the cache key generation and propose a plan.".into(),
            timestamp_ms: 1_746_721_500_000,
        },
        StructuredMessageView {
            author: "Codex".into(),
            engine: Some("codex".into()),
            model: None,
            text: "Plan ready. 4 tasks across 2 files.".into(),
            timestamp_ms: 1_746_721_560_000,
        },
        StructuredMessageView {
            author: "Claude".into(),
            engine: Some("claude".into()),
            model: None,
            text: "I'll implement behind the existing interface and add cross-version tests."
                .into(),
            timestamp_ms: 1_746_721_560_000,
        },
        StructuredMessageView {
            author: "Codex".into(),
            engine: Some("codex".into()),
            model: None,
            text: "Working on 2 tasks in parallel.".into(),
            timestamp_ms: 1_746_721_620_000,
        },
    ];

    let activity = vec![
        ActivityRowView {
            timestamp_ms: 1_746_721_640_000,
            engine: Some("Codex".into()),
            action: "Started task".into(),
            detail: "Add cross-version tests".into(),
            path: Some(".worktrees/run-20250508-1525".into()),
        },
        ActivityRowView {
            timestamp_ms: 1_746_721_630_000,
            engine: Some("Claude".into()),
            action: "Started task".into(),
            detail: "Implement key normalization".into(),
            path: Some(".worktrees/run-20250508-1525".into()),
        },
        ActivityRowView {
            timestamp_ms: 1_746_721_610_000,
            engine: Some("Codex".into()),
            action: "Completed".into(),
            detail: "Plan".into(),
            path: Some(".worktrees/run-20250508-1525".into()),
        },
        ActivityRowView {
            timestamp_ms: 1_746_721_578_000,
            engine: Some("Codex".into()),
            action: "Completed".into(),
            detail: "Analyze".into(),
            path: Some(".worktrees/run-20250508-1525".into()),
        },
    ];

    let detail = RunDetailView {
        run_id: "feat-cache-key-stability".into(),
        created_at_ms: Some(1_746_721_600_000),
        pending_question: None,
        terminal: Vec::new(),
        engine: Some("codex".into()),
        model: Some("gpt-5.6-sol".into()),
        reasoning_effort: Some("high".into()),
        route_shape: Some("parallel".into()),
        route: Some(crate::client::RouteView {
            shape: "swarm".into(),
            confidence: Some(0.91),
            reasons: vec!["plan decomposes into 3 nodes with confidence 0.91".into()],
            alternatives: vec!["bounded_loop".into()],
            max_turns: None,
            wall_time_secs: None,
        }),
        messages: messages.clone(),
        activity,
        changed_files: vec![
            ChangedFileView {
                path: "src/cache/key.ts".into(),
                status: ChangedFileStatus::Modified,
            },
            ChangedFileView {
                path: "test/cache/cross-version.test.ts".into(),
                status: ChangedFileStatus::Added,
            },
        ],
        checks: vec![
            CheckView {
                name: "Unit tests".into(),
                command: Some("npm test -- cache".into()),
                passed: Some(true),
                duration_ms: Some(42_000),
                output: Some("PASS test/cache/cross-version.test.ts".into()),
            },
            CheckView {
                name: "Type check".into(),
                command: Some("npm run typecheck".into()),
                passed: Some(true),
                duration_ms: Some(18_000),
                output: Some("No type errors".into()),
            },
            CheckView {
                name: "Lint".into(),
                command: Some("npm run lint".into()),
                passed: Some(true),
                duration_ms: Some(12_000),
                output: Some("No lint errors".into()),
            },
        ],
        artifacts: vec![
            ArtifactView {
                name: "test-results.xml".into(),
                path: "artifacts/test-results.xml".into(),
                kind: Some("junit".into()),
                size_bytes: Some(24 * 1024),
            },
            ArtifactView {
                name: "coverage/lcov.info".into(),
                path: "coverage/lcov.info".into(),
                kind: Some("coverage".into()),
                size_bytes: Some(86 * 1024),
            },
        ],
        worktree: Some(WorktreeDetail {
            path: Some(".worktrees/run-20250508-1525".into()),
            branch: Some("autoharness/run-20250508-1525".into()),
            base: Some("main (a1b2c3d)".into()),
            state: "created".into(),
            isolation: Some("full".into()),
            reason: None,
            created_at_ms: Some(1_746_721_560_000),
        }),
        // Fixture-only deterministic spent values: the daemon does not emit
        // these yet, so live replay leaves the spent/cost fields as None.
        budget: Some(BudgetView {
            wall_time_secs: Some(1800),
            max_turns: Some(200),
            max_tool_calls: Some(500),
            max_retries: Some(3),
            max_concurrent_workers: Some(2),
            max_graph_nodes: Some(8),
            spent_wall_time_secs: Some(1080),
            spent_turns: Some(54),
            spent_tool_calls: Some(37),
            spent_cost_usd: Some(0.012),
        }),
        // Fixture-only context counters: live `engine.usage` currently emits
        // input/output tokens only.
        usage: UsageView {
            input_tokens: 30_000,
            output_tokens: 24_000,
            context_tokens: Some(96_000),
            context_limit_tokens: Some(200_000),
        },
        diff: Some(diff.clone()),
        graph: Some(graph.clone()),
    };

    let chat_by_run = HashMap::from([(
        "feat-cache-key-stability".into(),
        messages
            .iter()
            .map(|message| match message.engine.as_deref() {
                None => format!("you  {}", message.text),
                Some(_) => format!("bot  {}", message.text),
            })
            .collect(),
    )]);
    let mut run_details = runs
        .iter()
        .enumerate()
        .map(|(index, run)| {
            let timestamp_ms = preview_timestamp_for_run(&run.id, index);
            (
                run.id.clone(),
                RunDetailView {
                    run_id: run.id.clone(),
                    engine: Some(run.engine.clone()),
                    messages: vec![StructuredMessageView {
                        author: "You".into(),
                        engine: None,
                        model: None,
                        text: run.objective.clone(),
                        timestamp_ms,
                    }],
                    worktree: Some(WorktreeDetail {
                        path: Some(format!(".worktrees/{}", run.id)),
                        branch: Some(format!("autoharness/{}", run.objective)),
                        base: Some("main (a1b2c3d)".into()),
                        state: "created".into(),
                        isolation: Some("full".into()),
                        reason: None,
                        created_at_ms: Some(timestamp_ms),
                    }),
                    ..RunDetailView::default()
                },
            )
        })
        .collect::<HashMap<_, _>>();
    run_details.insert("feat-cache-key-stability".into(), detail);

    UiState {
        execution_pinned_by_user: false,
        sandbox: Some(crate::client::SandboxView {
            ready: true,
            problems: Vec::new(),
        }),
        connected: false,
        status: "preview: exact cockpit fixture (no daemon)".into(),
        projects: vec![project],
        selected_project: 0,
        engines: vec![
            EngineStatus {
                name: "codex".into(),
                ready: true,
                installed: true,
                authenticated: Some(true),
                version: Some("preview-codex-1.0".into()),
                problems: Vec::new(),
                models: vec![
                    EngineModelView {
                        id: "gpt-5.6-sol".into(),
                        display_name: "5.6-Sol".into(),
                        description: "Frontier agentic coding model".into(),
                        reasoning_efforts: vec!["low".into(), "medium".into(), "high".into()],
                        default_reasoning_effort: Some("high".into()),
                        is_default: true,
                    },
                    EngineModelView {
                        id: "gpt-5.6-terra".into(),
                        display_name: "5.6-Terra".into(),
                        description: "Balanced agentic coding model".into(),
                        reasoning_efforts: vec!["low".into(), "medium".into(), "high".into()],
                        default_reasoning_effort: Some("medium".into()),
                        is_default: false,
                    },
                ],
                model_load_error: None,
            },
            EngineStatus {
                name: "claude".into(),
                ready: true,
                installed: true,
                authenticated: Some(true),
                version: Some("preview-claude-1.0".into()),
                problems: Vec::new(),
                models: vec![
                    EngineModelView {
                        id: "".into(),
                        display_name: "Claude default".into(),
                        description: "Provider-selected current default".into(),
                        reasoning_efforts: vec!["low".into(), "medium".into(), "high".into()],
                        default_reasoning_effort: Some("high".into()),
                        is_default: true,
                    },
                    EngineModelView {
                        id: "opus".into(),
                        display_name: "Opus".into(),
                        description: "Claude Opus alias".into(),
                        reasoning_efforts: vec!["low".into(), "medium".into(), "high".into()],
                        default_reasoning_effort: Some("high".into()),
                        is_default: false,
                    },
                ],
                model_load_error: None,
            },
        ],
        engine: "codex".into(),
        model: Some("gpt-5.6-sol".into()),
        reasoning_effort: Some("high".into()),
        check_command: Some("npm test -- cache && npm run typecheck && npm run lint".into()),
        input: String::new(),
        runs,
        run_id: Some("feat-cache-key-stability".into()),
        run_state: "working".into(),
        chat_by_run,
        run_details,
        events: vec![
            "Created run Make the cache key stable across Node 18 and 20".into(),
            "Prepared worktree .worktrees/run-20250508-1525".into(),
            "Routed parallel".into(),
            "Recorded diff Patch captured".into(),
        ],
        summary: vec![
            "4 tasks · 2 files · ~18m".into(),
            "worktree .worktrees/run-20250508-1525 on autoharness/run-20250508-1525".into(),
            "checks 3 / 3 passed".into(),
            "artifacts test-results.xml, coverage/lcov.info".into(),
        ],
        graph: Some(graph),
        diff: Some(diff),
        check_output: vec![
            "PASS test/cache/cross-version.test.ts".into(),
            "No type errors".into(),
            "No lint errors".into(),
        ],
        scrollback: 0,
        worktree: Some(".worktrees/run-20250508-1525".into()),
        history: HistoryPageState::default(),
        worktrees: WorktreePageState::default(),
        attention: crate::attention::AttentionState::default(),
        update_status: crate::update::UpdateStatus::default(),
        replay_through_seq: 0,
        last_sequence: 0,
        settings: SettingsView::default(),
        usage_summary: UsageSummaryView::default(),
        queue: crate::queue::QueueView::default(),
    }
}

fn add_preview_followups(
    runs: &mut Vec<RunView>,
    project_id: &str,
    root_id: &str,
    objective: &str,
    engine: &str,
    state: &str,
    count: usize,
) {
    for turn in 2..=count + 1 {
        let id = format!("{root_id}-turn-{turn}");
        runs.push(RunView {
            id: id.clone(),
            project_id: project_id.into(),
            objective: format!("{objective} turn {turn}"),
            state: state.into(),
            engine: engine.into(),
            parent_run_id: Some(root_id.into()),
            attempt_group: None,
        });
    }
}

fn preview_timestamp_for_run(run_id: &str, _fallback_index: usize) -> i64 {
    let root_id = run_id.split("-turn-").next().unwrap_or(run_id);
    let latest = match root_id {
        "feat-cache-key-stability" => 1_746_720_600_000,
        "fix-ui-regression" => 1_746_721_560_000,
        "refactor-worker-pool" => 1_746_635_000_000,
        "chore-deps-bump" => 1_746_548_540_000,
        "docs-readme-update" => 1_746_462_900_000,
        "test-e2e-stability" => 1_746_376_860_000,
        "perf-memoization" => 1_746_290_820_000,
        "ci-lint-fixes" => 1_746_204_780_000,
        _ => PREVIEW_NOW_MS - 7 * 86_400_000,
    };
    let turn_offset = run_id
        .split("-turn-")
        .nth(1)
        .and_then(|turn| turn.parse::<i64>().ok())
        .unwrap_or(0);
    latest - turn_offset * 60_000
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::ChangedFileStatus;

    #[test]
    fn populated_fixture_covers_the_exact_black_cockpit_target() {
        let state = populated();
        assert!(!state.connected);
        assert!(state.status.contains("preview"));
        assert_eq!(state.projects.len(), 1);
        assert_eq!(state.projects[0].name, "autoharness");
        assert_eq!(state.projects[0].path, "~/dev/autoharness");

        assert!(state.runs.len() >= 7);
        assert!(state.runs.iter().any(|run| run.state == "working"));
        assert!(state.runs.iter().any(|run| run.state == "needs-you"));
        assert!(state.runs.iter().any(|run| run.state == "completed"));
        assert!(state.runs.iter().any(|run| run.state == "failed"));
        assert!(state.runs.iter().any(|run| run.engine == "codex"));
        assert!(state.runs.iter().any(|run| run.engine == "claude"));
        assert_eq!(state.run_id.as_deref(), Some("feat-cache-key-stability"));
        assert_eq!(
            state
                .runs
                .iter()
                .filter(|run| run.parent_run_id.is_none())
                .map(|run| run.objective.as_str())
                .collect::<Vec<_>>(),
            vec![
                "ci/lint-fixes",
                "perf/memoization",
                "test/e2e-stability",
                "docs/readme-update",
                "chore/deps-bump",
                "refactor/worker-pool",
                "fix/ui-regression",
                "feat/cache-key-stability",
            ]
        );
        assert!(
            state
                .runs
                .iter()
                .all(|run| state.run_details.contains_key(&run.id)),
            "every preview sidebar row needs truthful timestamp/detail data"
        );
        assert_eq!(
            state
                .runs
                .iter()
                .filter(|run| run.parent_run_id.as_deref() == Some("feat-cache-key-stability"))
                .count(),
            3
        );
        assert_eq!(
            state
                .runs
                .iter()
                .filter(|run| run.parent_run_id.as_deref() == Some("fix-ui-regression"))
                .count(),
            2
        );

        let selected = state
            .selected_detail()
            .expect("preview selected run must have exact structured detail");
        assert_eq!(selected.run_id, "feat-cache-key-stability");
        assert_eq!(selected.route_shape.as_deref(), Some("parallel"));
        assert_eq!(
            selected.worktree.as_ref().unwrap().path.as_deref(),
            Some(".worktrees/run-20250508-1525")
        );
        assert_eq!(selected.budget.as_ref().unwrap().wall_time_secs, Some(1800));
        assert_eq!(
            selected.budget.as_ref().unwrap().spent_wall_time_secs,
            Some(1080)
        );
        assert_eq!(selected.usage.context_tokens, Some(96_000));
        assert_eq!(
            selected.usage.input_tokens + selected.usage.output_tokens,
            54_000
        );

        let graph = state.graph.as_ref().expect("fixture must include a DAG");
        assert_eq!(
            graph
                .nodes
                .iter()
                .map(|node| node.id.as_str())
                .collect::<Vec<_>>(),
            vec![
                "Analyze",
                "Plan",
                "Implement key normalization",
                "Add cross-version tests",
                "Verify"
            ]
        );
        assert_eq!(graph.wave_count(), 4);
        assert_eq!(graph.edges.len(), 5);
        assert!(
            graph
                .nodes
                .iter()
                .any(|node| node.id == "Implement key normalization"
                    && node.progress_percent == Some(60))
        );

        assert_eq!(
            selected
                .changed_files
                .iter()
                .map(|file| (file.path.as_str(), file.status))
                .collect::<Vec<_>>(),
            vec![
                ("src/cache/key.ts", ChangedFileStatus::Modified),
                ("test/cache/cross-version.test.ts", ChangedFileStatus::Added),
            ]
        );
        assert_eq!(selected.checks.len(), 3);
        assert!(
            selected
                .checks
                .iter()
                .all(|check| check.passed == Some(true))
        );
        assert_eq!(selected.artifacts.len(), 2);
        assert!(selected.messages.iter().any(|message| {
            message.author == "You"
                && message
                    .text
                    .contains("Make the cache key stable across Node 18 and 20")
        }));
        assert!(
            selected
                .activity
                .iter()
                .any(|row| row.action == "Started task" && row.engine.as_deref() == Some("Claude"))
        );
        assert!(
            crate::coordinator::transcript_rows(&state)
                .iter()
                .any(|row| { row.label == "Claude" && row.engine.as_deref() == Some("claude") })
        );
    }
}
