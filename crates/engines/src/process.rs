//! Shared subprocess plumbing and environment isolation for engine control
//! processes and daemon-owned workers.
//!
//! **Phase 3 wrap point**: every engine process in the product is spawned
//! through [`spawn_json_lines_child`], and every environment goes through
//! [`engine_control_env`] / [`worker_env`]. The daemon's Seatbelt runner
//! (crates/daemon/src/sandbox) wraps daemon-owned worker commands; this
//! module supplies their env and process-group plumbing.
//!
//! Isolation model (PLAN.md "macOS sandbox and authority"):
//! - Environments are built from scratch (`env_clear` + explicit allowlist),
//!   so SSH agents, GitHub/cloud tokens, shell profile state, and Docker
//!   sockets are absent by construction rather than scrubbed.
//! - HOME and TMPDIR are fakes under daemon-managed storage.
//! - Every spawned process becomes a process-group leader so cancel can kill
//!   the whole tree ([`kill_process_group`]).
//! - File-descriptor and process-count resource limits are applied.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::OnceLock;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

use crate::adapter::EngineError;

/// PATH every spawned process gets: system tools plus the two Homebrew
/// prefixes. No user-local dirs.
pub const CONTROLLED_PATH: &str = "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin";

/// Per-session fake HOME/TMPDIR under daemon-managed storage.
pub struct SessionDirs {
    pub home: PathBuf,
    pub tmp: PathBuf,
    /// The id these directories were created under.
    ///
    /// Carried here so a caller cannot hand an engine one home and a session
    /// key naming a different one — the engine writes its resumable transcript
    /// into `home`, and a mismatch means `--resume` looks somewhere that
    /// transcript was never written.
    pub key: String,
}

/// Files copied from the real home into an ENGINE session home so the engine
/// sees the user's existing setup instead of behaving like a first launch.
///
/// COPIES, never links: the engine may rewrite these, and a run must not mutate
/// the user's own configuration. No credential is ever copied.
const SEEDED_ENGINE_HOME_FILES: &[&str] = &[
    // claude's config/onboarding state; without it the CLI runs first-launch
    // onboarding on every session.
    ".claude.json",
];

/// The user's login keychain directory, linked into ENGINE session homes.
///
/// macOS resolves the default keychain search list relative to `$HOME`, so a
/// relocated HOME hides the login keychain and the engine cannot see the login
/// the user already performed — verified: `claude auth status` reports
/// `loggedIn: false` with a fake HOME and `true` the moment this link exists.
///
/// A LINK, never a copy: the keychain is a live database owned by the OS, and
/// duplicating it would duplicate secrets. This grants no authority the engine
/// control process did not already have — it is not Seatbelt-wrapped (by
/// design; its tool subprocesses are confined by the engine's native sandbox),
/// so it could always open this path directly. Per-item Keychain ACLs still
/// apply, so the engine gets exactly the access its own CLI has when the user
/// runs it. Daemon-owned WORKER commands never get this link, and their
/// Seatbelt profile denies the resolved path regardless.
const KEYCHAIN_DIR: &str = "Library/Keychains";

static ACCOUNT_HOME: OnceLock<Option<PathBuf>> = OnceLock::new();

/// Home directory recorded for the signed-in OS account.
///
/// `HOME` is intentionally overridable and may point at an isolated app-data
/// fixture. Provider login state belongs to the macOS account, so resolve its
/// passwd record independently and cache the result. No credential contents
/// are read here.
pub fn account_home() -> Option<PathBuf> {
    ACCOUNT_HOME.get_or_init(query_account_home).clone()
}

#[cfg(target_os = "macos")]
fn query_account_home() -> Option<PathBuf> {
    let output = std::process::Command::new("/usr/bin/id")
        .arg("-P")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_passwd_home(std::str::from_utf8(&output.stdout).ok()?)
}

#[cfg(not(target_os = "macos"))]
fn query_account_home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
}

fn parse_passwd_home(record: &str) -> Option<PathBuf> {
    let home = record.lines().next()?.split(':').nth(8)?.trim();
    let home = PathBuf::from(home);
    home.is_absolute().then_some(home)
}

