//! The user logs into Codex and Claude once, on their own machine. AutoHarness
//! must reuse that login without asking again — these tests prove the sanitized
//! engine environment does not hide it.
//!
//! **Differential, not absolute.** Each test asks the CLI whether it is logged
//! in twice: once with the user's inherited environment, once through
//! `engine_control_env_for` (exactly what a real session gets). A logged-out
//! machine answers "no" both times and the test passes. It fails only when the
//! user IS logged in and OUR environment is what broke it — the regression
//! that made `claude` report `loggedIn: false` when `USER`/`LOGNAME` were
//! scrubbed, so it could not reach its Keychain credentials.
//!
//! Token-free: `codex login status` and `claude auth status` never contact a
//! model. Auth output can carry an account email and organization, so only
//! booleans are asserted on and nothing is printed.

use std::path::Path;

use autoharness_engines::probe;
use autoharness_engines::process::find_binary;
use autoharness_engines::{CodexAdapter, EngineAdapter};

/// Ask the CLI for its auth state with the user's own environment, the way
/// they would from a terminal.
async fn logged_in_with_user_env(
    binary: &Path,
    args: &[&str],
    verdict: impl Fn(&str) -> Option<bool>,
) -> Option<bool> {
    let mut command = tokio::process::Command::new(binary);
    command.args(args);
    // The outer test process may deliberately override HOME to model a clean
    // AutoHarness profile. This baseline represents the signed-in macOS user,
    // so compare against that account's real home rather than the fixture.
    if let Some(home) = autoharness_engines::process::account_home() {
        command.env("HOME", home);
    }
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        command
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output(),
    )
    .await;
    let Ok(Ok(output)) = output else {
        return None;
    };
    // Exit status is ignored on purpose (`claude auth status` exits 1 while
    // reporting a logged-in account) and both streams are read (`codex login
    // status` prints its verdict to stderr).
    verdict(&probe::combined_output(&output))
}

#[tokio::test]
async fn codex_saved_login_survives_the_sanitized_engine_environment() {
    let diagnostics = CodexAdapter::new().detect().await;
    let Some(binary) = diagnostics.binary_path else {
        eprintln!("codex not installed; skipping");
        return;
    };
    let user_env =
        logged_in_with_user_env(&binary, &["login", "status"], probe::codex_login_verdict).await;
    let sandboxed = probe::codex_authenticated(&binary).await;
    assert!(
        user_env.is_some(),
        "codex is installed but `codex login status` gave no readable verdict — \
         the probe can no longer tell whether the user is logged in"
    );
    assert!(
        sandboxed.is_some(),
        "the sandboxed probe must reach codex too"
    );

    if user_env == Some(true) {
        assert_eq!(
            sandboxed,
            Some(true),
            "codex is logged in for the user but NOT through the engine \
             environment — AutoHarness would ask them to log in again. \
             CODEX_HOME must point at the real ~/.codex."
        );
    }
    // Never invent a login that the user does not have.
    if user_env == Some(false) {
        assert_ne!(sandboxed, Some(true));
    }
}

#[tokio::test]
async fn claude_saved_login_survives_the_sanitized_engine_environment() {
    let Some(binary) = find_binary("claude") else {
        eprintln!("claude not installed; skipping");
        return;
    };
    let user_env =
        logged_in_with_user_env(&binary, &["auth", "status"], probe::claude_login_verdict).await;
    let sandboxed = probe::claude_authenticated(&binary).await;
    assert!(
        user_env.is_some(),
        "claude is installed but `claude auth status` gave no readable verdict — \
         the probe can no longer tell whether the user is logged in"
    );
    assert!(
        sandboxed.is_some(),
        "the sandboxed probe must reach claude too"
    );

    if user_env == Some(true) {
        assert_eq!(
            sandboxed,
            Some(true),
            "claude is logged in for the user but NOT through the engine \
             environment — AutoHarness would ask them to log in again. \
             USER/LOGNAME must survive env_clear so the CLI can reach the \
             macOS Keychain."
        );
    }
    if user_env == Some(false) {
        assert_ne!(sandboxed, Some(true));
    }
}

