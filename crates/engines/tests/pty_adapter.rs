//! The PTY adapter against real terminal programs.
//!
//! These spawn short-lived children of the test process. Nothing here needs a
//! coding agent installed: the point is that an arbitrary program on a real
//! pseudo-terminal produces the same normalized events every other adapter
//! emits, which is what lets the rest of the product stay ignorant of
//! terminals.

use autoharness_core::EngineKind;
use autoharness_engines::adapter::{EngineAdapter, SessionSpec};
use autoharness_engines::event::EngineEvent;
use autoharness_engines::pty_adapter::PtyAdapter;

fn spec(dir: &std::path::Path) -> SessionSpec {
    SessionSpec {
        working_dir: dir.to_path_buf(),
        data_dir: dir.to_path_buf(),
        session_key: "thread-1".into(),
        model: None,
        reasoning_effort: None,
    }
}

/// Collect events until the session ends or the budget runs out. A hung agent
/// must fail the test rather than hang the suite.
async fn drain(adapter: &mut PtyAdapter, budget: std::time::Duration) -> Vec<EngineEvent> {
    let deadline = tokio::time::Instant::now() + budget;
    let mut events = Vec::new();
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return events;
        }
        match tokio::time::timeout(remaining, adapter.next_event()).await {
            Ok(Ok(Some(event))) => {
                let terminal = matches!(
                    event,
                    EngineEvent::Completed { .. } | EngineEvent::Failed { .. }
                );
                events.push(event);
                if terminal {
                    return events;
                }
            }
            Ok(Ok(None)) | Ok(Err(_)) | Err(_) => return events,
        }
    }
}

/// An agent id with no manifest is refused with a structured error naming it,
/// rather than spawning something unexpected.
#[tokio::test]
async fn an_unknown_agent_is_refused_not_guessed() {
    let dir = tempfile::tempdir().unwrap();
    let mut adapter =
        PtyAdapter::new(EngineKind::new("nonexistent-agent"), None).allow_unconfined_for_tests();

    let diagnostics = adapter.detect().await;
    assert!(!diagnostics.ready);
    assert!(
        diagnostics
            .problems
            .iter()
            .any(|problem| problem.contains("nonexistent-agent")),
        "{:?}",
        diagnostics.problems
    );

    let error = adapter.start_session(&spec(dir.path())).await.unwrap_err();
    assert!(
        error.to_string().contains("nonexistent-agent"),
        "the error names the agent: {error}"
    );
}

/// A manifest whose binary is not installed reports that, and never claims the
/// agent is ready. This is the difference between "you need to install cursor"
/// and a run that blocks with nothing to act on.
#[tokio::test]
async fn a_missing_binary_is_reported_rather_than_launched() {
    let adapter = PtyAdapter::new(EngineKind::new("cursor"), None).allow_unconfined_for_tests();
    let diagnostics = adapter.detect().await;

    if diagnostics.ready {
        // cursor really is installed on this machine; then it must be honest
        // about what a terminal agent can and cannot report.
        assert!(!diagnostics.structured_mode);
        assert_eq!(diagnostics.authenticated, None);
        return;
    }
    assert!(!diagnostics.installed);
    assert!(
        diagnostics
            .problems
            .iter()
            .any(|problem| problem.contains("not found on PATH")),
        "{:?}",
        diagnostics.problems
    );
}

/// The end-to-end claim, against a real process on a real pseudo-terminal:
/// spawn, emulate the screen, evaluate the manifest, fold it through the
/// reducer, and emit the same normalized events a structured adapter emits.
///
/// A temporary manifest is used rather than a shipped one so the test does not
/// depend on any coding agent being installed. The mechanism under test is the
/// bridge, not whether this machine has Cursor.
#[tokio::test]
async fn a_real_terminal_program_drives_the_normalized_events() {
    let manifests = tempfile::tempdir().unwrap();
    // `echo` paints a line and exits 0 — the smallest thing that exercises
    // spawn, screen, exit detection and translation.
    std::fs::write(
        manifests.path().join("probe.json"),
        r#"{
          "schemaVersion": 2,
          "id": "probe",
          "version": "1",
          "statusModel": "processOnly",
          "agent": {
            "displayName": "Probe",
            "shortLabel": "probe",
            "firstClass": false,
            "statusAuthority": "process",
            "binary": "echo"
          },
          "rules": []
        }"#,
    )
    .unwrap();

    let work = tempfile::tempdir().unwrap();
    let mut adapter = PtyAdapter::new(
        EngineKind::new("probe"),
        Some(manifests.path().to_path_buf()),
    )
    .allow_unconfined_for_tests();

    let diagnostics = adapter.detect().await;
    assert!(diagnostics.ready, "{:?}", diagnostics.problems);
    // A terminal agent never claims a fidelity it does not have.
    assert!(!diagnostics.structured_mode);

    adapter.start_session(&spec(work.path())).await.unwrap();
    assert!(
        adapter.session_id().is_some(),
        "a session identifies itself even though a terminal agent reports no id"
    );

    let events = drain(&mut adapter, std::time::Duration::from_secs(15)).await;
    assert!(
        events
            .iter()
            .any(|event| matches!(event, EngineEvent::SessionIdentity { .. })),
        "{events:?}"
    );
    // `echo` exits 0, so the run must see a completion rather than a failure.
    assert!(
        events
            .iter()
            .any(|event| matches!(event, EngineEvent::Completed { .. })),
        "a clean exit is a completed turn: {events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, EngineEvent::Failed { .. })),
        "{events:?}"
    );
}