impl SessionDirs {
    /// Create `data_dir/sessions/<id>/{home,tmp}` plus an empty gitconfig
    /// (pinned via GIT_CONFIG_GLOBAL so git never reads the user's config).
    ///
    /// A bare home: nothing of the user's is visible. This is what daemon-owned
    /// worker commands get. Engine control processes use
    /// [`SessionDirs::create_for_engine`].
    pub fn create(data_dir: &Path, session_id: &str) -> std::io::Result<Self> {
        let root = data_dir.join("sessions").join(session_id);
        let home = root.join("home");
        let tmp = root.join("tmp");
        std::fs::create_dir_all(&home)?;
        std::fs::create_dir_all(&tmp)?;
        std::fs::write(home.join(".gitconfig"), "")?;
        Ok(Self {
            home,
            tmp,
            key: session_id.to_string(),
        })
    }

    /// [`SessionDirs::create`] plus exactly what an engine needs to reuse the
    /// user's existing login: the seeded config files and a link to the login
    /// keychain. Both are best effort — a machine missing either still gets a
    /// working session, it just starts unauthenticated and says so.
    pub fn create_for_engine(data_dir: &Path, session_id: &str) -> std::io::Result<Self> {
        let dirs = Self::create(data_dir, session_id)?;
        let Some(real_home) = account_home() else {
            return Ok(dirs);
        };
        for name in SEEDED_ENGINE_HOME_FILES {
            let source = real_home.join(name);
            if source.is_file() {
                let _ = std::fs::copy(&source, dirs.home.join(name));
            }
        }
        let keychains = real_home.join(KEYCHAIN_DIR);
        let link = dirs.home.join(KEYCHAIN_DIR);
        if keychains.is_dir()
            && !link.exists()
            && let Some(parent) = link.parent()
        {
            let _ = std::fs::create_dir_all(parent);
            let _ = std::os::unix::fs::symlink(&keychains, &link);
        }
        Ok(dirs)
    }
}

/// Build the base sanitized environment: controlled PATH, fake HOME/TMPDIR,
/// no inherited secrets or shell state. Everything else is added explicitly
/// by the caller.
fn base_env(dirs: &SessionDirs) -> Vec<(String, String)> {
    let mut env = vec![
        ("PATH".into(), CONTROLLED_PATH.into()),
        ("HOME".into(), dirs.home.to_string_lossy().into_owned()),
        ("TMPDIR".into(), dirs.tmp.to_string_lossy().into_owned()),
        ("LANG".into(), "en_US.UTF-8".into()),
        ("TERM".into(), "dumb".into()),
        // Never let git read the user's global config or credentials.
        ("GIT_CONFIG_NOSYSTEM".into(), "1".into()),
        (
            "GIT_CONFIG_GLOBAL".into(),
            dirs.home.join(".gitconfig").to_string_lossy().into_owned(),
        ),
        // No SSH agent, ever.
        ("SSH_AUTH_SOCK".into(), String::new()),
    ];
    // The account name. Verified on macOS: without it the `claude` CLI cannot
    // reach its Keychain credentials and reports `loggedIn: false` even with
    // the real HOME — so an env built purely from scratch silently breaks the
    // user's existing login. This is an identity, not a secret.
    for key in ["USER", "LOGNAME"] {
        if let Some(user) = std::env::var_os(key) {
            env.push((key.into(), user.to_string_lossy().into_owned()));
        }
    }
    env
}

