//! Codex adapter: drives `codex app-server` (experimental structured JSON-RPC
//! 2.0 over newline-delimited stdio; v2 protocol). Interactive terminal
//! escape sequences are never parsed as the control protocol.
//!
//! Handshake: `initialize` → `initialized` → `thread/start` (or
//! `thread/resume`) → `turn/start`. Output arrives as server notifications
//! (`item/*`, `turn/*`, `thread/*`), mapped to normalized events by
//! [`map_notification`]. Approval policy is pinned to `"never"`; any server
//! approval request that still arrives is declined with a JSON-RPC error and
//! surfaced as a `Question` event.

use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, Ordering};

use autoharness_core::EngineKind;
use serde_json::{Value, json};

use crate::adapter::{EngineAdapter, EngineError, EngineModel, SessionSpec};
use crate::diagnostics::{EngineDiagnostics, assemble};
use crate::event::{EngineEvent, FileChangeKind, ToolStatus};
use crate::process::{
    JsonLinesChild, SessionDirs, binary_candidates, engine_control_env_for, spawn_json_lines_child,
};

pub struct CodexAdapter {
    process: Option<JsonLinesChild>,
    session_id: Option<String>,
    active_turn_id: Option<String>,
    next_rpc_id: AtomicI64,
    binary_override: Option<PathBuf>,
    /// Mapped events queued behind the one currently being returned.
    buffered: Vec<EngineEvent>,
    /// Applied on every `turn/start`, including turns after resume.
    reasoning_effort: Option<String>,
}

impl Default for CodexAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl CodexAdapter {
    pub fn new() -> Self {
        Self {
            process: None,
            session_id: None,
            active_turn_id: None,
            next_rpc_id: AtomicI64::new(1),
            binary_override: None,
            buffered: Vec::new(),
            reasoning_effort: None,
        }
    }

    fn drain_buffered(&mut self) -> Option<EngineEvent> {
        if self.buffered.is_empty() {
            None
        } else {
            Some(self.buffered.remove(0))
        }
    }

    /// Use a specific binary path (tests, unusual installs).
    pub fn with_binary(mut self, path: PathBuf) -> Self {
        self.binary_override = Some(path);
        self
    }

    async fn binary(&self) -> Result<PathBuf, EngineError> {
        if let Some(p) = &self.binary_override {
            return Ok(p.clone());
        }
        let candidates = binary_candidates("codex");
        let fallback = candidates.first().cloned();
        for candidate in candidates {
            // `login status` is token-free and reads the exact same saved
            // config/auth state as app-server. `None` means this executable
            // could not load that state (for example an older CLI rejecting a
            // config written by the bundled Codex), so try the next install.
            if crate::probe::codex_authenticated(&candidate)
                .await
                .is_some()
            {
                return Ok(candidate);
            }
        }
        fallback.ok_or_else(|| EngineError::NotInstalled("codex".into()))
    }

    async fn rpc(&mut self, method: &str, params: Value) -> Result<Value, EngineError> {
        let id = self.next_rpc_id.fetch_add(1, Ordering::SeqCst);
        let process = self
            .process
            .as_mut()
            .ok_or_else(|| EngineError::Protocol("not connected".into()))?;
        process
            .send(&json!({ "id": id, "method": method, "params": params }))
            .await?;
        // Await the matching response, skipping interleaved notifications.
        for _ in 0..64 {
            let Some(msg) = process.recv().await? else {
                return Err(EngineError::ProcessExited("codex app-server closed".into()));
            };
            if msg.get("id").and_then(Value::as_i64) == Some(id) {
                if let Some(error) = msg.get("error") {
                    return Err(EngineError::Provider(format!(
                        "{method} failed: {}",
                        error
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown")
                    )));
                }
                return Ok(msg.get("result").cloned().unwrap_or(Value::Null));
            }
            // Notifications arriving mid-handshake are logged and dropped;
            // streaming begins only after turn/start.
            tracing::debug!(?method, "skipping interleaved app-server message");
        }
        Err(EngineError::Protocol(format!(
            "no response to {method} after 64 messages"
        )))
    }

