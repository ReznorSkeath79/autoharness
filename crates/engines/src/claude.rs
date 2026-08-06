//! Claude adapter: drives `claude` with bidirectional stream-json
//! (`-p --verbose --input-format stream-json --output-format stream-json`).
//! Control and output are newline-delimited JSON on stdio only; interactive
//! terminal escape sequences are never parsed as the control protocol.
//!
//! Resume uses `--resume <session_id>`. Interrupt uses the stream-json
//! control channel (`{"type":"control_request","request":{"subtype":"interrupt"}}`).

use std::path::PathBuf;

use autoharness_core::EngineKind;
use serde_json::{Value, json};

use crate::adapter::{EngineAdapter, EngineError, EngineModel, SessionSpec};
use crate::diagnostics::{EngineDiagnostics, assemble};
use crate::event::{EngineEvent, FileChangeKind, ToolStatus};
use crate::process::{
    JsonLinesChild, SessionDirs, engine_control_env_for, find_binary, spawn_json_lines_child,
};

pub struct ClaudeAdapter {
    process: Option<JsonLinesChild>,
    session_id: Option<String>,
    resume_id: Option<String>,
    binary_override: Option<PathBuf>,
    buffered: Vec<EngineEvent>,
}

impl Default for ClaudeAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl ClaudeAdapter {
    pub fn new() -> Self {
        Self {
            process: None,
            session_id: None,
            resume_id: None,
            binary_override: None,
            buffered: Vec::new(),
        }
    }

    /// Use a specific binary path (tests, unusual installs).
    pub fn with_binary(mut self, path: PathBuf) -> Self {
        self.binary_override = Some(path);
        self
    }

    fn binary(&self) -> Result<PathBuf, EngineError> {
        if let Some(p) = &self.binary_override {
            return Ok(p.clone());
        }
        find_binary("claude").ok_or_else(|| EngineError::NotInstalled("claude".into()))
    }

    fn spawn(&mut self, spec: &SessionSpec) -> Result<(), EngineError> {
        let binary = self.binary()?;
        let args = process_args(spec, self.resume_id.as_deref());
        let argument_refs = args.iter().map(String::as_str).collect::<Vec<_>>();
        // Keyed by the caller's session key, never a fresh id: `--resume` can
        // only find the transcript if this turn lands in the same HOME the
        // previous turn wrote it to.
        let dirs = SessionDirs::create_for_engine(&spec.data_dir, &spec.session_key)
            .map_err(EngineError::Io)?;
        let env = engine_control_env_for(&dirs, &binary);
        let process = spawn_json_lines_child(&binary, &argument_refs, &spec.working_dir, &env)?;
        self.process = Some(process);
        Ok(())
    }

    fn drain_buffered(&mut self) -> Option<EngineEvent> {
        if self.buffered.is_empty() {
            None
        } else {
            Some(self.buffered.remove(0))
        }
    }
}

fn process_args(spec: &SessionSpec, resume_id: Option<&str>) -> Vec<String> {
    let mut args = vec![
        "-p".into(),
        "--verbose".into(),
        "--input-format".into(),
        "stream-json".into(),
        "--output-format".into(),
        "stream-json".into(),
        // Native sandbox/permission config: edits inside the working
        // directory are auto-accepted; anything else (Bash, network
        // tools) prompts, and our adapter declines unanswered prompts.
        // Direct-egress tools are disabled outright; brokered fetch goes
        // through the daemon proxy (Phase 4 wiring).
        "--permission-mode".into(),
        "acceptEdits".into(),
        "--disallowedTools".into(),
        "WebFetch,WebSearch,NotebookEdit".into(),
    ];
    if let Some(model) = spec.model.as_deref().filter(|model| !model.is_empty()) {
        args.extend(["--model".into(), model.into()]);
    }
    if let Some(effort) = spec.reasoning_effort.as_deref() {
        args.extend(["--effort".into(), effort.into()]);
    }
    // Same guard as `--model` above: an empty value is not an id, and
    // `--resume ""` fails the turn outright.
    if let Some(id) = resume_id.filter(|id| !id.is_empty()) {
        args.extend(["--resume".into(), id.into()]);
    }
    args
}