/// Environment for an engine CONTROL process (codex app-server, claude).
///
/// The control process must reach its model provider using the login the user
/// already performed on this machine, so this env is built to make that login
/// visible and nothing else: a fake HOME, a controlled PATH, no inherited
/// tokens, and no proxy (it talks to the provider directly). Model-driven TOOL
/// subprocesses are confined by the engine's NATIVE sandbox (codex
/// `workspace-write`, claude permission mode), not by this env.
///
/// How each engine's saved credentials are reached (all verified live against
/// codex 0.143.0 and claude 2.1.221 on macOS):
///
/// - **Codex** reads `$CODEX_HOME/auth.json`. Pointing `CODEX_HOME` at the
///   real `~/.codex` is necessary AND sufficient: without it `codex login
///   status` reports "Not logged in".
/// - **Claude** reads its OAuth credentials from the macOS Keychain, which
///   needs `USER`/`LOGNAME` (see [`base_env`]) and nothing else. It also reads
///   `$HOME/.claude.json` for config/onboarding state, which the fake HOME
///   hides — so that one file is COPIED in by
///   [`SessionDirs::create_for_engine`], which also links the login keychain
///   that macOS resolves through `$HOME`.
///   `CLAUDE_CONFIG_DIR` is deliberately NOT set: it relocates the expected
///   `.claude.json` to `$CLAUDE_CONFIG_DIR/.claude.json`, which does not exist
///   on a normal install and makes the CLI print a "configuration file not
///   found" banner on every launch.
pub fn engine_control_env(dirs: &SessionDirs) -> Vec<(String, String)> {
    let mut env = base_env(dirs);
    if let Some(home) = account_home() {
        let codex_home = home.join(".codex");
        if codex_home.is_dir() {
            env.push((
                "CODEX_HOME".into(),
                codex_home.to_string_lossy().into_owned(),
            ));
        }
    }
    env
}

/// [`engine_control_env`] plus the engine binary's own directory on PATH.
///
/// Node-based CLIs re-exec themselves and shell out to sibling tools by name.
/// `claude` installs to `~/.local/bin`, which is deliberately absent from
/// [`CONTROLLED_PATH`], so the directory the binary was actually found in is
/// prepended — that specific directory only, never the user's whole PATH.
pub fn engine_control_env_for(dirs: &SessionDirs, binary: &Path) -> Vec<(String, String)> {
    let mut env = engine_control_env(dirs);
    let Some(bin_dir) = binary.parent() else {
        return env;
    };
    let bin_dir = bin_dir.to_string_lossy();
    if CONTROLLED_PATH.split(':').any(|p| p == bin_dir) {
        return env;
    }
    for (key, value) in env.iter_mut() {
        if key == "PATH" {
            *value = format!("{bin_dir}:{value}");
        }
    }
    env
}

/// Environment for a daemon-owned WORKER process (shell/build/verification/
/// integration commands), always run under a Seatbelt profile.
///
/// `proxy_addr` (e.g. `127.0.0.1:8787`) routes any allowed network access
/// through the daemon's read-only broker; direct egress is denied by the
/// Seatbelt profile.
pub fn worker_env(dirs: &SessionDirs, proxy_addr: Option<&str>) -> Vec<(String, String)> {
    let mut env = base_env(dirs);
    if let Some(addr) = proxy_addr {
        let url = format!("http://{addr}");
        env.push(("HTTP_PROXY".into(), url.clone()));
        env.push(("HTTPS_PROXY".into(), url.clone()));
        env.push(("http_proxy".into(), url.clone()));
        env.push(("https_proxy".into(), url));
        env.push(("NO_PROXY".into(), String::new()));
        env.push(("no_proxy".into(), String::new()));
    }
    env
}

/// Forbidden inherited variables, asserted absent in tests. Since envs are
/// built from scratch these never appear — this list documents the policy.
pub const FORBIDDEN_ENV_VARS: &[&str] = &[
    "SSH_AUTH_SOCK",
    "SSH_AGENT_PID",
    "GH_TOKEN",
    "GITHUB_TOKEN",
    "GITLAB_TOKEN",
    "AWS_ACCESS_KEY_ID",
    "AWS_SECRET_ACCESS_KEY",
    "AWS_SESSION_TOKEN",
    "GOOGLE_APPLICATION_CREDENTIALS",
    "GCLOUD_PROJECT",
    "AZURE_CLIENT_ID",
    "AZURE_CLIENT_SECRET",
    "DOCKER_HOST",
    "KUBECONFIG",
    "OPENAI_API_KEY",
    "ANTHROPIC_API_KEY",
    "NPM_TOKEN",
    "CARGO_REGISTRY_TOKEN",
    "HOMEBREW_GITHUB_API_TOKEN",
    "BASH_ENV",
    "ENV",
    "ZDOTDIR",
    "PYTHONSTARTUP",
];

/// A spawned provider process speaking newline-delimited JSON on stdio.
pub struct JsonLinesChild {
    pub child: Child,
    pub stdin: BufWriter<ChildStdin>,
    pub stdout: Lines<BufReader<ChildStdout>>,
}