    async fn initialize(&mut self) -> Result<(), EngineError> {
        self.rpc(
            "initialize",
            json!({ "clientInfo": { "name": "autoharness", "title": "AutoHarness", "version": env!("CARGO_PKG_VERSION") } }),
        )
        .await?;
        let process = self.process.as_mut().expect("connected");
        process
            .send(&json!({ "method": "initialized", "params": {} }))
            .await?;

        Ok(())
    }

    async fn handshake(
        &mut self,
        spec: &SessionSpec,
        resume: Option<&str>,
    ) -> Result<String, EngineError> {
        self.initialize().await?;

        let (method, params) = thread_request(spec, resume);
        let result = self.rpc(method, params).await?;
        result
            .pointer("/thread/id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| EngineError::Protocol(format!("{method}: no thread.id in response")))
    }
}

fn thread_request(spec: &SessionSpec, resume: Option<&str>) -> (&'static str, Value) {
    let mut params = json!({
        "cwd": spec.working_dir,
        "approvalPolicy": "never",
        // Native sandbox: codex's own Seatbelt layer confines model-driven
        // tool subprocess writes to the worktree and blocks their network
        // access. The control process itself keeps provider network access.
        "sandbox": "workspace-write",
        "serviceName": "autoharness",
    });
    if let Some(model) = spec.model.as_deref().filter(|model| !model.is_empty()) {
        params["model"] = json!(model);
    }
    match resume {
        Some(thread_id) => {
            params["threadId"] = json!(thread_id);
            ("thread/resume", params)
        }
        None => ("thread/start", params),
    }
}

fn turn_request(thread_id: &str, prompt: &str, effort: Option<&str>) -> Value {
    let mut params = json!({
        "threadId": thread_id,
        "input": [{ "type": "text", "text": prompt }],
    });
    if let Some(effort) = effort.filter(|effort| !effort.is_empty()) {
        params["effort"] = json!(effort);
    }
    params
}

fn parse_model_catalog(result: &Value) -> Result<Vec<EngineModel>, EngineError> {
    let data = result
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| EngineError::Protocol("model/list returned no data array".into()))?;
    let mut models = Vec::new();
    for raw in data {
        if raw.get("hidden").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        let Some(id) = raw
            .get("model")
            .and_then(Value::as_str)
            .or_else(|| raw.get("id").and_then(Value::as_str))
            .filter(|id| !id.is_empty())
        else {
            continue;
        };
        let display_name = raw
            .get("displayName")
            .and_then(Value::as_str)
            .filter(|label| !label.is_empty())
            .unwrap_or(id);
        let reasoning_efforts = raw
            .get("supportedReasoningEfforts")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|effort| effort.get("reasoningEffort").and_then(Value::as_str))
            .filter(|effort| !effort.is_empty())
            .map(str::to_string)
            .collect::<Vec<_>>();
        models.push(EngineModel {
            id: id.into(),
            display_name: display_name.into(),
            description: raw
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .into(),
            reasoning_efforts,
            default_reasoning_effort: raw
                .get("defaultReasoningEffort")
                .and_then(Value::as_str)
                .map(str::to_string),
            is_default: raw.get("isDefault").and_then(Value::as_bool) == Some(true),
        });
    }
    if models.is_empty() {
        return Err(EngineError::Protocol(
            "model/list returned no visible usable models".into(),
        ));
    }
    Ok(models)
}

