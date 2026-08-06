//! Detection against the real CLIs. These tests are presence-agnostic: they
//! pass whether or not codex/claude are installed, and never spend tokens
//! (version/help/auth-existence probes only).

use autoharness_engines::probe;
use autoharness_engines::process::find_binary;
use autoharness_engines::{ClaudeAdapter, CodexAdapter, EngineAdapter, EngineRegistry};

#[tokio::test]
async fn codex_detection_is_consistent_with_installation() {
    let diagnostics = CodexAdapter::new().detect().await;
    let installed = find_binary("codex").is_some();
    assert_eq!(diagnostics.installed, installed);
    if installed {
        assert!(diagnostics.binary_path.is_some());
        assert!(diagnostics.version.is_some(), "version probe must parse");
        assert!(
            diagnostics.structured_mode,
            "codex app-server must be detected as available"
        );
    } else {
        assert!(!diagnostics.ready);
        assert!(!diagnostics.problems.is_empty());
    }
    // Readiness is exactly the conjunction of the checks.
    assert_eq!(
        diagnostics.ready,
        diagnostics.installed
            && diagnostics.version.is_some()
            && diagnostics.authenticated != Some(false)
            && diagnostics.structured_mode
    );
}

/// A user's config can be written by the Codex bundled with ChatGPT while an
/// older Homebrew CLI remains earlier on PATH. The older executable is
/// installed, but it is not usable if it cannot parse that config. Detection
/// and real sessions must agree on the first candidate that can actually load
/// the saved login/config state.
#[tokio::test]
async fn codex_detection_skips_a_path_binary_that_cannot_load_user_config() {
    let path_binary = std::path::PathBuf::from("/opt/homebrew/bin/codex");
    let bundled_binary =
        std::path::PathBuf::from("/Applications/ChatGPT.app/Contents/Resources/codex");
    if !path_binary.is_file() || !bundled_binary.is_file() {
        eprintln!("both Codex candidates are not installed; skipping differential test");
        return;
    }

    let path_auth = probe::codex_authenticated(&path_binary).await;
    let bundled_auth = probe::codex_authenticated(&bundled_binary).await;
    if path_auth.is_some() || bundled_auth.is_none() {
        eprintln!("the installed candidates do not reproduce the compatibility split; skipping");
        return;
    }

    let diagnostics = CodexAdapter::new().detect().await;
    assert_eq!(
        diagnostics.binary_path.as_deref(),
        Some(bundled_binary.as_path()),
        "AutoHarness must skip the earlier PATH binary when it cannot load the user's config"
    );
}

#[tokio::test]
async fn claude_detection_is_consistent_with_installation() {
    let diagnostics = ClaudeAdapter::new().detect().await;
    let installed = find_binary("claude").is_some();
    assert_eq!(diagnostics.installed, installed);
    if installed {
        assert!(diagnostics.binary_path.is_some());
        assert!(diagnostics.version.is_some(), "version probe must parse");
        assert!(
            diagnostics.structured_mode,
            "claude stream-json flags must be detected as available"
        );
    } else {
        assert!(!diagnostics.ready);
        assert!(!diagnostics.problems.is_empty());
    }
    assert_eq!(
        diagnostics.ready,
        diagnostics.installed
            && diagnostics.version.is_some()
            && diagnostics.authenticated != Some(false)
            && diagnostics.structured_mode
    );
}

#[tokio::test]
async fn production_registry_detects_without_panicking() {
    let results = EngineRegistry::production().detect_all().await;
    // Was exactly 2. The registry now also carries every manifest-described
    // agent, which is the point: adding one is a JSON file.
    assert!(results.len() > 2, "{} engines", results.len());
    for d in &results {
        // Never a panic, always a structured verdict.
        assert_eq!(d.ready, d.problems.is_empty());
    }
}

/// One agent, one entry. A manifest that names a built-in by id or alias
/// describes an agent that already has a structured adapter, and offering both
/// would put two rows for the same thing in front of the user — with the worse
/// one indistinguishable from the better.
#[tokio::test]
async fn an_agent_is_never_registered_twice_under_two_ids() {
    let kinds = EngineRegistry::production().kinds();
    assert!(
        !kinds.iter().any(|kind| kind.as_str() == "claude-code"),
        "claude-code is claude: {kinds:?}"
    );
    assert!(kinds.iter().any(|kind| kind.is_claude()));
    assert!(kinds.iter().any(|kind| kind.is_codex()));
}

/// An engine with nothing to launch is not offered. `shell` and `generic` take
/// their command from the caller, so they would be permanently unusable rows.
#[tokio::test]
async fn engines_with_no_binary_are_not_offered() {
    let kinds = EngineRegistry::production().kinds();
    for unusable in ["shell", "generic"] {
        assert!(
            !kinds.iter().any(|kind| kind.as_str() == unusable),
            "{unusable} has no binary: {kinds:?}"
        );
    }
}

#[tokio::test]
async fn missing_binary_adapter_reports_not_installed() {
    let adapter = CodexAdapter::new().with_binary(std::path::PathBuf::from("/nonexistent/codex"));
    let diagnostics = adapter.detect().await;
    // A bogus path is "installed" (path given) but nothing probes successfully.
    assert!(!diagnostics.ready);
    assert!(diagnostics.version.is_none());
    assert!(!diagnostics.structured_mode);
}