fn claude_model_catalog() -> Vec<EngineModel> {
    const EFFORTS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];
    let efforts = || EFFORTS.into_iter().map(str::to_string).collect();
    [
        (
            "",
            "Claude default",
            "Follow the installed Claude Code default",
            true,
        ),
        ("sonnet", "Sonnet", "Latest Claude Sonnet alias", false),
        ("opus", "Opus", "Latest Claude Opus alias", false),
        ("fable", "Fable", "Latest Claude Fable alias", false),
        ("haiku", "Haiku", "Latest Claude Haiku alias", false),
    ]
    .into_iter()
    .map(|(id, display_name, description, is_default)| EngineModel {
        id: id.into(),
        display_name: display_name.into(),
        description: description.into(),
        reasoning_efforts: efforts(),
        default_reasoning_effort: Some("high".into()),
        is_default,
    })
    .collect()
}

/// Normalize provider tool names to shared names (contract equivalence).
fn normalize_tool_name(name: &str) -> String {
    match name {
        "Bash" => "shell".into(),
        other => other.to_string(),
    }
}

/// Map one stream-json output line to normalized events.
/// Pure and total: unrecognized shapes map to nothing.
///
/// Detail contract for [`EngineEvent::ToolActivity`]:
/// - Started: `{"command": ...}` for shell, raw input otherwise.
/// - Finished: `{"success": bool}`.
///
/// File-editing tools map to [`EngineEvent::FileChange`] only — file changes
/// are first-class events, not tool activity.
pub fn map_message(msg: &Value) -> Vec<EngineEvent> {
    let kind = msg.get("type").and_then(Value::as_str).unwrap_or("");
    match kind {
        "system" if msg.get("subtype").and_then(Value::as_str) == Some("init") => msg
            .get("session_id")
            .and_then(Value::as_str)
            .map(|id| {
                vec![EngineEvent::SessionIdentity {
                    session_id: id.to_string(),
                }]
            })
            .unwrap_or_default(),
        "assistant" => msg
            .pointer("/message/content")
            .and_then(Value::as_array)
            .map(|blocks| blocks.iter().flat_map(map_content_block).collect())
            .unwrap_or_default(),
        "user" => msg
            .pointer("/message/content")
            .and_then(Value::as_array)
            .map(|blocks| {
                blocks
                    .iter()
                    .filter_map(|b| {
                        if b.get("type").and_then(Value::as_str) != Some("tool_result") {
                            return None;
                        }
                        Some(EngineEvent::ToolActivity {
                            name: b
                                .get("tool_use_name")
                                .and_then(Value::as_str)
                                .map(normalize_tool_name)
                                .unwrap_or_else(|| "tool".into()),
                            status: ToolStatus::Finished,
                            detail: json!({ "success": !b.get("is_error").and_then(Value::as_bool).unwrap_or(false) }),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default(),
        "control_request" => {
            // Permission / can-use-tool prompts: surfaced as questions.
            // (Requests are also auto-handled by the adapter; answering is a
            // Phase-4 concern.)
            let tool = msg
                .pointer("/request/tool_name")
                .and_then(Value::as_str)
                .map(normalize_tool_name)
                .unwrap_or_else(|| "tool".into());
            let id = msg
                .get("request_id")
                .and_then(Value::as_str)
                .unwrap_or("permission")
                .to_string();
            vec![EngineEvent::Question {
                id,
                prompt: format!("approve tool: {tool}"),
            }]
        }
        "result" => {
            let mut events = Vec::new();
            let usage = msg.get("usage");
            if let Some(usage) = usage {
                events.push(EngineEvent::Usage {
                    input_tokens: usage
                        .get("input_tokens")
                        .and_then(Value::as_u64)
                        .unwrap_or(0),
                    output_tokens: usage
                        .get("output_tokens")
                        .and_then(Value::as_u64)
                        .unwrap_or(0),
                });
            }
            if msg.get("is_error").and_then(Value::as_bool).unwrap_or(false) {
                events.push(EngineEvent::Failed {
                    message: msg
                        .get("result")
                        .and_then(Value::as_str)
                        .filter(|text| !text.trim().is_empty())
                        // `result` is missing exactly when the CLI dies before
                        // it can answer — the case most worth reporting — and
                        // "claude turn failed" on its own gives nobody anything
                        // to act on. `subtype` carries the provider's own
                        // reason, so it beats the generic fallback.
                        .or_else(|| msg.get("subtype").and_then(Value::as_str))
                        .unwrap_or("claude turn failed")
                        .to_string(),
                    recoverable: false,
                });
            } else {
                events.push(EngineEvent::Completed {
                    summary: msg
                        .get("result")
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string),
                });
            }
            events
        }
        _ => vec![],
    }
}

fn map_content_block(block: &Value) -> Vec<EngineEvent> {
    match block.get("type").and_then(Value::as_str) {
        Some("text") => block
            .get("text")
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty())
            .map(|text| {
                vec![EngineEvent::Text {
                    text: text.to_string(),
                }]
            })
            .unwrap_or_default(),
        Some("tool_use") => {
            let raw_name = block.get("name").and_then(Value::as_str).unwrap_or("tool");
            let input = block.get("input").cloned().unwrap_or(Value::Null);
            // File-editing tools map to FileChange only: file changes are
            // first-class events, not tool activity.
            if matches!(raw_name, "Edit" | "Write" | "MultiEdit" | "NotebookEdit") {
                return input
                    .get("file_path")
                    .and_then(Value::as_str)
                    .map(|path| {
                        vec![EngineEvent::FileChange {
                            path: path.to_string(),
                            kind: if raw_name == "Write" {
                                FileChangeKind::Created
                            } else {
                                FileChangeKind::Modified
                            },
                        }]
                    })
                    .unwrap_or_default();
            }
            let name = normalize_tool_name(raw_name);
            let detail = if name == "shell" {
                json!({ "command": input.get("command").cloned().unwrap_or(Value::Null) })
            } else {
                input
            };
            vec![EngineEvent::ToolActivity {
                name,
                status: ToolStatus::Started,
                detail,
            }]
        }
        _ => vec![],
    }
}

#[async_trait::async_trait]
impl EngineAdapter for ClaudeAdapter {
    fn kind(&self) -> EngineKind {
        EngineKind::claude()
    }

    async fn detect(&self) -> EngineDiagnostics {
        let binary = self
            .binary_override
            .clone()
            .or_else(|| find_binary("claude"));
        let version = match &binary {
            Some(path) => {
                crate::probe::version(path, &["--version"], |out| {
                    // "2.1.220 (Claude Code)"
                    out.split_whitespace().next().map(str::to_string)
                })
                .await
            }
            None => None,
        };
        let authenticated = match &binary {
            Some(path) => crate::probe::claude_authenticated(path).await,
            None => None,
        };
        let structured_mode = match &binary {
            Some(path) => crate::probe::help_mentions(path, &["--help"], "--input-format").await,
            None => false,
        };
        assemble(
            EngineKind::claude(),
            binary,
            version,
            authenticated,
            structured_mode,
        )
    }

    async fn available_models(
        &mut self,
        _data_dir: &std::path::Path,
    ) -> Result<Vec<EngineModel>, EngineError> {
        Ok(claude_model_catalog())
    }

    async fn start_session(&mut self, spec: &SessionSpec) -> Result<(), EngineError> {
        self.resume_id = None;
        self.spawn(spec)
    }

    async fn resume_session(
        &mut self,
        session_id: &str,
        spec: &SessionSpec,
    ) -> Result<(), EngineError> {
        self.resume_id = Some(session_id.to_string());
        self.spawn(spec)
    }

    async fn send_turn(&mut self, prompt: &str) -> Result<(), EngineError> {
        let process = self
            .process
            .as_mut()
            .ok_or_else(|| EngineError::Protocol("send_turn before start_session".into()))?;
        process
            .send(&json!({
                "type": "user",
                "message": {
                    "role": "user",
                    "content": [{ "type": "text", "text": prompt }]
                }
            }))
            .await
    }

    async fn next_event(&mut self) -> Result<Option<EngineEvent>, EngineError> {
        loop {
            if let Some(ev) = self.drain_buffered() {
                return Ok(Some(ev));
            }
            let Some(msg) = ({
                let process = match self.process.as_mut() {
                    Some(p) => p,
                    None => return Ok(None),
                };
                process.recv().await?
            }) else {
                return Ok(None);
            };
            if let Some(id) = msg.get("session_id").and_then(Value::as_str)
                && self.session_id.is_none()
            {
                self.session_id = Some(id.to_string());
            }
            // Permission prompts must be answered to unblock the provider;
            // Phase 2 declines them (V1 runs unattended with narrow scopes).
            if msg.get("type").and_then(Value::as_str) == Some("control_request")
                && let Some(request_id) = msg.get("request_id").cloned()
            {
                let process = self.process.as_mut().expect("connected");
                process
                    .send(&json!({
                        "type": "control_response",
                        "response": {
                            "subtype": "error",
                            "request_id": request_id,
                            "error": "permission declined: unattended run"
                        }
                    }))
                    .await?;
            }
            let mut events = map_message(&msg).into_iter();
            if let Some(first) = events.next() {
                self.buffered.extend(events);
                return Ok(Some(first));
            }
        }
    }

    async fn pause(&mut self) -> Result<(), EngineError> {
        // Soft pause: interrupt the current turn; send_turn resumes work.
        self.interrupt().await
    }

    async fn interrupt(&mut self) -> Result<(), EngineError> {
        let Some(process) = self.process.as_mut() else {
            return Ok(());
        };
        process
            .send(&json!({
                "type": "control_request",
                "request_id": format!("ah-{}", autoharness_core::new_id()),
                "request": { "subtype": "interrupt" }
            }))
            .await
    }

    async fn cancel(&mut self) -> Result<(), EngineError> {
        if let Some(mut process) = self.process.take() {
            process.kill().await;
        }
        self.session_id = None;
        Ok(())
    }

    fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }
}

#[cfg(test)]
mod selection_tests {
    use super::*;

    #[test]
    fn model_and_effort_are_real_claude_process_arguments() {
        let spec = SessionSpec {
            working_dir: PathBuf::from("/tmp/repo"),
            data_dir: PathBuf::from("/tmp/data"),
            session_key: "thread-1".into(),
            model: Some("opus".into()),
            reasoning_effort: Some("xhigh".into()),
        };
        let args = process_args(&spec, Some("session-1"));
        assert!(args.windows(2).any(|pair| pair == ["--model", "opus"]));
        assert!(args.windows(2).any(|pair| pair == ["--effort", "xhigh"]));
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--resume", "session-1"])
        );
    }

    #[test]
    fn an_empty_resume_id_never_reaches_the_command_line() {
        // A stored-but-empty session id used to become `--resume ""`, which
        // the CLI rejects — every follow-up turn failed with "claude turn
        // failed" instead of starting a fresh conversation.
        let spec = SessionSpec {
            working_dir: PathBuf::from("/tmp/repo"),
            data_dir: PathBuf::from("/tmp/data"),
            session_key: "thread-1".into(),
            model: None,
            reasoning_effort: None,
        };
        let args = process_args(&spec, Some(""));
        assert!(
            !args.iter().any(|arg| arg == "--resume"),
            "empty id must not produce a --resume flag: {args:?}"
        );
    }

    #[test]
    fn claude_catalog_exposes_cli_aliases_and_native_efforts() {
        let models = claude_model_catalog();
        assert!(
            models
                .iter()
                .any(|model| model.id.is_empty() && model.is_default)
        );
        assert!(models.iter().any(|model| model.id == "opus"));
        assert!(models.iter().any(|model| model.id == "sonnet"));
        assert!(
            models.iter().all(|model| {
                model.reasoning_efforts == ["low", "medium", "high", "xhigh", "max"]
            })
        );
    }
}