/// Map one app-server server notification to normalized events.
/// Pure and total: unknown methods/items map to nothing.
pub fn map_notification(method: &str, params: &Value) -> Vec<EngineEvent> {
    match method {
        "thread/started" => params
            .pointer("/thread/id")
            .and_then(Value::as_str)
            .map(|id| {
                vec![EngineEvent::SessionIdentity {
                    session_id: id.to_string(),
                }]
            })
            .unwrap_or_default(),
        "item/started" => map_item(params.get("item").unwrap_or(&Value::Null), true),
        "item/completed" => map_item(params.get("item").unwrap_or(&Value::Null), false),
        "item/agentMessage/delta" => params
            .get("delta")
            .and_then(Value::as_str)
            .map(|delta| {
                vec![EngineEvent::TextDelta {
                    delta: delta.to_string(),
                }]
            })
            .unwrap_or_default(),
        "thread/tokenUsage/updated" => {
            let total = params.pointer("/tokenUsage/total");
            let input = total
                .and_then(|t| t.get("inputTokens"))
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let output = total
                .and_then(|t| t.get("outputTokens"))
                .and_then(Value::as_u64)
                .unwrap_or(0);
            vec![EngineEvent::Usage {
                input_tokens: input,
                output_tokens: output,
            }]
        }
        "turn/completed" => {
            let status = params
                .pointer("/turn/status")
                .and_then(Value::as_str)
                .unwrap_or("");
            match status {
                "completed" => vec![EngineEvent::Completed { summary: None }],
                "failed" => vec![EngineEvent::Failed {
                    message: params
                        .pointer("/turn/error/message")
                        .and_then(Value::as_str)
                        .unwrap_or("turn failed")
                        .to_string(),
                    recoverable: false,
                }],
                "interrupted" => vec![EngineEvent::Failed {
                    message: "turn interrupted".into(),
                    recoverable: true,
                }],
                _ => vec![],
            }
        }
        "error" => vec![EngineEvent::Failed {
            message: params
                .pointer("/error/message")
                .and_then(Value::as_str)
                .unwrap_or("codex error")
                .to_string(),
            recoverable: params
                .get("willRetry")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        }],
        _ => vec![],
    }
}

/// Map a ThreadItem to normalized events. `started` selects the lifecycle
/// form for activity items.
fn map_item(item: &Value, started: bool) -> Vec<EngineEvent> {
    let kind = item.get("type").and_then(Value::as_str).unwrap_or("");
    match kind {
        "commandExecution" => {
            if started {
                let command = item
                    .get("command")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                vec![EngineEvent::ToolActivity {
                    name: "shell".into(),
                    status: ToolStatus::Started,
                    detail: json!({ "command": command }),
                }]
            } else {
                let success = match item.get("exitCode").and_then(Value::as_i64) {
                    Some(code) => code == 0,
                    None => true,
                };
                vec![EngineEvent::ToolActivity {
                    name: "shell".into(),
                    status: ToolStatus::Finished,
                    detail: json!({ "success": success }),
                }]
            }
        }
        "fileChange" if !started => item
            .get("changes")
            .and_then(Value::as_array)
            .map(|changes| {
                changes
                    .iter()
                    .filter_map(|c| {
                        let path = c.get("path")?.as_str()?.to_string();
                        let kind = match c.pointer("/kind/type").and_then(Value::as_str) {
                            Some("add") => FileChangeKind::Created,
                            Some("delete") => FileChangeKind::Deleted,
                            _ => FileChangeKind::Modified,
                        };
                        Some(EngineEvent::FileChange { path, kind })
                    })
                    .collect()
            })
            .unwrap_or_default(),
        "agentMessage" if !started => item
            .get("text")
            .and_then(Value::as_str)
            .map(|text| {
                vec![EngineEvent::Text {
                    text: text.to_string(),
                }]
            })
            .unwrap_or_default(),
        "mcpToolCall" => {
            let tool = item
                .get("tool")
                .and_then(Value::as_str)
                .unwrap_or("mcp_tool")
                .to_string();
            vec![EngineEvent::ToolActivity {
                name: tool,
                status: if started {
                    ToolStatus::Started
                } else {
                    ToolStatus::Finished
                },
                detail: item.get("arguments").cloned().unwrap_or(Value::Null),
            }]
        }
        _ => vec![],
    }
}

/// A server→client request (approval prompts etc.). Policy is `"never"`, so
/// these are surfaced as questions and declined. Public for equivalence tests.
pub fn question_from_server_request(method: &str, params: &Value) -> EngineEvent {
    let id = params
        .get("itemId")
        .or_else(|| params.get("callId"))
        .and_then(Value::as_str)
        .unwrap_or("approval")
        .to_string();
    let tool = if method.contains("commandExecution") || method.contains("execCommand") {
        "shell"
    } else if method.contains("fileChange") || method.contains("applyPatch") {
        "file_edit"
    } else {
        "tool"
    };
    EngineEvent::Question {
        id,
        prompt: format!("approve tool: {tool}"),
    }
}

