use autoharness_protocol::methods;
use serde_json::{Value, json};

use crate::client::{Command, HistoryEntryView, Pending, UiState};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryRow {
    pub label: String,
    pub value: String,
    pub adopt_provider: Option<String>,
    pub adopt_source_id: Option<String>,
    pub select_run_id: Option<String>,
}

pub fn grouped_rows(state: &UiState) -> Vec<HistoryRow> {
    if !state.history.scan_enabled && state.history.entries.is_empty() {
        return vec![info_row(
            "History",
            "Automatic scan disabled by AUTOHARNESS_HISTORY_SCAN=0",
        )];
    }
    let selected_path = state.selected_project().map(|p| p.path.as_str());
    let query = state.history.query.trim().to_ascii_lowercase();
    let mut entries: Vec<&HistoryEntryView> = state
        .history
        .entries
        .iter()
        .filter(|entry| query.is_empty() || matches_query(entry, &query))
        .collect();
    entries.sort_by(|a, b| {
        group_rank(a, selected_path)
            .cmp(&group_rank(b, selected_path))
            .then_with(|| a.provider.cmp(&b.provider))
            .then_with(|| group_label(a, selected_path).cmp(&group_label(b, selected_path)))
            .then_with(|| b.updated_at_ms.cmp(&a.updated_at_ms))
            .then_with(|| a.source_id.cmp(&b.source_id))
    });

    let mut rows = Vec::new();
    let mut current_group: Option<(String, String)> = None;
    for entry in entries {
        let group = (entry.provider.clone(), group_label(entry, selected_path));
        if current_group.as_ref() != Some(&group) {
            rows.push(info_row(
                format!("{} · {}", group.0, group.1),
                group_value(entry),
            ));
            current_group = Some(group);
        }
        rows.push(entry_row(entry));
    }
    if rows.is_empty() {
        rows.push(info_row(
            "History",
            if state.history.loading {
                "Scanning provider history…"
            } else if query.is_empty() {
                "No Codex or Claude history metadata found"
            } else {
                "No history matches this search"
            },
        ));
    }
    rows
}

pub fn merge_page(
    state: &mut UiState,
    entries: Vec<HistoryEntryView>,
    next_cursor: Option<String>,
) {
    for entry in entries {
        if let Some(existing) = state
            .history
            .entries
            .iter_mut()
            .find(|e| e.provider == entry.provider && e.source_id == entry.source_id)
        {
            *existing = entry;
        } else {
            state.history.entries.push(entry);
        }
    }
    state.history.entries.sort_by(|a, b| {
        b.updated_at_ms
            .cmp(&a.updated_at_ms)
            .then_with(|| a.provider.cmp(&b.provider))
            .then_with(|| a.source_id.cmp(&b.source_id))
    });
    state.history.next_cursor = next_cursor;
}

pub fn mark_adopted(
    state: &mut UiState,
    provider: &str,
    source_id: &str,
    run_id: &str,
    select: bool,
) {
    if let Some(entry) = state
        .history
        .entries
        .iter_mut()
        .find(|entry| entry.provider == provider && entry.source_id == source_id)
    {
        entry.adopted_run_id = Some(run_id.to_string());
        entry.eligible = false;
        entry.reason = Some("already adopted".into());
    }
    if select {
        state.run_id = Some(run_id.to_string());
        state.run_state.clear();
    }
}

pub(crate) fn encode_history_command(command: &Command) -> Option<(&'static str, Value, Pending)> {
    match command {
        Command::RefreshHistory { cursor } => Some((
            methods::HISTORY_LIST,
            json!({ "cursor": cursor, "limit": 50 }),
            Pending::HistoryList {
                append: cursor.is_some(),
            },
        )),
        Command::AdoptHistory {
            provider,
            source_id,
            project_id,
            engine,
        } => {
            let request_id = format!("ui-adopt-{}-{}", provider, source_id);
            Some((
                methods::HISTORY_ADOPT,
                json!({
                    "provider": provider,
                    "source_id": source_id,
                    "project_id": project_id,
                    "engine": engine,
                    "request_id": request_id,
                }),
                Pending::HistoryAdopt {
                    provider: provider.clone(),
                    source_id: source_id.clone(),
                },
            ))
        }
        _ => None,
    }
}

fn matches_query(entry: &HistoryEntryView, query: &str) -> bool {
    [
        Some(entry.provider.as_str()),
        Some(entry.source_id.as_str()),
        Some(entry.transcript_path.as_str()),
        entry.cwd.as_deref(),
        entry.title.as_deref(),
        entry.first_prompt.as_deref(),
    ]
    .into_iter()
    .flatten()
    .any(|field| field.to_ascii_lowercase().contains(query))
}

fn group_rank(entry: &HistoryEntryView, selected_path: Option<&str>) -> u8 {
    if entry.cwd.as_deref() == selected_path {
        0
    } else {
        1
    }
}

fn group_label(entry: &HistoryEntryView, selected_path: Option<&str>) -> String {
    if entry.cwd.as_deref() == selected_path {
        "current project".into()
    } else {
        entry
            .cwd
            .as_deref()
            .and_then(|cwd| std::path::Path::new(cwd).file_name())
            .map(|name| name.to_string_lossy().into_owned())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| "unknown project".into())
    }
}

fn group_value(entry: &HistoryEntryView) -> String {
    entry
        .cwd
        .clone()
        .unwrap_or_else(|| "No cwd recorded".into())
}

