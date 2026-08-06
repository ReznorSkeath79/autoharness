//! Contract equivalence: for each canonical scenario, the Claude stream-json
//! mapping, the Codex app-server mapping, and a FakeEngine script produce
//! the SAME normalized event sequence.
//!
//! Real-CLI equivalence can't be proven by live turns in CI (they cost
//! tokens), so equivalence is proven structurally: fixtures with the exact
//! shapes each provider emits (Codex shapes taken from
//! `codex app-server generate-json-schema`; Claude shapes from the
//! stream-json format) are mapped and compared against the canonical events.

use autoharness_core::EngineKind;
use autoharness_engines::event::EngineEvent;
use autoharness_engines::{EngineAdapter, FakeEngine, SessionSpec};
use serde_json::{Value, json};

struct Scenario {
    name: &'static str,
    expected: Vec<EngineEvent>,
    claude: Vec<Value>,
    codex: Vec<(&'static str, Value)>,
}

fn text(s: &str) -> EngineEvent {
    EngineEvent::Text { text: s.into() }
}

fn scenarios() -> Vec<Scenario> {
    use autoharness_engines::{FileChangeKind, ToolStatus};
    vec![
        Scenario {
            name: "session_identity",
            expected: vec![EngineEvent::SessionIdentity {
                session_id: "sess-123".into(),
            }],
            claude: vec![json!({"type":"system","subtype":"init","session_id":"sess-123"})],
            codex: vec![("thread/started", json!({"thread":{"id":"sess-123"}}))],
        },
        Scenario {
            name: "text_reply",
            expected: vec![
                text("Hello world"),
                EngineEvent::Completed { summary: None },
            ],
            claude: vec![
                json!({"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Hello world"}]}}),
                json!({"type":"result","subtype":"success","is_error":false,"session_id":"sess-123"}),
            ],
            codex: vec![
                (
                    "item/completed",
                    json!({"threadId":"sess-123","turnId":"t1","completedAtMs":1,"item":{"type":"agentMessage","id":"i1","text":"Hello world"}}),
                ),
                (
                    "turn/completed",
                    json!({"threadId":"sess-123","turn":{"id":"t1","items":[],"status":"completed"}}),
                ),
            ],
        },
        Scenario {
            name: "tool_use",
            expected: vec![
                EngineEvent::ToolActivity {
                    name: "shell".into(),
                    status: ToolStatus::Started,
                    detail: json!({ "command": "cargo test" }),
                },
                EngineEvent::ToolActivity {
                    name: "shell".into(),
                    status: ToolStatus::Finished,
                    detail: json!({ "success": true }),
                },
            ],
            claude: vec![
                json!({"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","name":"Bash","input":{"command":"cargo test"}}]}}),
                json!({"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_name":"Bash","is_error":false}]}}),
            ],
            codex: vec![
                (
                    "item/started",
                    json!({"threadId":"sess-123","turnId":"t1","startedAtMs":1,"item":{"type":"commandExecution","id":"c1","command":"cargo test","commandActions":[],"cwd":"/tmp","status":"inProgress"}}),
                ),
                (
                    "item/completed",
                    json!({"threadId":"sess-123","turnId":"t1","completedAtMs":2,"item":{"type":"commandExecution","id":"c1","command":"cargo test","commandActions":[],"cwd":"/tmp","status":"completed","exitCode":0}}),
                ),
            ],
        },
        Scenario {
            name: "file_change",
            expected: vec![EngineEvent::FileChange {
                path: "src/main.rs".into(),
                kind: FileChangeKind::Modified,
            }],
            claude: vec![
                json!({"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","name":"Edit","input":{"file_path":"src/main.rs","old_string":"a","new_string":"b"}}]}}),
            ],
            codex: vec![(
                "item/completed",
                json!({"threadId":"sess-123","turnId":"t1","completedAtMs":2,"item":{"type":"fileChange","id":"f1","status":"completed","changes":[{"path":"src/main.rs","kind":{"type":"update"},"diff":"@@"}]}}),
            )],
        },
        Scenario {
            name: "question",
            expected: vec![EngineEvent::Question {
                id: "q-1".into(),
                prompt: "approve tool: shell".into(),
            }],
            claude: vec![
                json!({"type":"control_request","request_id":"q-1","request":{"subtype":"can_use_tool","tool_name":"Bash"}}),
            ],
            // Codex approvals are server REQUESTS, not notifications.
            codex: vec![(
                "item/commandExecution/requestApproval",
                json!({"itemId":"q-1","command":["cargo","test"]}),
            )],
        },
        Scenario {
            name: "usage_and_completion",
            expected: vec![
                EngineEvent::Usage {
                    input_tokens: 1000,
                    output_tokens: 250,
                },
                EngineEvent::Completed { summary: None },
            ],
            claude: vec![
                json!({"type":"result","subtype":"success","is_error":false,"session_id":"sess-123","usage":{"input_tokens":1000,"output_tokens":250}}),
            ],
            codex: vec![
                (
                    "thread/tokenUsage/updated",
                    json!({"threadId":"sess-123","turnId":"t1","tokenUsage":{"total":{"inputTokens":1000,"outputTokens":250,"cachedInputTokens":0,"reasoningOutputTokens":0,"totalTokens":1250},"last":{"inputTokens":1000,"outputTokens":250,"cachedInputTokens":0,"reasoningOutputTokens":0,"totalTokens":1250}}}),
                ),
                (
                    "turn/completed",
                    json!({"threadId":"sess-123","turn":{"id":"t1","items":[],"status":"completed"}}),
                ),
            ],
        },
        Scenario {
            name: "failure",
            expected: vec![EngineEvent::Failed {
                message: "boom".into(),
                recoverable: false,
            }],
            claude: vec![
                json!({"type":"result","subtype":"success","is_error":true,"result":"boom","session_id":"sess-123"}),
            ],
            codex: vec![(
                "error",
                json!({"threadId":"sess-123","turnId":"t1","willRetry":false,"error":{"message":"boom"}}),
            )],
        },
    ]
}