/// The engine environment stays sanitized while carrying the login: the
/// account name is an identity, not a credential, and nothing else leaks.
#[tokio::test]
async fn the_auth_carrying_environment_is_still_sanitized() {
    let root = tempfile::tempdir().unwrap();
    let dirs =
        autoharness_engines::process::SessionDirs::create(root.path(), "auth-env-test").unwrap();
    let env = autoharness_engines::process::engine_control_env(&dirs);
    let get = |key: &str| env.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone());

    // No secret ever rides along, including an API key in the daemon's own
    // environment.
    for forbidden in autoharness_engines::process::FORBIDDEN_ENV_VARS {
        if *forbidden == "SSH_AUTH_SOCK" {
            assert_eq!(get(forbidden).as_deref(), Some(""));
        } else {
            assert!(
                get(forbidden).is_none(),
                "{forbidden} must not be forwarded"
            );
        }
    }
    // HOME is still the fake one; the login does not come from there.
    assert_eq!(get("HOME").as_deref(), Some(&*dirs.home.to_string_lossy()));
    // CLAUDE_CONFIG_DIR is deliberately unset: it relocates the expected
    // .claude.json and makes the CLI print a "config not found" banner.
    assert!(get("CLAUDE_CONFIG_DIR").is_none());

    if let Some(user) = std::env::var_os("USER") {
        assert_eq!(
            get("USER"),
            Some(user.to_string_lossy().into_owned()),
            "USER must survive env_clear or claude cannot reach the Keychain"
        );
    }
    if std::env::home_dir().is_some_and(|h| h.join(".codex").is_dir()) {
        assert!(
            get("CODEX_HOME").is_some_and(|v| v.ends_with(".codex")),
            "CODEX_HOME must point at the user's real codex home"
        );
    }
}

/// A fake HOME must not make an engine behave like a first launch: the user's
/// `.claude.json` is copied in, so onboarding state carries over. It is a copy,
/// so a run can never mutate the user's own configuration.
#[tokio::test]
async fn engine_session_home_is_seeded_without_touching_the_users_config() {
    let root = tempfile::tempdir().unwrap();
    let dirs =
        autoharness_engines::process::SessionDirs::create_for_engine(root.path(), "seed-test")
            .unwrap();

    let Some(real) = std::env::home_dir().map(|h| h.join(".claude.json")) else {
        return;
    };
    if !real.is_file() {
        eprintln!("no ~/.claude.json on this machine; skipping");
        return;
    }
    let seeded = dirs.home.join(".claude.json");
    assert!(seeded.is_file(), "session HOME must carry .claude.json");
    assert_eq!(
        std::fs::metadata(&seeded).unwrap().len(),
        std::fs::metadata(&real).unwrap().len(),
    );

    // A copy, not a link: writing in the session must not reach the user.
    assert!(!std::fs::symlink_metadata(&seeded).unwrap().is_symlink());
    let before = std::fs::metadata(&real).unwrap().len();
    std::fs::write(&seeded, b"{}").unwrap();
    assert_eq!(std::fs::metadata(&real).unwrap().len(), before);
}

/// Worker session homes stay bare. Only ENGINE sessions get the user's config
/// and the login keychain; a daemon-owned build/test/git command gets neither.
#[tokio::test]
async fn worker_session_home_carries_no_user_state() {
    let root = tempfile::tempdir().unwrap();
    let worker = autoharness_engines::process::SessionDirs::create(root.path(), "worker").unwrap();
    assert!(!worker.home.join(".claude.json").exists());
    assert!(!worker.home.join("Library/Keychains").exists());
    // The pinned-empty gitconfig is the only thing seeded.
    assert!(worker.home.join(".gitconfig").is_file());

    let engine =
        autoharness_engines::process::SessionDirs::create_for_engine(root.path(), "engine")
            .unwrap();
    if autoharness_engines::process::account_home()
        .is_some_and(|h| h.join("Library/Keychains").is_dir())
    {
        assert!(
            engine.home.join("Library/Keychains").is_symlink(),
            "engine sessions must link the login keychain so saved logins resolve"
        );
    }
}