/// A program that exits non-zero is a failed run, not a quiet completion.
/// Reporting a crashed agent as success is the one thing a harness must never
/// do.
#[tokio::test]
async fn a_nonzero_exit_is_reported_as_a_failure() {
    let manifests = tempfile::tempdir().unwrap();
    std::fs::write(
        manifests.path().join("failing.json"),
        r#"{
          "schemaVersion": 2,
          "id": "failing",
          "version": "1",
          "statusModel": "processOnly",
          "agent": {
            "displayName": "Failing",
            "shortLabel": "failing",
            "firstClass": false,
            "statusAuthority": "process",
            "binary": "false"
          },
          "rules": []
        }"#,
    )
    .unwrap();

    let work = tempfile::tempdir().unwrap();
    let mut adapter = PtyAdapter::new(
        EngineKind::new("failing"),
        Some(manifests.path().to_path_buf()),
    )
    .allow_unconfined_for_tests();
    if !adapter.detect().await.ready {
        return; // no `false` binary on this machine
    }
    adapter.start_session(&spec(work.path())).await.unwrap();

    let events = drain(&mut adapter, std::time::Duration::from_secs(15)).await;
    let failed = events
        .iter()
        .find(|event| matches!(event, EngineEvent::Failed { .. }));
    assert!(failed.is_some(), "a non-zero exit must fail: {events:?}");
    if let Some(EngineEvent::Failed { recoverable, .. }) = failed {
        assert!(!recoverable, "a dead terminal is not retryable in-run");
    }
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, EngineEvent::Completed { .. })),
        "a failed agent is never also completed: {events:?}"
    );
}

/// A PTY session cannot be reattached once its process is gone, and says so
/// rather than pretending to resume — a caller that believed it would get a
/// silently fresh agent holding none of the conversation.
#[tokio::test]
async fn resuming_a_dead_terminal_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let mut adapter =
        PtyAdapter::new(EngineKind::new("claude-code"), None).allow_unconfined_for_tests();
    let error = adapter
        .resume_session("pty-whatever", &spec(dir.path()))
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("reattached"),
        "the refusal explains itself: {error}"
    );
}

/// Every shipped manifest is reachable as an engine id. This is the property
/// the `EngineKind` refactor exists for: adding an agent is a JSON file, not a
/// code change.
#[tokio::test]
async fn every_shipped_manifest_is_a_usable_engine_id() {
    let mut checked = 0;
    for (id, _) in autoharness_pty::manifests::BUNDLED {
        let kind: EngineKind = id.parse().unwrap_or_else(|e| panic!("{id}: {e}"));
        let adapter = PtyAdapter::new(kind, None);
        // Detection must answer for every one of them without panicking,
        // whether or not the agent is installed on this machine.
        let diagnostics = adapter.detect().await;
        assert_eq!(diagnostics.engine.as_str(), *id);
        checked += 1;
    }
    assert!(
        checked >= 20,
        "expected the shipped manifests, got {checked}"
    );
}