#[async_trait::async_trait]
impl EngineAdapter for CodexAdapter {
    fn kind(&self) -> EngineKind {
        EngineKind::codex()
    }

    async fn detect(&self) -> EngineDiagnostics {
        let binary = self.binary().await.ok();
        let version = match &binary {
            Some(path) => {
                crate::probe::version(path, &["--version"], |out| {
                    // "codex-cli 0.143.0"
                    out.split_whitespace().nth(1).map(str::to_string)
                })
                .await
            }
            None => None,
        };
        let authenticated = match &binary {
            Some(path) => crate::probe::codex_authenticated(path).await,
            None => None,
        };
        let structured_mode = match &binary {
            Some(path) => crate::probe::succeeds(path, &["app-server", "--help"]).await,
            None => false,
        };
        assemble(
            EngineKind::codex(),
            binary,
            version,
            authenticated,
            structured_mode,
        )
    }

    async fn available_models(
        &mut self,
        data_dir: &std::path::Path,
    ) -> Result<Vec<EngineModel>, EngineError> {
        std::fs::create_dir_all(data_dir)?;
        let binary = self.binary().await?;
        let dirs = SessionDirs::create_for_engine(data_dir, &autoharness_core::new_id())
            .map_err(EngineError::Io)?;
        let env = engine_control_env_for(&dirs, &binary);
        let process = spawn_json_lines_child(&binary, &["app-server"], data_dir, &env)?;
        self.process = Some(process);
        let result = async {
            self.initialize().await?;
            self.rpc(
                "model/list",
                json!({ "limit": 100, "includeHidden": false }),
            )
            .await
        }
        .await;
        let _ = self.cancel().await;
        parse_model_catalog(&result?)
    }

    async fn start_session(&mut self, spec: &SessionSpec) -> Result<(), EngineError> {
        self.reasoning_effort.clone_from(&spec.reasoning_effort);
        let binary = self.binary().await?;
        // Keyed by the caller's session key, never a fresh id: a resume can
        // only find the thread if this turn lands in the same HOME the
        // previous turn wrote it to.
        let dirs = SessionDirs::create_for_engine(&spec.data_dir, &spec.session_key)
            .map_err(EngineError::Io)?;
        let env = engine_control_env_for(&dirs, &binary);
        let process = spawn_json_lines_child(&binary, &["app-server"], &spec.working_dir, &env)?;
        self.process = Some(process);
        let thread_id = self.handshake(spec, None).await?;
        self.session_id = Some(thread_id);
        Ok(())
    }

    async fn resume_session(
        &mut self,
        session_id: &str,
        spec: &SessionSpec,
    ) -> Result<(), EngineError> {
        self.reasoning_effort.clone_from(&spec.reasoning_effort);
        let binary = self.binary().await?;
        // Keyed by the caller's session key, never a fresh id: a resume can
        // only find the thread if this turn lands in the same HOME the
        // previous turn wrote it to.
        let dirs = SessionDirs::create_for_engine(&spec.data_dir, &spec.session_key)
            .map_err(EngineError::Io)?;
        let env = engine_control_env_for(&dirs, &binary);
        let process = spawn_json_lines_child(&binary, &["app-server"], &spec.working_dir, &env)?;
        self.process = Some(process);
        let thread_id = self.handshake(spec, Some(session_id)).await?;
        self.session_id = Some(thread_id);
        Ok(())
    }

