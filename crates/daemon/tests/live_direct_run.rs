//! End-to-end direct run against a REAL engine CLI, through the real daemon
//! over the real socket. This is the test that proves the product works: the
//! user's saved login is reused, the engine edits its own worktree, the check
//! command runs sandboxed, and the work lands as a local commit on the run
//! branch while the user's checkout never moves.
//!
//! **Spends tokens**, so it is gated:
//!
//! ```sh
//! AUTOHARNESS_LIVE_TESTS=1 cargo test -p autoharness-daemon --test live_direct_run -- --nocapture
//! ```
//!
//! Without the gate it skips. It also skips when the engine is not installed
//! or not logged in, so it never fails for a reason outside the product.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use autoharness_daemon::{Daemon, DaemonConfig, load_or_create_token};
use autoharness_protocol as proto;
use autoharness_protocol::{Event, Request, Response, methods};
use serde_json::json;
use tokio::io::{ReadHalf, WriteHalf};
use tokio::net::UnixStream;

/// One tiny, deterministic, verifiable edit. Kept minimal on purpose: this
/// test exists to prove the harness works, not to exercise the model.
const OBJECTIVE: &str = "Create a file named hello.txt in the repository root \
     whose entire contents are the single word: harness\n\
     Do not create, modify, or delete anything else. Then stop.";
const CHECK: &str = "test \"$(cat hello.txt)\" = harness";

fn live_enabled() -> bool {
    std::env::var("AUTOHARNESS_LIVE_TESTS").is_ok_and(|v| v == "1")
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("git must be installed");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn init_repo(path: &Path) -> String {
    std::fs::create_dir_all(path).unwrap();
    git(path, &["init", "-q", "-b", "main"]);
    git(path, &["config", "user.name", "AutoHarness Live Test"]);
    git(path, &["config", "user.email", "live@localhost"]);
    std::fs::write(path.join("README.md"), "seed\n").unwrap();
    git(path, &["add", "-A"]);
    git(path, &["commit", "-q", "-m", "seed"]);
    git(path, &["rev-parse", "HEAD"])
}

struct Client {
    reader: ReadHalf<UnixStream>,
    writer: WriteHalf<UnixStream>,
}

impl Client {
    async fn connect(socket: &Path, token: &str) -> Self {
        let stream = UnixStream::connect(socket).await.expect("daemon socket");
        let (reader, writer) = tokio::io::split(stream);
        let mut client = Self { reader, writer };
        let hello = client
            .call("auth-1", methods::AUTH_HELLO, json!({ "token": token }))
            .await;
        assert!(hello.error.is_none(), "auth failed: {:?}", hello.error);
        client
    }

    async fn call(&mut self, id: &str, method: &str, params: serde_json::Value) -> Response {
        proto::write_frame(&mut self.writer, &Request::new(id, method, params))
            .await
            .unwrap();
        proto::read_frame::<_, Response>(&mut self.reader)
            .await
            .unwrap()
            .unwrap()
    }
}

/// Start the real daemon on a throwaway data dir. Returns socket path + token.
async fn start_daemon(data_dir: PathBuf) -> (PathBuf, String) {
    let config = DaemonConfig::in_dir(data_dir.clone());
    let socket = config.socket_path.clone();
    let daemon = Daemon::new(config).expect("daemon init");
    tokio::spawn(async move {
        let _ = daemon.run().await;
    });
    let deadline = Instant::now() + Duration::from_secs(60);
    while !socket.exists() {
        assert!(Instant::now() < deadline, "daemon never bound its socket");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let (token, _) = load_or_create_token(&data_dir).expect("client token");
    (socket, token)
}

async fn run_live_direct(engine: &str) {
    if !live_enabled() {
        eprintln!("AUTOHARNESS_LIVE_TESTS != 1; skipping live {engine} run");
        return;
    }
    let Some(binary) = autoharness_engines::process::find_binary(engine) else {
        eprintln!("{engine} not installed; skipping");
        return;
    };
    let authenticated = match engine {
        "codex" => autoharness_engines::probe::codex_authenticated(&binary).await,
        _ => autoharness_engines::probe::claude_authenticated(&binary).await,
    };
    if authenticated != Some(true) {
        eprintln!("{engine} is not logged in ({authenticated:?}); skipping live run");
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    let base = init_repo(&repo);
    let (socket, token) = start_daemon(dir.path().join("data")).await;
    let mut client = Client::connect(&socket, &token).await;

    let project = client
        .call(
            "p1",
            methods::PROJECT_ADD,
            json!({ "name": "live", "path": repo.to_string_lossy() }),
        )
        .await;
    let project_id = project.result.as_ref().unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();

    let run = client
        .call(
            "r1",
            methods::RUN_CREATE,
            json!({
                "project_id": project_id,
                "engine": engine,
                "objective": OBJECTIVE,
                "check_command": CHECK,
            }),
        )
        .await;
    let run_id = run.result.as_ref().unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();

    // Subscribe on a second connection so the run's events stream live while
    // the first connection issues run.start.
    let mut events = Client::connect(&socket, &token).await;
    let ack = events
        .call(
            "sub-1",
            methods::EVENTS_SUBSCRIBE,
            json!({ "since_sequence": 0, "run_id": run_id }),
        )
        .await;
    assert!(ack.error.is_none());

    let started = client
        .call("s1", methods::RUN_START, json!({ "run_id": run_id }))
        .await;
    let result = started.result.as_ref().expect("run.start must respond");
    assert_eq!(
        result["started"],
        true,
        "run did not start: {}",
        serde_json::to_string_pretty(result).unwrap()
    );

    // Follow the ledger to a terminal event.
    let mut kinds = Vec::new();
    let mut commit = None;
    let mut check_passed = None;
    let deadline = Instant::now() + Duration::from_secs(600);
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .expect("live run timed out");
        let frame =
            tokio::time::timeout(remaining, proto::read_frame::<_, Event>(&mut events.reader))
                .await
                .expect("live run timed out")
                .unwrap()
                .unwrap();
        eprintln!("  {} {}", frame.sequence, frame.kind);
        kinds.push(frame.kind.clone());
        match frame.kind.as_str() {
            "run.check" => check_passed = frame.payload["passed"].as_bool(),
            "run.commit" => {
                commit = frame.payload["commit"].as_str().map(str::to_string);
            }
            "run.succeeded" => break,
            "run.failed" | "run.cancelled" | "run.blocked" => panic!(
                "live {engine} run ended as {}: {}",
                frame.kind,
                serde_json::to_string_pretty(&frame.payload).unwrap()
            ),
            _ => {}
        }
    }

    // The engine edited its own worktree, the check ran, the work committed.
    assert_eq!(check_passed, Some(true), "check command must pass");
    let commit = commit.expect("a run that edits files must produce a commit");
    let branch = kinds
        .iter()
        .position(|k| k == "run.worktree_created")
        .map(|_| format!("ah/run-{}", &run_id[..8]))
        .unwrap();
    assert_eq!(git(&repo, &["rev-parse", &branch]), commit);
    assert_eq!(
        git(&repo, &["show", &format!("{commit}:hello.txt")]),
        "harness"
    );

    // The user's checkout never moved and stayed clean.
    assert_eq!(git(&repo, &["rev-parse", "HEAD"]), base);
    assert_eq!(git(&repo, &["status", "--porcelain"]), "");
    assert!(!repo.join("hello.txt").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn live_codex_direct_run_commits_verified_work() {
    run_live_direct("codex").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn live_claude_direct_run_commits_verified_work() {
    run_live_direct("claude").await;
}