/// The environment policy is the same for a terminal agent as for a structured
/// one: built from scratch, never inherited.
///
/// The regression this exists for is a real one this adapter shipped with for
/// an hour: it built the child's environment from `std::env::vars()` minus a
/// small scrub list. `spawn_json_lines_child` calls `env_clear()`, and
/// `FORBIDDEN_ENV_VARS` documents that as "since envs are built from scratch
/// these never appear". Inheriting would have handed every terminal agent the
/// daemon's GITHUB_TOKEN, AWS keys, SSH_AUTH_SOCK and BASH_ENV — the exact set
/// that list exists to keep out.
#[tokio::test]
async fn a_terminal_agent_never_inherits_the_daemons_secrets() {
    let manifests = tempfile::tempdir().unwrap();
    // `env` prints the environment it was given, so the child reports its own
    // environment back through the terminal.
    std::fs::write(
        manifests.path().join("envprobe.json"),
        r#"{
          "schemaVersion": 2,
          "id": "envprobe",
          "version": "1",
          "statusModel": "processOnly",
          "agent": {
            "displayName": "Env probe",
            "shortLabel": "envprobe",
            "firstClass": false,
            "statusAuthority": "process",
            "binary": "env"
          },
          "rules": []
        }"#,
    )
    .unwrap();

    // Poison the daemon's own environment with exactly the things that must
    // not reach an agent.
    unsafe {
        std::env::set_var("GITHUB_TOKEN", "ghp_leaked_secret_value");
        std::env::set_var("AWS_SECRET_ACCESS_KEY", "aws_leaked_secret_value");
        std::env::set_var("ANTHROPIC_API_KEY", "sk-ant-leaked-value");
    }

    let work = tempfile::tempdir().unwrap();
    let mut adapter = PtyAdapter::new(
        EngineKind::new("envprobe"),
        Some(manifests.path().to_path_buf()),
    )
    .allow_unconfined_for_tests();
    if !adapter.detect().await.ready {
        return; // no `env` binary on this machine
    }
    adapter.start_session(&spec(work.path())).await.unwrap();
    let events = drain(&mut adapter, std::time::Duration::from_secs(15)).await;

    let printed: String = events
        .iter()
        .filter_map(|event| match event {
            EngineEvent::TextDelta { delta } => Some(delta.clone()),
            EngineEvent::Completed {
                summary: Some(summary),
            } => Some(summary.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");

    for leaked in [
        "ghp_leaked_secret_value",
        "aws_leaked_secret_value",
        "sk-ant-leaked-value",
    ] {
        assert!(
            !printed.contains(leaked),
            "the agent was handed {leaked}:\n{printed}"
        );
    }
    // And it did get the terminal contract it needs.
    assert!(
        printed.contains("TERM=xterm-256color"),
        "the agent must believe it has a colour terminal:\n{printed}"
    );
}

/// The production registry knows every agent a manifest describes, and adding
/// one is a JSON file rather than a code change.
///
/// The built-ins keep their hand-written adapters: a structured protocol
/// reports what an agent did instead of painting it, and shadowing that with a
/// screen reader would be a downgrade.
#[tokio::test]
async fn the_production_registry_covers_every_manifest_agent() {
    let registry = autoharness_engines::EngineRegistry::production();
    let kinds = registry.kinds();

    for builtin in EngineKind::builtins() {
        assert!(kinds.contains(&builtin), "{builtin} must stay registered");
    }
    // Terminal-only agents nobody wrote code for.
    for id in ["cursor", "gemini", "aider", "opencode"] {
        let kind = EngineKind::new(id);
        assert!(kinds.contains(&kind), "{id} must be a usable engine");
        let adapter = registry.create(kind).expect("adapter");
        assert_eq!(adapter.kind().as_str(), id);
    }
    // Every bundled manifest is reachable EXCEPT the two kinds that must not
    // be offered: one that names a built-in (a second row for one agent) and
    // one with no binary (a row that could never run).
    let (engine, _) = autoharness_pty::manifests::load(None);
    for id in engine.ids() {
        let kind = EngineKind::new(id);
        let descriptor = engine.manifest(id).and_then(|m| m.agent.as_ref());
        let names_builtin = kind.is_builtin()
            || descriptor.is_some_and(|d| {
                d.aliases
                    .iter()
                    .any(|alias| EngineKind::new(alias).is_builtin())
            });
        let launchable = descriptor.is_some_and(|d| d.binary.is_some());
        // A built-in id stays registered under its own structured adapter. An
        // id that merely ALIASES a built-in (claude-code) does not, because
        // that would be a second row for the same agent.
        let expected = kind.is_builtin() || (launchable && !names_builtin);
        assert_eq!(
            kinds.contains(&kind),
            expected,
            "{id}: launchable={launchable} names_builtin={names_builtin}"
        );
    }
}

/// A blocked agent can be answered with the keystrokes ITS manifest declares.
///
/// "1" means yes to Claude Code and "y" means yes to aider; guessing wrong is a
/// keystroke sent to an agent that is waiting. An agent that declares no way to
/// refuse says so rather than silently doing nothing — a run stuck on an
/// unanswerable prompt is indistinguishable from a hang.
#[tokio::test]
async fn answers_use_the_keystrokes_the_manifest_declares() {
    use autoharness_core::Answer;

    let manifests = tempfile::tempdir().unwrap();
    std::fs::write(
        manifests.path().join("asker.json"),
        r#"{
          "schemaVersion": 2,
          "id": "asker",
          "version": "1",
          "statusModel": "processOnly",
          "agent": {
            "displayName": "Asker",
            "shortLabel": "asker",
            "firstClass": false,
            "statusAuthority": "process",
            "binary": "cat",
            "approve": { "text": "1", "submit": true },
            "deny": { "text": "n", "submit": true }
          },
          "rules": []
        }"#,
    )
    .unwrap();

    let work = tempfile::tempdir().unwrap();
    let mut adapter = PtyAdapter::new(
        EngineKind::new("asker"),
        Some(manifests.path().to_path_buf()),
    )
    .allow_unconfined_for_tests();
    if !adapter.detect().await.ready {
        return; // no `cat` on this machine
    }
    adapter.start_session(&spec(work.path())).await.unwrap();

    // `cat` echoes, so what the agent received comes back on the terminal.
    adapter.answer(Answer::Approve).await.unwrap();
    adapter.answer(Answer::Deny).await.unwrap();
    adapter
        .answer(Answer::Text("free form".into()))
        .await
        .unwrap();

    let mut seen = String::new();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(8);
    while tokio::time::Instant::now() < deadline && !seen.contains("free form") {
        match tokio::time::timeout(std::time::Duration::from_millis(400), adapter.next_event())
            .await
        {
            Ok(Ok(Some(EngineEvent::TextDelta { delta }))) => seen.push_str(&delta),
            Ok(Ok(Some(_))) => {}
            _ => break,
        }
    }
    let _ = adapter.cancel().await;

    assert!(
        seen.contains('1'),
        "the approve keystroke reached it: {seen:?}"
    );
    assert!(
        seen.contains('n'),
        "the deny keystroke reached it: {seen:?}"
    );
    assert!(seen.contains("free form"), "free text reached it: {seen:?}");
}

/// An agent with no declared refusal says so instead of quietly doing nothing.
#[tokio::test]
async fn an_agent_that_cannot_refuse_reports_it_rather_than_no_opping() {
    use autoharness_core::Answer;

    let manifests = tempfile::tempdir().unwrap();
    std::fs::write(
        manifests.path().join("yesonly.json"),
        r#"{
          "schemaVersion": 2,
          "id": "yesonly",
          "version": "1",
          "statusModel": "processOnly",
          "agent": {
            "displayName": "Yes only",
            "shortLabel": "yesonly",
            "firstClass": false,
            "statusAuthority": "process",
            "binary": "cat",
            "approve": { "text": "1", "submit": true }
          },
          "rules": []
        }"#,
    )
    .unwrap();

    let work = tempfile::tempdir().unwrap();
    let mut adapter = PtyAdapter::new(
        EngineKind::new("yesonly"),
        Some(manifests.path().to_path_buf()),
    )
    .allow_unconfined_for_tests();
    if !adapter.detect().await.ready {
        return;
    }
    adapter.start_session(&spec(work.path())).await.unwrap();

    assert!(adapter.answer(Answer::Approve).await.is_ok());
    let error = adapter.answer(Answer::Deny).await.unwrap_err();
    assert!(error.to_string().contains("deny"), "{error}");
    let _ = adapter.cancel().await;
}