    async fn send_turn(&mut self, prompt: &str) -> Result<(), EngineError> {
        let thread_id = self
            .session_id
            .clone()
            .ok_or_else(|| EngineError::Protocol("send_turn before start_session".into()))?;
        let result = self
            .rpc(
                "turn/start",
                turn_request(&thread_id, prompt, self.reasoning_effort.as_deref()),
            )
            .await?;
        self.active_turn_id = result
            .pointer("/turn/id")
            .and_then(Value::as_str)
            .map(str::to_string);
        Ok(())
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
            // Server→client requests (approvals): decline per pinned policy,
            // surface as a question.
            if let Some(method) = msg.get("method").and_then(Value::as_str) {
                if let Some(id) = msg.get("id").cloned() {
                    let question = question_from_server_request(
                        method,
                        msg.get("params").unwrap_or(&Value::Null),
                    );
                    let process = self.process.as_mut().expect("connected");
                    process
                        .send(&json!({
                            "id": id,
                            "error": { "code": -32000, "message": "approval declined: policy is never" }
                        }))
                        .await?;
                    return Ok(Some(question));
                }
                let params = msg.get("params").cloned().unwrap_or(Value::Null);
                if method == "turn/started" {
                    self.active_turn_id = params
                        .pointer("/turn/id")
                        .and_then(Value::as_str)
                        .map(str::to_string);
                }
                let mut events = map_notification(method, &params).into_iter();
                if let Some(first) = events.next() {
                    self.buffered.extend(events);
                    return Ok(Some(first));
                }
            }
            // Responses to our own requests mid-stream and unmapped
            // notifications: skip and keep reading.
        }
    }

    async fn pause(&mut self) -> Result<(), EngineError> {
        // Soft pause: interrupt the current turn; send_turn resumes work.
        self.interrupt().await
    }

    async fn interrupt(&mut self) -> Result<(), EngineError> {
        let (Some(thread_id), Some(turn_id)) =
            (self.session_id.clone(), self.active_turn_id.clone())
        else {
            return Ok(()); // nothing in flight
        };
        self.rpc(
            "turn/interrupt",
            json!({ "threadId": thread_id, "turnId": turn_id }),
        )
        .await?;
        Ok(())
    }

    async fn cancel(&mut self) -> Result<(), EngineError> {
        if let Some(mut process) = self.process.take() {
            process.kill().await;
        }
        self.session_id = None;
        self.active_turn_id = None;
        Ok(())
    }

    fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }
}

#[cfg(test)]
mod selection_tests {
    use super::*;

    fn spec() -> SessionSpec {
        SessionSpec {
            working_dir: PathBuf::from("/tmp/repo"),
            data_dir: PathBuf::from("/tmp/data"),
            session_key: "thread-1".into(),
            model: Some("gpt-5.6-sol".into()),
            reasoning_effort: Some("high".into()),
        }
    }

    #[test]
    fn model_is_pinned_on_start_and_effort_is_pinned_on_every_turn() {
        let spec = spec();
        let (method, params) = thread_request(&spec, None);
        assert_eq!(method, "thread/start");
        assert_eq!(params["model"], "gpt-5.6-sol");
        assert_eq!(params["cwd"], "/tmp/repo");

        let resumed = thread_request(&spec, Some("thread-1"));
        assert_eq!(resumed.0, "thread/resume");
        assert_eq!(resumed.1["threadId"], "thread-1");
        assert_eq!(resumed.1["model"], "gpt-5.6-sol");

        let turn = turn_request("thread-1", "fix it", spec.reasoning_effort.as_deref());
        assert_eq!(turn["effort"], "high");
    }

    #[test]
    fn codex_catalog_preserves_only_visible_usable_models_and_native_efforts() {
        let models = parse_model_catalog(&json!({
            "data": [
                {
                    "id": "gpt-5.6-sol",
                    "model": "gpt-5.6-sol",
                    "displayName": "5.6 Sol",
                    "description": "Agentic coding",
                    "hidden": false,
                    "supportedReasoningEfforts": [
                        {"reasoningEffort": "medium", "description": "Balanced"},
                        {"reasoningEffort": "high", "description": "Deep"}
                    ],
                    "defaultReasoningEffort": "high",
                    "isDefault": true
                },
                {"id": "hidden", "model": "hidden", "hidden": true},
                {"displayName": "missing id"}
            ]
        }))
        .unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "gpt-5.6-sol");
        assert_eq!(models[0].reasoning_efforts, ["medium", "high"]);
        assert_eq!(models[0].default_reasoning_effort.as_deref(), Some("high"));
        assert!(models[0].is_default);
    }
}
