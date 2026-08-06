//! Detection probes shared by the real adapters. Every probe is cheap,
//! read-only, and token-free: `--version`, `--help`, and credential
//! file/keychain existence checks only.

use std::path::Path;

use crate::process::{SessionDirs, apply_isolation, engine_control_env_for};

/// Run `program args`, parse a version string from stdout with `extract`.
/// Returns `None` on spawn failure, non-zero exit, or parse failure.
pub async fn version(
    program: &Path,
    args: &[&str],
    extract: impl Fn(&str) -> Option<String>,
) -> Option<String> {
    let output = tokio::process::Command::new(program)
        .args(args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    extract(stdout.trim())
}

/// Run `program args` and report whether it exited successfully.
/// Used for structured-mode availability (`app-server --help`, `--help`
/// containing stream-json). Bounded by a timeout so a hung probe can never
/// wedge detection.
pub async fn succeeds(program: &Path, args: &[&str]) -> bool {
    let run = tokio::process::Command::new(program)
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    matches!(
        tokio::time::timeout(std::time::Duration::from_secs(10), run).await,
        Ok(Ok(status)) if status.success()
    )
}

/// Run `program args` and report whether stdout contains `needle`.
pub async fn help_mentions(program: &Path, args: &[&str], needle: &str) -> bool {
    let run = tokio::process::Command::new(program)
        .args(args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output();
    match tokio::time::timeout(std::time::Duration::from_secs(10), run).await {
        Ok(Ok(output)) => {
            output.status.success() && String::from_utf8_lossy(&output.stdout).contains(needle)
        }
        _ => false,
    }
}

/// Ask the CLI itself whether it is logged in, **through the exact sanitized
/// environment a real session will get**.
///
/// Checking the user's home directory or the Keychain from the daemon's own
/// environment is not good enough: the daemon has the user's full environment
/// and the engine does not, so a file-existence check can report "logged in"
/// for a session that will fail. This runs the engine's own token-free status
/// command under [`engine_control_env_for`], which is the only check that
/// cannot disagree with reality.
///
/// `verdict` inspects stdout and returns `None` when it cannot tell. The
/// output may contain the account email and organization, so only the returned
/// bool ever leaves this function — the text is never logged, stored, or
/// surfaced.
///
/// Two CLI quirks make the obvious implementation wrong, both verified live:
/// `claude auth status` exits **1** while printing `"loggedIn": true`, and
/// `codex login status` prints its verdict to **stderr**. So exit status is
/// ignored and the verdict sees stdout and stderr together.
pub async fn auth_via_control_env(
    binary: &Path,
    args: &[&str],
    verdict: impl Fn(&str) -> Option<bool>,
) -> Option<bool> {
    // A fresh HOME per probe. A shared one races: concurrent detection of two
    // engines would re-seed `.claude.json` underneath a CLI that is reading
    // it, and a half-written config reads as "logged out".
    let probe_root = std::env::temp_dir()
        .join("dev.autoharness.app-detect")
        .join(autoharness_core::new_id());
    let dirs = SessionDirs::create_for_engine(&probe_root, "auth-probe").ok()?;
    let env = engine_control_env_for(&dirs, binary);

    let mut command = tokio::process::Command::new(binary);
    command
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    apply_isolation(&mut command, &env);
    let output = tokio::time::timeout(std::time::Duration::from_secs(30), command.output()).await;
    // Detection must not leave litter behind; it runs on every `engine.list`.
    let _ = std::fs::remove_dir_all(&probe_root);

    let Ok(Ok(output)) = output else {
        // Unreachable CLI: unknown, not "unauthenticated".
        return None;
    };
    verdict(&combined_output(&output))
}

/// stdout followed by stderr — engines disagree about which one carries the
/// verdict, and neither stream is meaningful on its own here.
pub fn combined_output(output: &std::process::Output) -> String {
    format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// Codex: `codex login status` prints "Logged in using …" or "Not logged in".
pub fn codex_login_verdict(out: &str) -> Option<bool> {
    let out = out.to_lowercase();
    if out.contains("not logged in") {
        Some(false)
    } else if out.contains("logged in") {
        Some(true)
    } else {
        None
    }
}

/// Claude: `claude auth status` prints JSON carrying `"loggedIn": <bool>`.
pub fn claude_login_verdict(out: &str) -> Option<bool> {
    serde_json::from_str::<serde_json::Value>(out.trim())
        .ok()
        .and_then(|v| v.get("loggedIn").and_then(serde_json::Value::as_bool))
}

pub async fn codex_authenticated(binary: &Path) -> Option<bool> {
    auth_via_control_env(binary, &["login", "status"], codex_login_verdict).await
}

/// An `ANTHROPIC_API_KEY` in the daemon's environment is never forwarded to
/// engines, so it is not treated as authentication here either.
pub async fn claude_authenticated(binary: &Path) -> Option<bool> {
    auth_via_control_env(binary, &["auth", "status"], claude_login_verdict).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn version_probe_parses_and_handles_missing_binary() {
        // /bin/echo echoes its args; extract treats them as the version.
        let v = version(Path::new("/bin/echo"), &["1.2.3"], |out| {
            Some(out.to_string())
        })
        .await;
        assert_eq!(v.as_deref(), Some("1.2.3"));

        let missing = version(
            Path::new("/nonexistent/binary-xyz"),
            &["--version"],
            |out| Some(out.to_string()),
        )
        .await;
        assert!(missing.is_none());
    }

    #[tokio::test]
    async fn succeeds_probe() {
        assert!(succeeds(Path::new("/usr/bin/true"), &[]).await);
        assert!(!succeeds(Path::new("/usr/bin/false"), &[]).await);
        assert!(!succeeds(Path::new("/nonexistent/binary-xyz"), &[]).await);
    }
}