/// Apply isolation to a Command: from-scratch env, process-group leadership,
/// resource limits. Shared by the JSON-lines spawn and the sandbox runner.
///
/// # Safety
/// Uses `pre_exec` for setrlimit; the closure is async-signal-safe (only
/// libc setrlimit calls) and runs in the child after fork.
pub fn apply_isolation(command: &mut Command, env: &[(String, String)]) {
    command.env_clear();
    for (key, value) in env {
        command.env(key, value);
    }
    command.process_group(0);
    unsafe {
        command.pre_exec(|| {
            // RLIMIT_NOFILE: 1024, RLIMIT_NPROC: 512. Errors are ignored:
            // limits are defense-in-depth, not a hard failure mode.
            let nofile = libc::rlimit {
                rlim_cur: 1024,
                rlim_max: libc::RLIM_INFINITY,
            };
            libc::setrlimit(libc::RLIMIT_NOFILE, &nofile);
            let nproc = libc::rlimit {
                rlim_cur: 512,
                rlim_max: libc::RLIM_INFINITY,
            };
            libc::setrlimit(libc::RLIMIT_NPROC, &nproc);
            Ok(())
        });
    }
}

/// Kill a whole process group (leader pid == pgid via process_group(0)).
pub fn kill_process_group(pgid: i32) {
    unsafe {
        libc::killpg(pgid, libc::SIGKILL);
    }
}

/// Spawn `program args` with piped stdio in `cwd` under the given sanitized
/// environment. Stderr is captured to a drain task so a chatty provider can
/// never deadlock the pipe. Interactive terminal escape sequences are never
/// interpreted as the control protocol: only stdout JSON lines are.
pub fn spawn_json_lines_child(
    program: &Path,
    args: &[&str],
    cwd: &Path,
    env: &[(String, String)],
) -> Result<JsonLinesChild, EngineError> {
    let mut command = Command::new(program);
    command
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    apply_isolation(&mut command, env);
    let mut child = command.spawn().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            EngineError::NotInstalled(program.display().to_string())
        } else {
            EngineError::Io(e)
        }
    })?;

    let stdin = BufWriter::new(child.stdin.take().expect("piped stdin"));
    let stdout = BufReader::new(child.stdout.take().expect("piped stdout")).lines();
    if let Some(stderr) = child.stderr.take() {
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::debug!(target: "autoharness_engines::stderr", %line, "engine stderr");
            }
        });
    }
    Ok(JsonLinesChild {
        child,
        stdin,
        stdout,
    })
}

impl JsonLinesChild {
    /// Write one JSON value as a single line.
    pub async fn send(&mut self, value: &serde_json::Value) -> Result<(), EngineError> {
        let mut line = serde_json::to_vec(value)?;
        line.push(b'\n');
        self.stdin.write_all(&line).await?;
        self.stdin.flush().await?;
        Ok(())
    }

    /// Read the next stdout line and parse it as JSON.
    /// `Ok(None)` on EOF (provider exited).
    pub async fn recv(&mut self) -> Result<Option<serde_json::Value>, EngineError> {
        loop {
            match self.stdout.next_line().await? {
                None => return Ok(None),
                Some(line) if line.trim().is_empty() => continue,
                Some(line) => return Ok(Some(serde_json::from_str(&line)?)),
            }
        }
    }

    /// Whether the process is still running.
    pub fn is_alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// Kill the process and its group (best effort).
    pub async fn kill(&mut self) {
        if let Some(id) = self.child.id() {
            kill_process_group(id as i32);
        }
        let _ = self.child.kill().await;
    }
}

/// Locate every matching binary on PATH and in known macOS install locations.
/// The order is stable and duplicates are removed without resolving symlinks.
pub fn binary_candidates(name: &str) -> Vec<std::path::PathBuf> {
    let mut candidates = Vec::new();
    let mut push = |candidate: std::path::PathBuf| {
        if candidate.is_file() && !candidates.contains(&candidate) {
            candidates.push(candidate);
        }
    };
    if let Ok(path) = std::env::var("PATH") {
        for dir in path.split(':') {
            push(std::path::PathBuf::from(dir).join(name));
        }
    }
    // ChatGPT ships the Codex CLI that writes and reads the same ~/.codex
    // configuration as the desktop app. Keep it as a candidate even when an
    // older Homebrew wrapper appears first on PATH.
    if name == "codex" {
        push(std::path::PathBuf::from(
            "/Applications/ChatGPT.app/Contents/Resources/codex",
        ));
    }
    for dir in ["/opt/homebrew/bin", "/usr/local/bin"] {
        push(std::path::PathBuf::from(dir).join(name));
    }
    if let Some(home) = account_home() {
        push(home.join(".local").join("bin").join(name));
    }
    candidates
}

