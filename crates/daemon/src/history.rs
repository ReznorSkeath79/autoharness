use std::collections::{HashMap, VecDeque};
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use autoharness_store::{ExternalHistoryEntry, Store};
use serde_json::Value;

pub const MAX_LINE_BYTES: usize = 64 * 1024;
const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_LINES_PER_FILE: usize = 256;
const TEXT_CAP: usize = 240;
const DIAGNOSTIC_CAP: usize = 160;

#[derive(Debug, Clone, Default)]
pub struct HistoryRoots {
    pub codex: Option<PathBuf>,
    pub claude: Option<PathBuf>,
}

impl HistoryRoots {
    pub fn defaults() -> Self {
        let home = std::env::home_dir().unwrap_or_else(|| PathBuf::from("."));
        Self {
            codex: Some(home.join(".codex").join("sessions")),
            claude: Some(home.join(".claude").join("projects")),
        }
    }

    pub fn provider_root(&self, provider: &str) -> Option<&Path> {
        match provider {
            "codex" => self.codex.as_deref(),
            "claude" => self.claude.as_deref(),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryDiagnostic {
    pub provider: String,
    pub path: String,
    pub reason: String,
}

#[derive(Debug, Clone, Default)]
pub struct ScanResult {
    pub entries: Vec<ExternalHistoryEntry>,
    pub diagnostics: Vec<HistoryDiagnostic>,
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn sanitize_text(value: &str, cap: usize) -> Option<String> {
    let collapsed = value
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if collapsed.is_empty() {
        return None;
    }
    let mut out = String::new();
    for ch in collapsed.chars() {
        if out.len() + ch.len_utf8() > cap {
            break;
        }
        out.push(ch);
    }
    Some(out)
}

fn diag(provider: &str, path: &Path, reason: impl Into<String>) -> HistoryDiagnostic {
    let reason = sanitize_text(&reason.into(), DIAGNOSTIC_CAP).unwrap_or_else(|| "skipped".into());
    HistoryDiagnostic {
        provider: provider.into(),
        path: path.to_string_lossy().into_owned(),
        reason,
    }
}

pub fn scan_roots(roots: &HistoryRoots) -> ScanResult {
    let mut result = ScanResult::default();
    scan_provider("codex", roots.codex.as_deref(), &mut result);
    scan_provider("claude", roots.claude.as_deref(), &mut result);

    let mut dedup: HashMap<(String, String), ExternalHistoryEntry> = HashMap::new();
    for entry in result.entries.drain(..) {
        let key = (entry.provider.clone(), entry.source_id.clone());
        match dedup.get(&key) {
            Some(existing)
                if (existing.updated_at_ms, existing.transcript_path.as_str())
                    > (entry.updated_at_ms, entry.transcript_path.as_str()) => {}
            _ => {
                dedup.insert(key, entry);
            }
        }
    }
    result.entries = dedup.into_values().collect();
    result.entries.sort_by(|a, b| {
        b.updated_at_ms
            .cmp(&a.updated_at_ms)
            .then_with(|| a.provider.cmp(&b.provider))
            .then_with(|| a.source_id.cmp(&b.source_id))
    });
    result
}

pub fn refresh_index(
    store: &Store,
    roots: &HistoryRoots,
) -> autoharness_store::Result<Vec<String>> {
    let scan_seen_ms = now_ms();
    let mut scan = scan_roots(roots);
    for entry in &mut scan.entries {
        entry.last_seen_ms = scan_seen_ms;
        store.upsert_external_history(entry)?;
    }
    if roots.codex.is_some() {
        let _ = store.mark_external_history_missing_except_seen("codex", scan_seen_ms)?;
    }
    if roots.claude.is_some() {
        let _ = store.mark_external_history_missing_except_seen("claude", scan_seen_ms)?;
    }
    Ok(scan.diagnostics.into_iter().map(|d| d.reason).collect())
}

fn scan_provider(provider: &str, root: Option<&Path>, result: &mut ScanResult) {
    let Some(root) = root else { return };
    let Ok(root_canon) = root.canonicalize() else {
        return;
    };
    let mut queue = VecDeque::from([root.to_path_buf()]);
    let mut visited = 0usize;
    while let Some(path) = queue.pop_front() {
        visited += 1;
        if visited > 10_000 {
            result
                .diagnostics
                .push(diag(provider, &path, "scan entry cap reached"));
            break;
        }
        let meta = match fs::symlink_metadata(&path) {
            Ok(meta) => meta,
            Err(e) => {
                result
                    .diagnostics
                    .push(diag(provider, &path, e.to_string()));
                continue;
            }
        };
        if meta.file_type().is_symlink() {
            result.diagnostics.push(diag(
                provider,
                &path,
                "skipped symlink to prevent path escape",
            ));
            continue;
        }
        if meta.is_dir() {
            let read = match fs::read_dir(&path) {
                Ok(read) => read,
                Err(e) => {
                    result
                        .diagnostics
                        .push(diag(provider, &path, e.to_string()));
                    continue;
                }
            };
            for entry in read.flatten() {
                queue.push_back(entry.path());
            }
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let Ok(canon) = path.canonicalize() else {
            result
                .diagnostics
                .push(diag(provider, &path, "could not canonicalize file"));
            continue;
        };
        if !canon.starts_with(&root_canon) {
            result
                .diagnostics
                .push(diag(provider, &path, "path escapes provider root"));
            continue;
        }
        if meta.len() > MAX_FILE_BYTES {
            result
                .diagnostics
                .push(diag(provider, &path, "file exceeds metadata scan cap"));
            continue;
        }
        match parse_file(provider, &canon, &meta) {
            Ok(Some(entry)) => result.entries.push(entry),
            Ok(None) => {}
            Err(reason) => result.diagnostics.push(diag(provider, &path, reason)),
        }
    }
}

fn parse_file(
    provider: &str,
    path: &Path,
    meta: &fs::Metadata,
) -> Result<Option<ExternalHistoryEntry>, String> {
    let file = fs::File::open(path).map_err(|e| e.to_string())?;
    let reader = BufReader::new(file);
    let mut source_id = None;
    let mut cwd = None;
    let mut title = None;
    let mut first_prompt = None;
    let mut saw_json = false;

    for (idx, line) in reader.lines().enumerate() {
        if idx >= MAX_LINES_PER_FILE {
            break;
        }
        let line = line.map_err(|e| e.to_string())?;
        if line.len() > MAX_LINE_BYTES {
            return Err("line exceeds metadata scan cap".into());
        }
        if line.trim().is_empty() {
            continue;
        }
        let value: Value =
            serde_json::from_str(&line).map_err(|e| format!("malformed jsonl: {e}"))?;
        saw_json = true;
        let metadata = if provider == "codex" {
            value.get("payload").unwrap_or(&value)
        } else {
            &value
        };
        if source_id.is_none() {
            source_id = metadata
                .get("session_id")
                .or_else(|| metadata.get("sessionId"))
                .or_else(|| metadata.get("id"))
                .and_then(Value::as_str)
                .and_then(|s| sanitize_text(s, TEXT_CAP));
        }
        if cwd.is_none() {
            cwd = metadata
                .get("cwd")
                .or_else(|| metadata.get("workingDirectory"))
                .and_then(Value::as_str)
                .and_then(|s| sanitize_text(s, TEXT_CAP));
        }
        if title.is_none() {
            title = value
                .get("title")
                .or_else(|| value.get("summary"))
                .or_else(|| metadata.get("title"))
                .or_else(|| metadata.get("summary"))
                .and_then(Value::as_str)
                .and_then(|s| sanitize_text(s, TEXT_CAP));
        }
        if first_prompt.is_none() {
            first_prompt = extract_prompt(&value).and_then(|s| sanitize_text(&s, TEXT_CAP));
        }
    }
    if !saw_json {
        return Ok(None);
    }
    let source_id = source_id.unwrap_or_else(|| {
        path.file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.to_string_lossy().into_owned())
    });
    if title.is_none() {
        title = first_prompt.clone();
    }
    let updated_at_ms = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or_else(now_ms);
    Ok(Some(ExternalHistoryEntry {
        provider: provider.into(),
        source_id,
        transcript_path: path.to_string_lossy().into_owned(),
        cwd,
        title,
        first_prompt,
        updated_at_ms,
        last_seen_ms: updated_at_ms,
        missing: false,
        diagnostic: None,
        adopted_run_id: None,
    }))
}

fn extract_prompt(value: &Value) -> Option<String> {
    if value.get("type").and_then(Value::as_str) == Some("event_msg") {
        return value.get("payload").and_then(extract_prompt);
    }
    if value.get("type").and_then(Value::as_str) == Some("user_message") {
        return value
            .get("message")
            .and_then(Value::as_str)
            .map(str::to_string);
    }
    if value.get("type").and_then(Value::as_str) == Some("user") {
        if let Some(text) = value.get("message").and_then(Value::as_str) {
            return Some(text.to_string());
        }
        if let Some(content) = value.pointer("/message/content").and_then(Value::as_array) {
            let joined = content
                .iter()
                .filter_map(|part| part.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join(" ");
            if !joined.is_empty() {
                return Some(joined);
            }
        }
        if let Some(text) = value.pointer("/message/content").and_then(Value::as_str) {
            return Some(text.to_string());
        }
    }
    None
}

pub fn validate_adoption_source(
    entry: &ExternalHistoryEntry,
    provider_root: &Path,
    project_path: &Path,
) -> Result<(), String> {
    let root = provider_root
        .canonicalize()
        .map_err(|e| format!("provider root unavailable: {e}"))?;
    let transcript = PathBuf::from(&entry.transcript_path);
    let transcript = transcript
        .canonicalize()
        .map_err(|e| format!("transcript unavailable: {e}"))?;
    if !transcript.starts_with(&root) {
        return Err("transcript is outside provider root".into());
    }
    if !project_path.is_dir() {
        return Err("adoption project path does not exist".into());
    }
    if let Some(cwd) = entry.cwd.as_deref()
        && !Path::new(cwd).is_dir()
    {
        return Err("source cwd no longer exists".into());
    }
    let meta = fs::metadata(&transcript).map_err(|e| e.to_string())?;
    let reparsed = parse_file(&entry.provider, &transcript, &meta)?
        .ok_or_else(|| "transcript no longer contains recognizable metadata".to_string())?;
    if reparsed.source_id != entry.source_id
        || reparsed.provider != entry.provider
        || reparsed.cwd != entry.cwd
        || reparsed.first_prompt != entry.first_prompt
    {
        return Err("transcript metadata changed since it was indexed".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;
    use std::os::unix::fs::symlink;
    use std::time::SystemTime;

    fn codex_session(path: &std::path::Path, id: &str, cwd: &str, prompt: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut file = fs::File::create(path).unwrap();
        writeln!(
            file,
            r#"{{"timestamp":"2026-08-05T01:02:03Z","type":"session_meta","payload":{{"id":"{id}","timestamp":"2026-08-05T01:02:03Z","cwd":"{cwd}","originator":"codex_cli_rs","cli_version":"1.2.3","source":"cli"}}}}"#
        )
        .unwrap();
        writeln!(file, r#"{{"timestamp":"2026-08-05T01:02:04Z","type":"turn_context","payload":{{"cwd":"{cwd}","approval_policy":"never"}}}}"#).unwrap();
        writeln!(file, r#"{{"timestamp":"2026-08-05T01:02:05Z","type":"event_msg","payload":{{"type":"user_message","message":"{prompt}","images":[]}}}}"#).unwrap();
    }

    fn claude_session(path: &std::path::Path, id: &str, cwd: &str, prompt: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut file = fs::File::create(path).unwrap();
        writeln!(
            file,
            r#"{{"parentUuid":null,"isSidechain":false,"userType":"external","cwd":"{cwd}","sessionId":"{id}","version":"1.0.0","gitBranch":"main","type":"user","message":{{"role":"user","content":"{prompt}"}},"uuid":"message-1","timestamp":"2026-08-05T03:04:05Z"}}"#
        )
        .unwrap();
        writeln!(
            file,
            r#"{{"type":"summary","summary":"Claude title","leafUuid":"message-1"}}"#
        )
        .unwrap();
    }

    #[test]
    fn scanner_parses_codex_and_claude_fixtures_without_mutating_provider_files() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("repo");
        fs::create_dir_all(&project).unwrap();
        let codex_root = dir.path().join("codex");
        let claude_root = dir.path().join("claude");
        let codex = codex_root.join("2026/08/05/session.jsonl");
        let claude = claude_root.join("-tmp-repo/session.jsonl");
        codex_session(
            &codex,
            "codex-session",
            &project.to_string_lossy(),
            "hello from codex",
        );
        claude_session(
            &claude,
            "claude-session",
            &project.to_string_lossy(),
            "hello from claude",
        );
        let codex_before = (
            fs::metadata(&codex).unwrap().modified().unwrap(),
            fs::read(&codex).unwrap(),
        );
        let claude_before = (
            fs::metadata(&claude).unwrap().modified().unwrap(),
            fs::read(&claude).unwrap(),
        );

        let discovered = scan_roots(&HistoryRoots {
            codex: Some(codex_root),
            claude: Some(claude_root),
        });

        assert_eq!(discovered.entries.len(), 2, "{:?}", discovered.diagnostics);
        assert!(discovered.diagnostics.is_empty());
        assert!(discovered.entries.iter().any(|e| {
            e.provider == "codex"
                && e.source_id == "codex-session"
                && e.cwd.as_deref() == Some(project.to_string_lossy().as_ref())
                && e.title.as_deref() == Some("hello from codex")
                && e.first_prompt.as_deref() == Some("hello from codex")
        }));
        assert!(discovered.entries.iter().any(|e| {
            e.provider == "claude"
                && e.source_id == "claude-session"
                && e.title.as_deref() == Some("Claude title")
                && e.first_prompt.as_deref() == Some("hello from claude")
        }));
        assert_eq!(
            fs::metadata(&codex).unwrap().modified().unwrap(),
            codex_before.0
        );
        assert_eq!(fs::read(&codex).unwrap(), codex_before.1);
        assert_eq!(
            fs::metadata(&claude).unwrap().modified().unwrap(),
            claude_before.0
        );
        assert_eq!(fs::read(&claude).unwrap(), claude_before.1);
    }

    #[test]
    fn scanner_skips_malformed_huge_duplicate_and_symlink_escape_with_bounded_diagnostics() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("codex");
        let outside = dir.path().join("outside.jsonl");
        fs::create_dir_all(&root).unwrap();
        fs::write(&outside, r#"{"session_id":"outside"}"#).unwrap();
        symlink(&outside, root.join("escape.jsonl")).unwrap();
        fs::write(root.join("bad.jsonl"), "{not-json\n").unwrap();
        fs::write(
            root.join("huge.jsonl"),
            format!("{}\n", "x".repeat(MAX_LINE_BYTES + 8)),
        )
        .unwrap();
        codex_session(&root.join("a.jsonl"), "dup", "/tmp/repo", "one");
        codex_session(&root.join("b.jsonl"), "dup", "/tmp/repo", "two newer");
        fs::File::options()
            .append(true)
            .open(root.join("b.jsonl"))
            .unwrap()
            .write_all(br#"{"timestamp":"2026-08-05T06:00:00Z"}"#)
            .unwrap();

        let discovered = scan_roots(&HistoryRoots {
            codex: Some(root),
            claude: None,
        });

        assert_eq!(discovered.entries.len(), 1);
        assert_eq!(discovered.entries[0].source_id, "dup");
        assert_eq!(
            discovered.entries[0].first_prompt.as_deref(),
            Some("two newer")
        );
        assert!(
            discovered
                .diagnostics
                .iter()
                .any(|d| d.reason.contains("symlink"))
        );
        assert!(
            discovered
                .diagnostics
                .iter()
                .any(|d| d.reason.contains("malformed") || d.reason.contains("line"))
        );
        assert!(discovered.diagnostics.iter().all(|d| d.reason.len() <= 160));
    }

    #[test]
    fn adoption_validation_refuses_changed_or_out_of_root_sources() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("codex");
        let project = dir.path().join("repo");
        fs::create_dir_all(&project).unwrap();
        let transcript = root.join("session.jsonl");
        codex_session(&transcript, "s1", &project.to_string_lossy(), "adopt me");
        let entry = scan_roots(&HistoryRoots {
            codex: Some(root.clone()),
            claude: None,
        })
        .entries
        .pop()
        .unwrap();

        validate_adoption_source(&entry, &root, &project).unwrap();
        fs::write(&transcript, "changed\n").unwrap();
        assert!(validate_adoption_source(&entry, &root, &project).is_err());

        let outside = root.join("../outside.jsonl");
        fs::write(&outside, "{}").unwrap();
        let mut escaped = entry.clone();
        escaped.transcript_path = outside.to_string_lossy().into_owned();
        escaped.last_seen_ms = SystemTime::UNIX_EPOCH.elapsed().unwrap().as_millis() as i64;
        assert!(validate_adoption_source(&escaped, &root, &project).is_err());
    }
}