/// A terminal agent with no confinement is REFUSED, never run loose.
///
/// It is the least trusted process in the product: somebody else's CLI, driven
/// by a model, with a real terminal. Every daemon-owned command goes through
/// Seatbelt; an interactive agent is not an exception to that, and running it
/// unconfined would be worse than not running it.
#[tokio::test]
async fn an_unconfined_terminal_agent_is_refused() {
    let manifests = tempfile::tempdir().unwrap();
    std::fs::write(
        manifests.path().join("loose.json"),
        r#"{
          "schemaVersion": 2,
          "id": "loose",
          "version": "1",
          "statusModel": "processOnly",
          "agent": {
            "displayName": "Loose",
            "shortLabel": "loose",
            "firstClass": false,
            "statusAuthority": "process",
            "binary": "echo"
          },
          "rules": []
        }"#,
    )
    .unwrap();

    let work = tempfile::tempdir().unwrap();
    // No confinement, and NOT opted out of the requirement.
    let mut adapter = PtyAdapter::new(
        EngineKind::new("loose"),
        Some(manifests.path().to_path_buf()),
    );
    let error = adapter.start_session(&spec(work.path())).await.unwrap_err();
    assert!(
        error.to_string().contains("confinement"),
        "the refusal names the reason: {error}"
    );
}