fn entry_row(entry: &HistoryEntryView) -> HistoryRow {
    let title = entry
        .title
        .as_deref()
        .or(entry.first_prompt.as_deref())
        .unwrap_or(entry.source_id.as_str());
    let mut value = format!(
        "{} · {}",
        date_label(entry.updated_at_ms),
        entry.cwd.as_deref().unwrap_or("no cwd")
    );
    if let Some(reason) = entry.reason.as_deref() {
        value.push_str(" · ");
        value.push_str(reason);
    } else if entry.eligible {
        value.push_str(" · Eligible");
    }
    HistoryRow {
        label: title_case(title),
        value,
        adopt_provider: entry.eligible.then(|| entry.provider.clone()),
        adopt_source_id: entry.eligible.then(|| entry.source_id.clone()),
        select_run_id: entry.adopted_run_id.clone(),
    }
}

fn info_row(label: impl Into<String>, value: impl Into<String>) -> HistoryRow {
    HistoryRow {
        label: label.into(),
        value: value.into(),
        adopt_provider: None,
        adopt_source_id: None,
        select_run_id: None,
    }
}

fn title_case(value: &str) -> String {
    let mut chars = value.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

fn date_label(updated_at_ms: i64) -> String {
    if updated_at_ms <= 0 {
        return "unknown date".into();
    }
    format!("{updated_at_ms}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::{Command, HistoryEntryView, HistoryPageState, Project, UiState};

    fn entry(
        provider: &str,
        source_id: &str,
        cwd: &str,
        title: &str,
        updated_at_ms: i64,
    ) -> HistoryEntryView {
        HistoryEntryView {
            provider: provider.into(),
            source_id: source_id.into(),
            transcript_path: format!("/history/{provider}/{source_id}.jsonl"),
            cwd: Some(cwd.into()),
            title: Some(title.into()),
            first_prompt: None,
            updated_at_ms,
            eligible: true,
            reason: None,
            adopted_run_id: None,
        }
    }

    #[test]
    fn grouped_history_orders_current_project_first_then_provider_and_recency() {
        let state = UiState {
            projects: vec![
                Project {
                    id: "p1".into(),
                    name: "repo".into(),
                    path: "/repo".into(),
                },
                Project {
                    id: "p2".into(),
                    name: "other".into(),
                    path: "/other".into(),
                },
            ],
            selected_project: 0,
            history: HistoryPageState {
                entries: vec![
                    entry("claude", "old-other", "/other", "older other", 1),
                    entry("codex", "new-repo", "/repo", "new repo", 10),
                    entry("codex", "old-repo", "/repo", "old repo", 2),
                    entry("claude", "new-other", "/other", "newer other", 11),
                ],
                ..HistoryPageState::default()
            },
            ..UiState::default()
        };

        let rows = grouped_rows(&state);
        assert_eq!(
            rows.iter()
                .map(|row| row.label.as_str())
                .collect::<Vec<_>>(),
            vec![
                "codex · current project",
                "New repo",
                "Old repo",
                "claude · other",
                "Newer other",
                "Older other",
            ]
        );
        assert_eq!(rows[1].adopt_source_id.as_deref(), Some("new-repo"));
    }

    #[test]
    fn search_filters_provider_cwd_title_and_prompt() {
        let mut state = UiState::default();
        state.history.entries = vec![
            entry("codex", "one", "/repo", "Router work", 1),
            HistoryEntryView {
                first_prompt: Some("fix the cache key".into()),
                ..entry("claude", "two", "/cache", "Unrelated", 2)
            },
        ];
        state.history.query = "cache".into();

        let rows = grouped_rows(&state);
        assert!(rows.iter().any(|r| r.label == "Unrelated"));
        assert!(!rows.iter().any(|r| r.label == "Router work"));
    }

    #[test]
    fn page_merge_dedupes_and_marks_adopted_without_corrupting_selection() {
        let mut state = UiState {
            run_id: Some("existing".into()),
            history: HistoryPageState {
                entries: vec![entry("codex", "one", "/repo", "first", 1)],
                ..HistoryPageState::default()
            },
            ..UiState::default()
        };
        merge_page(
            &mut state,
            vec![
                HistoryEntryView {
                    title: Some("updated".into()),
                    updated_at_ms: 5,
                    ..entry("codex", "one", "/repo", "updated", 5)
                },
                entry("claude", "two", "/repo", "second", 2),
            ],
            Some("next".into()),
        );
        assert_eq!(state.history.entries.len(), 2);
        assert_eq!(state.history.next_cursor.as_deref(), Some("next"));
        mark_adopted(&mut state, "codex", "one", "new-run", true);
        assert_eq!(
            state.history.entries[0].adopted_run_id.as_deref(),
            Some("new-run")
        );
        assert_eq!(state.run_id.as_deref(), Some("new-run"));
    }

    #[test]
    fn commands_encode_history_list_next_page_and_adopt() {
        let list = encode_history_command(&Command::RefreshHistory { cursor: None }).unwrap();
        assert_eq!(list.0, autoharness_protocol::methods::HISTORY_LIST);
        assert_eq!(list.1["limit"], 50);

        let next = encode_history_command(&Command::RefreshHistory {
            cursor: Some("cursor".into()),
        })
        .unwrap();
        assert_eq!(next.1["cursor"], "cursor");

        let adopt = encode_history_command(&Command::AdoptHistory {
            provider: "codex".into(),
            source_id: "one".into(),
            project_id: "project".into(),
            engine: "claude".into(),
        })
        .unwrap();
        assert_eq!(adopt.0, autoharness_protocol::methods::HISTORY_ADOPT);
        assert_eq!(adopt.1["source_id"], "one");
        assert!(
            adopt.1["request_id"]
                .as_str()
                .unwrap()
                .starts_with("ui-adopt-")
        );
    }
}
