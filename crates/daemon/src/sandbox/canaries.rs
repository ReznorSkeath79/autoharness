//! Startup canaries proving the sandbox actually confines a worker.
//! Each probe runs through the real backend (sandbox-exec on this Mac).
//! Forbidden probes encode success as "breach": they exit 0 only when the
//! forbidden action SUCCEEDED, so `passed = !success` for them.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::{Sandbox, SandboxError};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CanaryResult {
    pub name: String,
    pub passed: bool,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CanaryReport {
    pub ok: bool,
    pub results: Vec<CanaryResult>,
}

impl CanaryReport {
    fn from_results(results: Vec<CanaryResult>) -> Self {
        let ok = results.iter().all(|r| r.passed);
        Self { ok, results }
    }
}

/// Run all canaries against `worktree` (must exist).
pub async fn run_canaries(sandbox: &Sandbox, worktree: &Path) -> CanaryReport {
    let real_home = super::home::primary_home().unwrap_or_else(|| PathBuf::from("/nonexistent"));
    let mut results = Vec::new();

    // (a) A worker CAN write inside its worktree.
    results.push(expect_success(
        "write_inside_worktree",
        sandbox
            .probe(
                worktree,
                "/bin/sh",
                &[
                    "-c".into(),
                    "echo ok > \"$0/.ah-canary\" && cat \"$0/.ah-canary\" && rm \"$0/.ah-canary\""
                        .into(),
                    worktree.to_string_lossy().into_owned(),
                ],
            )
            .await,
    ));

    // (b) A worker CANNOT read ~/.ssh.
    results.push(expect_denied(
        "cannot_read_ssh",
        sandbox
            .probe(
                worktree,
                "/bin/ls",
                &[real_home.join(".ssh").to_string_lossy().into_owned()],
            )
            .await,
    ));

    // (b2) A worker CANNOT read the login keychain. Engine CONTROL sessions
    // deliberately link this directory into their fake HOME so the user's
    // saved Codex/Claude login resolves; daemon-owned workers must never get
    // that reach, whether by absolute path or through a session home.
    results.push(expect_denied(
        "cannot_read_login_keychain",
        sandbox
            .probe(
                worktree,
                "/bin/ls",
                &[real_home
                    .join("Library")
                    .join("Keychains")
                    .to_string_lossy()
                    .into_owned()],
            )
            .await,
    ));

    // (c) A worker CANNOT write outside allowed roots.
    let forbidden_target = PathBuf::from(format!("/private/tmp/ah-canary-{}", std::process::id()));
    let result = sandbox
        .probe(
            worktree,
            "/bin/sh",
            &[
                "-c".into(),
                "echo breach > \"$0\"".into(),
                forbidden_target.to_string_lossy().into_owned(),
            ],
        )
        .await;
    // If the file somehow exists, the sandbox failed — clean up and report.
    let breach_file_existed = forbidden_target.exists();
    if breach_file_existed {
        let _ = std::fs::remove_file(&forbidden_target);
    }
    let denied = matches!(&result, Ok(r) if !r.success()) && !breach_file_existed;
    results.push(CanaryResult {
        name: "cannot_write_outside_allowed_roots".into(),
        passed: denied,
        detail: match &result {
            Ok(r) => format!("exit={:?} stderr={}", r.status.code(), r.stderr.trim()),
            Err(e) => format!("probe error: {e}"),
        },
    });

    // (d) A worker CANNOT reach arbitrary network destinations.
    results.push(expect_denied(
        "cannot_egress_network",
        sandbox
            .probe(
                worktree,
                "/usr/bin/nc",
                &[
                    "-z".into(),
                    "-G".into(),
                    "2".into(),
                    "example.com".into(),
                    "80".into(),
                ],
            )
            .await,
    ));

    // (e) A worker CANNOT read git credentials.
    results.push(expect_denied(
        "cannot_read_git_credentials",
        sandbox
            .probe(
                worktree,
                "/bin/cat",
                &[real_home
                    .join(".git-credentials")
                    .to_string_lossy()
                    .into_owned()],
            )
            .await,
    ));

    // (f) A worker CANNOT open the Docker socket.
    results.push(expect_denied(
        "cannot_open_docker_socket",
        sandbox
            .probe(worktree, "/bin/ls", &["/var/run/docker.sock".into()])
            .await,
    ));

    // (g) A worker CAN reach the brokered proxy (the one allowed network
    // destination).
    if let Some(port) = sandbox.proxy_port() {
        results.push(expect_success(
            "can_reach_brokered_proxy",
            sandbox
                .probe(
                    worktree,
                    "/usr/bin/nc",
                    &["-z".into(), "127.0.0.1".into(), port.to_string()],
                )
                .await,
        ));
    }

    CanaryReport::from_results(results)
}

fn expect_success(
    name: &'static str,
    result: Result<super::backend::CommandResult, SandboxError>,
) -> CanaryResult {
    match result {
        Ok(r) => CanaryResult {
            name: name.into(),
            passed: r.success(),
            detail: format!("exit={:?} stderr={}", r.status.code(), r.stderr.trim()),
        },
        Err(e) => CanaryResult {
            name: name.into(),
            passed: false,
            detail: format!("probe error: {e}"),
        },
    }
}

fn expect_denied(
    name: &'static str,
    result: Result<super::backend::CommandResult, SandboxError>,
) -> CanaryResult {
    match result {
        Ok(r) => CanaryResult {
            name: name.into(),
            // Exit 0 means the forbidden action succeeded — a breach.
            passed: !r.success(),
            detail: format!("exit={:?} stderr={}", r.status.code(), r.stderr.trim()),
        },
        Err(e) => CanaryResult {
            name: name.into(),
            passed: false,
            detail: format!("probe error: {e}"),
        },
    }
}