fn map_claude(lines: &[Value]) -> Vec<EngineEvent> {
    lines
        .iter()
        .flat_map(autoharness_engines::claude::map_message)
        .collect()
}

fn map_codex(notifications: &[(&str, Value)]) -> Vec<EngineEvent> {
    notifications
        .iter()
        .flat_map(|(method, params)| {
            if method.contains("requestApproval") || method.contains("Approval") {
                vec![autoharness_engines::codex::question_from_server_request(
                    method, params,
                )]
            } else {
                autoharness_engines::codex::map_notification(method, params)
            }
        })
        .collect()
}

#[test]
fn codex_and_claude_map_equivalently_for_every_scenario() {
    for scenario in scenarios() {
        let claude = map_claude(&scenario.claude);
        let codex = map_codex(&scenario.codex);
        assert_eq!(
            claude, scenario.expected,
            "scenario {}: claude mapping diverged",
            scenario.name
        );
        assert_eq!(
            codex, scenario.expected,
            "scenario {}: codex mapping diverged",
            scenario.name
        );
    }
}

#[tokio::test]
async fn fake_engine_emits_the_same_scenarios() {
    for scenario in scenarios() {
        let mut fake = FakeEngine::scripted(EngineKind::codex(), vec![scenario.expected.clone()]);
        fake.start_session(&SessionSpec {
            working_dir: std::env::temp_dir(),
            data_dir: std::env::temp_dir(),
            session_key: "thread-1".into(),
            model: None,
            reasoning_effort: None,
        })
        .await
        .unwrap();
        // Drain the identity event queued by start_session.
        fake.next_event().await.unwrap();
        fake.send_turn("scenario").await.unwrap();

        let mut emitted = Vec::new();
        while let Some(event) = fake.next_event().await.unwrap() {
            emitted.push(event);
        }
        assert_eq!(
            emitted, scenario.expected,
            "scenario {}: fake engine diverged",
            scenario.name
        );
    }
}

#[test]
fn unknown_provider_messages_map_to_nothing() {
    assert!(autoharness_engines::claude::map_message(&json!({"type":"keepalive"})).is_empty());
    assert!(autoharness_engines::codex::map_notification("skills/changed", &json!({})).is_empty());
    assert!(autoharness_engines::claude::map_message(&json!({"garbage": true})).is_empty());
    assert!(autoharness_engines::codex::map_notification("", &json!(null)).is_empty());
}
