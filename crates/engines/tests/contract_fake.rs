//! The full adapter contract suite, exercised against FakeEngine.

use autoharness_core::EngineKind;
use autoharness_engines::event::EngineEvent;
use autoharness_engines::{EngineAdapter, FakeEngine, SessionSpec, contract};

fn spec() -> SessionSpec {
    SessionSpec {
        working_dir: std::env::temp_dir(),
        data_dir: std::env::temp_dir(),
        session_key: "thread-1".into(),
        model: None,
        reasoning_effort: None,
    }
}

#[tokio::test]
async fn fake_engine_passes_full_contract() {
    let turns = vec![
        vec![
            EngineEvent::Text {
                text: "Hello world".into(),
            },
            EngineEvent::Completed { summary: None },
        ],
        vec![
            EngineEvent::ToolActivity {
                name: "shell".into(),
                status: autoharness_engines::ToolStatus::Started,
                detail: serde_json::json!({ "command": "cargo test" }),
            },
            EngineEvent::Usage {
                input_tokens: 10,
                output_tokens: 5,
            },
            EngineEvent::Completed { summary: None },
        ],
    ];
    let mut fake = FakeEngine::scripted(EngineKind::codex(), turns.clone());
    contract::run_adapter_contract(&mut fake, &std::env::temp_dir(), &turns)
        .await
        .expect("fake engine must satisfy the adapter contract");
}

#[tokio::test]
async fn contract_fails_cleanly_on_unavailable_engine() {
    let mut fake = FakeEngine::unavailable(EngineKind::claude(), vec!["no auth".into()]);
    let result = contract::run_adapter_contract(&mut fake, &std::env::temp_dir(), &[]).await;
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("not ready"));
}

#[tokio::test]
async fn fake_engine_interrupt_and_cancel_semantics() {
    let mut fake = FakeEngine::scripted(
        EngineKind::codex(),
        vec![vec![
            EngineEvent::Text { text: "a".into() },
            EngineEvent::Text { text: "b".into() },
            EngineEvent::Completed { summary: None },
        ]],
    );
    fake.start_session(&spec()).await.unwrap();
    // Drain identity.
    assert!(matches!(
        fake.next_event().await.unwrap(),
        Some(EngineEvent::SessionIdentity { .. })
    ));

    fake.send_turn("work").await.unwrap();
    assert!(matches!(
        fake.next_event().await.unwrap(),
        Some(EngineEvent::Text { .. })
    ));
    // Interrupt drops the remainder of the turn.
    fake.interrupt().await.unwrap();
    assert_eq!(fake.pending_len(), 0);
    assert!(fake.next_event().await.unwrap().is_none());

    // A new turn works after interrupt (default script).
    fake.send_turn("again").await.unwrap();
    assert!(matches!(
        fake.next_event().await.unwrap(),
        Some(EngineEvent::Text { .. })
    ));

    // Cancel ends the session permanently.
    fake.cancel().await.unwrap();
    assert!(fake.next_event().await.unwrap().is_none());
    assert!(fake.send_turn("nope").await.is_err());
}

#[tokio::test]
async fn fake_engine_resume_keeps_session_id() {
    let mut fake = FakeEngine::scripted(EngineKind::claude(), vec![]);
    fake.start_session(&spec()).await.unwrap();
    assert_eq!(fake.session_id(), Some("fake-session-0001"));
    fake.resume_session("persisted-42", &spec()).await.unwrap();
    assert_eq!(fake.session_id(), Some("persisted-42"));
    assert!(matches!(
        fake.next_event().await.unwrap(),
        Some(EngineEvent::SessionIdentity { session_id }) if session_id == "persisted-42"
    ));
}