/// Locate the first binary on PATH or in known macOS install locations.
pub fn find_binary(name: &str) -> Option<std::path::PathBuf> {
    binary_candidates(name).into_iter().next()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_dirs() -> (tempfile::TempDir, SessionDirs) {
        let root = tempfile::tempdir().unwrap();
        let dirs = SessionDirs::create(root.path(), "test-session").unwrap();
        (root, dirs)
    }

    #[test]
    fn parses_the_macos_account_home_without_using_environment_home() {
        assert_eq!(
            parse_passwd_home("dev:********:501:20::0:0:Dev User:/Users/dev:/bin/zsh\n"),
            Some(PathBuf::from("/Users/dev"))
        );
        assert_eq!(parse_passwd_home("not:a:passwd:record"), None);
        assert_eq!(
            parse_passwd_home("dev:x:501:20::0:0:Dev User:relative:/bin/zsh"),
            None
        );
    }

    #[test]
    fn account_home_is_absolute_when_available() {
        assert!(account_home().is_none_or(|home| home.is_absolute()));
    }

    #[test]
    fn sanitized_env_contains_no_forbidden_vars() {
        let (_root, dirs) = test_dirs();
        for env in [
            engine_control_env(&dirs),
            worker_env(&dirs, Some("127.0.0.1:8787")),
            worker_env(&dirs, None),
        ] {
            let keys: Vec<&str> = env.iter().map(|(k, _)| k.as_str()).collect();
            for forbidden in FORBIDDEN_ENV_VARS {
                if let Some((_, value)) = env.iter().find(|(k, _)| k == forbidden) {
                    // SSH_AUTH_SOCK is pinned to empty by design; every other
                    // forbidden variable must be entirely absent.
                    assert_eq!(forbidden, &"SSH_AUTH_SOCK", "unexpected var {forbidden}");
                    assert!(value.is_empty(), "{forbidden} must be empty");
                } else {
                    assert!(!keys.contains(forbidden));
                }
            }
            // PATH is controlled, HOME is the fake.
            assert_eq!(
                env.iter().find(|(k, _)| k == "PATH").unwrap().1,
                CONTROLLED_PATH
            );
            assert!(
                env.iter()
                    .find(|(k, _)| k == "HOME")
                    .unwrap()
                    .1
                    .contains("test-session")
            );
            assert!(
                env.iter()
                    .find(|(k, _)| k == "TMPDIR")
                    .unwrap()
                    .1
                    .contains("test-session")
            );
        }
    }

    #[test]
    fn worker_env_carries_proxy_vars_only_when_set() {
        let (_root, dirs) = test_dirs();
        let env = worker_env(&dirs, Some("127.0.0.1:9999"));
        assert_eq!(
            env.iter().find(|(k, _)| k == "HTTPS_PROXY").unwrap().1,
            "http://127.0.0.1:9999"
        );
        let env = worker_env(&dirs, None);
        assert!(env.iter().all(|(k, _)| k != "HTTPS_PROXY"));

        // Engine control processes never get proxy vars (direct provider
        // access) but do get provider config passthroughs.
        let env = engine_control_env(&dirs);
        assert!(env.iter().all(|(k, _)| k != "HTTPS_PROXY"));
    }

    #[test]
    fn forbidden_var_list_covers_plan_requirements() {
        for required in [
            "SSH_AUTH_SOCK",
            "GH_TOKEN",
            "GITHUB_TOKEN",
            "AWS_ACCESS_KEY_ID",
            "DOCKER_HOST",
            "KUBECONFIG",
        ] {
            assert!(FORBIDDEN_ENV_VARS.contains(&required), "missing {required}");
        }
    }
}
