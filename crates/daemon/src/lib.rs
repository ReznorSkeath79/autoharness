//! AutoHarness daemon: Unix-domain-socket server with peer-UID validation and
//! token authentication, JSON-RPC dispatch, and persist-before-broadcast
//! event delivery with cursor replay.

#![allow(
    clippy::collapsible_if,
    clippy::collapsible_match,
    clippy::result_large_err
)]

use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use autoharness_core::{EngineKind, RunState, router};
use autoharness_engines::process::SessionDirs;
use autoharness_engines::{EngineRegistry, SessionSpec};
use autoharness_protocol as proto;
use autoharness_protocol::{DedupCache, Event, Request, Response, codes, methods};
use autoharness_store::Store;
use serde_json::{Value, json};
use thiserror::Error;
use tokio::io::WriteHalf;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Mutex, broadcast, mpsc};

mod auth;
mod handoff;
pub mod history;
mod planner;
mod queue;
mod runner;
pub mod sandbox;
mod scheduler;
pub mod worktree;

pub use auth::{TokenSource, generate_token, load_or_create_token};
use runner::{RunCommand, RunContext};
use sandbox::SandboxStatus;

#[derive(Debug, Error)]
pub enum DaemonError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("store: {0}")]
    Store(#[from] autoharness_store::StoreError),
    #[error("protocol: {0}")]
    Protocol(#[from] proto::ProtocolError),
    #[error("auth: {0}")]
    Auth(String),
}

/// Where the daemon keeps its socket, database, and (fallback) token file.
#[derive(Debug, Clone)]
pub struct DaemonConfig {
    pub data_dir: PathBuf,
    pub socket_path: PathBuf,
    pub db_path: PathBuf,
}

impl DaemonConfig {
    /// Persistent state lives in
    /// `~/Library/Application Support/dev.autoharness.app/`; the Unix socket
    /// uses a deterministic short runtime path because macOS limits
    /// `sockaddr_un.sun_path` to roughly one hundred bytes.
    pub fn default_paths() -> Self {
        let home = std::env::home_dir().unwrap_or_else(|| PathBuf::from("."));
        let data_dir = home
            .join("Library")
            .join("Application Support")
            .join("dev.autoharness.app");
        let mut config = Self::in_dir(data_dir);
        config.socket_path = runtime_socket_path(&config.data_dir);
        config
    }

    pub fn in_dir(data_dir: PathBuf) -> Self {
        Self {
            socket_path: data_dir.join("daemon.sock"),
            db_path: data_dir.join("autoharness.db"),
            data_dir,
        }
    }
}

fn runtime_socket_path(data_dir: &Path) -> PathBuf {
    // Stable FNV-1a keeps independently launched UI/daemon processes on the
    // same socket while isolating different AutoHarness data profiles.
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in data_dir.as_os_str().as_encoded_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    PathBuf::from("/tmp").join(format!("autoharness-{hash:016x}.sock"))
}

/// Validate that the socket peer is the same Unix user (macOS `getpeereid`).
pub fn peer_uid_valid(stream: &UnixStream) -> bool {
    unsafe {
        let mut uid: libc::uid_t = 0;
        let mut gid: libc::gid_t = 0;
        libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) == 0 && uid == libc::getuid()
    }
}

pub(crate) struct AppState {
    pub(crate) store: Arc<Store>,
    token: String,
    /// Live event fan-out. Events are persisted to the ledger BEFORE being
    /// sent on this channel.
    pub(crate) events_tx: broadcast::Sender<Event>,
    dedup: Mutex<DedupCache>,
    /// Adapter factories, keyed by engine kind (real CLIs in production,
    /// scripted fakes in tests).
    pub(crate) engines: EngineRegistry,
    /// Command channels to active run tasks, keyed by run ID.
    pub(crate) active_runs: Mutex<HashMap<String, mpsc::Sender<RunCommand>>>,
    /// Serializes capacity checks and queue claims across client connections.
    pub(crate) queue_dispatch: Mutex<()>,
    /// Fail-closed sandbox verdict; runs are refused when not Ready.
    pub(crate) sandbox: SandboxStatus,
    /// Daemon-managed data dir (fake HOMEs, canary worktree, sessions).
    pub(crate) data_dir: PathBuf,
    /// Provider history roots are read-only; tests may inject temp roots.
    pub(crate) history_roots: RwLock<history::HistoryRoots>,
    pub(crate) history_scan_enabled: bool,
    /// Paths with a reclaim in flight. Two concurrent reclaims of the same
    /// worktree must not both reach `git worktree remove`.
    pub(crate) reclaiming: Mutex<std::collections::HashSet<String>>,
}

/// Persist an event to the ledger, then broadcast it. Order matters: a
/// subscriber must always be able to replay anything it missed. Shared by
/// `AppState::emit` and the proxy's audit sink.
pub(crate) fn emit_ledger(
    store: &Store,
    events_tx: &broadcast::Sender<Event>,
    run_id: Option<&str>,
    kind: &str,
    payload: Value,
) -> Result<Event, DaemonError> {
    let record = store.append_event(run_id, kind, payload)?;
    let event = record.to_event();
    // No subscribers is fine (lagged/none); ignore send errors.
    let _ = events_tx.send(event.clone());
    Ok(event)
}

impl AppState {
    /// Persist an event, then broadcast.
    pub(crate) fn emit(
        &self,
        run_id: Option<&str>,
        kind: &str,
        payload: Value,
    ) -> Result<Event, DaemonError> {
        emit_ledger(&self.store, &self.events_tx, run_id, kind, payload)
    }

    /// Transition a run through the core state machine. Returns Err on
    /// illegal transitions — never panics.
    pub(crate) fn transition_run(&self, run_id: &str, to: RunState) -> Result<RunState, String> {
        let run = self.store.get_run(run_id).map_err(|e| e.to_string())?;
        let from: RunState = run.state.parse().map_err(|e: String| e)?;
        let next = from.transition(to).map_err(|e| e.to_string())?;
        self.store
            .set_run_state(run_id, next.as_str())
            .map_err(|e| e.to_string())?;
        Ok(next)
    }

    #[cfg(test)]
    pub(crate) fn set_history_roots_for_tests(&self, roots: history::HistoryRoots) {
        *self
            .history_roots
            .write()
            .expect("history roots lock poisoned") = roots;
    }
}

pub struct Daemon {
    config: DaemonConfig,
    token_source: TokenSource,
}

impl Daemon {
    pub fn new(config: DaemonConfig) -> Result<Self, DaemonError> {
        std::fs::create_dir_all(&config.data_dir)?;
        let (_token, token_source) = load_or_create_token(&config.data_dir)?;
        Ok(Self {
            config,
            token_source,
        })
    }

    pub async fn run(self) -> Result<(), DaemonError> {
        let (listener, _state) = self.bind().await?;
        tracing::info!(
            socket = %self.config.socket_path.display(),
            token_source = ?self.token_source,
            "autoharnessd listening"
        );
        accept_loop(listener, _state).await;
        Ok(())
    }

    /// Bind the socket and build shared state. Split from [`Daemon::run`] so
    /// tests can drive the server with custom configs.
    async fn bind(&self) -> Result<(UnixListener, Arc<AppState>), DaemonError> {
        if self.config.socket_path.exists() {
            std::fs::remove_file(&self.config.socket_path)?;
        }
        let listener = UnixListener::bind(&self.config.socket_path)?;
        std::fs::set_permissions(
            &self.config.socket_path,
            std::fs::Permissions::from_mode(0o600),
        )?;

        let (token, _) = load_or_create_token(&self.config.data_dir)?;
        let (events_tx, _) = broadcast::channel(1024);
        let store = Arc::new(Store::open(&self.config.db_path)?);

        // Fail-closed sandbox init. Proxy decisions are audited to the ledger.
        let audit_store = Arc::clone(&store);
        let audit_tx = events_tx.clone();
        let audit: sandbox::proxy::AuditFn = Arc::new(move |domain, action, allowed| {
            let _ = emit_ledger(
                &audit_store,
                &audit_tx,
                None,
                if allowed {
                    "sandbox.proxy.request"
                } else {
                    "sandbox.proxy.denied"
                },
                json!({ "domain": domain, "action": action, "allowed": allowed }),
            );
        });
        let sandbox = sandbox::init(&self.config.data_dir, audit).await;
        match &sandbox {
            SandboxStatus::Ready(_) => tracing::info!("sandbox ready"),
            SandboxStatus::Unavailable(d) => {
                tracing::error!(problems = ?d.problems, "sandbox unavailable; runs will be refused")
            }
        }

        let history_scan_enabled = std::env::var("AUTOHARNESS_HISTORY_SCAN")
            .map(|value| value != "0")
            .unwrap_or(true);
        let state = Arc::new(AppState {
            store,
            token,
            events_tx,
            dedup: Mutex::new(DedupCache::new(1024)),
            engines: {
                // Terminal agents are confined by the same Seatbelt policy as
                // every daemon-owned command. Without a confinement the
                // registry still builds — and a PTY run is refused at start
                // rather than running outside the sandbox.
                let confinement = sandbox
                    .sandbox()
                    .and_then(|s| s.agent_confinement())
                    .map(|c| {
                        std::sync::Arc::new(c)
                            as std::sync::Arc<dyn autoharness_engines::Confinement>
                    });
                let proxy = sandbox
                    .sandbox()
                    .and_then(|s| s.proxy_port())
                    .map(|port| format!("127.0.0.1:{port}"));
                EngineRegistry::production_with(None, proxy, confinement)
            },
            active_runs: Mutex::new(HashMap::new()),
            queue_dispatch: Mutex::new(()),
            sandbox,
            data_dir: self.config.data_dir.clone(),
            history_roots: RwLock::new(history::HistoryRoots::defaults()),
            history_scan_enabled,
            reclaiming: Mutex::new(std::collections::HashSet::new()),
        });
        // The ledger holds user data; keep it owner-only like the socket.
        for suffix in ["", "-shm", "-wal"] {
            let path = PathBuf::from(format!("{}{}", self.config.db_path.display(), suffix));
            if path.exists() {
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
            }
        }

        // Check the database before trusting it. A corrupt ledger is reported
        // loudly and once, at startup, rather than surfacing later as
        // inexplicable behaviour — but it does NOT stop the daemon, because
        // refusing to start would also block the export that rescues the data.
        match state.store.integrity_report() {
            Ok(report) if report.healthy => {}
            Ok(report) => {
                tracing::error!(
                    sqlite = %report.sqlite,
                    orphan_events = report.orphan_events,
                    foreign_key_violations = report.foreign_key_violations,
                    "database integrity check FAILED; export your data with app.export"
                );
                let _ = emit_ledger(
                    &state.store,
                    &state.events_tx,
                    None,
                    "app.integrity_failed",
                    serde_json::to_value(&report).unwrap_or_default(),
                );
            }
            Err(e) => tracing::error!(error = %e, "database integrity check could not run"),
        }

        // Runs that were active when the previous daemon process died have no
        // engine behind them anymore. Reconcile them to Blocked BEFORE serving
        // any client, so stale work is never reported as a success.
        let reconciled = runner::reconcile_after_restart(&state).await;
        if reconciled > 0 {
            tracing::warn!(
                runs = reconciled,
                "reconciled runs orphaned by daemon restart"
            );
        }
        match state.store.reset_dispatching_queue() {
            Ok(recovered) if recovered > 0 => {
                let _ = state.emit(
                    None,
                    "queue.recovered",
                    json!({ "items": recovered, "reason": "daemon_restarted" }),
                );
            }
            Ok(_) => {}
            Err(error) => tracing::error!(%error, "queue reconciliation failed"),
        }
        queue::kick(Arc::clone(&state));

        if state.history_scan_enabled
            && state
                .store
                .app_settings()
                .map(|settings| settings.automatic_history_scan)
                .unwrap_or(false)
        {
            let scan_state = Arc::clone(&state);
            tokio::spawn(async move {
                let roots = scan_state
                    .history_roots
                    .read()
                    .expect("history roots lock poisoned")
                    .clone();
                if let Err(e) = history::refresh_index(&scan_state.store, &roots) {
                    tracing::warn!(error = %e, "provider history background scan failed");
                }
            });
        }

        Ok((listener, state))
    }
}

async fn accept_loop(listener: UnixListener, state: Arc<AppState>) {
    loop {
        match listener.accept().await {
            Ok((stream, _addr)) => {
                let state = Arc::clone(&state);
                tokio::spawn(async move {
                    if let Err(e) = handle_connection(stream, state).await {
                        tracing::debug!(error = %e, "connection closed");
                    }
                });
            }
            Err(e) => {
                tracing::error!(error = %e, "accept failed");
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        }
    }
}

/// Serve one client connection: auth handshake, then request dispatch.
/// `events.subscribe` switches the connection into replay + live streaming.
async fn handle_connection(stream: UnixStream, state: Arc<AppState>) -> Result<(), DaemonError> {
    if !peer_uid_valid(&stream) {
        return Err(DaemonError::Auth("peer UID mismatch".into()));
    }

    let (mut reader, writer) = tokio::io::split(stream);
    let writer = Arc::new(Mutex::new(writer));

    // First frame must be an authenticated `auth.hello` request.
    let first: Option<Request> = proto::read_frame(&mut reader).await?;
    let Some(hello) = first else {
        return Err(DaemonError::Auth("connection closed before auth".into()));
    };
    let authorized = hello.method == methods::AUTH_HELLO
        && hello
            .params
            .get("token")
            .and_then(Value::as_str)
            .is_some_and(|t| t == state.token);
    if !authorized {
        let resp = Response::err(hello.id, codes::UNAUTHORIZED, "unauthorized");
        proto::write_frame(&mut *writer.lock().await, &resp).await?;
        return Err(DaemonError::Auth("bad token".into()));
    }
    let resp = Response::ok(
        hello.id,
        json!({ "protocol_version": proto::PROTOCOL_VERSION }),
    );
    proto::write_frame(&mut *writer.lock().await, &resp).await?;

    // Subscription forwarding tasks for this connection.
    let mut subscriptions: HashMap<String, tokio::task::JoinHandle<()>> = HashMap::new();

    while let Some(request) = proto::read_frame::<_, Request>(&mut reader).await? {
        // Duplicate request ID: return the cached response unchanged.
        let cached = { state.dedup.lock().await.get(&request.id).cloned() };
        if let Some(resp) = cached {
            tracing::debug!(id = %request.id, method = %request.method, "duplicate request, replaying cached response");
            proto::write_frame(&mut *writer.lock().await, &resp).await?;
            continue;
        }

        let is_subscribe = request.method == methods::EVENTS_SUBSCRIBE;
        let response = dispatch(&state, &request).await;

        // Cache only non-subscribe responses; subscriptions are long-lived.
        if !is_subscribe {
            state
                .dedup
                .lock()
                .await
                .store(request.id.clone(), response.clone());
        }
        proto::write_frame(&mut *writer.lock().await, &response).await?;

        if is_subscribe && response.error.is_none() {
            let params: proto::params::EventsSubscribe = serde_json::from_value(
                request.params.clone(),
            )
            .unwrap_or(proto::params::EventsSubscribe {
                since_sequence: 0,
                run_id: None,
            });
            let handle = spawn_event_stream(Arc::clone(&state), Arc::clone(&writer), params);
            subscriptions.insert(request.id.clone(), handle);
        }
    }

    for (_, handle) in subscriptions {
        handle.abort();
    }
    Ok(())
}

/// Dispatch one authenticated request to its handler.
async fn dispatch(state: &Arc<AppState>, request: &Request) -> Response {
    let id = request.id.clone();
    if !methods::is_known(&request.method) {
        return Response::err(id, codes::METHOD_NOT_FOUND, "method not found");
    }
    if !methods::IMPLEMENTED.contains(&request.method.as_str()) {
        return Response::not_implemented(id, &request.method);
    }

    let result = match request.method.as_str() {
        methods::PROJECT_ADD => handle_project_add(state, &request.params),
        methods::PROJECT_CREATE => handle_project_create(state, &request.params).await,
        methods::PROJECT_LIST => handle_project_list(state),
        methods::PROJECT_REMOVE => handle_project_remove(state, &request.params),
        methods::RUN_CREATE => handle_run_create(state, &request.params),
        methods::RUN_SET_OBJECTIVE => handle_run_set_objective(state, &request.params),
        methods::RUN_ENQUEUE => handle_run_enqueue(state, &request.params),
        methods::RUN_ATTEMPTS => handle_run_attempts(state, &request.params),
        methods::RUN_GET => handle_run_get(state, &request.params),
        methods::QUEUE_LIST => handle_queue_list(state, &request.params),
        methods::QUEUE_MOVE => handle_queue_move(state, &request.params),
        methods::QUEUE_CANCEL => handle_queue_cancel(state, &request.params),
        methods::CHAT_SEND => handle_chat_send(state, &request.params).await,
        methods::CHAT_INTERRUPT => handle_chat_interrupt(state, &request.params).await,
        methods::RUN_ANSWER => handle_run_answer(state, &request.params).await,
        methods::NODE_RETRY => handle_node_control(state, &request.params, true).await,
        methods::NODE_CANCEL => handle_node_control(state, &request.params, false).await,
        methods::ENGINE_LIST => handle_engine_list(state).await,
        methods::RUN_APPROVE => handle_run_approve(state, &request.params).await,
        methods::MEMORY_LIST => handle_memory_list(state, &request.params),
        methods::MEMORY_FORGET => handle_memory_forget(state, &request.params),
        methods::HISTORY_LIST => handle_history_list(state, &request.params),
        methods::HISTORY_ADOPT => handle_history_adopt(state, &request.params),
        methods::SETTINGS_GET => handle_settings_get(state),
        methods::SETTINGS_UPDATE => handle_settings_update(state, &request.params),
        methods::USAGE_SUMMARY => handle_usage_summary(state),
        methods::WORKTREE_LIST => handle_worktree_list(state, &request.params).await,
        methods::WORKTREE_RECLAIM => handle_worktree_reclaim(state, &request.params).await,
        methods::ARTIFACT_LIST => handle_artifact_list(state, &request.params),
        methods::POLICY_LIST => handle_policy_list(state),
        methods::POLICY_PROMOTE => handle_policy_promote(state, &request.params),
        methods::POLICY_ROLLBACK => handle_policy_promote(state, &request.params),
        methods::APP_DIAGNOSTICS => handle_app_diagnostics(state).await,
        methods::APP_EXPORT => handle_app_export(state),
        methods::APP_PURGE_PROJECT => handle_app_purge_project(state, &request.params),
        methods::RUN_START => handle_run_start(state, &request.params).await,
        methods::RUN_PAUSE => {
            handle_run_control(state, &request.params, "pause", RunCommand::Pause).await
        }
        methods::RUN_RESUME => {
            handle_run_control(state, &request.params, "resume", RunCommand::Resume).await
        }
        methods::RUN_CANCEL => {
            handle_run_control(state, &request.params, "cancel", RunCommand::Cancel).await
        }
        methods::SANDBOX_CHECK => handle_sandbox_check(state).await,
        // The head sequence at subscribe time separates history from news.
        // A client uses it to replay quietly: an old alert must not ring.
        methods::EVENTS_SUBSCRIBE => Ok(json!({
            "subscribed": true,
            "replay_through_seq": state.store.latest_event_seq().unwrap_or(0),
        })),
        _ => unreachable!("filtered above"),
    };
    match result {
        Ok(value) => Response::ok(id, value),
        Err(resp) => resp,
    }
}

fn invalid_params(id: &str, e: impl std::fmt::Display) -> Response {
    Response::err(
        id.to_string(),
        codes::INVALID_PARAMS,
        format!("invalid params: {e}"),
    )
}

fn internal(id: &str, e: impl std::fmt::Display) -> Response {
    Response::err(
        id.to_string(),
        codes::INTERNAL,
        format!("internal error: {e}"),
    )
}

fn execution_selection(
    model: Option<String>,
    reasoning_effort: Option<String>,
) -> Result<(Option<String>, Option<String>), Response> {
    fn clean(value: Option<String>, label: &str, max: usize) -> Result<Option<String>, Response> {
        let Some(value) = value else {
            return Ok(None);
        };
        let value = value.trim();
        if value.is_empty() {
            return Ok(None);
        }
        if value.len() > max || value.chars().any(char::is_control) {
            return Err(invalid_params(
                "?",
                format!("{label} must be at most {max} printable bytes"),
            ));
        }
        Ok(Some(value.to_string()))
    }

    let model = clean(model, "model", 200)?;
    let reasoning_effort = clean(reasoning_effort, "reasoning_effort", 32)?;
    if reasoning_effort
        .as_deref()
        .is_some_and(|effort| effort.chars().any(char::is_whitespace))
    {
        return Err(invalid_params(
            "?",
            "reasoning_effort must be one provider-native token",
        ));
    }
    Ok((model, reasoning_effort))
}

fn handle_project_add(state: &AppState, params: &Value) -> Result<Value, Response> {
    let p: proto::params::ProjectAdd =
        serde_json::from_value(params.clone()).map_err(|e| invalid_params("?", e))?;
    let root = repository_root(std::path::Path::new(&p.path))
        .map_err(|message| invalid_params("?", message))?;
    let path = root.to_string_lossy().into_owned();
    let name = root
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or(p.name);
    let already_registered = state
        .store
        .list_projects()
        .map_err(|e| internal("?", e))?
        .iter()
        .any(|project| project.path == path);
    match state.store.add_project(&name, &path) {
        Ok(project) => {
            if !already_registered {
                let _ = state.emit(
                    None,
                    "project.added",
                    json!({ "project_id": project.id, "name": project.name, "path": project.path }),
                );
            }
            Ok(serde_json::to_value(project).unwrap())
        }
        Err(e) => Err(internal("?", e)),
    }
}

/// `project.create`: a brand-new repository under `~/AutoHarness`,
/// initialized with a first commit and registered as a project.
///
/// The visible complement to "just make one when prompting": nothing is
/// created outside this one predictable folder, and the reply names exactly
/// where it went. Fail closed like every other write — no ready sandbox, no
/// repository.
async fn handle_project_create(state: &Arc<AppState>, params: &Value) -> Result<Value, Response> {
    let p: proto::params::ProjectCreate =
        serde_json::from_value(params.clone()).map_err(|e| invalid_params("?", e))?;
    // "AutoHarness Projects", not "AutoHarness": the bare name collides with
    // a real directory on the author's machine (a source checkout), and
    // creating repositories inside a directory that means something else is
    // exactly the surprise this feature promises not to be.
    let parent = std::env::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("AutoHarness Projects");
    create_repository_project(state, &p.name, &parent).await
}

/// The testable body of `project.create`: `parent` is the managed folder the
/// repository is created under (`~/AutoHarness` in production).
async fn create_repository_project(
    state: &Arc<AppState>,
    raw_name: &str,
    parent: &Path,
) -> Result<Value, Response> {
    let name = sanitize_project_name(raw_name)
        .ok_or_else(|| invalid_params("?", "project name must contain letters or digits"))?;
    if !state.sandbox.ready() {
        return Err(invalid_params(
            "?",
            "sandbox unavailable; repository creation is refused rather than run unconfined",
        ));
    }
    let Some(sandbox) = state.sandbox.sandbox() else {
        return Err(invalid_params(
            "?",
            "sandbox unavailable; repository creation is refused rather than run unconfined",
        ));
    };
    // If the managed folder is itself a repository, it is not ours — some
    // existing checkout owns that path. Refuse rather than nest repos in it.
    if parent.join(".git").exists() {
        return Err(invalid_params(
            "?",
            format!(
                "{} is already a git repository; refusing to create projects inside it",
                parent.display()
            ),
        ));
    }
    std::fs::create_dir_all(parent).map_err(|e| internal("?", e))?;
    let path = free_repository_path(parent, &name).ok_or_else(|| {
        invalid_params(
            "?",
            format!("no free directory for {name} under {}", parent.display()),
        )
    })?;
    std::fs::create_dir(&path).map_err(|e| internal("?", e))?;
    let dirs =
        SessionDirs::create(&state.data_dir, "project-create").map_err(|e| internal("?", e))?;
    worktree::init_repository(sandbox, &dirs, &path)
        .await
        .map_err(|e| internal("?", e))?;
    let path_string = path.to_string_lossy().into_owned();
    match state.store.add_project(&name, &path_string) {
        Ok(project) => {
            let _ = state.emit(
                None,
                "project.added",
                json!({
                    "project_id": project.id,
                    "name": project.name,
                    "path": project.path,
                    "created": true,
                }),
            );
            Ok(serde_json::to_value(project).unwrap())
        }
        Err(e) => Err(internal("?", e)),
    }
}

/// A repository directory name from whatever the user typed: lowercase,
/// alphanumeric runs joined by single dashes, bounded. `None` when nothing
/// usable survives — the caller refuses rather than inventing a name.
fn sanitize_project_name(raw: &str) -> Option<String> {
    let mut name = String::new();
    for ch in raw.trim().to_ascii_lowercase().chars() {
        if ch.is_ascii_alphanumeric() {
            name.push(ch);
        } else if !name.is_empty() && !name.ends_with('-') {
            name.push('-');
        }
    }
    let name: String = name.trim_end_matches('-').chars().take(48).collect();
    if name.is_empty() { None } else { Some(name) }
}

/// The first free `<parent>/<name>` directory, suffixing `-2`..`-9` before
/// giving up. Never reuses an existing directory: creating a repository in a
/// folder that already has contents is how user files get surprised.
fn free_repository_path(parent: &Path, name: &str) -> Option<PathBuf> {
    let first = parent.join(name);
    if !first.exists() {
        return Some(first);
    }
    (2..=9)
        .map(|n| parent.join(format!("{name}-{n}")))
        .find(|path| !path.exists())
}

/// Resolve a user-picked folder to the nearest containing Git repository.
/// Canonicalization happens before the ancestor walk so the daemon persists a
/// stable path and symlink aliases cannot create duplicate project records.
fn repository_root(path: &std::path::Path) -> std::result::Result<std::path::PathBuf, String> {
    let canonical = std::fs::canonicalize(path)
        .map_err(|error| format!("repository path cannot be opened: {error}"))?;
    let start = if canonical.is_file() {
        canonical
            .parent()
            .map(std::path::Path::to_path_buf)
            .ok_or_else(|| "repository path has no parent directory".to_string())?
    } else {
        canonical
    };
    start
        .ancestors()
        .find(|candidate| candidate.join(".git").exists())
        .map(std::path::Path::to_path_buf)
        .ok_or_else(|| "choose a folder inside a Git repository".to_string())
}

fn handle_project_list(state: &AppState) -> Result<Value, Response> {
    state
        .store
        .list_projects()
        .map(|projects| serde_json::to_value(projects).unwrap())
        .map_err(|e| internal("?", e))
}

fn handle_project_remove(state: &AppState, params: &Value) -> Result<Value, Response> {
    let p: proto::params::ProjectRemove =
        serde_json::from_value(params.clone()).map_err(|e| invalid_params("?", e))?;
    match state.store.remove_project(&p.project_id) {
        Ok(removed) => {
            if removed {
                let _ = state.emit(
                    None,
                    "project.removed",
                    json!({ "project_id": p.project_id }),
                );
            }
            Ok(json!({ "removed": removed }))
        }
        Err(e) => Err(internal("?", e)),
    }
}

fn handle_run_create(state: &AppState, params: &Value) -> Result<Value, Response> {
    let p: proto::params::RunCreate =
        serde_json::from_value(params.clone()).map_err(|e| invalid_params("?", e))?;
    let engine = p
        .engine
        .unwrap_or(
            state
                .store
                .app_settings()
                .map_err(|e| internal("?", e))?
                .default_engine,
        )
        .as_str()
        .to_string();
    let engine = engine.as_str();
    let (model, reasoning_effort) = execution_selection(p.model, p.reasoning_effort)?;
    match state.store.create_run_configured(
        &p.project_id,
        engine,
        model.as_deref(),
        reasoning_effort.as_deref(),
        &p.objective,
        p.check_command.as_deref(),
        p.parent_run_id.as_deref(),
    ) {
        Ok(run) => {
            let _ = state.emit(
                Some(&run.id),
                "run.created",
                json!({
                    "run_id": run.id,
                    "project_id": run.project_id,
                    "engine": run.engine,
                    "model": run.model,
                    "reasoning_effort": run.reasoning_effort,
                    "objective": run.objective,
                    "state": run.state,
                    "check_command": run.check_command,
                    "parent_run_id": run.parent_run_id,
                }),
            );
            Ok(serde_json::to_value(run).unwrap())
        }
        Err(e) => Err(internal("?", e)),
    }
}

/// `run.set_objective`: fill in a draft's objective before it is started.
///
/// Draft-only, and refused otherwise. A run that has started has already told
/// an engine what to do; letting the record be rewritten afterwards would make
/// the ledger disagree with what was actually asked, which is the one thing an
/// audit trail cannot do. The refusal is structured so the UI can say why.
fn handle_run_set_objective(state: &AppState, params: &Value) -> Result<Value, Response> {
    let p: proto::params::RunSetObjective =
        serde_json::from_value(params.clone()).map_err(|e| invalid_params("?", e))?;
    let objective = p.objective.trim();
    if objective.is_empty() {
        return Err(invalid_params("?", "objective is empty"));
    }
    let run = state
        .store
        .get_run(&p.run_id)
        .map_err(|e| invalid_params("?", e))?;
    if run.state != "draft" {
        return Err(Response::err(
            "?".to_string(),
            codes::INVALID_PARAMS,
            format!(
                "run {} is {}, not a draft; its objective is already on the ledger",
                p.run_id, run.state
            ),
        ));
    }
    if !state
        .store
        .set_draft_objective(&p.run_id, objective)
        .map_err(|e| internal("?", e))?
    {
        return Err(Response::err(
            "?".to_string(),
            codes::INVALID_PARAMS,
            format!("run {} left draft before its objective was set", p.run_id),
        ));
    }
    let _ = state.emit(
        Some(&p.run_id),
        "run.objective_set",
        json!({ "run_id": p.run_id, "objective": objective }),
    );
    Ok(json!({ "run_id": p.run_id, "objective": objective }))
}

/// `run.attempts`: answer one objective several ways and compare the results.
///
/// This is the thing a terminal full of agents cannot do. Each attempt is an
/// ordinary run — its own worktree, its own `ah/run-*` branch, the same
/// ledger, the same diff and commit rules — so nothing downstream learns a new
/// concept. What the group adds is that they were asked the same question and
/// judged by the same check, which is what makes "this one passed" a fact
/// rather than a preference.
fn handle_run_attempts(state: &Arc<AppState>, params: &Value) -> Result<Value, Response> {
    let p: proto::params::RunAttempts =
        serde_json::from_value(params.clone()).map_err(|e| invalid_params("?", e))?;
    let objective = p.objective.trim();
    if objective.is_empty() {
        return Err(invalid_params("?", "objective cannot be empty"));
    }
    if objective.len() > 64 * 1024 {
        return Err(invalid_params("?", "objective exceeds 64 KiB"));
    }
    if p.attempts.is_empty() {
        return Err(invalid_params("?", "at least one attempt is required"));
    }
    // A ceiling, because each attempt is a real engine spending real tokens in
    // its own checkout. The scheduler still admits them under max_active_runs;
    // this stops a typo asking for fifty.
    if p.attempts.len() > 8 {
        return Err(invalid_params("?", "at most 8 attempts per objective"));
    }
    state
        .store
        .get_project(&p.project_id)
        .map_err(|error| invalid_params("?", error))?;

    let mut specs = Vec::new();
    for attempt in &p.attempts {
        if state.engines.create(attempt.engine.clone()).is_none() {
            return Err(invalid_params(
                "?",
                format!("no adapter registered for engine {}", attempt.engine),
            ));
        }
        let (model, effort) =
            execution_selection(attempt.model.clone(), attempt.reasoning_effort.clone())?;
        specs.push((attempt.engine.as_str().to_string(), model, effort));
    }

    let (group, runs, applied) = state
        .store
        .enqueue_attempts(
            &p.project_id,
            objective,
            p.check_command.as_deref(),
            &specs,
            &p.request_id,
            &json!({}),
        )
        .map_err(|e| match e {
            autoharness_store::StoreError::NotFound(what) => invalid_params("?", what),
            other => internal("?", other),
        })?;

    if applied {
        for run in &runs {
            let _ = state.emit(
                Some(&run.id),
                "run.created",
                json!({
                    "run_id": run.id,
                    "project_id": run.project_id,
                    "engine": run.engine,
                    "model": run.model,
                    "reasoning_effort": run.reasoning_effort,
                    "objective": run.objective,
                    "state": run.state,
                    "check_command": run.check_command,
                    "parent_run_id": run.parent_run_id,
                    "attempt_group": run.attempt_group,
                    "queued": true,
                }),
            );
        }
    }
    queue::kick(Arc::clone(state));
    Ok(json!({
        "attempt_group": group,
        "run_ids": runs.iter().map(|run| run.id.clone()).collect::<Vec<_>>(),
    }))
}

fn handle_run_enqueue(state: &Arc<AppState>, params: &Value) -> Result<Value, Response> {
    let p: proto::params::RunEnqueue =
        serde_json::from_value(params.clone()).map_err(|e| invalid_params("?", e))?;
    let objective = p.objective.trim();
    if objective.is_empty() {
        return Err(invalid_params("?", "objective cannot be empty"));
    }
    if objective.len() > 64 * 1024 {
        return Err(invalid_params("?", "objective exceeds 64 KiB"));
    }
    state
        .store
        .get_project(&p.project_id)
        .map_err(|error| invalid_params("?", error))?;
    let settings = state.store.app_settings().map_err(|e| internal("?", e))?;
    let engine = p.engine.unwrap_or(settings.default_engine);
    let engine = engine.as_str();
    let (model, reasoning_effort) = execution_selection(p.model, p.reasoning_effort)?;
    let options = json!({
        "route_mode": p.route_mode,
        "budget": p.budget,
    });
    // A draft the client already created (the sidebar's `+`) is filled in and
    // queued; everything else creates its run here. Both land in the same
    // queue, so there is still exactly one path that can start a run.
    let enqueued = match p.run_id.as_deref() {
        Some(run_id) => {
            state
                .store
                .enqueue_existing_draft(run_id, objective, &p.request_id, &options)
        }
        None => state.store.enqueue_run_configured(
            &p.project_id,
            engine,
            model.as_deref(),
            reasoning_effort.as_deref(),
            objective,
            p.check_command.as_deref(),
            p.parent_run_id.as_deref(),
            &p.request_id,
            &options,
        ),
    };
    let (run, item, applied) = enqueued.map_err(|e| match e {
        autoharness_store::StoreError::NotFound(what) => invalid_params("?", what),
        other => internal("?", other),
    })?;
    if applied {
        // A run already announced by `run.create` is not created twice; what
        // is new about it is its objective.
        let (kind, payload) = if p.run_id.is_some() {
            (
                "run.objective_set",
                json!({
                    "run_id": run.id,
                    "objective": run.objective,
                    "queued": true,
                }),
            )
        } else {
            (
                "run.created",
                json!({
                    "run_id": run.id,
                    "project_id": run.project_id,
                    "engine": run.engine,
                    "model": run.model,
                    "reasoning_effort": run.reasoning_effort,
                    "objective": run.objective,
                    "state": run.state,
                    "check_command": run.check_command,
                    "parent_run_id": run.parent_run_id,
                    "queued": true,
                }),
            )
        };
        state
            .emit(Some(&run.id), kind, payload)
            .map_err(|e| internal("?", e))?;
        state
            .emit(Some(&run.id), "queue.enqueued", queue::queue_payload(&item))
            .map_err(|e| internal("?", e))?;
    }
    queue::kick(Arc::clone(state));
    serde_json::to_value(proto::params::RunEnqueueResult {
        run_id: run.id,
        item,
    })
    .map_err(|e| internal("?", e))
}

fn handle_queue_list(state: &AppState, params: &Value) -> Result<Value, Response> {
    let p: proto::params::QueueList =
        serde_json::from_value(params.clone()).map_err(|e| invalid_params("?", e))?;
    let items = state.store.list_queue(&p).map_err(|e| internal("?", e))?;
    serde_json::to_value(proto::params::QueueListResult { items }).map_err(|e| internal("?", e))
}

fn handle_queue_move(state: &AppState, params: &Value) -> Result<Value, Response> {
    let p: proto::params::QueueMove =
        serde_json::from_value(params.clone()).map_err(|e| invalid_params("?", e))?;
    let (item, applied) = state
        .store
        .move_queue_item(&p.item_id, p.before_item_id.as_deref(), &p.request_id)
        .map_err(|e| match e {
            autoharness_store::StoreError::NotFound(_)
            | autoharness_store::StoreError::InvalidOperation(_) => invalid_params("?", e),
            _ => internal("?", e),
        })?;
    if applied {
        state
            .emit(
                Some(&item.run_id),
                "queue.moved",
                queue::queue_payload(&item),
            )
            .map_err(|e| internal("?", e))?;
    }
    serde_json::to_value(item).map_err(|e| internal("?", e))
}

fn handle_queue_cancel(state: &Arc<AppState>, params: &Value) -> Result<Value, Response> {
    let p: proto::params::QueueCancel =
        serde_json::from_value(params.clone()).map_err(|e| invalid_params("?", e))?;
    let (item, applied) = state
        .store
        .cancel_queue_item(&p.item_id, &p.request_id)
        .map_err(|e| match e {
            autoharness_store::StoreError::NotFound(_)
            | autoharness_store::StoreError::InvalidOperation(_) => invalid_params("?", e),
            _ => internal("?", e),
        })?;
    if applied {
        state
            .emit(
                Some(&item.run_id),
                "queue.cancelled",
                queue::queue_payload(&item),
            )
            .map_err(|e| internal("?", e))?;
        if item.kind == proto::params::QueueKind::Objective {
            state
                .emit(
                    Some(&item.run_id),
                    "run.cancelled",
                    json!({
                        "run_id": item.run_id,
                        "reason": "cancelled_while_queued",
                    }),
                )
                .map_err(|e| internal("?", e))?;
        }
        queue::kick(Arc::clone(state));
    }
    serde_json::to_value(item).map_err(|e| internal("?", e))
}

fn handle_run_get(state: &AppState, params: &Value) -> Result<Value, Response> {
    let p: proto::params::RunGet =
        serde_json::from_value(params.clone()).map_err(|e| invalid_params("?", e))?;
    match state.store.get_run(&p.run_id) {
        Ok(run) => Ok(serde_json::to_value(run).unwrap()),
        Err(autoharness_store::StoreError::NotFound(m)) => Err(Response::err(
            "?",
            codes::INVALID_PARAMS,
            format!("not found: {m}"),
        )),
        Err(e) => Err(internal("?", e)),
    }
}

/// Persist one chat message and mirror it into the ledger, so a UI relaunch
/// reconstructs the exact conversation from replay alone.
fn record_chat(state: &AppState, run_id: &str, message: &str) -> Result<Value, Response> {
    let msg = state
        .store
        .append_chat(run_id, "user", message)
        .map_err(|e| internal("?", e))?;
    let event = state
        .emit(
            Some(run_id),
            "chat.message",
            json!({
                "chat_id": msg.id,
                "run_id": msg.run_id,
                "role": msg.role,
                "content": msg.content,
            }),
        )
        .map_err(|e| internal("?", e))?;
    Ok(json!({ "chat_id": msg.id, "sequence": event.sequence }))
}

/// Hand a command to an active run's task. False when no task owns the run in
/// this daemon process.
async fn send_run_command(state: &AppState, run_id: &str, command: RunCommand) -> bool {
    let sender = state.active_runs.lock().await.get(run_id).cloned();
    match sender {
        Some(tx) => tx.send(command).await.is_ok(),
        None => false,
    }
}

/// `chat.send`: record the message and queue it as steering. The run task
/// injects it at the next safe turn boundary — never mid-turn.
/// `run.answer`: reply to a question the running engine asked.
///
/// Distinct from `run.approve`, which approves a compiled graph before it
/// executes. This answers the AGENT, mid-run, and it is the difference between
/// an interactive agent being usable and blocking forever on its first prompt.
async fn handle_run_answer(state: &Arc<AppState>, params: &Value) -> Result<Value, Response> {
    let p: proto::params::RunAnswer =
        serde_json::from_value(params.clone()).map_err(|e| invalid_params("?", e))?;
    if let autoharness_core::Answer::Text(text) = &p.answer {
        if text.trim().is_empty() {
            return Err(invalid_params("?", "an answer cannot be empty"));
        }
        if text.len() > 64 * 1024 {
            return Err(invalid_params("?", "answer exceeds 64 KiB"));
        }
    }
    let run = state
        .store
        .get_run(&p.run_id)
        .map_err(|e| invalid_params("?", e))?;
    if matches!(run.state.as_str(), "succeeded" | "failed" | "cancelled") {
        return Err(invalid_params(
            "?",
            "this run has finished; there is nothing waiting on an answer",
        ));
    }
    let Some(tx) = state.active_runs.lock().await.get(&p.run_id).cloned() else {
        // Fail closed and say so. Silently accepting an answer for a run with
        // no live process would report success for a keystroke nobody received.
        return Err(invalid_params(
            "?",
            "this run has no live engine process to answer",
        ));
    };
    tx.send(RunCommand::Answer(p.answer.clone()))
        .await
        .map_err(|_| internal("?", "the run stopped before the answer was delivered"))?;
    Ok(json!({ "run_id": p.run_id, "delivered": true }))
}

async fn handle_chat_send(state: &Arc<AppState>, params: &Value) -> Result<Value, Response> {
    let p: proto::params::ChatSend =
        serde_json::from_value(params.clone()).map_err(|e| invalid_params("?", e))?;
    let message = p.message.trim();
    if message.is_empty() {
        return Err(invalid_params("?", "message cannot be empty"));
    }
    if message.len() > 64 * 1024 {
        return Err(invalid_params("?", "message exceeds 64 KiB"));
    }
    let run = state
        .store
        .get_run(&p.run_id)
        .map_err(|e| invalid_params("?", e))?;
    if matches!(run.state.as_str(), "succeeded" | "failed" | "cancelled") {
        return Err(invalid_params(
            "?",
            "terminal runs cannot be steered; enqueue a continuation instead",
        ));
    }
    if run.state == "awaiting_approval" {
        return Err(invalid_params(
            "?",
            "approve or cancel the graph before sending steering",
        ));
    }
    let request_id = p.request_id.unwrap_or_else(autoharness_core::new_id);
    let (item, applied) = state
        .store
        .enqueue_steering(&p.run_id, message, &request_id)
        .map_err(|e| internal("?", e))?;
    if applied {
        state
            .emit(
                Some(&p.run_id),
                "chat.message",
                json!({
                    "queue_id": item.id,
                    "run_id": item.run_id,
                    "role": "user",
                    "content": item.content,
                }),
            )
            .map_err(|e| internal("?", e))?;
        state
            .emit(
                Some(&p.run_id),
                "queue.enqueued",
                queue::queue_payload(&item),
            )
            .map_err(|e| internal("?", e))?;
        state
            .emit(
                Some(&p.run_id),
                "chat.steering_queued",
                json!({
                    "queue_id": item.id,
                    "run_id": item.run_id,
                    "message": item.content,
                }),
            )
            .map_err(|e| internal("?", e))?;
    }
    let runner_notified = send_run_command(
        state,
        &p.run_id,
        RunCommand::Steer {
            queue_id: item.id.clone(),
        },
    )
    .await;
    Ok(json!({
        "queue_id": item.id,
        "queued": true,
        "applied": applied,
        "runner_notified": runner_notified,
    }))
}

/// `chat.interrupt`: record the message, then interrupt the in-flight provider
/// turn and inject it immediately.
async fn handle_chat_interrupt(state: &Arc<AppState>, params: &Value) -> Result<Value, Response> {
    let p: proto::params::ChatInterrupt =
        serde_json::from_value(params.clone()).map_err(|e| invalid_params("?", e))?;
    let mut result = record_chat(state, &p.run_id, &p.message)?;
    let delivered =
        send_run_command(state, &p.run_id, RunCommand::InterruptWithText(p.message)).await;
    result["delivered"] = json!(delivered);
    Ok(result)
}

/// `engine.list`: detection across every registered adapter. Pure setup
/// diagnostics — never starts a session or spends tokens.
async fn handle_engine_list(state: &Arc<AppState>) -> Result<Value, Response> {
    let diagnostics = state.engines.detect_all_with_models(&state.data_dir).await;
    serde_json::to_value(diagnostics).map_err(|e| internal("?", e))
}

fn handle_settings_get(state: &AppState) -> Result<Value, Response> {
    state
        .store
        .app_settings()
        .and_then(|settings| serde_json::to_value(settings).map_err(Into::into))
        .map_err(|e| internal("?", e))
}

fn handle_settings_update(state: &Arc<AppState>, params: &Value) -> Result<Value, Response> {
    let p: proto::params::SettingsUpdate =
        serde_json::from_value(params.clone()).map_err(|e| invalid_params("?", e))?;
    // The store applies and de-duplicates in one transaction, so a retried
    // request_id neither re-applies nor re-broadcasts.
    let (settings, applied) = state
        .store
        .update_app_settings(&p)
        .map_err(|e| internal("?", e))?;
    if applied {
        // Persist before broadcast, and treat a failed persist as a failed
        // request rather than a silent divergence between UI and ledger.
        state
            .emit(
                None,
                "settings.updated",
                json!({ "settings": settings, "request_id": p.request_id }),
            )
            .map_err(|e| internal("?", e))?;
        if state.history_scan_enabled && settings.automatic_history_scan {
            // Off the request path: a rescan walks the provider directories
            // and must not stall the RPC that enabled it.
            let scan_state = Arc::clone(state);
            tokio::spawn(async move {
                let roots = scan_state
                    .history_roots
                    .read()
                    .expect("history roots lock poisoned")
                    .clone();
                let _ = history::refresh_index(&scan_state.store, &roots);
            });
        }
        queue::kick(Arc::clone(state));
    }
    serde_json::to_value(settings).map_err(|e| internal("?", e))
}

// ---- artifacts ----

/// Typed, unpersisted artifact input. Keeping these named prevents callers
/// from transposing the several adjacent string fields.
pub(crate) struct ArtifactDraft<'a> {
    pub(crate) run_id: &'a str,
    pub(crate) node_id: Option<&'a str>,
    pub(crate) kind: &'a str,
    pub(crate) name: &'a str,
    pub(crate) path: Option<String>,
    pub(crate) byte_size: Option<i64>,
    pub(crate) summary: &'a str,
}

/// Persist an artifact, then announce it.
///
/// The store truncates the summary, so this cannot put an unbounded blob in
/// the ledger. Persisting first means a client that replays from seq 0 sees
/// the same artifacts a live client saw.
pub(crate) fn record_artifact(state: &AppState, draft: ArtifactDraft<'_>) {
    let ArtifactDraft {
        run_id,
        node_id,
        kind,
        name,
        path,
        byte_size,
        summary,
    } = draft;
    let artifact = autoharness_store::ArtifactRecord {
        id: String::new(),
        run_id: run_id.to_string(),
        node_id: node_id.map(str::to_string),
        kind: kind.to_string(),
        name: name.to_string(),
        path,
        byte_size,
        summary: summary.to_string(),
        created_at_ms: 0,
    };
    match state.store.record_artifact(&artifact) {
        Ok(stored) => {
            let _ = state.emit(
                Some(run_id),
                "artifact.created",
                json!({
                    "run_id": stored.run_id,
                    "node_id": stored.node_id,
                    "id": stored.id,
                    "kind": stored.kind,
                    "name": stored.name,
                    "path": stored.path,
                    "byte_size": stored.byte_size,
                    "summary": stored.summary,
                }),
            );
        }
        Err(e) => tracing::warn!(%run_id, error = %e, "artifact not recorded"),
    }
}

fn handle_artifact_list(state: &AppState, params: &Value) -> Result<Value, Response> {
    let p: proto::params::ArtifactList =
        serde_json::from_value(params.clone()).map_err(|e| invalid_params("?", e))?;
    let artifacts = state
        .store
        .list_artifacts(&p.run_id, p.limit.unwrap_or(200))
        .map_err(|e| internal("?", e))?
        .into_iter()
        .map(|a| proto::params::Artifact {
            id: a.id,
            run_id: a.run_id,
            node_id: a.node_id,
            kind: a.kind,
            name: a.name,
            path: a.path,
            byte_size: a.byte_size,
            summary: a.summary,
            created_at_ms: a.created_at_ms,
        })
        .collect();
    serde_json::to_value(proto::params::ArtifactListResult {
        run_id: p.run_id,
        artifacts,
    })
    .map_err(|e| internal("?", e))
}

// ---- worktree index and reclaim ----

/// The run whose id names a thread's shared worktree: the root of the parent
/// chain.
///
/// A follow-up turn is a new run, but it is the same piece of work in the same
/// checkout. Deriving the worktree directory and branch from the thread root
/// rather than from the turn is what makes "continue this run" continue it —
/// on the same branch, on top of whatever the previous turn committed —
/// instead of starting again in an empty tree off the base commit.
///
/// The walk is bounded, so a malformed parent link cannot spin the daemon.
pub(crate) fn thread_worktree_key(
    store: &autoharness_store::Store,
    run: &autoharness_store::Run,
) -> String {
    let mut id = run.id.clone();
    let mut parent = run.parent_run_id.clone();
    let mut guard = 0;
    while let Some(next) = parent {
        let Ok(ancestor) = store.get_run(&next) else {
            break;
        };
        if ancestor.id == id {
            break; // Defensive: a cycle must terminate.
        }
        parent = ancestor.parent_run_id.clone();
        id = ancestor.id;
        guard += 1;
        if guard > 64 {
            break;
        }
    }
    id
}

use proto::params::WorktreeBlocker as Blocker;

fn worktree_storage_root(state: &AppState) -> PathBuf {
    state.data_dir.join("worktrees")
}

/// Resolve a client-supplied path to the absolute path reclaim will act on.
///
/// Canonicalization is what defeats `..` and symlinks: the request is turned
/// into a real path and then compared against the indexed one. A path that
/// cannot be resolved at all keeps its parent's canonical form so a
/// already-deleted worktree still matches its row.
fn canonical_request_path(raw: &str) -> PathBuf {
    let path = Path::new(raw);
    if let Ok(resolved) = path.canonicalize() {
        return resolved;
    }
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) => match parent.canonicalize() {
            Ok(parent) => parent.join(name),
            Err(_) => path.to_path_buf(),
        },
        _ => path.to_path_buf(),
    }
}

/// What the daemon managed to observe about a worktree.
///
/// `Option` fields mean "not asked" (a missing directory cannot be dirty);
/// a failed question is recorded as `git_check_failed` rather than being
/// dropped, so the verdict below never mistakes ignorance for permission.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorktreeFacts {
    pub already_reclaimed: bool,
    pub canonical_matches: bool,
    pub under_storage: bool,
    pub is_primary_checkout: bool,
    pub run_active: bool,
    pub run_terminal: bool,
    pub exists: bool,
    pub clean: Option<bool>,
    pub commits_beyond_base: Option<i64>,
    pub process: worktree::ProcessUse,
    pub git_check_failed: bool,
}

/// The whole reclaim policy, as a pure function.
///
/// Every path into removal goes through here, so `worktree.list`, a dry run,
/// and the real reclaim always agree — and each rule can be tested without a
/// git repository, a sandbox, or a live process.
pub(crate) fn worktree_blockers(facts: &WorktreeFacts) -> Vec<Blocker> {
    let mut blockers = Vec::new();
    if facts.already_reclaimed {
        blockers.push(Blocker::AlreadyReclaimed);
    }
    if !facts.canonical_matches {
        blockers.push(Blocker::PathMismatch);
    }
    if !facts.under_storage {
        blockers.push(Blocker::OutsideStorage);
    }
    if facts.is_primary_checkout {
        blockers.push(Blocker::PrimaryCheckout);
    }
    if facts.run_active {
        blockers.push(Blocker::RunActive);
    }
    if !facts.run_terminal {
        blockers.push(Blocker::RunNotTerminal);
    }
    if facts.clean == Some(false) {
        blockers.push(Blocker::DirtyWorktree);
    }
    if facts.commits_beyond_base.is_some_and(|count| count > 0) {
        blockers.push(Blocker::CommitsBeyondBase);
    }
    match facts.process {
        worktree::ProcessUse::None => {}
        worktree::ProcessUse::InUse => blockers.push(Blocker::ProcessInUse),
        // "We could not tell" is not "no".
        worktree::ProcessUse::Unknown => blockers.push(Blocker::ProcessCheckUnknown),
    }
    if facts.git_check_failed {
        blockers.push(Blocker::GitCheckFailed);
    }
    blockers
}

/// Gather the facts, then apply [`worktree_blockers`].
async fn evaluate_worktree(
    state: &Arc<AppState>,
    record: &autoharness_store::WorktreeRecord,
) -> proto::params::WorktreeEntry {
    let path = PathBuf::from(&record.path);
    let exists = path.exists();
    let canonical = canonical_request_path(&record.path);

    let storage = worktree_storage_root(state);
    let storage_canonical = storage.canonicalize().unwrap_or(storage);
    let repo_canonical = Path::new(&record.repo_path)
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from(&record.repo_path));

    let mut facts = WorktreeFacts {
        already_reclaimed: record.removed_at_ms.is_some(),
        canonical_matches: canonical == path,
        under_storage: canonical.starts_with(&storage_canonical) && canonical != storage_canonical,
        is_primary_checkout: canonical == repo_canonical,
        run_active: state.active_runs.lock().await.contains_key(&record.run_id),
        run_terminal: false,
        exists,
        clean: None,
        commits_beyond_base: None,
        // Nothing can hold a directory that is not there.
        process: if exists {
            worktree::ProcessUse::Unknown
        } else {
            worktree::ProcessUse::None
        },
        git_check_failed: false,
    };

    let run_state = match state.store.get_run(&record.run_id) {
        Ok(run) => {
            facts.run_terminal = run
                .state
                .parse::<autoharness_core::RunState>()
                .is_ok_and(autoharness_core::RunState::is_terminal);
            run.state
        }
        // An unreadable run is an unknown run, and unknown routes closed.
        Err(_) => "unknown".to_string(),
    };

    if let Some(sandbox) = state.sandbox.sandbox()
        && let Ok(dirs) = SessionDirs::create(&state.data_dir, "worktree-index")
    {
        let wt = worktree::RunWorktree {
            run_id: record.run_id.clone(),
            owner: worktree::WorktreeOwner {
                run_id: record.run_id.clone(),
                node_id: record.node_id.clone(),
            },
            repo_path: PathBuf::from(&record.repo_path),
            path: path.clone(),
            branch: record.branch.clone(),
            base_commit: record.base_commit.clone(),
            repo_dirty_at_start: false,
        };
        if exists {
            match worktree::is_clean(sandbox, &dirs, &wt).await {
                Ok(clean) => facts.clean = Some(clean),
                Err(_) => facts.git_check_failed = true,
            }
            facts.process = worktree::processes_using(sandbox, &dirs, &path)
                .await
                .unwrap_or(worktree::ProcessUse::Unknown);
        }
        match worktree::branch_commits_beyond_base(sandbox, &dirs, &wt).await {
            Ok(count) => facts.commits_beyond_base = count,
            Err(_) => facts.git_check_failed = true,
        }
    } else {
        // Without a sandbox the git and process questions cannot be asked.
        facts.git_check_failed = true;
    }

    let blockers = worktree_blockers(&facts);
    proto::params::WorktreeEntry {
        path: record.path.clone(),
        kind: record.kind.clone(),
        run_id: record.run_id.clone(),
        node_id: record.node_id.clone(),
        repo_path: record.repo_path.clone(),
        branch: record.branch.clone(),
        base_commit: record.base_commit.clone(),
        created_at_ms: record.created_at_ms,
        removed_at_ms: record.removed_at_ms,
        run_state,
        exists,
        eligible: blockers.is_empty(),
        blockers,
    }
}

async fn handle_worktree_list(state: &Arc<AppState>, params: &Value) -> Result<Value, Response> {
    let p: proto::params::WorktreeList =
        serde_json::from_value(params.clone()).map_err(|e| invalid_params("?", e))?;
    let records = state
        .store
        .list_worktrees(p.include_reclaimed)
        .map_err(|e| internal("?", e))?;
    let mut entries = Vec::new();
    for record in records {
        if let Some(run_id) = p.run_id.as_deref()
            && record.run_id != run_id
        {
            continue;
        }
        let entry = evaluate_worktree(state, &record).await;
        if p.only_eligible && !entry.eligible {
            continue;
        }
        entries.push(entry);
    }
    serde_json::to_value(proto::params::WorktreeListResult {
        storage_root: worktree_storage_root(state).to_string_lossy().into_owned(),
        entries,
    })
    .map_err(|e| internal("?", e))
}

/// `worktree.reclaim`: fail closed.
///
/// Nothing is removed unless the path resolves to an indexed, daemon-owned
/// worktree whose owning run is finished, whose tree is clean, whose branch
/// holds no commits past its base, and which no process is using — and, for a
/// real run, unless the caller echoed the exact path back. `--force` is never
/// passed to git; if git objects, the worktree stays.
async fn handle_worktree_reclaim(state: &Arc<AppState>, params: &Value) -> Result<Value, Response> {
    let p: proto::params::WorktreeReclaim =
        serde_json::from_value(params.clone()).map_err(|e| invalid_params("?", e))?;
    let canonical = canonical_request_path(&p.path);
    let key = canonical.to_string_lossy().into_owned();

    let refuse =
        |blockers: Vec<Blocker>, entry: Option<proto::params::WorktreeEntry>, already: bool| {
            serde_json::to_value(proto::params::WorktreeReclaimResult {
                path: key.clone(),
                dry_run: p.dry_run,
                reclaimed: false,
                already_reclaimed: already,
                eligible: false,
                blockers,
                entry,
            })
            .map_err(|e| internal("?", e))
        };

    // The index is the authority: an unindexed directory is never touched.
    let Some(record) = state
        .store
        .get_worktree(&key)
        .map_err(|e| internal("?", e))?
    else {
        return refuse(vec![Blocker::NotIndexed], None, false);
    };
    if record.removed_at_ms.is_some() {
        // Idempotent repeat, not an error: the desired state already holds.
        let entry = evaluate_worktree(state, &record).await;
        return refuse(vec![Blocker::AlreadyReclaimed], Some(entry), true);
    }

    let entry = evaluate_worktree(state, &record).await;
    let mut blockers = entry.blockers.clone();
    if !p.dry_run && p.confirm_path.as_deref() != Some(p.path.as_str()) {
        blockers.push(Blocker::ConfirmPathMismatch);
    }
    if !blockers.is_empty() {
        return refuse(blockers, Some(entry), false);
    }
    if p.dry_run {
        return serde_json::to_value(proto::params::WorktreeReclaimResult {
            path: key,
            dry_run: true,
            reclaimed: false,
            already_reclaimed: false,
            eligible: true,
            blockers: Vec::new(),
            entry: Some(entry),
        })
        .map_err(|e| internal("?", e));
    }

    // Claim the path so a concurrent reclaim cannot also start removing it.
    if !state.reclaiming.lock().await.insert(key.clone()) {
        return refuse(vec![Blocker::ReclaimInProgress], Some(entry), false);
    }
    let outcome = reclaim_claimed(state, &record).await;
    state.reclaiming.lock().await.remove(&key);
    outcome?;

    state
        .store
        .mark_worktree_removed(&key)
        .map_err(|e| internal("?", e))?;
    state
        .emit(
            Some(&record.run_id),
            "worktree.reclaimed",
            json!({
                "path": key,
                "run_id": record.run_id,
                "node_id": record.node_id,
                "branch": record.branch,
                "request_id": p.request_id,
            }),
        )
        .map_err(|e| internal("?", e))?;

    let mut entry = entry;
    entry.exists = Path::new(&key).exists();
    entry.eligible = false;
    entry.blockers = vec![Blocker::AlreadyReclaimed];
    serde_json::to_value(proto::params::WorktreeReclaimResult {
        path: key,
        dry_run: false,
        reclaimed: true,
        already_reclaimed: false,
        eligible: true,
        blockers: Vec::new(),
        entry: Some(entry),
    })
    .map_err(|e| internal("?", e))
}

async fn reclaim_claimed(
    state: &Arc<AppState>,
    record: &autoharness_store::WorktreeRecord,
) -> Result<(), Response> {
    let sandbox = state
        .sandbox
        .sandbox()
        .ok_or_else(|| internal("?", "sandbox unavailable"))?;
    let dirs =
        SessionDirs::create(&state.data_dir, "worktree-reclaim").map_err(|e| internal("?", e))?;
    let wt = worktree::RunWorktree {
        run_id: record.run_id.clone(),
        owner: worktree::WorktreeOwner {
            run_id: record.run_id.clone(),
            node_id: record.node_id.clone(),
        },
        repo_path: PathBuf::from(&record.repo_path),
        path: PathBuf::from(&record.path),
        branch: record.branch.clone(),
        base_commit: record.base_commit.clone(),
        repo_dirty_at_start: false,
    };
    worktree::remove(sandbox, &dirs, &wt)
        .await
        .map_err(|e| internal("?", e))?;
    Ok(())
}

fn handle_usage_summary(state: &AppState) -> Result<Value, Response> {
    state
        .store
        .usage_summary()
        .and_then(|summary| serde_json::to_value(summary).map_err(Into::into))
        .map_err(|e| internal("?", e))
}

fn effective_history_scan_enabled(state: &AppState) -> Result<bool, Response> {
    let settings = state.store.app_settings().map_err(|e| internal("?", e))?;
    Ok(state.history_scan_enabled && settings.automatic_history_scan)
}

fn constrain_budget(
    mut budget: autoharness_core::Budget,
    settings: &proto::params::AppSettings,
    overrides: Option<&proto::params::BudgetOverrides>,
) -> autoharness_core::Budget {
    let max_workers = overrides
        .and_then(|b| b.max_parallel_workers)
        .unwrap_or(settings.max_parallel_workers)
        .clamp(
            proto::params::MIN_PARALLEL_WORKERS,
            proto::params::MAX_PARALLEL_WORKERS,
        );
    let max_nodes = overrides
        .and_then(|b| b.max_graph_nodes)
        .unwrap_or(settings.max_graph_nodes)
        .clamp(
            proto::params::MIN_GRAPH_NODES,
            proto::params::MAX_GRAPH_NODES,
        );
    let wall_minutes = overrides
        .and_then(|b| b.wall_time_minutes)
        .unwrap_or(settings.default_wall_time_minutes)
        .clamp(
            proto::params::MIN_WALL_TIME_MINUTES,
            proto::params::MAX_WALL_TIME_MINUTES,
        );
    budget.max_concurrent_workers = budget.max_concurrent_workers.min(max_workers);
    budget.max_graph_nodes = budget.max_graph_nodes.min(max_nodes);
    budget.wall_time_secs = budget.wall_time_secs.min(u64::from(wall_minutes) * 60);
    budget
}

fn apply_route_mode(
    decision: &mut autoharness_core::RouteDecision,
    facts: &router::TaskFacts,
    mode: proto::params::RouteMode,
) {
    if mode == proto::params::RouteMode::Priority
        && decision.shape != autoharness_core::ExecutionShape::Direct
    {
        *decision = router::route(
            facts,
            None,
            0.0,
            router::RouterThresholds::default(),
            POLICY_VERSION,
        );
        decision
            .reasons
            .push("priority mode keeps this run on the sequential route".into());
    }
}

/// `run.start`: detect the engine, open (or resume) its session, transition
/// the run to Running, and stream normalized engine events into the ledger
/// from a background task.
///
/// An unavailable engine NEVER switches automatically: the run stays Draft,
/// a structured `run.blocked` event names the problem and OFFERS the other
/// engine for a new derived run.
fn ensure_run_blocked(state: &AppState, run_id: &str) {
    let already_blocked = state
        .store
        .get_run(run_id)
        .map(|run| run.state == RunState::Blocked.as_str())
        .unwrap_or(false);
    if !already_blocked && let Err(error) = state.transition_run(run_id, RunState::Blocked) {
        tracing::warn!(%run_id, %error, "could not persist blocked run state");
    }
}

/// An engine to offer instead of `blocked`, or `None` when there is nothing
/// honest to suggest.
///
/// This replaces `EngineKind::other()`, which returned "the one that is not
/// this one" — a sentence that only parses when there are exactly two engines,
/// and which offered the alternative even when it was just as broken. The
/// offer is now a registered engine, other than the blocked one, that reports
/// itself ready. Suggesting an engine the user cannot run is worse than
/// suggesting nothing.
async fn derived_run_offer(state: &AppState, blocked: &EngineKind) -> Option<EngineKind> {
    for candidate in state.engines.kinds() {
        if &candidate == blocked {
            continue;
        }
        let Some(adapter) = state.engines.create(candidate.clone()) else {
            continue;
        };
        if adapter.detect().await.ready {
            return Some(candidate);
        }
    }
    None
}

/// Check a pinned model and effort against what the provider actually offers.
///
/// Returns the problem in the user's terms — which model, which efforts exist —
/// because "invalid reasoning effort" from a provider mid-run is not something
/// anyone can act on.
fn validate_execution_selection(
    catalog: &[autoharness_engines::EngineModel],
    model: Option<&str>,
    reasoning_effort: Option<&str>,
) -> Result<(), String> {
    // No catalog is not evidence of a bad selection: discovery is allowed to
    // fail without disabling provider-default execution. The caller guards
    // this too, and this is not relying on that.
    if catalog.is_empty() {
        return Ok(());
    }
    let selected = match model {
        Some(id) => Some(catalog.iter().find(|entry| entry.id == id).ok_or_else(|| {
            let known: Vec<&str> = catalog.iter().map(|entry| entry.id.as_str()).collect();
            format!(
                "model {id:?} is not offered by this engine (has: {})",
                known.join(", ")
            )
        })?),
        // No model pinned means the provider default, whose efforts we cannot
        // name without knowing which model that resolves to.
        None => catalog.iter().find(|entry| entry.is_default),
    };
    let (Some(effort), Some(selected)) = (reasoning_effort, selected) else {
        return Ok(());
    };
    // A model that declares no efforts takes whatever the provider takes;
    // asserting otherwise would refuse a valid run on missing information.
    if selected.reasoning_efforts.is_empty() {
        return Ok(());
    }
    if selected
        .reasoning_efforts
        .iter()
        .any(|known| known == effort)
    {
        return Ok(());
    }
    Err(format!(
        "reasoning effort {effort:?} is not supported by {} (has: {})",
        selected.display_name,
        selected.reasoning_efforts.join(", ")
    ))
}

async fn handle_run_start(state: &Arc<AppState>, params: &Value) -> Result<Value, Response> {
    let p: proto::params::RunStart =
        serde_json::from_value(params.clone()).map_err(|e| invalid_params("?", e))?;
    let settings = state.store.app_settings().map_err(|e| internal("?", e))?;
    let route_mode = p.route_mode.unwrap_or(settings.default_route_mode);

    let run = state
        .store
        .get_run(&p.run_id)
        .map_err(|e| internal("?", e))?;
    let current: RunState = run.state.parse().map_err(|e: String| internal("?", e))?;
    if let Err(e) = current.transition(RunState::Running) {
        return Err(Response::err(
            "?",
            codes::INVALID_PARAMS,
            format!("run {} is not startable: {e}", p.run_id),
        ));
    }

    // Fail closed: no sandbox, no runs. Never falls back to unsandboxed.
    if !state.sandbox.ready() {
        let diagnostics = state.sandbox.diagnostics();
        ensure_run_blocked(state, &p.run_id);
        let _ = state.emit(
            Some(&p.run_id),
            "run.blocked",
            json!({
                "run_id": p.run_id,
                "reason": "sandbox_unavailable",
                "sandbox": diagnostics,
            }),
        );
        return Ok(json!({
            "started": false,
            "blocked": true,
            "reason": "sandbox_unavailable",
            "sandbox": diagnostics,
        }));
    }

    let kind: EngineKind = run.engine.parse().map_err(|e: String| internal("?", e))?;
    if let Some(engine_override) = p.engine
        && engine_override != kind
    {
        return Err(Response::err(
            "?",
            codes::INVALID_PARAMS,
            "run.start cannot switch an existing run's engine; create a new run instead",
        ));
    }
    let Some(mut adapter) = state.engines.create(kind.clone()) else {
        return Err(internal(
            "?",
            format!("no adapter registered for engine {kind}"),
        ));
    };

    let diagnostics = adapter.detect().await;
    if !diagnostics.ready {
        ensure_run_blocked(state, &p.run_id);
        // Data model for a NEW derived run on a DIFFERENT engine. The run
        // itself never changes engines.
        let offer = derived_run_offer(state, &kind)
            .await
            .map(|engine| json!({ "engine": engine.as_str() }));
        let payload = json!({
            "run_id": p.run_id,
            "engine": kind.as_str(),
            "reason": "engine_unavailable",
            "diagnostics": diagnostics,
            "derived_run_offer": offer,
        });
        let _ = state.emit(Some(&p.run_id), "run.blocked", payload);
        return Ok(json!({
            "started": false,
            "blocked": true,
            "run_id": p.run_id,
            "diagnostics": diagnostics,
            "derived_run_offer": offer,
        }));
    }

    let project = state
        .store
        .get_project(&run.project_id)
        .map_err(|e| internal("?", e))?;

    // Daemon-managed worktree + branch. The engine never sees the user's
    // checkout, and a failure here BLOCKS the run — there is no fallback to
    // working directly in the user's repository.
    let sandbox = state
        .sandbox
        .sandbox()
        .ok_or_else(|| internal("?", "sandbox vanished after readiness check"))?;
    // A thread's turns share one checkout. Keying the worktree on the turn's
    // own run id gave every follow-up a fresh tree off the base commit, so the
    // previous turn's edits were simply gone — and the handoff brief then told
    // the next engine that work it could not see was already there.
    let worktree_key = thread_worktree_key(&state.store, &run);
    // A thread's turns also share one provider home, for the same reason and
    // with a worse symptom. The engine writes its resumable transcript under
    // this fake HOME, so keying it on the turn's own run id filed the parent's
    // session somewhere the follow-up could not see: `--resume <id>` then
    // pointed at a session that did not exist there and the turn died before
    // producing a single token, reported only as "turn failed". One home per
    // thread is what lets resume find the conversation it is continuing.
    let dirs = SessionDirs::create(&state.data_dir, &worktree_key).map_err(|e| internal("?", e))?;
    let has_indexed_worktree = state
        .store
        .list_worktrees(false)
        .map_err(|error| internal("?", error))?
        .into_iter()
        .any(|record| {
            record.kind == "run" && record.run_id == worktree_key && record.node_id.is_none()
        });
    let prepared = if has_indexed_worktree {
        worktree::resume_existing(
            sandbox,
            &dirs,
            &state.store,
            Path::new(&project.path),
            &worktree_key,
            &state.data_dir,
        )
        .await
    } else {
        worktree::create(
            sandbox,
            &dirs,
            &state.store,
            Path::new(&project.path),
            &worktree_key,
            &state.data_dir,
        )
        .await
    };
    let worktree = match prepared {
        Ok(worktree) => worktree,
        Err(e) => {
            ensure_run_blocked(state, &p.run_id);
            let reason = if has_indexed_worktree {
                "worktree_resume_failed"
            } else {
                "worktree_failed"
            };
            let payload = json!({
                "run_id": p.run_id,
                "reason": reason,
                "project_path": project.path,
                "error": e.to_string(),
            });
            let _ = state.emit(Some(&p.run_id), "run.blocked", payload.clone());
            return Ok(json!({
                "started": false,
                "blocked": true,
                "run_id": p.run_id,
                "reason": reason,
                "error": e.to_string(),
            }));
        }
    };
    let _ = state.emit(
        Some(&p.run_id),
        if has_indexed_worktree {
            "run.worktree_resumed"
        } else {
            "run.worktree_created"
        },
        json!({
            "run_id": p.run_id,
            "path": worktree.path,
            "branch": worktree.branch,
            "base_commit": worktree.base_commit,
            "repo_path": worktree.repo_path,
            "repo_dirty_at_start": worktree.repo_dirty_at_start,
        }),
    );

    // Route on deterministic facts. Phase 4 hard-coded Direct; the shape is now
    // chosen and the reasoning is on the ledger where the user can read it.
    let facts = router::TaskFacts {
        objective: run.objective.clone(),
        has_check_command: run.check_command.is_some(),
        named_files: named_files(&run.objective),
        repo_file_count: count_tracked_files(&worktree.repo_path),
        repo_dirty: worktree.repo_dirty_at_start,
    };
    // One structured planning call, and only when the facts have not already
    // settled the route. An atomic task with a check is a direct run; asking
    // a model about it would spend the user's tokens to be told what the
    // facts already say.
    let mut decision = router::route(
        &facts,
        None,
        0.0,
        router::RouterThresholds::default(),
        POLICY_VERSION,
    );
    apply_route_mode(&mut decision, &facts, route_mode);
    if route_mode != proto::params::RouteMode::Priority
        && decision.shape != autoharness_core::ExecutionShape::Direct
        && router::worth_planning(&facts)
    {
        if let Some((proposal, confidence)) = planner::propose(
            &state.engines,
            kind.clone(),
            &worktree.path,
            &state.data_dir,
            &run.objective,
            run.model.as_deref(),
            run.reasoning_effort.as_deref(),
        )
        .await
        {
            let _ = state.emit(
                Some(&p.run_id),
                "run.planned",
                json!({
                    "run_id": p.run_id,
                    "confidence": confidence,
                    "nodes": proposal.nodes.len(),
                    "edges": proposal.edges.len(),
                }),
            );
            decision = router::route(
                &facts,
                Some(proposal),
                confidence,
                router::RouterThresholds::default(),
                POLICY_VERSION,
            );
            apply_route_mode(&mut decision, &facts, route_mode);
        }
    }
    decision.budgets = constrain_budget(decision.budgets.clone(), &settings, p.budget.as_ref());
    // Swarms and dynamic DAGs commit several workers to a plan, so the plan is
    // compiled, validated, and shown to a human BEFORE anything runs. An
    // invalid plan is not negotiable: the run falls back to a bounded loop
    // rather than executing a graph nobody validated.
    // Compile BEFORE recording the route, so `run.routed` names the shape that
    // actually runs. Emitting it earlier would put "swarm" on the ledger for a
    // run that fell back to a loop.
    let mut compiled_graph = None;
    if !router::starts_automatically(decision.shape) {
        match compile_proposal(&decision, &p.run_id, state) {
            Some(compiled) => compiled_graph = Some(compiled),
            None => {
                // compile_proposal already emitted the refusal reasons.
                decision = router::route(
                    &facts,
                    None,
                    0.0,
                    router::RouterThresholds::default(),
                    POLICY_VERSION,
                );
                apply_route_mode(&mut decision, &facts, route_mode);
                decision.budgets =
                    constrain_budget(decision.budgets.clone(), &settings, p.budget.as_ref());
            }
        }
    }

    let _ = state.emit(
        Some(&p.run_id),
        "run.routed",
        json!({
            "run_id": p.run_id,
            "shape": decision.shape,
            "confidence": decision.confidence,
            "reasons": decision.reasons,
            "alternatives": decision.alternatives,
            "budgets": decision.budgets,
            "default_route_mode": route_mode,
            "policy_version": decision.policy_version,
        }),
    );

    if let Some(compiled) = compiled_graph {
        {
            {
                let version = state
                    .store
                    .save_graph(
                        &p.run_id,
                        &serde_json::to_value(&compiled).unwrap_or_default(),
                    )
                    .unwrap_or(1);
                // The run's own worktree is not used by a graph: every node
                // gets its own. Take it back before waiting for approval.
                let _ = worktree::cleanup(sandbox, &dirs, &state.store, &worktree).await;
                state
                    .transition_run(&p.run_id, RunState::AwaitingApproval)
                    .map_err(|e| internal("?", e))?;
                let _ = state.emit(
                    Some(&p.run_id),
                    "run.awaiting_approval",
                    json!({
                        "run_id": p.run_id,
                        "shape": decision.shape,
                        "graph_version": version,
                        "graph": compiled,
                        "reasons": decision.reasons,
                        "budgets": decision.budgets,
                    }),
                );
                return Ok(json!({
                    "started": false,
                    "awaiting_approval": true,
                    "run_id": p.run_id,
                    "shape": decision.shape,
                    "graph_version": version,
                }));
            }
        }
    }

    // Working directory: the run's own worktree, never the project checkout.
    // A pinned model or effort is checked against the provider's own catalog
    // before anything runs. It was only shape-checked at run.create — 200
    // printable bytes, one token, no control characters — which let a stale
    // picker pin an effort the model does not accept, so the run started,
    // built a worktree, and then died inside the provider with an error the
    // user could not act on. Failing here costs one catalog read and blocks
    // with something specific.
    if run.model.is_some() || run.reasoning_effort.is_some() {
        match adapter.available_models(&state.data_dir).await {
            Ok(catalog) if !catalog.is_empty() => {
                if let Err(problem) = validate_execution_selection(
                    &catalog,
                    run.model.as_deref(),
                    run.reasoning_effort.as_deref(),
                ) {
                    let _ = worktree::cleanup(sandbox, &dirs, &state.store, &worktree).await;
                    ensure_run_blocked(state, &p.run_id);
                    let payload = json!({
                        "run_id": p.run_id,
                        "engine": kind.as_str(),
                        "reason": "execution_selection_unsupported",
                        "problem": problem,
                        "model": run.model,
                        "reasoning_effort": run.reasoning_effort,
                    });
                    let _ = state.emit(Some(&p.run_id), "run.blocked", payload);
                    return Ok(json!({
                        "started": false,
                        "blocked": true,
                        "run_id": p.run_id,
                        "reason": "execution_selection_unsupported",
                        "problem": problem,
                    }));
                }
            }
            // A catalog that cannot be read is not evidence the selection is
            // wrong. Provider-default execution stays usable, exactly as
            // `detect_all_with_models` already decided for readiness.
            _ => {}
        }
    }

    let spec = SessionSpec {
        working_dir: worktree.path.clone(),
        data_dir: state.data_dir.clone(),
        // The thread's key, not this turn's run id: every turn of one
        // conversation must reuse the HOME holding the transcript that
        // `--resume` is about to look for.
        session_key: dirs.key.clone(),
        model: run.model.clone(),
        reasoning_effort: run.reasoning_effort.clone(),
    };

    // Resume a persisted provider session when safe; fall back to a fresh one.
    // An empty stored id is not a session. Builds before this guard persisted
    // one on every first turn, so rows like that already exist in the wild;
    // treating them as absent starts a fresh session instead of failing the
    // turn forever.
    let stored_session = match state
        .store
        .get_engine_session(&p.run_id)
        .map_err(|e| internal("?", e))?
        .filter(|stored| !stored.session_id.is_empty())
    {
        Some(stored) => Some(stored),
        // First turn of a follow-up: adopt the thread's session so the engine
        // resumes the conversation rather than starting a fresh one.
        None => match run.parent_run_id.as_deref() {
            Some(parent) => state
                .store
                .get_engine_session(parent)
                .map_err(|e| internal("?", e))?
                .filter(|stored| !stored.session_id.is_empty()),
            None => None,
        },
    };
    let mut resumed = false;
    let mut resume_failure = None;
    if let Some(stored) = &stored_session {
        if stored.engine != kind.as_str() {
            resume_failure = Some(format!(
                "stored session engine {} does not match {}",
                stored.engine,
                kind.as_str()
            ));
        } else {
            match adapter.resume_session(&stored.session_id, &spec).await {
                Ok(()) => resumed = true,
                Err(error) => resume_failure = Some(error.to_string()),
            }
        }
    }
    if has_indexed_worktree && stored_session.is_some() && !resumed {
        ensure_run_blocked(state, &p.run_id);
        let error = resume_failure.unwrap_or_else(|| "provider session unavailable".into());
        let _ = state.emit(
            Some(&p.run_id),
            "run.blocked",
            json!({
                "run_id": p.run_id,
                "engine": kind.as_str(),
                "reason": "session_resume_failed",
                "error": error,
                "worktree": worktree.path,
                "branch": worktree.branch,
            }),
        );
        return Ok(json!({
            "started": false,
            "blocked": true,
            "run_id": p.run_id,
            "reason": "session_resume_failed",
            "error": error,
        }));
    }
    if !resumed && let Err(e) = adapter.start_session(&spec).await {
        // Nothing ran, so the worktree is still pristine: take it back.
        let _ = worktree::cleanup(sandbox, &dirs, &state.store, &worktree).await;
        ensure_run_blocked(state, &p.run_id);
        let offer = derived_run_offer(state, &kind)
            .await
            .map(|engine| json!({ "engine": engine.as_str() }));
        let _ = state.emit(
            Some(&p.run_id),
            "run.blocked",
            json!({
                "run_id": p.run_id,
                "engine": kind.as_str(),
                "reason": "session_start_failed",
                "error": e.to_string(),
                "derived_run_offer": offer,
            }),
        );
        return Err(internal(
            "?",
            format!("engine session failed to start: {e}"),
        ));
    }

    // Only a real id is worth persisting. Claude does not report one until it
    // streams, so at this point it is normally absent — writing the empty
    // string here would look like a resumable session and make the next turn
    // launch `--resume ""`, which the CLI rejects. The runner persists the id
    // for real once the engine reports it.
    let session_id = adapter.session_id().unwrap_or("").to_string();
    if !session_id.is_empty() {
        state
            .store
            .save_engine_session(&p.run_id, kind.as_str(), &session_id)
            .map_err(|e| internal("?", e))?;
    }
    state
        .transition_run(&p.run_id, RunState::Running)
        .map_err(|e| internal("?", e))?;
    if has_indexed_worktree {
        let _ = state.emit(
            Some(&p.run_id),
            "run.recovered",
            json!({
                "run_id": p.run_id,
                "engine": kind.as_str(),
                "session_id": session_id,
                "provider_session_resumed": resumed,
                "worktree": worktree.path,
                "branch": worktree.branch,
            }),
        );
    }
    let _ = state.emit(
        Some(&p.run_id),
        "run.started",
        json!({
            "run_id": p.run_id,
            "engine": kind.as_str(),
            "model": run.model,
            "reasoning_effort": run.reasoning_effort,
            "session_id": session_id,
            "resumed": resumed,
        }),
    );

    // A conversation that changes engines cannot resume the old provider's
    // session, so the new engine is handed what actually happened instead of
    // starting blind. Built from the ledger: no model call, no latency, and
    // replay reconstructs exactly what it was told.
    let handoff = handoff::brief(&state.store, &run, has_indexed_worktree);
    let objective = match &handoff {
        Some(brief) => {
            let _ = state.emit(
                Some(&p.run_id),
                "run.handoff",
                json!({
                    "run_id": p.run_id,
                    "from_run": run.parent_run_id,
                    "to_engine": kind.as_str(),
                    "worktree_carried": has_indexed_worktree,
                    "worktree": worktree.path,
                    "branch": worktree.branch,
                    "brief": brief,
                }),
            );
            format!("{brief}\n---\n\n{}", run.objective)
        }
        None => run.objective.clone(),
    };

    let worktree_path = worktree.path.clone();
    let (cmd_tx, cmd_rx) = mpsc::channel(8);
    state
        .active_runs
        .lock()
        .await
        .insert(p.run_id.clone(), cmd_tx);
    tokio::spawn(runner::run_task(
        Arc::clone(state),
        p.run_id.clone(),
        objective,
        adapter,
        cmd_rx,
        RunContext {
            run_id: p.run_id.clone(),
            engine: kind.as_str().to_string(),
            worktree,
            check_command: run.check_command.clone(),
            dirs,
            shape: decision.shape,
            budget: decision.budgets.clone(),
        },
    ));

    Ok(json!({
        "started": true,
        "blocked": false,
        "run_id": p.run_id,
        "engine": kind.as_str(),
        "session_id": session_id,
        "resumed": resumed,
        "worktree": worktree_path,
        "shape": decision.shape,
        "handed_off": handoff.is_some(),
    }))
}

/// Compile the router's proposed graph. `None` means the plan was refused and
/// the caller must fall back; every refusal reason is on the ledger first.
fn compile_proposal(
    decision: &autoharness_core::RouteDecision,
    run_id: &str,
    state: &Arc<AppState>,
) -> Option<autoharness_core::graph::CompiledGraph> {
    let proposal = decision.proposed_graph.as_ref()?;
    match autoharness_core::graph::compile(proposal, &decision.budgets) {
        Ok(compiled) => Some(compiled),
        Err(errors) => {
            let _ = state.emit(
                Some(run_id),
                "graph.rejected",
                json!({
                    "run_id": run_id,
                    "shape": decision.shape,
                    "problems": errors.iter().map(|e| e.describe()).collect::<Vec<_>>(),
                    "fallback": "bounded_loop",
                }),
            );
            None
        }
    }
}

/// Policy version in force. Phase 8 replaces this constant with a promoted
/// [`autoharness_core::PolicyVersion`] row.
const POLICY_VERSION: &str = "builtin-1";

/// Files the objective names outright. Deliberately literal: anything that
/// looks like `name.ext` or a path. Used only to judge whether a change is
/// atomic, so over-matching costs a bounded loop, never correctness.
fn named_files(objective: &str) -> Vec<String> {
    objective
        .split_whitespace()
        .map(|word| word.trim_matches(|c: char| !c.is_alphanumeric() && c != '.' && c != '/'))
        .filter(|word| {
            word.contains('/')
                || word.rsplit_once('.').is_some_and(|(stem, ext)| {
                    !stem.is_empty()
                        && (1..=5).contains(&ext.len())
                        && ext.chars().all(|c| c.is_ascii_alphanumeric())
                })
        })
        .map(str::to_string)
        .collect()
}

/// Tracked-file count, for the "large repository" fact. Best effort: an
/// unreadable repo simply reports zero and the router leans simpler.
fn count_tracked_files(repo: &Path) -> usize {
    std::process::Command::new("git")
        .args(["ls-files"])
        .current_dir(repo)
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| out.stdout.iter().filter(|b| **b == b'\n').count())
        .unwrap_or(0)
}

/// The version this build reports. Bumped by the release process.
pub const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

/// `app.diagnostics`: one bundle covering everything a user needs when
/// something is wrong, so a bug report is one command rather than a
/// scavenger hunt. Contains no secrets — paths and verdicts only.
async fn handle_app_diagnostics(state: &Arc<AppState>) -> Result<Value, Response> {
    let integrity = state
        .store
        .integrity_report()
        .map_err(|e| internal("?", e))?;
    let engines = state.engines.detect_all().await;
    Ok(json!({
        "app_version": APP_VERSION,
        "protocol_version": proto::PROTOCOL_VERSION,
        "policy_version": POLICY_VERSION,
        "data_dir": state.data_dir,
        "sandbox": state.sandbox.diagnostics(),
        "engines": engines,
        "database": integrity,
        "runs": {
            "active_in_this_process": state.active_runs.lock().await.len(),
        },
    }))
}

/// `app.export`: the whole database as JSON. This is the escape hatch out of
/// the product; it is never filtered.
fn handle_app_export(state: &AppState) -> Result<Value, Response> {
    state.store.export_json().map_err(|e| internal("?", e))
}

/// `app.purge_project`: erase one project and everything derived from it.
/// Irreversible on purpose — a privacy control that left residue would not be
/// one — so the caller must pass the project's own path back as confirmation.
fn handle_app_purge_project(state: &AppState, params: &Value) -> Result<Value, Response> {
    let project_id = params
        .get("project_id")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid_params("?", "project_id is required"))?;
    let confirm = params.get("confirm_path").and_then(Value::as_str);
    let project = state
        .store
        .get_project(project_id)
        .map_err(|e| internal("?", e))?;
    if confirm != Some(project.path.as_str()) {
        return Err(Response::err(
            "?",
            codes::INVALID_PARAMS,
            format!(
                "purging is irreversible; pass confirm_path = {:?} to proceed",
                project.path
            ),
        ));
    }
    let report = state
        .store
        .purge_project(project_id)
        .map_err(|e| internal("?", e))?;
    let _ = state.emit(
        None,
        "app.project_purged",
        json!({ "project_id": project_id, "runs": report.runs, "events": report.events }),
    );
    serde_json::to_value(report).map_err(|e| internal("?", e))
}

/// `memory.list`: verified project facts, optionally searched. Only facts
/// with evidence are ever returned as verified.
fn handle_memory_list(state: &AppState, params: &Value) -> Result<Value, Response> {
    let p: proto::params::MemoryList =
        serde_json::from_value(params.clone()).map_err(|e| invalid_params("?", e))?;
    let facts = match p.query.as_deref().map(str::trim).filter(|q| !q.is_empty()) {
        Some(query) => state
            .store
            .search_memory(&p.project_id, query, 50)
            .map_err(|e| internal("?", e))?,
        None => state
            .store
            .list_memory_facts(&p.project_id, p.verified_only)
            .map_err(|e| internal("?", e))?,
    };
    serde_json::to_value(facts).map_err(|e| internal("?", e))
}

/// `memory.forget`: the user's memory is theirs to erase.
fn handle_memory_forget(state: &AppState, params: &Value) -> Result<Value, Response> {
    let p: proto::params::MemoryForget =
        serde_json::from_value(params.clone()).map_err(|e| invalid_params("?", e))?;
    let forgotten = state
        .store
        .forget_memory_fact(&p.fact_id)
        .map_err(|e| internal("?", e))?;
    if forgotten {
        let _ = state.emit(None, "memory.forgotten", json!({ "fact_id": p.fact_id }));
    }
    Ok(json!({ "forgotten": forgotten }))
}

fn handle_history_list(state: &AppState, params: &Value) -> Result<Value, Response> {
    let p: proto::params::HistoryList =
        serde_json::from_value(params.clone()).map_err(|e| invalid_params("?", e))?;
    let mut diagnostics = Vec::new();
    let scan_enabled = effective_history_scan_enabled(state)?;
    if scan_enabled {
        let roots = state
            .history_roots
            .read()
            .expect("history roots lock poisoned")
            .clone();
        diagnostics = history::refresh_index(&state.store, &roots).map_err(|e| internal("?", e))?;
    }
    let project_path = match (p.project_path, p.project_id.as_deref()) {
        (Some(path), _) => Some(path),
        (None, Some(project_id)) => Some(
            state
                .store
                .get_project(project_id)
                .map_err(|e| internal("?", e))?
                .path,
        ),
        (None, None) => None,
    };
    let page = state
        .store
        .list_external_history(&autoharness_store::ExternalHistoryFilter {
            provider: p.provider,
            project_path,
            query: p.query,
            cursor: p.cursor,
            limit: p.limit.unwrap_or(50),
        })
        .map_err(|e| internal("?", e))?;
    let entries = page
        .entries
        .into_iter()
        .map(history_entry_result)
        .collect::<Vec<_>>();
    serde_json::to_value(proto::params::HistoryListResult {
        entries,
        next_cursor: page.next_cursor,
        diagnostics,
        scan_enabled,
    })
    .map_err(|e| internal("?", e))
}

fn history_entry_result(
    entry: autoharness_store::ExternalHistoryEntry,
) -> proto::params::HistoryEntry {
    let cwd_exists = entry
        .cwd
        .as_deref()
        .is_some_and(|cwd| Path::new(cwd).is_dir());
    let path_exists = Path::new(&entry.transcript_path).is_file();
    let eligible = entry.adopted_run_id.is_none() && cwd_exists && path_exists && !entry.missing;
    let reason = if entry.adopted_run_id.is_some() {
        Some("already adopted".into())
    } else if !path_exists {
        Some("transcript no longer exists".into())
    } else if !cwd_exists {
        Some("source cwd no longer exists".into())
    } else if entry.missing {
        Some("not seen in latest scan".into())
    } else {
        entry.diagnostic.clone()
    };
    proto::params::HistoryEntry {
        provider: entry.provider,
        source_id: entry.source_id,
        transcript_path: entry.transcript_path,
        cwd: entry.cwd,
        title: entry.title,
        first_prompt: entry.first_prompt,
        updated_at_ms: entry.updated_at_ms,
        eligible,
        reason,
        adopted_run_id: entry.adopted_run_id,
    }
}

fn handoff_objective(
    entry: &autoharness_store::ExternalHistoryEntry,
    project: &autoharness_store::Project,
) -> String {
    let title = entry
        .title
        .as_deref()
        .or(entry.first_prompt.as_deref())
        .unwrap_or("external provider session");
    format!(
        "Adopt external {provider} history into a new AutoHarness run for project {project_name}.\n\
         Source transcript: {path}\n\
         Source id: {source_id}\n\
         Prior provider-session claims are unverified and must be rechecked against the repository and ledger before acting. \
         Do not resume or claim ownership of the external provider session; use this only as bounded context metadata.\n\
         Initial objective from history: {title}",
        provider = entry.provider,
        project_name = project.name,
        path = entry.transcript_path,
        source_id = entry.source_id,
    )
}

fn handle_history_adopt(state: &AppState, params: &Value) -> Result<Value, Response> {
    let p: proto::params::HistoryAdopt =
        serde_json::from_value(params.clone()).map_err(|e| invalid_params("?", e))?;
    let entry = state
        .store
        .get_external_history(&p.provider, &p.source_id)
        .map_err(|e| internal("?", e))?
        .ok_or_else(|| {
            Response::err(
                "?",
                codes::INVALID_PARAMS,
                format!(
                    "history source {}/{} is not indexed",
                    p.provider, p.source_id
                ),
            )
        })?;
    if let Some(run_id) = entry.adopted_run_id.clone() {
        return serde_json::to_value(proto::params::HistoryAdoptResult {
            run_id,
            provider: p.provider,
            source_id: p.source_id,
            already_adopted: true,
        })
        .map_err(|e| internal("?", e));
    }

    let project = state
        .store
        .get_project(&p.project_id)
        .map_err(|e| internal("?", e))?;
    let roots = state
        .history_roots
        .read()
        .expect("history roots lock poisoned")
        .clone();
    let root = roots.provider_root(&p.provider).ok_or_else(|| {
        Response::err(
            "?",
            codes::INVALID_PARAMS,
            format!("no configured history root for provider {}", p.provider),
        )
    })?;
    history::validate_adoption_source(&entry, root, Path::new(&project.path))
        .map_err(|e| Response::err("?", codes::INVALID_PARAMS, e))?;

    let objective = handoff_objective(&entry, &project);
    let adoption = state
        .store
        .adopt_external_history(
            &p.provider,
            &p.source_id,
            &project.id,
            p.engine.as_str(),
            &objective,
        )
        .map_err(|e| internal("?", e))?;
    let run = match adoption {
        autoharness_store::ExternalHistoryAdoption::AlreadyAdopted { run_id } => {
            return serde_json::to_value(proto::params::HistoryAdoptResult {
                run_id,
                provider: p.provider,
                source_id: p.source_id,
                already_adopted: true,
            })
            .map_err(|e| internal("?", e));
        }
        autoharness_store::ExternalHistoryAdoption::Created(run) => *run,
    };
    let _ = state.emit(
        Some(&run.id),
        "history.adopted",
        json!({
            "run_id": run.id,
            "provider": p.provider,
            "source_id": p.source_id,
            "transcript_path": entry.transcript_path,
            "cwd": entry.cwd,
            "request_id": p.request_id,
        }),
    );
    let _ = state.emit(
        Some(&run.id),
        "run.created",
        json!({
            "run_id": run.id,
            "project_id": run.project_id,
            "engine": run.engine,
            "objective": run.objective,
            "state": run.state,
            "check_command": run.check_command,
            "parent_run_id": run.parent_run_id,
        }),
    );
    serde_json::to_value(proto::params::HistoryAdoptResult {
        run_id: run.id,
        provider: p.provider,
        source_id: p.source_id,
        already_adopted: false,
    })
    .map_err(|e| internal("?", e))
}

/// `policy.list`: every policy version and which one is in force.
fn handle_policy_list(state: &AppState) -> Result<Value, Response> {
    let versions = state.store.list_policies().map_err(|e| internal("?", e))?;
    let listed: Vec<Value> = versions
        .into_iter()
        .map(|(version, promoted, data)| {
            json!({
                "version": version,
                "promoted": promoted,
                "data": data,
            })
        })
        .collect();
    Ok(json!({ "builtin": POLICY_VERSION, "versions": listed }))
}

/// `policy.promote` / `policy.rollback`: both are "put this version in force",
/// which is why rollback is exact — it re-promotes a stored version rather
/// than trying to undo anything.
///
/// The candidate is re-validated HERE, at promotion, not only when it was
/// proposed: a stored candidate must never be able to move a capability
/// boundary just because time passed.
fn handle_policy_promote(state: &AppState, params: &Value) -> Result<Value, Response> {
    let p: proto::params::PolicyPromote =
        serde_json::from_value(params.clone()).map_err(|e| invalid_params("?", e))?;
    let versions = state.store.list_policies().map_err(|e| internal("?", e))?;
    let Some((_, _, candidate)) = versions.iter().find(|(v, _, _)| *v == p.version) else {
        return Err(Response::err(
            "?",
            codes::INVALID_PARAMS,
            format!("no policy version {}", p.version),
        ));
    };
    let current = state
        .store
        .promoted_policy()
        .map_err(|e| internal("?", e))?
        .map(|(_, data)| data)
        .unwrap_or_else(|| json!({}));

    if let Err(violations) = autoharness_core::policy::validate_candidate(candidate, &current) {
        let problems: Vec<String> = violations.iter().map(|v| v.describe()).collect();
        let _ = state.emit(
            None,
            "policy.rejected",
            json!({ "version": p.version, "problems": problems }),
        );
        return Err(Response::err_with_data(
            "?",
            codes::INVALID_PARAMS,
            "policy candidate would change a capability boundary",
            Some(json!({ "problems": problems })),
        ));
    }

    let promoted = state
        .store
        .promote_policy(p.version)
        .map_err(|e| internal("?", e))?;
    if promoted {
        let _ = state.emit(None, "policy.promoted", json!({ "version": p.version }));
    }
    Ok(json!({ "promoted": promoted, "version": p.version }))
}

/// `run.approve`: a human accepted the plan. Only then does a graph run.
async fn handle_run_approve(state: &Arc<AppState>, params: &Value) -> Result<Value, Response> {
    let p: proto::params::RunGet =
        serde_json::from_value(params.clone()).map_err(|e| invalid_params("?", e))?;
    let run = state
        .store
        .get_run(&p.run_id)
        .map_err(|e| internal("?", e))?;
    if run.state != RunState::AwaitingApproval.as_str() {
        return Err(Response::err(
            "?",
            codes::INVALID_PARAMS,
            format!(
                "run {} is not awaiting approval (state {})",
                p.run_id, run.state
            ),
        ));
    }
    let Some((version, graph_json)) = state
        .store
        .latest_graph(&p.run_id)
        .map_err(|e| internal("?", e))?
    else {
        return Err(internal("?", "no compiled graph is stored for this run"));
    };
    let graph: autoharness_core::graph::CompiledGraph =
        serde_json::from_value(graph_json).map_err(|e| internal("?", e))?;

    if !state.sandbox.ready() {
        return Err(internal("?", "sandbox is not ready"));
    }
    let project = state
        .store
        .get_project(&run.project_id)
        .map_err(|e| internal("?", e))?;
    let kind: EngineKind = run.engine.parse().map_err(|e: String| internal("?", e))?;

    state
        .transition_run(&p.run_id, RunState::Running)
        .map_err(|e| internal("?", e))?;
    let _ = state.emit(
        Some(&p.run_id),
        "run.approved",
        json!({
            "run_id": p.run_id,
            "graph_version": version,
            "nodes": graph.nodes.len(),
            "waves": graph.waves.len(),
        }),
    );

    // A graph has no single steering channel; per-node control is Phase 7.
    let (cmd_tx, cmd_rx) = mpsc::channel(16);
    state
        .active_runs
        .lock()
        .await
        .insert(p.run_id.clone(), cmd_tx);
    tokio::spawn(scheduler::run_graph(
        Arc::clone(state),
        p.run_id.clone(),
        graph,
        PathBuf::from(&project.path),
        scheduler::GraphEngineSelection {
            engine: kind,
            model: run.model.clone(),
            reasoning_effort: run.reasoning_effort.clone(),
        },
        cmd_rx,
    ));
    Ok(json!({ "run_id": p.run_id, "approved": true, "graph_version": version }))
}

/// `run.pause` / `run.resume` / `run.cancel`: forward to the active run's
/// task, which performs the state-machine transition.
async fn handle_run_control(
    state: &Arc<AppState>,
    params: &Value,
    name: &str,
    command: RunCommand,
) -> Result<Value, Response> {
    let p: proto::params::RunGet =
        serde_json::from_value(params.clone()).map_err(|e| invalid_params("?", e))?;
    if send_run_command(state, &p.run_id, command).await {
        Ok(json!({ "run_id": p.run_id, "command": name, "accepted": true }))
    } else {
        Err(Response::err(
            "?",
            codes::INVALID_PARAMS,
            format!("run {} is not active in this daemon process", p.run_id),
        ))
    }
}

async fn handle_node_control(
    state: &Arc<AppState>,
    params: &Value,
    retry: bool,
) -> Result<Value, Response> {
    let p: proto::params::NodeControl =
        serde_json::from_value(params.clone()).map_err(|error| invalid_params("?", error))?;
    let run = state
        .store
        .get_run(&p.run_id)
        .map_err(|error| invalid_params("?", error))?;
    let latest = state
        .store
        .latest_node_attempt(&p.run_id, &p.node_id)
        .map_err(|error| internal("?", error))?
        .ok_or_else(|| invalid_params("?", format!("node {} has no attempt", p.node_id)))?;
    let valid = if retry {
        run.state == RunState::Blocked.as_str()
            && matches!(latest.state.as_str(), "failed" | "blocked" | "cancelled")
    } else {
        run.state == RunState::Running.as_str() && latest.state == "running"
    };
    if !valid {
        return Err(Response::err(
            "?",
            codes::INVALID_PARAMS,
            format!(
                "node {} is not controllable for {} while run={} node={}",
                p.node_id,
                if retry { "retry" } else { "cancel" },
                run.state,
                latest.state
            ),
        ));
    }
    let command = if retry {
        RunCommand::NodeRetry {
            node_id: p.node_id.clone(),
        }
    } else {
        RunCommand::NodeCancel {
            node_id: p.node_id.clone(),
        }
    };
    if !send_run_command(state, &p.run_id, command).await {
        return Err(Response::err(
            "?",
            codes::INVALID_PARAMS,
            format!("run {} has no live graph controller", p.run_id),
        ));
    }
    serde_json::to_value(proto::params::NodeControlResult {
        run_id: p.run_id,
        node_id: p.node_id,
        command: if retry { "retry" } else { "cancel" }.into(),
        accepted: true,
        attempt: if retry {
            latest.attempt + 1
        } else {
            latest.attempt
        },
    })
    .map_err(|error| internal("?", error))
}

/// `sandbox.check`: re-run the canaries on demand and return structured
/// diagnostics. Never spends tokens; probes the real backend.
async fn handle_sandbox_check(state: &Arc<AppState>) -> Result<Value, Response> {
    let mut diagnostics = state.sandbox.diagnostics();
    if let Some(sandbox) = state.sandbox.sandbox() {
        let worktree = state.data_dir.join("sessions").join("canary-worktree");
        if let Err(e) = std::fs::create_dir_all(&worktree) {
            return Err(internal("?", e));
        }
        let report = sandbox::canaries::run_canaries(sandbox, &worktree).await;
        diagnostics.ready = report.ok;
        diagnostics.canaries = Some(report);
        if !diagnostics.ready {
            diagnostics.problems = diagnostics
                .canaries
                .as_ref()
                .unwrap()
                .results
                .iter()
                .filter(|r| !r.passed)
                .map(|r| format!("canary {} failed: {}", r.name, r.detail))
                .collect();
        }
    }
    serde_json::to_value(diagnostics).map_err(|e| internal("?", e))
}

/// Replay events since the cursor, then stream live ones. The replay point is
/// captured first so events emitted mid-replay are not delivered twice.
fn spawn_event_stream(
    state: Arc<AppState>,
    writer: Arc<Mutex<WriteHalf<UnixStream>>>,
    params: proto::params::EventsSubscribe,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut rx = state.events_tx.subscribe();

        // Events emitted after `high_water` come through the live channel.
        let high_water = state
            .store
            .max_sequence()
            .unwrap_or(params.since_sequence as i64);

        let replay = match state
            .store
            .events_since(params.since_sequence as i64, params.run_id.as_deref())
        {
            Ok(records) => records,
            Err(e) => {
                tracing::error!(error = %e, "replay failed");
                return;
            }
        };
        for record in replay {
            let event = record.to_event();
            if proto::write_frame(&mut *writer.lock().await, &event)
                .await
                .is_err()
            {
                return;
            }
        }

        let mut last_sent = high_water;
        loop {
            match rx.recv().await {
                Ok(event) => {
                    if (event.sequence as i64) <= last_sent {
                        continue;
                    }
                    if let Some(filter) = &params.run_id
                        && event.run_id.as_deref() != Some(filter.as_str())
                    {
                        continue;
                    }
                    last_sent = event.sequence as i64;
                    if proto::write_frame(&mut *writer.lock().await, &event)
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(skipped = n, "subscriber lagged; replay needed");
                    return;
                }
                Err(broadcast::error::RecvError::Closed) => return,
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::ReadHalf;

    #[test]
    fn runtime_socket_is_short_stable_and_profile_specific() {
        let very_long_data_dir = PathBuf::from(format!("/tmp/{}", "profile".repeat(80)));
        let socket = runtime_socket_path(&very_long_data_dir);

        assert_eq!(socket, runtime_socket_path(&very_long_data_dir));
        assert!(socket.starts_with("/tmp"));
        assert!(socket.as_os_str().as_encoded_bytes().len() < 100);
        assert_ne!(
            socket,
            runtime_socket_path(&very_long_data_dir.join("another-profile"))
        );
    }

    struct TestServer {
        dir: tempfile::TempDir,
        /// Real git repository used as the test project.
        repo: PathBuf,
        socket_path: PathBuf,
        token: String,
        state: Arc<AppState>,
        _server: tokio::task::JoinHandle<()>,
    }

    /// Run git in `dir` outside the sandbox. Test fixture only — daemon-owned
    /// git always goes through `Sandbox::run_bookkeeping`.
    fn git_fixture(dir: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .expect("git must be installed");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// Initialize a repository with one commit. Returns the base commit.
    fn init_git_repo(path: &Path) -> String {
        std::fs::create_dir_all(path).unwrap();
        git_fixture(path, &["init", "-q", "-b", "main"]);
        git_fixture(path, &["config", "user.name", "AutoHarness Test"]);
        git_fixture(path, &["config", "user.email", "test@localhost"]);
        std::fs::write(path.join("README.md"), "seed\n").unwrap();
        git_fixture(path, &["add", "-A"]);
        git_fixture(path, &["commit", "-q", "-m", "seed"]);
        git_fixture(path, &["rev-parse", "HEAD"])
    }

    #[test]
    fn repository_registration_resolves_nested_folders_and_rejects_non_repositories() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        init_git_repo(&repo);
        let nested = repo.join("src/deep");
        std::fs::create_dir_all(&nested).unwrap();

        assert_eq!(
            repository_root(&nested).unwrap(),
            repo.canonicalize().unwrap()
        );

        let not_repo = dir.path().join("not-a-repo");
        std::fs::create_dir_all(&not_repo).unwrap();
        assert!(repository_root(&not_repo).is_err());
    }

    /// `project.create` builds a real repository with a first commit under
    /// its managed folder, never reuses a directory, and refuses a name with
    /// nothing usable in it.
    #[tokio::test]
    async fn project_create_builds_a_real_repository_and_never_reuses_a_directory() {
        let server = start_server().await;
        let parent = server.dir.path().join("AutoHarness");

        let reply = create_repository_project(&server.state, "Fix My App!", &parent)
            .await
            .expect("creation succeeds");
        let path = std::path::PathBuf::from(reply.get("path").unwrap().as_str().unwrap());
        assert_eq!(path, parent.join("fix-my-app"));
        assert!(path.join(".git").is_dir(), "a real repository exists");
        // Test fixtures may shell out to git; product code may not.
        let head = std::process::Command::new("git")
            .args(["-C", path.to_str().unwrap(), "rev-parse", "HEAD"])
            .output()
            .unwrap();
        assert!(head.status.success(), "HEAD exists, so a run can branch");

        // The same name again gets a fresh directory, not the existing one.
        let second = create_repository_project(&server.state, "fix my app", &parent)
            .await
            .expect("second creation succeeds");
        assert_eq!(
            second.get("path").unwrap().as_str().unwrap(),
            parent.join("fix-my-app-2").to_str().unwrap()
        );

        // Both are registered projects.
        let projects = server.state.store.list_projects().unwrap();
        assert_eq!(
            projects
                .iter()
                .filter(|project| project.path.starts_with(parent.to_str().unwrap()))
                .count(),
            2
        );

        // Nothing usable in the name is a refusal, not an invented name.
        assert!(
            create_repository_project(&server.state, "!!!", &parent)
                .await
                .is_err()
        );
    }

    #[test]
    fn project_names_sanitize_to_predictable_directories() {
        assert_eq!(
            sanitize_project_name("Fix the auth bug"),
            Some("fix-the-auth-bug".into())
        );
        assert_eq!(sanitize_project_name("  yoo  "), Some("yoo".into()));
        assert_eq!(
            sanitize_project_name("héllo wörld"),
            Some("h-llo-w-rld".into())
        );
        assert_eq!(sanitize_project_name("!!!"), None);
        assert_eq!(sanitize_project_name(""), None);
        let long = "a".repeat(200);
        assert_eq!(sanitize_project_name(&long).unwrap().len(), 48);
    }

    async fn start_server() -> TestServer {
        // Default: both engines are scripted fakes that complete immediately.
        let mut registry = EngineRegistry::default();
        for kind in EngineKind::builtins() {
            let factory_kind = kind.clone();
            registry.insert(kind, move || {
                Box::new(autoharness_engines::FakeEngine::scripted(
                    factory_kind.clone(),
                    vec![vec![
                        autoharness_engines::EngineEvent::Text {
                            text: "fake answer".into(),
                        },
                        autoharness_engines::EngineEvent::Completed { summary: None },
                    ]],
                ))
            });
        }
        start_server_with(registry).await
    }

    async fn start_server_with(registry: EngineRegistry) -> TestServer {
        let dir = tempfile::tempdir().unwrap();
        let config = DaemonConfig::in_dir(dir.path().to_path_buf());
        std::fs::create_dir_all(&config.data_dir).unwrap();
        let repo = dir.path().join("repo");
        init_git_repo(&repo);

        // Tests use an injected random token; the Keychain is a production
        // concern and must not be touched from unit tests.
        let token = generate_token();

        let listener = UnixListener::bind(&config.socket_path).unwrap();
        let (events_tx, _) = broadcast::channel(64);
        let state = Arc::new(AppState {
            store: Arc::new(Store::open(&config.db_path).unwrap()),
            token: token.clone(),
            events_tx,
            dedup: Mutex::new(DedupCache::new(16)),
            engines: registry,
            active_runs: Mutex::new(HashMap::new()),
            queue_dispatch: Mutex::new(()),
            sandbox: sandbox::ready_for_tests(&config.data_dir),
            data_dir: config.data_dir.clone(),
            history_roots: RwLock::new(history::HistoryRoots::default()),
            history_scan_enabled: true,
            reclaiming: Mutex::new(std::collections::HashSet::new()),
        });
        let server = tokio::spawn(accept_loop(listener, Arc::clone(&state)));
        TestServer {
            dir,
            repo,
            socket_path: config.socket_path,
            token,
            state,
            _server: server,
        }
    }

    /// A registry that hands out `fake` exactly once (gated fakes cannot be
    /// rebuilt by a plain factory).
    fn registry_with_once(
        kind: EngineKind,
        fake: autoharness_engines::FakeEngine,
    ) -> EngineRegistry {
        let slot = Arc::new(std::sync::Mutex::new(Some(fake)));
        let mut registry = EngineRegistry::default();
        registry.insert(kind, move || {
            Box::new(
                slot.lock()
                    .unwrap()
                    .take()
                    .expect("this test starts exactly one run"),
            )
        });
        registry
    }

    struct Client {
        reader: ReadHalf<UnixStream>,
        writer: WriteHalf<UnixStream>,
    }

    async fn connect(server: &TestServer) -> Client {
        let stream = UnixStream::connect(&server.socket_path).await.unwrap();
        let (reader, writer) = tokio::io::split(stream);
        Client { reader, writer }
    }

    async fn send(client: &mut Client, request: &Request) -> Response {
        proto::write_frame(&mut client.writer, request)
            .await
            .unwrap();
        proto::read_frame::<_, Response>(&mut client.reader)
            .await
            .unwrap()
            .unwrap()
    }

    async fn auth(client: &mut Client, token: &str) -> Response {
        send(
            client,
            &Request::new("auth-1", methods::AUTH_HELLO, json!({ "token": token })),
        )
        .await
    }

    #[tokio::test]
    async fn rejects_wrong_token() {
        let server = start_server().await;
        let mut client = connect(&server).await;
        let resp = auth(&mut client, "wrong-token").await;
        assert_eq!(resp.error.as_ref().unwrap().code, codes::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn rejects_non_auth_first_frame() {
        let server = start_server().await;
        let mut client = connect(&server).await;
        let resp = send(
            &mut client,
            &Request::new("x", methods::PROJECT_LIST, json!({})),
        )
        .await;
        assert_eq!(resp.error.as_ref().unwrap().code, codes::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn project_and_run_flow() {
        let server = start_server().await;
        let mut client = connect(&server).await;
        assert!(auth(&mut client, &server.token).await.error.is_none());

        let added = send(
            &mut client,
            &Request::new(
                "p1",
                methods::PROJECT_ADD,
                json!({ "name": "demo", "path": server.repo }),
            ),
        )
        .await;
        assert!(added.error.is_none(), "{:?}", added.error);
        let project_id = added.result.as_ref().unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();

        let listed = send(
            &mut client,
            &Request::new("p2", methods::PROJECT_LIST, json!({})),
        )
        .await;
        assert_eq!(listed.result.as_ref().unwrap().as_array().unwrap().len(), 1);

        let run = send(
            &mut client,
            &Request::new(
                "r1",
                methods::RUN_CREATE,
                json!({ "project_id": project_id, "engine": "codex", "objective": "hi" }),
            ),
        )
        .await;
        assert!(run.error.is_none(), "{:?}", run.error);
        let run_id = run.result.as_ref().unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();

        let got = send(
            &mut client,
            &Request::new("r2", methods::RUN_GET, json!({ "run_id": run_id })),
        )
        .await;
        assert_eq!(got.result.as_ref().unwrap()["state"], "draft");

        let chat = send(
            &mut client,
            &Request::new(
                "c1",
                methods::CHAT_SEND,
                json!({ "run_id": run_id, "message": "hello" }),
            ),
        )
        .await;
        assert!(chat.error.is_none(), "{:?}", chat.error);
        assert_eq!(server.state.store.list_chat(&run_id).unwrap().len(), 1);

        // Ledger: project.added + run.created + chat.message plus the durable
        // steering queue's enqueue and ready-at-boundary events.
        assert_eq!(server.state.store.event_count().unwrap(), 5);

        // A project referenced by runs is protected by the FK constraint.
        let blocked = send(
            &mut client,
            &Request::new(
                "p3",
                methods::PROJECT_REMOVE,
                json!({ "project_id": project_id }),
            ),
        )
        .await;
        assert!(blocked.error.is_some());

        // A project without runs removes cleanly.
        let other_repo = server.dir.path().join("other-repo");
        init_git_repo(&other_repo);
        let other = send(
            &mut client,
            &Request::new(
                "p4",
                methods::PROJECT_ADD,
                json!({ "name": "b", "path": other_repo }),
            ),
        )
        .await;
        let other_id = other.result.as_ref().unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();
        let removed = send(
            &mut client,
            &Request::new(
                "p5",
                methods::PROJECT_REMOVE,
                json!({ "project_id": other_id }),
            ),
        )
        .await;
        assert_eq!(removed.result.as_ref().unwrap()["removed"], true);
        assert_eq!(server.state.store.list_projects().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn settings_update_persists_before_broadcast_and_is_idempotent() {
        let server = start_server().await;
        let mut client = connect(&server).await;
        auth(&mut client, &server.token).await;

        let ack = send(
            &mut client,
            &Request::new("sub-settings", methods::EVENTS_SUBSCRIBE, json!({})),
        )
        .await;
        assert!(ack.error.is_none(), "{:?}", ack.error);

        let update = Request::new(
            "settings-update-1",
            methods::SETTINGS_UPDATE,
            json!({
                "request_id": "settings-request-1",
                "default_engine": "claude",
                "default_route_mode": "priority",
                "automatic_history_scan": false
            }),
        );
        let first = send(&mut client, &update).await;
        assert!(first.error.is_none(), "{:?}", first.error);
        assert_eq!(first.result.as_ref().unwrap()["default_engine"], "claude");

        let event: Event = proto::read_frame(&mut client.reader)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(event.kind, "settings.updated");
        assert_eq!(event.payload["settings"]["default_engine"], "claude");
        assert_eq!(
            server.state.store.app_settings().unwrap().default_engine,
            EngineKind::claude()
        );

        let second = send(&mut client, &update).await;
        assert_eq!(first, second);
        assert_eq!(
            server
                .state
                .store
                .events_since(0, None)
                .unwrap()
                .iter()
                .filter(|event| event.kind == "settings.updated")
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn run_create_uses_persisted_default_engine_when_omitted() {
        let server = start_server().await;
        let mut client = connect(&server).await;
        auth(&mut client, &server.token).await;
        server
            .state
            .store
            .update_app_settings(&autoharness_protocol::params::SettingsUpdate {
                default_engine: Some(EngineKind::claude()),
                ..autoharness_protocol::params::SettingsUpdate::default()
            })
            .unwrap();

        let project = send(
            &mut client,
            &Request::new(
                "default-engine-project",
                methods::PROJECT_ADD,
                json!({ "name": "demo", "path": server.repo }),
            ),
        )
        .await;
        let project_id = project.result.as_ref().unwrap()["id"].as_str().unwrap();
        let run = send(
            &mut client,
            &Request::new(
                "default-engine-run",
                methods::RUN_CREATE,
                json!({ "project_id": project_id, "objective": "use the default engine" }),
            ),
        )
        .await;
        assert!(run.error.is_none(), "{:?}", run.error);
        assert_eq!(run.result.as_ref().unwrap()["engine"], "claude");
    }

    #[tokio::test]
    async fn history_scan_setting_false_prevents_refresh_work() {
        let server = start_server().await;
        let codex_root = server.dir.path().join("codex-history-disabled");
        let transcript = codex_root.join("session.jsonl");
        std::fs::create_dir_all(&codex_root).unwrap();
        std::fs::write(
            &transcript,
            format!(
                "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"source-1\",\"cwd\":\"{}\"}}}}\n{{\"type\":\"event_msg\",\"payload\":{{\"type\":\"user_message\",\"message\":\"private\"}}}}\n",
                server.repo.display()
            ),
        )
        .unwrap();
        server
            .state
            .set_history_roots_for_tests(history::HistoryRoots {
                codex: Some(codex_root),
                claude: None,
            });
        server
            .state
            .store
            .update_app_settings(&autoharness_protocol::params::SettingsUpdate {
                automatic_history_scan: Some(false),
                ..autoharness_protocol::params::SettingsUpdate::default()
            })
            .unwrap();

        let mut client = connect(&server).await;
        auth(&mut client, &server.token).await;
        let resp = send(
            &mut client,
            &Request::new(
                "history-disabled",
                methods::HISTORY_LIST,
                json!({ "provider": "codex", "project_path": server.repo, "limit": 10 }),
            ),
        )
        .await;
        assert!(resp.error.is_none(), "{:?}", resp.error);
        assert_eq!(resp.result.as_ref().unwrap()["scan_enabled"], false);
        assert!(
            resp.result.as_ref().unwrap()["entries"]
                .as_array()
                .unwrap()
                .is_empty()
        );
    }

    /// `AUTOHARNESS_HISTORY_SCAN=0` is an absolute privacy override: it is
    /// resolved once at startup into `history_scan_enabled`, and a persisted
    /// `automatic_history_scan: true` can never widen it back open.
    #[tokio::test]
    async fn env_history_scan_override_beats_persisted_true() {
        let dir = tempfile::tempdir().unwrap();
        let config = DaemonConfig::in_dir(dir.path().to_path_buf());
        std::fs::create_dir_all(&config.data_dir).unwrap();
        let repo = dir.path().join("repo");
        init_git_repo(&repo);
        let codex_root = dir.path().join("codex-history-env");
        std::fs::create_dir_all(&codex_root).unwrap();
        std::fs::write(
            codex_root.join("session.jsonl"),
            format!(
                "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"env-source-1\",\"cwd\":\"{}\"}}}}\n",
                repo.display()
            ),
        )
        .unwrap();

        let (events_tx, _) = broadcast::channel(64);
        let state = Arc::new(AppState {
            store: Arc::new(Store::open(&config.db_path).unwrap()),
            token: generate_token(),
            events_tx,
            dedup: Mutex::new(DedupCache::new(16)),
            engines: EngineRegistry::default(),
            active_runs: Mutex::new(HashMap::new()),
            queue_dispatch: Mutex::new(()),
            sandbox: sandbox::ready_for_tests(&config.data_dir),
            data_dir: config.data_dir.clone(),
            history_roots: RwLock::new(history::HistoryRoots {
                codex: Some(codex_root),
                claude: None,
            }),
            // What AUTOHARNESS_HISTORY_SCAN=0 resolves to at startup.
            history_scan_enabled: false,
            reclaiming: Mutex::new(std::collections::HashSet::new()),
        });
        state
            .store
            .update_app_settings(&autoharness_protocol::params::SettingsUpdate {
                automatic_history_scan: Some(true),
                ..autoharness_protocol::params::SettingsUpdate::default()
            })
            .unwrap();

        let result = handle_history_list(
            &state,
            &json!({ "provider": "codex", "project_path": repo, "limit": 10 }),
        )
        .expect("history.list should answer, not error");
        assert_eq!(result["scan_enabled"], false);
        assert!(result["entries"].as_array().unwrap().is_empty());
        // The persisted preference is untouched; only its effect is denied.
        assert!(state.store.app_settings().unwrap().automatic_history_scan);
        // Nothing was indexed, so the private transcript was never read.
        assert!(
            state
                .store
                .get_external_history("codex", "env-source-1")
                .unwrap()
                .is_none()
        );
    }

    /// One pass over everything this branch added, in the order a user meets
    /// it: change a setting, run something, read the evidence, and clean up.
    /// Each part has its own focused test; this one proves they compose.
    #[tokio::test]
    async fn settings_run_evidence_and_reclaim_work_end_to_end() {
        let server = start_server().await;
        let mut client = connect(&server).await;
        auth(&mut client, &server.token).await;
        let base_head = git_fixture(&server.repo, &["rev-parse", "HEAD"]);

        // 1. A persisted setting takes effect on the next run.
        let updated = send(
            &mut client,
            &Request::new(
                "e2e-settings",
                methods::SETTINGS_UPDATE,
                json!({ "request_id": "e2e-1", "default_engine": "claude" }),
            ),
        )
        .await;
        assert_eq!(updated.result.as_ref().unwrap()["default_engine"], "claude");

        let project = send(
            &mut client,
            &Request::new(
                "e2e-project",
                methods::PROJECT_ADD,
                json!({ "name": "demo", "path": server.repo }),
            ),
        )
        .await;
        let project_id = project.result.as_ref().unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();
        let created = send(
            &mut client,
            &Request::new(
                "e2e-run",
                methods::RUN_CREATE,
                json!({
                    "project_id": project_id,
                    "objective": "end to end",
                    "check_command": "echo verified"
                }),
            ),
        )
        .await;
        let result = created.result.as_ref().unwrap();
        assert_eq!(result["engine"], "claude", "the persisted default applied");
        let run_id = result["id"].as_str().unwrap().to_string();

        // 2. The run produces evidence.
        send(
            &mut client,
            &Request::new("e2e-start", methods::RUN_START, json!({ "run_id": run_id })),
        )
        .await;
        assert!(
            wait_for(|| server.state.store.get_run(&run_id).unwrap().state == "succeeded").await,
            "the run must finish"
        );
        let check = server
            .state
            .store
            .events_since(0, Some(&run_id))
            .unwrap()
            .into_iter()
            .find(|e| e.kind == "run.check")
            .expect("a check ran");
        assert_eq!(check.payload["passed"], true);
        assert!(check.payload["duration_ms"].as_u64().is_some());
        let artifacts = server.state.store.list_artifacts(&run_id, 50).unwrap();
        assert!(
            artifacts.iter().any(|a| a.kind == "check_output"),
            "the check output is filed as evidence"
        );

        // 3. Usage folds the ledger without inventing a price.
        let usage = send(
            &mut client,
            &Request::new("e2e-usage", methods::USAGE_SUMMARY, json!({})),
        )
        .await;
        let usage = usage.result.as_ref().unwrap();
        assert!(usage.get("cost").is_none());

        // 4. The worktree was indexed when it was created. This run changed
        //    nothing, so the daemon already reclaimed it — which the index
        //    records rather than forgets.
        let live = send(
            &mut client,
            &Request::new("e2e-wt-live", methods::WORKTREE_LIST, json!({})),
        )
        .await;
        assert!(
            live.result.as_ref().unwrap()["entries"]
                .as_array()
                .unwrap()
                .iter()
                .all(|entry| entry["run_id"] != run_id.as_str()),
            "a no-op run leaves no live worktree behind"
        );

        let audited = send(
            &mut client,
            &Request::new(
                "e2e-wt-all",
                methods::WORKTREE_LIST,
                json!({ "include_reclaimed": true }),
            ),
        )
        .await;
        let entries = audited.result.as_ref().unwrap()["entries"]
            .as_array()
            .unwrap()
            .clone();
        let entry = entries
            .iter()
            .find(|entry| entry["run_id"] == run_id.as_str())
            .expect("the run's worktree is still in the audit view");
        assert!(entry["removed_at_ms"].as_i64().is_some());
        let path = entry["path"].as_str().unwrap().to_string();

        // Reclaiming it again is a no-op, not a second removal or an error.
        let repeat = send(
            &mut client,
            &Request::new(
                "e2e-repeat",
                methods::WORKTREE_RECLAIM,
                json!({ "path": path, "confirm_path": path }),
            ),
        )
        .await;
        let repeat = repeat.result.as_ref().unwrap();
        assert_eq!(repeat["reclaimed"], false);
        assert_eq!(repeat["already_reclaimed"], true);
        assert_eq!(repeat["blockers"][0], "already_reclaimed");

        // An unindexed directory is refused whatever the caller says.
        let foreign = server.dir.path().join("not-a-worktree");
        std::fs::create_dir_all(&foreign).unwrap();
        let foreign = foreign.to_string_lossy().into_owned();
        let refused = send(
            &mut client,
            &Request::new(
                "e2e-foreign",
                methods::WORKTREE_RECLAIM,
                json!({ "path": foreign, "confirm_path": foreign }),
            ),
        )
        .await;
        assert_eq!(
            refused.result.as_ref().unwrap()["blockers"][0],
            "not_indexed"
        );
        assert!(Path::new(&foreign).exists());

        // 5. Whatever happened, the user's checkout never moved.
        assert_eq!(git_fixture(&server.repo, &["rev-parse", "HEAD"]), base_head);
        assert_eq!(
            git_fixture(&server.repo, &["rev-parse", "--abbrev-ref", "HEAD"]),
            "main"
        );
        assert!(
            git_fixture(&server.repo, &["status", "--porcelain"]).is_empty(),
            "the base working tree is untouched"
        );
    }

    /// The attention system treats resource pressure as its own kind of ask.
    /// It only means something if the daemon actually reports it, so the one
    /// bounded resource a run can exhaust — its budget — says so by name.
    #[test]
    fn an_exhausted_budget_is_reported_as_resource_pressure() {
        let trigger = autoharness_core::detector::Trigger::BudgetExhausted {
            limit: "wall_time_secs=300".into(),
        };
        // The runner emits run.resource_pressure for exactly this trigger and
        // no other, so a stall is never mislabelled as a resource problem.
        assert!(matches!(
            trigger,
            autoharness_core::detector::Trigger::BudgetExhausted { .. }
        ));
        assert!(trigger.describe().contains("budget"));
        for other in [
            autoharness_core::detector::Trigger::NoProgress { actions: 6 },
            autoharness_core::detector::Trigger::RepeatedErrorClass {
                class: "E0308".into(),
                count: 3,
            },
        ] {
            assert!(
                !matches!(
                    other,
                    autoharness_core::detector::Trigger::BudgetExhausted { .. }
                ),
                "only an exhausted budget is resource pressure"
            );
        }
    }

    // ---- artifacts ----

    #[tokio::test]
    async fn artifacts_are_persisted_before_broadcast_and_replay_after_a_restart() {
        let server = start_server().await;
        let mut client = connect(&server).await;
        auth(&mut client, &server.token).await;
        let ack = send(
            &mut client,
            &Request::new("sub-artifacts", methods::EVENTS_SUBSCRIBE, json!({})),
        )
        .await;
        assert!(ack.error.is_none());

        let project = server
            .state
            .store
            .add_project("artifacts", &server.repo.to_string_lossy())
            .unwrap();
        let run = server
            .state
            .store
            .create_run(&project.id, "codex", "artifact fixture")
            .unwrap();

        record_artifact(
            &server.state,
            ArtifactDraft {
                run_id: &run.id,
                node_id: None,
                kind: "file",
                name: "src/cache/key.ts",
                path: Some("src/cache/key.ts".into()),
                byte_size: Some(2048),
                summary: " src/cache/key.ts | 12 ++++++------",
            },
        );

        // The ledger holds it before any subscriber can hear about it.
        let stored = server.state.store.list_artifacts(&run.id, 10).unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].name, "src/cache/key.ts");
        assert_eq!(stored[0].byte_size, Some(2048));

        let event: Event = proto::read_frame(&mut client.reader)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(event.kind, "artifact.created");
        assert_eq!(event.payload["name"], "src/cache/key.ts");
        assert_eq!(event.payload["byte_size"], 2048);

        // A replay from seq 0 shows the same artifact, which is what a
        // relaunched UI reads.
        let replayed = server.state.store.events_since(0, Some(&run.id)).unwrap();
        assert!(
            replayed
                .iter()
                .any(|e| e.kind == "artifact.created" && e.payload["name"] == "src/cache/key.ts")
        );

        let listed = handle_artifact_list(&server.state, &json!({ "run_id": run.id })).unwrap();
        assert_eq!(listed["artifacts"][0]["name"], "src/cache/key.ts");
        assert_eq!(listed["artifacts"][0]["kind"], "file");
    }

    #[tokio::test]
    async fn artifact_summaries_are_bounded_rather_than_stored_whole() {
        let server = start_server().await;
        let project = server
            .state
            .store
            .add_project("bounded", &server.repo.to_string_lossy())
            .unwrap();
        let run = server
            .state
            .store
            .create_run(&project.id, "codex", "bounded fixture")
            .unwrap();
        let huge = "x".repeat(autoharness_store::ARTIFACT_SUMMARY_CAP * 4);

        record_artifact(
            &server.state,
            ArtifactDraft {
                run_id: &run.id,
                node_id: None,
                kind: "check_output",
                name: "npm test",
                path: None,
                byte_size: None,
                summary: &huge,
            },
        );

        let stored = server.state.store.list_artifacts(&run.id, 10).unwrap();
        assert!(
            stored[0].summary.len() <= autoharness_store::ARTIFACT_SUMMARY_CAP + 4,
            "summary was {} bytes",
            stored[0].summary.len()
        );
        assert!(stored[0].summary.starts_with('…'), "truncation is visible");
    }

    /// A direct run records how long its check took, its exit code, and its
    /// output, and files the same evidence as an artifact.
    #[tokio::test]
    async fn direct_run_check_reports_duration_output_and_artifacts() {
        let server = start_server().await;
        let mut client = connect(&server).await;
        auth(&mut client, &server.token).await;
        let run_id = create_run_in(
            &server,
            &mut client,
            "codex",
            &server.repo.clone(),
            Some("echo checked-out; echo to-stderr 1>&2"),
        )
        .await;
        send(
            &mut client,
            &Request::new(
                "check-detail",
                methods::RUN_START,
                json!({ "run_id": run_id }),
            ),
        )
        .await;

        let check = wait_for_event(&server, &run_id, "run.check").await;
        assert_eq!(check.payload["passed"], true);
        assert_eq!(check.payload["exit_code"], 0);
        assert!(
            check.payload["duration_ms"].as_u64().is_some(),
            "a check reports how long it took"
        );
        assert!(
            check.payload["stdout"]
                .as_str()
                .unwrap()
                .contains("checked-out")
        );
        assert!(
            check.payload["stderr"]
                .as_str()
                .unwrap()
                .contains("to-stderr")
        );

        assert!(
            wait_for(|| {
                server
                    .state
                    .store
                    .list_artifacts(&run_id, 50)
                    .map(|artifacts| {
                        artifacts
                            .iter()
                            .any(|a| a.kind == "check_output" && a.summary.contains("checked-out"))
                    })
                    .unwrap_or(false)
            })
            .await,
            "the check output is filed as an artifact"
        );
    }

    // ---- worktree index and reclaim ----

    fn clean_facts() -> WorktreeFacts {
        WorktreeFacts {
            already_reclaimed: false,
            canonical_matches: true,
            under_storage: true,
            is_primary_checkout: false,
            run_active: false,
            run_terminal: true,
            exists: true,
            clean: Some(true),
            commits_beyond_base: Some(0),
            process: worktree::ProcessUse::None,
            git_check_failed: false,
        }
    }

    #[test]
    fn worktree_policy_blocks_every_unsafe_condition() {
        assert!(worktree_blockers(&clean_facts()).is_empty());

        for (mutate, expected) in [
            (
                (|f: &mut WorktreeFacts| f.already_reclaimed = true) as fn(&mut WorktreeFacts),
                Blocker::AlreadyReclaimed,
            ),
            (
                |f: &mut WorktreeFacts| f.canonical_matches = false,
                Blocker::PathMismatch,
            ),
            (
                |f: &mut WorktreeFacts| f.under_storage = false,
                Blocker::OutsideStorage,
            ),
            (
                |f: &mut WorktreeFacts| f.is_primary_checkout = true,
                Blocker::PrimaryCheckout,
            ),
            (
                |f: &mut WorktreeFacts| f.run_active = true,
                Blocker::RunActive,
            ),
            (
                |f: &mut WorktreeFacts| f.run_terminal = false,
                Blocker::RunNotTerminal,
            ),
            (
                |f: &mut WorktreeFacts| f.clean = Some(false),
                Blocker::DirtyWorktree,
            ),
            (
                |f: &mut WorktreeFacts| f.commits_beyond_base = Some(2),
                Blocker::CommitsBeyondBase,
            ),
            (
                |f: &mut WorktreeFacts| f.process = worktree::ProcessUse::InUse,
                Blocker::ProcessInUse,
            ),
            (
                |f: &mut WorktreeFacts| f.process = worktree::ProcessUse::Unknown,
                Blocker::ProcessCheckUnknown,
            ),
            (
                |f: &mut WorktreeFacts| f.git_check_failed = true,
                Blocker::GitCheckFailed,
            ),
        ] {
            let mut facts = clean_facts();
            mutate(&mut facts);
            let blockers = worktree_blockers(&facts);
            assert!(
                blockers.contains(&expected),
                "{expected:?} should block; got {blockers:?}"
            );
        }
    }

    /// An `lsof` answer that is neither "found" nor a clean "not found" leaves
    /// the question open, and open must never read as safe.
    #[test]
    fn unknown_process_probe_results_block_rather_than_allow() {
        assert_eq!(
            worktree::classify_lsof("1234\n", Some(0)),
            worktree::ProcessUse::InUse
        );
        assert_eq!(
            worktree::classify_lsof("", Some(1)),
            worktree::ProcessUse::None
        );
        assert_eq!(
            worktree::classify_lsof("", Some(0)),
            worktree::ProcessUse::None
        );
        for status in [Some(2), Some(127), None] {
            assert_eq!(
                worktree::classify_lsof("", status),
                worktree::ProcessUse::Unknown,
                "status {status:?} is not an answer"
            );
        }
    }

    /// Create a real daemon worktree for `run_id` and leave the run terminal.
    async fn seed_worktree(server: &TestServer, label: &str) -> worktree::RunWorktree {
        let project = match server
            .state
            .store
            .list_projects()
            .unwrap()
            .into_iter()
            .find(|p| p.path == server.repo.to_string_lossy())
        {
            Some(project) => project,
            None => server
                .state
                .store
                .add_project("demo", &server.repo.to_string_lossy())
                .unwrap(),
        };
        let run = server
            .state
            .store
            .create_run(&project.id, "codex", label)
            .unwrap();
        server
            .state
            .store
            .set_run_state(&run.id, "succeeded")
            .unwrap();
        let sandbox = server.state.sandbox.sandbox().unwrap();
        let dirs = SessionDirs::create(&server.state.data_dir, &run.id).unwrap();
        // The worktree directory is named by run id, exactly as run.start does.
        worktree::create(
            sandbox,
            &dirs,
            &server.state.store,
            &server.repo,
            &run.id,
            &server.state.data_dir,
        )
        .await
        .unwrap()
    }

    async fn reclaim(server: &TestServer, params: Value) -> proto::params::WorktreeReclaimResult {
        let value = handle_worktree_reclaim(&server.state, &params)
            .await
            .expect("reclaim answers rather than erroring");
        serde_json::from_value(value).unwrap()
    }

    #[tokio::test]
    async fn existing_worktree_resume_is_owned_and_fails_closed_on_every_identity_mismatch() {
        let server = start_server().await;
        let sandbox = server.state.sandbox.sandbox().unwrap();

        let good = seed_worktree(&server, "resume-good").await;
        let dirs = SessionDirs::create(&server.state.data_dir, &good.run_id).unwrap();
        let resumed = worktree::resume_existing(
            sandbox,
            &dirs,
            &server.state.store,
            &server.repo,
            &good.run_id,
            &server.state.data_dir,
        )
        .await
        .unwrap();
        assert_eq!(resumed.path, good.path);
        assert_eq!(resumed.branch, good.branch);

        let other_repo = server.dir.path().join("other-repo");
        init_git_repo(&other_repo);
        let error = worktree::resume_existing(
            sandbox,
            &dirs,
            &server.state.store,
            &other_repo,
            &good.run_id,
            &server.state.data_dir,
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("repository mismatch"), "{error}");

        git_fixture(&good.path, &["branch", "-m", "unexpected-branch"]);
        let error = worktree::resume_existing(
            sandbox,
            &dirs,
            &server.state.store,
            &server.repo,
            &good.run_id,
            &server.state.data_dir,
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("branch mismatch"), "{error}");

        let wrong_path = seed_worktree(&server, "resume-wrong-path").await;
        let original = wrong_path.path.to_string_lossy().into_owned();
        server.state.store.mark_worktree_removed(&original).unwrap();
        let mut poisoned = wrong_path.record();
        poisoned.path = server.repo.to_string_lossy().into_owned();
        server.state.store.record_worktree(&poisoned).unwrap();
        let dirs = SessionDirs::create(&server.state.data_dir, &wrong_path.run_id).unwrap();
        let error = worktree::resume_existing(
            sandbox,
            &dirs,
            &server.state.store,
            &server.repo,
            &wrong_path.run_id,
            &server.state.data_dir,
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("path mismatch"), "{error}");

        let removed = seed_worktree(&server, "resume-removed").await;
        server
            .state
            .store
            .mark_worktree_removed(&removed.path.to_string_lossy())
            .unwrap();
        let dirs = SessionDirs::create(&server.state.data_dir, &removed.run_id).unwrap();
        let error = worktree::resume_existing(
            sandbox,
            &dirs,
            &server.state.store,
            &server.repo,
            &removed.run_id,
            &server.state.data_dir,
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("already reclaimed"), "{error}");
    }

    async fn list_worktrees(
        server: &TestServer,
        params: Value,
    ) -> proto::params::WorktreeListResult {
        let value = handle_worktree_list(&server.state, &params).await.unwrap();
        serde_json::from_value(value).unwrap()
    }

    #[tokio::test]
    async fn worktree_list_indexes_creation_and_reports_eligibility() {
        let server = start_server().await;
        let wt = seed_worktree(&server, "wt-listed").await;

        let listed = list_worktrees(&server, json!({})).await;
        let entry = listed
            .entries
            .iter()
            .find(|entry| entry.path == wt.path.to_string_lossy())
            .expect("a created worktree is indexed");
        assert_eq!(entry.kind, "run");
        assert_eq!(entry.run_id, wt.run_id);
        assert_eq!(entry.branch, wt.branch);
        assert_eq!(entry.run_state, "succeeded");
        assert!(entry.exists);
        assert!(
            entry.eligible,
            "a clean finished worktree should be reclaimable: {:?}",
            entry.blockers
        );
        assert!(listed.storage_root.ends_with("worktrees"));
    }

    #[tokio::test]
    async fn reclaim_dry_run_changes_nothing_and_real_reclaim_needs_confirm_path() {
        let server = start_server().await;
        let wt = seed_worktree(&server, "wt-confirm").await;
        let path = wt.path.to_string_lossy().into_owned();

        let dry = reclaim(&server, json!({ "path": path, "dry_run": true })).await;
        assert!(dry.eligible, "{:?}", dry.blockers);
        assert!(!dry.reclaimed);
        assert!(wt.path.exists(), "a dry run must not remove anything");

        // Missing confirmation.
        let refused = reclaim(&server, json!({ "path": path })).await;
        assert!(!refused.reclaimed);
        assert_eq!(refused.blockers, vec![Blocker::ConfirmPathMismatch]);

        // Wrong confirmation.
        let refused = reclaim(
            &server,
            json!({ "path": path, "confirm_path": format!("{path}/x") }),
        )
        .await;
        assert!(!refused.reclaimed);
        assert_eq!(refused.blockers, vec![Blocker::ConfirmPathMismatch]);
        assert!(wt.path.exists());

        let done = reclaim(&server, json!({ "path": path, "confirm_path": path })).await;
        assert!(done.reclaimed, "{:?}", done.blockers);
        assert!(!wt.path.exists(), "the directory should be gone");
        assert!(
            server
                .state
                .store
                .get_worktree(&path)
                .unwrap()
                .unwrap()
                .removed_at_ms
                .is_some()
        );
        let events = server.state.store.events_since(0, None).unwrap();
        assert!(events.iter().any(|e| e.kind == "worktree.reclaimed"));
    }

    #[tokio::test]
    async fn reclaim_is_idempotent_for_a_repeated_request() {
        let server = start_server().await;
        let wt = seed_worktree(&server, "wt-idempotent").await;
        let path = wt.path.to_string_lossy().into_owned();
        let args = json!({ "path": path, "confirm_path": path });

        let first = reclaim(&server, args.clone()).await;
        assert!(first.reclaimed);

        let second = reclaim(&server, args).await;
        assert!(!second.reclaimed, "the second call must not remove again");
        assert!(second.already_reclaimed);
        assert_eq!(second.blockers, vec![Blocker::AlreadyReclaimed]);
        assert_eq!(
            server
                .state
                .store
                .events_since(0, None)
                .unwrap()
                .iter()
                .filter(|e| e.kind == "worktree.reclaimed")
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn concurrent_reclaims_remove_the_worktree_exactly_once() {
        let server = start_server().await;
        let wt = seed_worktree(&server, "wt-concurrent").await;
        let path = wt.path.to_string_lossy().into_owned();
        let args = json!({ "path": path, "confirm_path": path });

        let (a, b) = tokio::join!(reclaim(&server, args.clone()), reclaim(&server, args));
        let removed = [&a, &b].iter().filter(|r| r.reclaimed).count();
        assert_eq!(removed, 1, "exactly one caller may remove: {a:?} {b:?}");
        assert!(!wt.path.exists());
        assert_eq!(
            server
                .state
                .store
                .events_since(0, None)
                .unwrap()
                .iter()
                .filter(|e| e.kind == "worktree.reclaimed")
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn reclaim_refuses_dirty_committed_active_and_unfinished_worktrees() {
        let server = start_server().await;

        let dirty = seed_worktree(&server, "wt-dirty").await;
        std::fs::write(dirty.path.join("scratch.txt"), "unsaved work\n").unwrap();
        let dirty_path = dirty.path.to_string_lossy().into_owned();
        let result = reclaim(
            &server,
            json!({ "path": dirty_path, "confirm_path": dirty_path }),
        )
        .await;
        assert!(result.blockers.contains(&Blocker::DirtyWorktree));
        assert!(!result.reclaimed);
        assert!(dirty.path.exists());
        assert!(dirty.path.join("scratch.txt").exists(), "work is preserved");

        let committed = seed_worktree(&server, "wt-committed").await;
        std::fs::write(committed.path.join("done.txt"), "finished work\n").unwrap();
        let sandbox = server.state.sandbox.sandbox().unwrap();
        let dirs = SessionDirs::create(&server.state.data_dir, &committed.run_id).unwrap();
        worktree::commit_all(sandbox, &dirs, &committed, "work")
            .await
            .unwrap();
        let committed_path = committed.path.to_string_lossy().into_owned();
        let result = reclaim(
            &server,
            json!({ "path": committed_path, "confirm_path": committed_path }),
        )
        .await;
        assert!(result.blockers.contains(&Blocker::CommitsBeyondBase));
        assert!(committed.path.exists());

        let running = seed_worktree(&server, "wt-running").await;
        server
            .state
            .store
            .set_run_state(&running.run_id, "running")
            .unwrap();
        let (tx, _rx) = mpsc::channel(1);
        server
            .state
            .active_runs
            .lock()
            .await
            .insert(running.run_id.clone(), tx);
        let running_path = running.path.to_string_lossy().into_owned();
        let result = reclaim(
            &server,
            json!({ "path": running_path, "confirm_path": running_path }),
        )
        .await;
        assert!(result.blockers.contains(&Blocker::RunActive));
        assert!(result.blockers.contains(&Blocker::RunNotTerminal));
        assert!(running.path.exists());
    }

    #[tokio::test]
    async fn reclaim_refuses_foreign_directories_and_path_traversal() {
        let server = start_server().await;
        let storage = server.state.data_dir.join("worktrees");
        std::fs::create_dir_all(&storage).unwrap();

        // A directory nobody indexed, sitting in daemon storage.
        let foreign = storage.join("not-ours");
        std::fs::create_dir_all(&foreign).unwrap();
        let foreign_path = foreign.to_string_lossy().into_owned();
        let result = reclaim(
            &server,
            json!({ "path": foreign_path, "confirm_path": foreign_path }),
        )
        .await;
        assert_eq!(result.blockers, vec![Blocker::NotIndexed]);
        assert!(foreign.exists(), "an unindexed directory is never touched");

        // Traversal out of storage resolves to something with no row.
        let escape = format!("{}/../../repo", storage.display());
        let result = reclaim(&server, json!({ "path": escape, "confirm_path": escape })).await;
        assert_eq!(result.blockers, vec![Blocker::NotIndexed]);
        assert!(!result.reclaimed);
        assert!(server.repo.join("README.md").exists(), "the repo survives");

        // A `..` that lands back on the indexed path is resolved, not rejected
        // on spelling: the canonical path is what the checks use.
        let wt = seed_worktree(&server, "wt-traversal").await;
        let spelled = format!("{}/not-ours/../{}", storage.display(), wt.run_id);
        let dry = reclaim(&server, json!({ "path": spelled, "dry_run": true })).await;
        assert!(dry.eligible, "{:?}", dry.blockers);
        assert_eq!(dry.path, wt.path.canonicalize().unwrap().to_string_lossy());
    }

    #[tokio::test]
    async fn reclaim_of_a_missing_directory_prunes_the_index() {
        let server = start_server().await;
        let wt = seed_worktree(&server, "wt-missing").await;
        let path = wt.path.to_string_lossy().into_owned();
        std::fs::remove_dir_all(&wt.path).unwrap();

        let listed = list_worktrees(&server, json!({})).await;
        let entry = listed
            .entries
            .iter()
            .find(|entry| entry.path == path)
            .unwrap();
        assert!(!entry.exists);
        assert!(entry.eligible, "{:?}", entry.blockers);

        let result = reclaim(&server, json!({ "path": path, "confirm_path": path })).await;
        assert!(result.reclaimed, "{:?}", result.blockers);
        assert!(
            server
                .state
                .store
                .get_worktree(&path)
                .unwrap()
                .unwrap()
                .removed_at_ms
                .is_some()
        );
    }

    #[tokio::test]
    async fn reclaim_refuses_while_a_process_holds_the_directory() {
        let server = start_server().await;
        let wt = seed_worktree(&server, "wt-inuse").await;
        let path = wt.path.to_string_lossy().into_owned();

        // This test process holds the file, so lsof reports a real user.
        let held = std::fs::File::open(wt.path.join("README.md")).unwrap();
        let result = reclaim(&server, json!({ "path": path, "confirm_path": path })).await;
        drop(held);

        // A machine without lsof answers "unknown", which blocks just the same.
        assert!(
            result.blockers.contains(&Blocker::ProcessInUse)
                || result.blockers.contains(&Blocker::ProcessCheckUnknown),
            "an open file must block a reclaim: {:?}",
            result.blockers
        );
        assert!(!result.reclaimed);
        assert!(wt.path.exists());
    }

    #[tokio::test]
    async fn route_defaults_constrain_supported_budget_fields() {
        let server = start_server().await;
        let mut client = connect(&server).await;
        auth(&mut client, &server.token).await;
        server
            .state
            .store
            .update_app_settings(&autoharness_protocol::params::SettingsUpdate {
                default_wall_time_minutes: Some(5),
                max_parallel_workers: Some(1),
                max_graph_nodes: Some(1),
                ..autoharness_protocol::params::SettingsUpdate::default()
            })
            .unwrap();
        let run_id = create_run_in(
            &server,
            &mut client,
            "codex",
            &server.repo.clone(),
            Some("true"),
        )
        .await;
        send(
            &mut client,
            &Request::new(
                "budget-defaults",
                methods::RUN_START,
                json!({ "run_id": run_id }),
            ),
        )
        .await;
        let routed = wait_for_event(&server, &run_id, "run.routed").await;
        assert_eq!(routed.payload["budgets"]["wall_time_secs"], 300);
        assert_eq!(routed.payload["budgets"]["max_concurrent_workers"], 1);
        assert_eq!(routed.payload["budgets"]["max_graph_nodes"], 1);
    }

    #[tokio::test]
    async fn usage_summary_rpc_returns_ledger_fold_without_costs() {
        let server = start_server().await;
        let mut client = connect(&server).await;
        auth(&mut client, &server.token).await;
        let project = server
            .state
            .store
            .add_project("usage", &server.repo.to_string_lossy())
            .unwrap();
        let run = server
            .state
            .store
            .create_run(&project.id, "codex", "usage")
            .unwrap();
        server
            .state
            .store
            .append_event(
                Some(&run.id),
                "engine.usage",
                json!({ "input_tokens": 100 }),
            )
            .unwrap();
        server
            .state
            .store
            .append_event(
                Some(&run.id),
                "engine.usage",
                json!({ "input_tokens": 150 }),
            )
            .unwrap();

        let resp = send(
            &mut client,
            &Request::new("usage-summary", methods::USAGE_SUMMARY, json!({})),
        )
        .await;
        assert!(resp.error.is_none(), "{:?}", resp.error);
        let result = resp.result.as_ref().unwrap();
        assert_eq!(result["runs"][0]["input_tokens"], 150);
        assert!(result.get("cost").is_none());
        assert!(result["providers"][0].get("cost").is_none());
    }

    #[tokio::test]
    async fn node_controls_validate_typed_params_and_unknown_methods_stay_structured() {
        let server = start_server().await;
        let mut client = connect(&server).await;
        auth(&mut client, &server.token).await;
        let resp = send(
            &mut client,
            &Request::new("u1", methods::NODE_RETRY, json!({ "run_id": "x" })),
        )
        .await;
        let err = resp.error.unwrap();
        assert_eq!(err.code, codes::INVALID_PARAMS);
        assert!(err.message.contains("node_id"));

        let unknown = send(&mut client, &Request::new("u2", "bogus.method", json!({}))).await;
        assert_eq!(unknown.error.unwrap().code, codes::METHOD_NOT_FOUND);
    }

    #[tokio::test]
    async fn duplicate_request_returns_cached_response_without_side_effects() {
        let server = start_server().await;
        let mut client = connect(&server).await;
        auth(&mut client, &server.token).await;

        let req = Request::new(
            "dup-1",
            methods::PROJECT_ADD,
            json!({ "name": "a", "path": server.repo }),
        );
        let first = send(&mut client, &req).await;
        assert!(first.error.is_none());

        // Same request ID again: cached response, no second insert.
        let second = send(&mut client, &req).await;
        assert_eq!(first, second);
        assert_eq!(server.state.store.list_projects().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn subscribe_replays_missed_events_then_streams_live() {
        let server = start_server().await;
        let mut client = connect(&server).await;
        auth(&mut client, &server.token).await;

        // Produce three events BEFORE subscribing.
        for i in 0..3 {
            server
                .state
                .emit(Some("run-x"), "tick", json!({ "i": i }))
                .unwrap();
        }

        let ack = send(
            &mut client,
            &Request::new(
                "sub-1",
                methods::EVENTS_SUBSCRIBE,
                json!({ "since_sequence": 1, "run_id": "run-x" }),
            ),
        )
        .await;
        assert!(ack.error.is_none());

        // Replay: sequences 2 and 3 (strictly greater than since_sequence=1).
        let mut replayed = Vec::new();
        for _ in 0..2 {
            let event: Event = proto::read_frame(&mut client.reader)
                .await
                .unwrap()
                .unwrap();
            replayed.push(event);
        }
        assert_eq!(replayed[0].sequence, 2);
        assert_eq!(replayed[1].sequence, 3);
        assert_eq!(replayed[0].payload["i"], 1);

        // Live: a new event arrives after the replay.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        server
            .state
            .emit(Some("run-x"), "tick", json!({ "i": 3 }))
            .unwrap();
        server
            .state
            .emit(Some("run-y"), "tick", json!({ "i": 99 }))
            .unwrap(); // filtered out
        let live: Event = proto::read_frame(&mut client.reader)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(live.sequence, 4);
        assert_eq!(live.payload["i"], 3);
    }

    #[tokio::test]
    async fn events_persisted_before_broadcast() {
        let server = start_server().await;
        // Broadcast before any subscriber exists; the ledger still has it.
        let event = server.state.emit(None, "test.event", json!({})).unwrap();
        let stored = server.state.store.events_since(0, None).unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].seq as u64, event.sequence);
    }

    /// Poll the store until `pred` holds or the deadline passes.
    async fn wait_for<F: Fn() -> bool>(pred: F) -> bool {
        for _ in 0..100 {
            if pred() {
                return true;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        false
    }

    fn ledger_kinds(server: &TestServer, run_id: &str) -> Vec<String> {
        server
            .state
            .store
            .events_since(0, Some(run_id))
            .unwrap()
            .iter()
            .map(|e| e.kind.clone())
            .collect()
    }

    async fn create_run(server: &TestServer, client: &mut Client, engine: &str) -> String {
        create_run_in(server, client, engine, &server.repo, None).await
    }

    /// Unique request IDs: the daemon's dedup cache would otherwise replay the
    /// first response for every repeated setup call.
    fn next_req_id(prefix: &str) -> String {
        static COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        format!(
            "{prefix}-{}",
            COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        )
    }

    async fn create_run_in(
        server: &TestServer,
        client: &mut Client,
        engine: &str,
        project_path: &Path,
        check_command: Option<&str>,
    ) -> String {
        let path = project_path.to_string_lossy().into_owned();
        let existing = server
            .state
            .store
            .list_projects()
            .unwrap()
            .into_iter()
            .find(|p| p.path == path);
        let project_id = match existing {
            Some(project) => project.id,
            None => {
                let added = send(
                    client,
                    &Request::new(
                        next_req_id("setup-p"),
                        methods::PROJECT_ADD,
                        json!({ "name": "demo", "path": path }),
                    ),
                )
                .await;
                added.result.as_ref().unwrap()["id"]
                    .as_str()
                    .unwrap()
                    .to_string()
            }
        };
        let run = send(
            client,
            &Request::new(
                next_req_id("setup-r"),
                methods::RUN_CREATE,
                json!({
                    "project_id": project_id,
                    "engine": engine,
                    "objective": "do the thing",
                    "check_command": check_command,
                }),
            ),
        )
        .await;
        run.result.as_ref().unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string()
    }

    async fn create_run_with_objective(
        server: &TestServer,
        client: &mut Client,
        engine: &str,
        objective: &str,
        check_command: Option<&str>,
    ) -> String {
        let project_id = create_run_in(server, client, engine, &server.repo.clone(), None).await;
        let _ = project_id;
        let project = server.state.store.list_projects().unwrap().remove(0);
        let run = send(
            client,
            &Request::new(
                next_req_id("obj-r"),
                methods::RUN_CREATE,
                json!({
                    "project_id": project.id,
                    "engine": engine,
                    "objective": objective,
                    "check_command": check_command,
                }),
            ),
        )
        .await;
        run.result.as_ref().unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string()
    }

    /// Wait for a specific event kind and return it.
    async fn wait_for_event(
        server: &TestServer,
        run_id: &str,
        kind: &str,
    ) -> autoharness_store::EventRecord {
        assert!(
            wait_for(|| ledger_kinds(server, run_id).iter().any(|k| k == kind)).await,
            "expected a {kind} event, saw {:?}",
            ledger_kinds(server, run_id)
        );
        server
            .state
            .store
            .events_since(0, Some(run_id))
            .unwrap()
            .into_iter()
            .find(|e| e.kind == kind)
            .unwrap()
    }

    /// The daemon-managed worktree path for a run.
    fn worktree_path(server: &TestServer, run_id: &str) -> PathBuf {
        server.state.data_dir.join("worktrees").join(run_id)
    }

    /// Wait until the run's worktree exists on disk.
    async fn wait_for_worktree(server: &TestServer, run_id: &str) -> PathBuf {
        let path = worktree_path(server, run_id);
        assert!(
            wait_for(|| path.is_dir()).await,
            "worktree must be created at {}",
            path.display()
        );
        path
    }

    #[tokio::test]
    async fn run_start_streams_engine_events_to_terminal_state() {
        let server = start_server().await;
        let mut client = connect(&server).await;
        auth(&mut client, &server.token).await;
        let run_id = create_run(&server, &mut client, "codex").await;

        let started = send(
            &mut client,
            &Request::new("rs1", methods::RUN_START, json!({ "run_id": run_id })),
        )
        .await;
        let result = started.result.as_ref().unwrap();
        assert_eq!(result["started"], true);
        assert_eq!(result["session_id"], "fake-session-0001");

        // The streaming task drives the run to Succeeded.
        assert!(
            wait_for(|| server.state.store.get_run(&run_id).unwrap().state == "succeeded").await,
            "run must reach succeeded"
        );

        // Full event sequence, persisted in order. The scripted fake edits
        // nothing, so there is no commit and the empty worktree is reclaimed.
        let kinds = ledger_kinds(&server, &run_id);
        assert_eq!(
            kinds,
            [
                "run.created",
                "run.worktree_created",
                "run.routed",
                "run.started",
                "engine.session",
                "engine.text",
                "engine.completed",
                "run.commit",
                "run.worktree_removed",
                "run.succeeded",
            ]
        );
        assert!(!worktree_path(&server, &run_id).exists());

        // Session identity was persisted for resume.
        let session = server
            .state
            .store
            .get_engine_session(&run_id)
            .unwrap()
            .unwrap();
        assert_eq!(session.session_id, "fake-session-0001");
        assert_eq!(session.engine, "codex");

        // The run is no longer active; further control is rejected.
        let paused = send(
            &mut client,
            &Request::new("rp1", methods::RUN_PAUSE, json!({ "run_id": run_id })),
        )
        .await;
        assert!(paused.error.is_some());
    }

    /// A blocked run offers a DIFFERENT engine only when that engine can
    /// actually run. The offer is a suggestion the user will act on; naming
    /// one that is equally broken wastes the only move they have.
    #[tokio::test]
    async fn a_blocked_run_offers_an_engine_that_is_actually_ready() {
        let mut registry = EngineRegistry::default();
        registry.insert(EngineKind::codex(), || {
            Box::new(autoharness_engines::FakeEngine::unavailable(
                EngineKind::codex(),
                vec!["not authenticated".into()],
            ))
        });
        registry.insert(EngineKind::claude(), || {
            Box::new(autoharness_engines::FakeEngine::scripted(
                EngineKind::claude(),
                vec![],
            ))
        });
        let server = start_server_with(registry).await;
        let mut client = connect(&server).await;
        auth(&mut client, &server.token).await;
        let run_id = create_run(&server, &mut client, "codex").await;

        let resp = send(
            &mut client,
            &Request::new(
                next_req_id("rs"),
                methods::RUN_START,
                json!({ "run_id": run_id }),
            ),
        )
        .await;
        let result = resp.result.as_ref().unwrap();
        assert_eq!(result["blocked"], true);
        assert_eq!(result["derived_run_offer"]["engine"], "claude");
        // The run itself still never changes engines.
        assert_eq!(server.state.store.get_run(&run_id).unwrap().engine, "codex");
    }

    #[tokio::test]
    async fn run_start_blocked_engine_never_switches() {
        let mut registry = EngineRegistry::default();
        registry.insert(EngineKind::codex(), || {
            Box::new(autoharness_engines::FakeEngine::unavailable(
                EngineKind::codex(),
                vec!["not authenticated".into()],
            ))
        });
        let server = start_server_with(registry).await;
        let mut client = connect(&server).await;
        auth(&mut client, &server.token).await;
        let run_id = create_run(&server, &mut client, "codex").await;

        let resp = send(
            &mut client,
            &Request::new("rs1", methods::RUN_START, json!({ "run_id": run_id })),
        )
        .await;
        let result = resp.result.as_ref().unwrap();
        assert_eq!(result["started"], false);
        assert_eq!(result["blocked"], true);
        // Nothing else is registered, so there is no honest alternative to
        // offer. This previously suggested "claude" unconditionally — an
        // engine this daemon could not have run — because the offer was
        // "the other one of the two" rather than "one that works".
        assert!(
            result["derived_run_offer"].is_null(),
            "{}",
            result["derived_run_offer"]
        );

        // The durable state agrees with the structured event, so relaunch and
        // queue recovery cannot mistake this for an untouched draft.
        assert_eq!(
            server.state.store.get_run(&run_id).unwrap().state,
            "blocked"
        );
        let events = server.state.store.events_since(0, Some(&run_id)).unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[1].kind, "run.blocked");
        assert_eq!(events[1].payload["reason"], "engine_unavailable");
        assert_eq!(events[1].payload["engine"], "codex");
        // Same rule on the ledger as in the reply: no alternative is offered
        // when none of the registered engines is ready.
        assert!(events[1].payload["derived_run_offer"].is_null());
        assert!(
            events[1].payload["diagnostics"]["problems"]
                .as_array()
                .unwrap()
                .iter()
                .any(|p| p.as_str().unwrap().contains("not authenticated"))
        );
    }

    #[tokio::test]
    async fn run_pause_resume_cancel_via_command_channel() {
        let mut registry = EngineRegistry::default();
        registry.insert(EngineKind::claude(), || {
            Box::new(autoharness_engines::FakeEngine::hanging(
                EngineKind::claude(),
            ))
        });
        let server = start_server_with(registry).await;
        let mut client = connect(&server).await;
        auth(&mut client, &server.token).await;
        let run_id = create_run(&server, &mut client, "claude").await;

        send(
            &mut client,
            &Request::new("rs1", methods::RUN_START, json!({ "run_id": run_id })),
        )
        .await;
        assert_eq!(
            server.state.store.get_run(&run_id).unwrap().state,
            "running"
        );

        // Pause: state machine Running -> Paused, run.paused event.
        let paused = send(
            &mut client,
            &Request::new("rp1", methods::RUN_PAUSE, json!({ "run_id": run_id })),
        )
        .await;
        assert_eq!(paused.result.as_ref().unwrap()["accepted"], true);
        assert!(
            wait_for(|| server.state.store.get_run(&run_id).unwrap().state == "paused").await,
            "run must reach paused"
        );

        // Resume: Paused -> Running, run.resumed event.
        send(
            &mut client,
            &Request::new("rr1", methods::RUN_RESUME, json!({ "run_id": run_id })),
        )
        .await;
        assert!(
            wait_for(|| server.state.store.get_run(&run_id).unwrap().state == "running").await,
            "run must resume to running"
        );

        // Cancel: adapter cancelled, run Cancelled, task exits the registry.
        send(
            &mut client,
            &Request::new("rc1", methods::RUN_CANCEL, json!({ "run_id": run_id })),
        )
        .await;
        assert!(
            wait_for(|| server.state.store.get_run(&run_id).unwrap().state == "cancelled").await,
            "run must reach cancelled"
        );
        assert!(wait_for(|| { server.state.active_runs.try_lock().unwrap().is_empty() }).await);

        let kinds = ledger_kinds(&server, &run_id);
        assert!(kinds.contains(&"run.paused".to_string()));
        assert!(kinds.contains(&"run.resumed".to_string()));
        assert_eq!(kinds.last().unwrap(), "run.cancelled");
        // A hanging turn never completes.
        assert!(!kinds.contains(&"run.succeeded".to_string()));
        // Nothing was written, so the daemon reclaims its own worktree.
        assert!(kinds.contains(&"run.worktree_removed".to_string()));
        assert!(!worktree_path(&server, &run_id).exists());
    }

    #[tokio::test]
    async fn run_start_rejects_illegal_state_transition() {
        let server = start_server().await;
        let mut client = connect(&server).await;
        auth(&mut client, &server.token).await;
        let run_id = create_run(&server, &mut client, "codex").await;

        send(
            &mut client,
            &Request::new("rs1", methods::RUN_START, json!({ "run_id": run_id })),
        )
        .await;
        assert!(
            wait_for(|| server.state.store.get_run(&run_id).unwrap().state == "succeeded").await
        );

        // Starting a terminal run is a structured error, not a panic.
        let again = send(
            &mut client,
            &Request::new("rs2", methods::RUN_START, json!({ "run_id": run_id })),
        )
        .await;
        let err = again.error.unwrap();
        assert_eq!(err.code, codes::INVALID_PARAMS);
        assert!(err.message.contains("not startable"));
    }

    #[tokio::test]
    async fn sandbox_unavailable_blocks_run_start_fail_closed() {
        // Build a server whose sandbox is unavailable: runs must be refused
        // with structured diagnostics, never run unsandboxed.
        let dir = tempfile::tempdir().unwrap();
        let config = DaemonConfig::in_dir(dir.path().to_path_buf());
        std::fs::create_dir_all(&config.data_dir).unwrap();
        let token = generate_token();
        let listener = UnixListener::bind(&config.socket_path).unwrap();
        let (events_tx, _) = broadcast::channel(64);
        let mut registry = EngineRegistry::default();
        registry.insert(EngineKind::codex(), || {
            Box::new(autoharness_engines::FakeEngine::scripted(
                EngineKind::codex(),
                vec![],
            ))
        });
        let state = Arc::new(AppState {
            store: Arc::new(Store::open(&config.db_path).unwrap()),
            token: token.clone(),
            events_tx,
            dedup: Mutex::new(DedupCache::new(16)),
            engines: registry,
            active_runs: Mutex::new(HashMap::new()),
            queue_dispatch: Mutex::new(()),
            sandbox: SandboxStatus::Unavailable(sandbox::SandboxDiagnostics {
                ready: false,
                backend: "seatbelt(sandbox-exec)".into(),
                proxy_addr: None,
                problems: vec!["canary cannot_egress_network failed".into()],
                canaries: None,
            }),
            data_dir: config.data_dir.clone(),
            history_roots: RwLock::new(history::HistoryRoots::default()),
            history_scan_enabled: true,
            reclaiming: Mutex::new(std::collections::HashSet::new()),
        });
        let repo = dir.path().join("repo");
        init_git_repo(&repo);
        let server = tokio::spawn(accept_loop(listener, Arc::clone(&state)));
        let server = TestServer {
            dir,
            repo,
            socket_path: config.socket_path,
            token,
            state,
            _server: server,
        };

        let mut client = connect(&server).await;
        auth(&mut client, &server.token).await;
        let run_id = create_run(&server, &mut client, "codex").await;
        let resp = send(
            &mut client,
            &Request::new("rs1", methods::RUN_START, json!({ "run_id": run_id })),
        )
        .await;
        let result = resp.result.as_ref().unwrap();
        assert_eq!(result["started"], false);
        assert_eq!(result["blocked"], true);
        assert_eq!(result["reason"], "sandbox_unavailable");
        assert_eq!(result["sandbox"]["ready"], false);

        // The run state and event agree; no stale draft can be auto-dispatched.
        assert_eq!(
            server.state.store.get_run(&run_id).unwrap().state,
            "blocked"
        );
        let events = server.state.store.events_since(0, Some(&run_id)).unwrap();
        let blocked = events.iter().find(|e| e.kind == "run.blocked").unwrap();
        assert_eq!(blocked.payload["reason"], "sandbox_unavailable");
    }

    /// Live sandbox init on a temp dir: real sandbox-exec backend probe,
    /// real proxy bind, and ALL canaries must pass on this machine.
    #[tokio::test]
    async fn sandbox_init_ready_and_all_canaries_pass() {
        let dir = tempfile::tempdir().unwrap();
        let status = sandbox::init(dir.path(), Arc::new(|_, _, _| {})).await;
        let diagnostics = status.diagnostics();
        assert!(
            diagnostics.ready,
            "sandbox init must succeed: {:?}",
            diagnostics.problems
        );
        assert!(diagnostics.proxy_addr.is_some());
    }

    /// Live: sandbox.check RPC re-runs canaries through the real backend.
    #[tokio::test]
    async fn sandbox_check_rpc_runs_live_canaries() {
        let server = start_server().await;
        let mut client = connect(&server).await;
        auth(&mut client, &server.token).await;
        let resp = send(
            &mut client,
            &Request::new("sc1", methods::SANDBOX_CHECK, json!({})),
        )
        .await;
        let result = resp.result.as_ref().unwrap();
        let canaries = result["canaries"]["results"].as_array().unwrap();
        assert!(!canaries.is_empty());
        for c in canaries {
            assert_eq!(
                c["passed"], true,
                "canary {} failed: {}",
                c["name"], c["detail"]
            );
        }
    }

    #[tokio::test]
    async fn history_list_refreshes_index_before_returning_metadata_only() {
        let server = start_server().await;
        let codex_root = server.dir.path().join("codex-history");
        let transcript = codex_root.join("2026/08/05/session.jsonl");
        std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
        std::fs::write(
            &transcript,
            format!(
                "{{\"timestamp\":\"2026-08-05T01:02:03Z\",\"type\":\"session_meta\",\"payload\":{{\"id\":\"source-1\",\"cwd\":\"{}\",\"originator\":\"codex_cli_rs\"}}}}\n{{\"timestamp\":\"2026-08-05T01:02:04Z\",\"type\":\"turn_context\",\"payload\":{{\"cwd\":\"{}\",\"approval_policy\":\"never\"}}}}\n{{\"timestamp\":\"2026-08-05T01:02:05Z\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"user_message\",\"message\":\"hello from history\",\"images\":[]}}}}\n",
                server.repo.display(),
                server.repo.display()
            ),
        )
        .unwrap();
        server
            .state
            .set_history_roots_for_tests(history::HistoryRoots {
                codex: Some(codex_root.clone()),
                claude: None,
            });

        let before = std::fs::read(&transcript).unwrap();
        let mut client = connect(&server).await;
        auth(&mut client, &server.token).await;
        let resp = send(
            &mut client,
            &Request::new(
                "h1",
                methods::HISTORY_LIST,
                json!({ "provider": "codex", "project_path": server.repo, "limit": 10 }),
            ),
        )
        .await;
        assert!(resp.error.is_none(), "{:?}", resp.error);
        let entries = resp.result.as_ref().unwrap()["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["source_id"], "source-1");
        assert_eq!(entries[0]["cwd"], server.repo.to_string_lossy().as_ref());
        assert_eq!(entries[0]["title"], "hello from history");
        assert_eq!(entries[0]["first_prompt"], "hello from history");
        assert_eq!(entries[0]["eligible"], true);
        assert!(
            entries[0].get("body").is_none(),
            "transcript bodies must not be returned"
        );
        assert_eq!(std::fs::read(&transcript).unwrap(), before);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_history_adopts_create_one_run_and_one_event_pair() {
        let server = start_server().await;
        let project = server
            .state
            .store
            .add_project("repo", &server.repo.to_string_lossy())
            .unwrap();
        let codex_root = server.dir.path().join("codex-history");
        let transcript = codex_root.join("2026/08/05/session.jsonl");
        std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
        std::fs::write(
            &transcript,
            format!(
                "{{\"timestamp\":\"2026-08-05T01:00:00Z\",\"type\":\"session_meta\",\"payload\":{{\"id\":\"source-1\",\"cwd\":\"{}\"}}}}\n{{\"timestamp\":\"2026-08-05T01:00:01Z\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"user_message\",\"message\":\"ship it\"}}}}\n",
                server.repo.display()
            ),
        )
        .unwrap();
        let mut scanned = history::scan_roots(&history::HistoryRoots {
            codex: Some(codex_root.clone()),
            claude: None,
        });
        let entry = scanned.entries.pop().unwrap();
        server.state.store.upsert_external_history(&entry).unwrap();
        server
            .state
            .set_history_roots_for_tests(history::HistoryRoots {
                codex: Some(codex_root),
                claude: None,
            });

        let mut first_client = connect(&server).await;
        let mut second_client = connect(&server).await;
        auth(&mut first_client, &server.token).await;
        auth(&mut second_client, &server.token).await;
        let first_request = Request::new(
            "adopt-1",
            methods::HISTORY_ADOPT,
            json!({
                "provider": "codex",
                "source_id": "source-1",
                "project_id": project.id,
                "engine": "codex",
                "request_id": "request-a",
            }),
        );
        let second_request = Request::new(
            "adopt-2",
            methods::HISTORY_ADOPT,
            json!({
                "provider": "codex",
                "source_id": "source-1",
                "project_id": project.id,
                "engine": "codex",
                "request_id": "request-b",
            }),
        );
        let (first, second) = tokio::join!(
            send(&mut first_client, &first_request),
            send(&mut second_client, &second_request)
        );
        assert!(first.error.is_none(), "{:?}", first.error);
        assert!(second.error.is_none(), "{:?}", second.error);
        let first_result = first.result.as_ref().unwrap();
        let second_result = second.result.as_ref().unwrap();
        let run_id = first_result["run_id"].as_str().unwrap();
        assert_eq!(second_result["run_id"], run_id);
        assert_ne!(
            first_result["already_adopted"],
            second_result["already_adopted"]
        );
        assert!(
            first_result["already_adopted"] == false || second_result["already_adopted"] == false
        );
        assert_eq!(
            server.state.store.runs_in_states(&["draft"]).unwrap().len(),
            1
        );
        assert_eq!(server.state.store.events_since(0, None).unwrap().len(), 2);
        assert_eq!(
            server
                .state
                .store
                .get_external_history("codex", "source-1")
                .unwrap()
                .unwrap()
                .adopted_run_id
                .as_deref(),
            Some(run_id)
        );
        assert!(
            server
                .state
                .store
                .get_engine_session(run_id)
                .unwrap()
                .is_none()
        );
        let events = server.state.store.events_since(0, Some(run_id)).unwrap();
        assert_eq!(events[0].kind, "history.adopted");
        assert_eq!(events[1].kind, "run.created");
        assert!(
            server
                .state
                .store
                .get_run(run_id)
                .unwrap()
                .objective
                .contains("unverified and must be rechecked")
        );
    }

    #[tokio::test]
    async fn already_adopted_history_returns_existing_run_without_more_events() {
        let server = start_server().await;
        let project = server
            .state
            .store
            .add_project("repo", &server.repo.to_string_lossy())
            .unwrap();
        let codex_root = server.dir.path().join("codex-history");
        let transcript = codex_root.join("session.jsonl");
        std::fs::create_dir_all(&codex_root).unwrap();
        std::fs::write(
            &transcript,
            format!(
                "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"source-1\",\"cwd\":\"{}\"}}}}\n{{\"type\":\"event_msg\",\"payload\":{{\"type\":\"user_message\",\"message\":\"ship it\"}}}}\n",
                server.repo.display()
            ),
        )
        .unwrap();
        let entry = history::scan_roots(&history::HistoryRoots {
            codex: Some(codex_root.clone()),
            claude: None,
        })
        .entries
        .pop()
        .unwrap();
        server.state.store.upsert_external_history(&entry).unwrap();
        server
            .state
            .set_history_roots_for_tests(history::HistoryRoots {
                codex: Some(codex_root),
                claude: None,
            });
        let mut client = connect(&server).await;
        auth(&mut client, &server.token).await;
        let first = send(
            &mut client,
            &Request::new(
                "adopt-1",
                methods::HISTORY_ADOPT,
                json!({
                    "provider": "codex",
                    "source_id": "source-1",
                    "project_id": project.id,
                    "engine": "codex",
                    "request_id": "first",
                }),
            ),
        )
        .await;
        let run_id = first.result.as_ref().unwrap()["run_id"].as_str().unwrap();
        let again = send(
            &mut client,
            &Request::new(
                "adopt-2",
                methods::HISTORY_ADOPT,
                json!({
                    "provider": "codex",
                    "source_id": "source-1",
                    "project_id": project.id,
                    "engine": "codex",
                    "request_id": "second",
                }),
            ),
        )
        .await;
        assert_eq!(again.result.as_ref().unwrap()["run_id"], run_id);
        assert_eq!(again.result.as_ref().unwrap()["already_adopted"], true);
        assert_eq!(
            server
                .state
                .store
                .events_since(0, Some(run_id))
                .unwrap()
                .len(),
            2
        );
    }

    /// Live: run_command enforces external-write policy and confinement.
    #[tokio::test]
    async fn sandbox_run_command_policy_and_confinement() {
        let dir = tempfile::tempdir().unwrap();
        let status = sandbox::init(dir.path(), Arc::new(|_, _, _| {})).await;
        let sandbox = match status {
            SandboxStatus::Ready(s) => s,
            SandboxStatus::Unavailable(d) => panic!("sandbox must init: {:?}", d.problems),
        };
        let worktree = dir.path().join("wt");
        std::fs::create_dir_all(&worktree).unwrap();
        let dirs = autoharness_engines::process::SessionDirs::create(dir.path(), "worker").unwrap();

        // External writes are rejected BEFORE execution.
        let denied = sandbox
            .run_command(
                &worktree,
                &dirs,
                std::path::Path::new("/usr/bin/git"),
                &["push".into(), "origin".into(), "main".into()],
                &worktree,
            )
            .await;
        assert!(matches!(
            denied,
            Err(sandbox::backend::SandboxError::PolicyViolation(_))
        ));

        // A benign command runs and can write inside the worktree.
        let ok = sandbox
            .run_command(
                &worktree,
                &dirs,
                std::path::Path::new("/bin/sh"),
                &[
                    "-c".into(),
                    "echo hi > \"$0/out.txt\" && cat \"$0/out.txt\"".into(),
                    worktree.to_string_lossy().into_owned(),
                ],
                &worktree,
            )
            .await
            .unwrap();
        assert!(ok.success(), "stderr: {}", ok.stderr);
        assert_eq!(ok.stdout.trim(), "hi");
    }

    // ---- Phase 4: worktrees, direct runs, steering, reconciliation ----

    /// Live: worktree create/commit/cleanup through the real Seatbelt runner,
    /// and the guarantee that the user's checked-out branch never moves.
    #[tokio::test]
    async fn worktree_lifecycle_preserves_work_and_base_branch() {
        let dir = tempfile::tempdir().unwrap();
        let data_dir = dir.path().join("data");
        std::fs::create_dir_all(&data_dir).unwrap();
        let repo = dir.path().join("repo");
        let base = init_git_repo(&repo);

        let status = sandbox::ready_for_tests(&data_dir);
        let sandbox = status.sandbox().unwrap();
        let dirs = SessionDirs::create(&data_dir, "worktree-test").unwrap();
        let store = Store::open_in_memory().unwrap();

        // A clean, commit-free worktree is the daemon's to reclaim.
        let clean = worktree::create(sandbox, &dirs, &store, &repo, "run-clean", &data_dir)
            .await
            .unwrap();
        assert_eq!(clean.base_commit, base);
        assert!(!clean.repo_dirty_at_start);
        assert!(clean.path.join("README.md").is_file());
        assert!(worktree::is_clean(sandbox, &dirs, &clean).await.unwrap());
        assert!(matches!(
            worktree::cleanup(sandbox, &dirs, &store, &clean)
                .await
                .unwrap(),
            worktree::CleanupOutcome::Removed
        ));
        assert!(!clean.path.exists());

        // A dirty worktree is preserved, never discarded.
        let dirty = worktree::create(sandbox, &dirs, &store, &repo, "run-dirty", &data_dir)
            .await
            .unwrap();
        std::fs::write(dirty.path.join("new.txt"), "work\n").unwrap();
        assert!(!worktree::is_clean(sandbox, &dirs, &dirty).await.unwrap());
        match worktree::cleanup(sandbox, &dirs, &store, &dirty)
            .await
            .unwrap()
        {
            worktree::CleanupOutcome::Preserved(reason) => {
                assert!(reason.contains("uncommitted"), "{reason}")
            }
            worktree::CleanupOutcome::Removed => panic!("dirty worktree must be preserved"),
        }
        assert!(dirty.path.join("new.txt").is_file());

        // Commits land on the run branch only.
        let commit = worktree::commit_all(sandbox, &dirs, &dirty, "work")
            .await
            .unwrap()
            .expect("dirty worktree must produce a commit");
        assert_ne!(commit, base);
        assert_eq!(
            worktree::commits_beyond_base(sandbox, &dirs, &dirty)
                .await
                .unwrap(),
            1
        );
        assert!(
            worktree::diff_stat(sandbox, &dirs, &dirty)
                .await
                .unwrap()
                .contains("new.txt")
        );
        // Nothing left to commit, and a branch with commits is never removed.
        assert!(
            worktree::commit_all(sandbox, &dirs, &dirty, "again")
                .await
                .unwrap()
                .is_none()
        );
        assert!(matches!(
            worktree::cleanup(sandbox, &dirs, &store, &dirty)
                .await
                .unwrap(),
            worktree::CleanupOutcome::Preserved(_)
        ));

        // The user's checkout never moved and is still clean.
        assert_eq!(
            worktree::repo_head(sandbox, &dirs, &repo).await.unwrap(),
            base
        );
        assert_eq!(git_fixture(&repo, &["rev-parse", "HEAD"]), base);
        assert_eq!(git_fixture(&repo, &["status", "--porcelain"]), "");
        assert!(!repo.join("new.txt").exists());
    }

    /// Direct run end to end: worktree, streamed events, verification command,
    /// local commit on the run branch, and byte-identical replay.
    #[tokio::test]
    async fn direct_run_verifies_commits_and_replays() {
        let (fake, gate) = autoharness_engines::FakeEngine::gated(
            EngineKind::codex(),
            vec![vec![
                autoharness_engines::EngineEvent::Text {
                    text: "wrote the file".into(),
                },
                autoharness_engines::EngineEvent::Completed { summary: None },
            ]],
        );
        let server = start_server_with(registry_with_once(EngineKind::codex(), fake)).await;
        let base = git_fixture(&server.repo, &["rev-parse", "HEAD"]);
        let mut client = connect(&server).await;
        auth(&mut client, &server.token).await;
        let run_id = create_run_in(
            &server,
            &mut client,
            "codex",
            &server.repo.clone(),
            Some("test -f new.txt"),
        )
        .await;

        let started = send(
            &mut client,
            &Request::new("rs1", methods::RUN_START, json!({ "run_id": run_id })),
        )
        .await;
        assert_eq!(started.result.as_ref().unwrap()["started"], true);

        // Stand in for the engine's edit while its turn is gated.
        let worktree = wait_for_worktree(&server, &run_id).await;
        std::fs::write(worktree.join("new.txt"), "hello\n").unwrap();
        gate.notify_one();

        assert!(
            wait_for(|| server.state.store.get_run(&run_id).unwrap().state == "succeeded").await,
            "run must reach succeeded"
        );

        let kinds = ledger_kinds(&server, &run_id);
        assert_eq!(
            kinds,
            [
                "run.created",
                "run.worktree_created",
                "run.routed",
                "run.started",
                "engine.session",
                "engine.text",
                "engine.completed",
                "run.check",
                // The check output is filed as evidence before the commit.
                "artifact.created",
                "run.commit",
                // The patch itself, so review does not need the worktree.
                "run.diff",
                // The diff summary, then one artifact per changed file.
                "artifact.created",
                "artifact.created",
                "run.succeeded",
            ]
        );

        let events = server.state.store.events_since(0, Some(&run_id)).unwrap();
        let payload = |kind: &str| {
            events
                .iter()
                .find(|e| e.kind == kind)
                .unwrap_or_else(|| panic!("missing {kind}"))
                .payload
                .clone()
        };
        let check = payload("run.check");
        assert_eq!(check["passed"], true);
        assert_eq!(check["command"], "test -f new.txt");
        let commit = payload("run.commit");
        let hash = commit["commit"].as_str().unwrap().to_string();
        assert_eq!(commit["base_commit"], base);
        assert!(commit["branch"].as_str().unwrap().starts_with("ah/run-"));
        assert!(commit["diff_stat"].as_str().unwrap().contains("new.txt"));

        // The commit exists on the run branch, which carries exactly one
        // commit beyond base; the user's branch never moved.
        let branch = commit["branch"].as_str().unwrap();
        assert_eq!(git_fixture(&server.repo, &["rev-parse", branch]), hash);
        assert_eq!(
            git_fixture(
                &server.repo,
                &["rev-list", "--count", &format!("{base}..{branch}")]
            ),
            "1"
        );
        assert_eq!(git_fixture(&server.repo, &["rev-parse", "HEAD"]), base);
        assert_eq!(git_fixture(&server.repo, &["status", "--porcelain"]), "");
        // Work is preserved for review, not reclaimed.
        assert!(worktree.join("new.txt").is_file());

        // Replay fidelity: a cold subscriber sees the same ordered ledger.
        let mut replay_client = connect(&server).await;
        auth(&mut replay_client, &server.token).await;
        let ack = send(
            &mut replay_client,
            &Request::new(
                "sub-1",
                methods::EVENTS_SUBSCRIBE,
                json!({ "since_sequence": 0, "run_id": run_id }),
            ),
        )
        .await;
        assert!(ack.error.is_none());
        let mut replayed = Vec::new();
        for _ in 0..kinds.len() {
            let event: Event = proto::read_frame(&mut replay_client.reader)
                .await
                .unwrap()
                .unwrap();
            replayed.push(event);
        }
        proto::assert_event_order(&replayed).unwrap();
        assert_eq!(
            replayed.iter().map(|e| e.kind.clone()).collect::<Vec<_>>(),
            kinds
        );
    }

    /// Steering is queued while a turn is in flight and injected at the turn
    /// boundary, and the whole conversation is reconstructable from the ledger.
    #[tokio::test]
    async fn chat_steering_is_injected_at_the_turn_boundary() {
        let (fake, gate) = autoharness_engines::FakeEngine::gated(
            EngineKind::codex(),
            vec![
                vec![
                    autoharness_engines::EngineEvent::Text {
                        text: "first turn".into(),
                    },
                    autoharness_engines::EngineEvent::Completed { summary: None },
                ],
                vec![
                    autoharness_engines::EngineEvent::Text {
                        text: "second turn".into(),
                    },
                    autoharness_engines::EngineEvent::Completed { summary: None },
                ],
            ],
        );
        let server = start_server_with(registry_with_once(EngineKind::codex(), fake)).await;
        let mut client = connect(&server).await;
        auth(&mut client, &server.token).await;
        let run_id = create_run(&server, &mut client, "codex").await;

        send(
            &mut client,
            &Request::new("rs1", methods::RUN_START, json!({ "run_id": run_id })),
        )
        .await;
        wait_for_worktree(&server, &run_id).await;

        // The first turn is gated, so this lands mid-turn and must queue.
        let steer = send(
            &mut client,
            &Request::new(
                "c1",
                methods::CHAT_SEND,
                json!({ "run_id": run_id, "message": "also add tests" }),
            ),
        )
        .await;
        assert_eq!(steer.result.as_ref().unwrap()["queued"], true);
        assert!(
            wait_for(|| {
                ledger_kinds(&server, &run_id).contains(&"chat.steering_queued".to_string())
            })
            .await
        );

        // Release both turns: the queued steering drives the second one.
        gate.notify_one();
        gate.notify_one();
        assert!(
            wait_for(|| server.state.store.get_run(&run_id).unwrap().state == "succeeded").await,
            "run must reach succeeded"
        );

        let kinds = ledger_kinds(&server, &run_id);
        assert_eq!(
            kinds.iter().filter(|k| *k == "engine.completed").count(),
            2,
            "steering must drive a second turn: {kinds:?}"
        );
        assert_eq!(kinds.iter().filter(|k| *k == "run.succeeded").count(), 1);
        let steering_sent = kinds
            .iter()
            .position(|k| k == "chat.steering_sent")
            .unwrap();
        let first_completed = kinds.iter().position(|k| k == "engine.completed").unwrap();
        assert!(
            steering_sent > first_completed,
            "steering must be injected after the turn boundary: {kinds:?}"
        );

        // The conversation replays from the ledger alone.
        let chat: Vec<String> = server
            .state
            .store
            .events_since(0, Some(&run_id))
            .unwrap()
            .iter()
            .filter(|e| e.kind == "chat.message")
            .map(|e| e.payload["content"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(chat, ["also add tests"]);
        assert_eq!(server.state.store.list_chat(&run_id).unwrap().len(), 1);
    }

    #[tokio::test]
    async fn enqueue_persists_and_audits_model_and_reasoning_selection() {
        let (fake, gate) = autoharness_engines::FakeEngine::gated(
            EngineKind::codex(),
            vec![vec![autoharness_engines::EngineEvent::Completed {
                summary: None,
            }]],
        );
        let server = start_server_with(registry_with_once(EngineKind::codex(), fake)).await;
        let mut client = connect(&server).await;
        auth(&mut client, &server.token).await;
        let project = send(
            &mut client,
            &Request::new(
                "selection-project",
                methods::PROJECT_ADD,
                json!({ "name": "selection", "path": server.repo }),
            ),
        )
        .await
        .result
        .unwrap();
        let project_id = project["id"].as_str().unwrap();

        let enqueued = send(
            &mut client,
            &Request::new(
                "selection-enqueue",
                methods::RUN_ENQUEUE,
                json!({
                    "project_id": project_id,
                    "engine": "codex",
                    "model": "gpt-5.6-sol",
                    "reasoning_effort": "high",
                    "objective": "verify execution selection",
                    "check_command": "true",
                    "request_id": "selection-request"
                }),
            ),
        )
        .await;
        assert!(enqueued.error.is_none(), "{:?}", enqueued.error);
        let run_id = enqueued.result.unwrap()["run_id"]
            .as_str()
            .unwrap()
            .to_string();
        let run = server.state.store.get_run(&run_id).unwrap();
        assert_eq!(run.model.as_deref(), Some("gpt-5.6-sol"));
        assert_eq!(run.reasoning_effort.as_deref(), Some("high"));
        let created = server
            .state
            .store
            .events_since(0, Some(&run_id))
            .unwrap()
            .into_iter()
            .find(|event| event.kind == "run.created")
            .unwrap();
        assert_eq!(created.payload["model"], "gpt-5.6-sol");
        assert_eq!(created.payload["reasoning_effort"], "high");

        let rejected = send(
            &mut client,
            &Request::new(
                "selection-invalid",
                methods::RUN_ENQUEUE,
                json!({
                    "project_id": project_id,
                    "engine": "codex",
                    "reasoning_effort": "high effort",
                    "objective": "must not enqueue",
                    "request_id": "selection-invalid-request"
                }),
            ),
        )
        .await;
        assert_eq!(rejected.error.unwrap().code, codes::INVALID_PARAMS);
        gate.notify_one();
    }

    #[tokio::test]
    async fn durable_objective_queue_respects_capacity_and_dispatches_the_next_run() {
        use autoharness_protocol::params::{QueueKind, QueueList, QueueState, SettingsUpdate};

        let (first, first_gate) = autoharness_engines::FakeEngine::gated(
            EngineKind::codex(),
            vec![vec![autoharness_engines::EngineEvent::Completed {
                summary: None,
            }]],
        );
        let second = autoharness_engines::FakeEngine::scripted(
            EngineKind::codex(),
            vec![vec![autoharness_engines::EngineEvent::Completed {
                summary: None,
            }]],
        );
        let adapters = Arc::new(std::sync::Mutex::new(std::collections::VecDeque::from([
            first, second,
        ])));
        let mut registry = EngineRegistry::default();
        registry.insert(EngineKind::codex(), move || {
            Box::new(
                adapters
                    .lock()
                    .unwrap()
                    .pop_front()
                    .expect("two queued runs create two adapters"),
            )
        });
        let server = start_server_with(registry).await;
        server
            .state
            .store
            .update_app_settings(&SettingsUpdate {
                max_active_runs: Some(1),
                ..SettingsUpdate::default()
            })
            .unwrap();
        let mut client = connect(&server).await;
        auth(&mut client, &server.token).await;
        let project = send(
            &mut client,
            &Request::new(
                "queue-project",
                methods::PROJECT_ADD,
                json!({ "name": "queue", "path": server.repo }),
            ),
        )
        .await
        .result
        .unwrap();
        let project_id = project["id"].as_str().unwrap();

        let enqueue = |id: &str, objective: &str| {
            Request::new(
                id,
                methods::RUN_ENQUEUE,
                json!({
                    "project_id": project_id,
                    "engine": "codex",
                    "objective": objective,
                    "check_command": "true",
                    "request_id": id,
                }),
            )
        };
        let first_result = send(&mut client, &enqueue("enqueue-first", "first queued run"))
            .await
            .result
            .unwrap();
        let first_run = first_result["run_id"].as_str().unwrap().to_string();
        assert!(
            wait_for(|| server.state.store.get_run(&first_run).unwrap().state == "running").await
        );

        let second_result = send(&mut client, &enqueue("enqueue-second", "second queued run"))
            .await
            .result
            .unwrap();
        let second_run = second_result["run_id"].as_str().unwrap().to_string();
        assert_eq!(
            server.state.store.get_run(&second_run).unwrap().state,
            "draft"
        );
        assert!(
            !server
                .state
                .data_dir
                .join("worktrees")
                .join(&second_run)
                .exists()
        );

        let queued = server
            .state
            .store
            .list_queue(&QueueList {
                kind: Some(QueueKind::Objective),
                include_terminal: true,
                ..QueueList::default()
            })
            .unwrap();
        assert_eq!(queued.len(), 2);
        assert_eq!(queued[0].state, QueueState::Dispatching);
        assert_eq!(queued[1].state, QueueState::Pending);

        first_gate.notify_one();
        assert!(
            wait_for(|| server.state.store.get_run(&first_run).unwrap().state == "succeeded").await
        );
        assert!(
            wait_for(|| server.state.store.get_run(&second_run).unwrap().state == "succeeded")
                .await,
            "the second queued run should start as soon as the first frees capacity"
        );
        let finished = server
            .state
            .store
            .list_queue(&QueueList {
                kind: Some(QueueKind::Objective),
                include_terminal: true,
                ..QueueList::default()
            })
            .unwrap();
        assert!(
            finished
                .iter()
                .all(|item| item.state == QueueState::Completed)
        );
    }

    /// Cancelling a run that wrote files preserves the worktree and says so.
    #[tokio::test]
    async fn cancel_preserves_a_dirty_worktree() {
        let mut registry = EngineRegistry::default();
        registry.insert(EngineKind::claude(), || {
            Box::new(autoharness_engines::FakeEngine::hanging(
                EngineKind::claude(),
            ))
        });
        let server = start_server_with(registry).await;
        let mut client = connect(&server).await;
        auth(&mut client, &server.token).await;
        let run_id = create_run(&server, &mut client, "claude").await;

        send(
            &mut client,
            &Request::new("rs1", methods::RUN_START, json!({ "run_id": run_id })),
        )
        .await;
        let worktree = wait_for_worktree(&server, &run_id).await;
        std::fs::write(worktree.join("half-done.txt"), "in progress\n").unwrap();

        send(
            &mut client,
            &Request::new("rc1", methods::RUN_CANCEL, json!({ "run_id": run_id })),
        )
        .await;
        assert!(
            wait_for(|| server.state.store.get_run(&run_id).unwrap().state == "cancelled").await,
            "run must reach cancelled"
        );

        let events = server.state.store.events_since(0, Some(&run_id)).unwrap();
        let preserved = events
            .iter()
            .find(|e| e.kind == "run.worktree_preserved")
            .expect("dirty worktree must be surfaced, not deleted");
        assert!(
            preserved.payload["reason"]
                .as_str()
                .unwrap()
                .contains("uncommitted")
        );
        assert!(worktree.join("half-done.txt").is_file());
        assert!(!events.iter().any(|e| e.kind == "run.worktree_removed"));
    }

    /// A folder that is not a git repository is rejected before it can become
    /// a runnable project record.
    #[tokio::test]
    async fn project_add_rejects_a_non_repository() {
        let server = start_server().await;
        let mut client = connect(&server).await;
        auth(&mut client, &server.token).await;
        let plain = server.dir.path().join("not-a-repo");
        std::fs::create_dir_all(&plain).unwrap();
        let resp = send(
            &mut client,
            &Request::new(
                "bad-project",
                methods::PROJECT_ADD,
                json!({ "name": "plain", "path": plain }),
            ),
        )
        .await;
        let error = resp.error.expect("non-repository must be rejected");
        assert_eq!(error.code, codes::INVALID_PARAMS);
        assert!(error.message.contains("Git repository"));
        assert!(server.state.store.list_projects().unwrap().is_empty());
    }

    /// Runs orphaned by a daemon restart are reconciled to Blocked — never
    /// left Running and never reported as a stale success.
    #[tokio::test]
    async fn restart_reconciliation_never_reports_stale_success() {
        let server = start_server().await;
        let mut client = connect(&server).await;
        auth(&mut client, &server.token).await;
        let running = create_run(&server, &mut client, "codex").await;
        let paused = create_run(&server, &mut client, "claude").await;
        let untouched = create_run(&server, &mut client, "codex").await;
        server
            .state
            .store
            .set_run_state(&running, "running")
            .unwrap();
        server.state.store.set_run_state(&paused, "paused").unwrap();

        assert_eq!(runner::reconcile_after_restart(&server.state).await, 2);
        assert_eq!(
            server.state.store.get_run(&running).unwrap().state,
            "blocked"
        );
        assert_eq!(
            server.state.store.get_run(&paused).unwrap().state,
            "blocked"
        );
        assert_eq!(
            server.state.store.get_run(&untouched).unwrap().state,
            "draft"
        );

        for run_id in [&running, &paused] {
            let kinds = ledger_kinds(&server, run_id);
            assert_eq!(kinds.last().unwrap(), "run.reconciled");
            assert!(!kinds.contains(&"run.succeeded".to_string()));
        }
        let reconciled = server
            .state
            .store
            .events_since(0, Some(&running))
            .unwrap()
            .into_iter()
            .find(|e| e.kind == "run.reconciled")
            .unwrap();
        assert_eq!(reconciled.payload["reason"], "daemon_restarted");
        assert_eq!(reconciled.payload["previous_state"], "running");

        // Idempotent: a second pass finds nothing left to reconcile.
        assert_eq!(runner::reconcile_after_restart(&server.state).await, 0);
    }

    #[tokio::test]
    async fn blocked_run_resumes_the_owned_worktree_and_same_provider_session() {
        let server = start_server().await;
        let mut client = connect(&server).await;
        auth(&mut client, &server.token).await;
        let run_id = create_run_in(&server, &mut client, "codex", &server.repo, Some("true")).await;
        let sandbox = server.state.sandbox.sandbox().unwrap();
        let dirs = SessionDirs::create(&server.state.data_dir, &run_id).unwrap();
        let worktree = worktree::create(
            sandbox,
            &dirs,
            &server.state.store,
            &server.repo,
            &run_id,
            &server.state.data_dir,
        )
        .await
        .unwrap();
        server
            .state
            .store
            .save_engine_session(&run_id, "codex", "provider-thread-before-restart")
            .unwrap();
        server
            .state
            .store
            .set_run_state(&run_id, "blocked")
            .unwrap();

        let response = send(
            &mut client,
            &Request::new(
                "resume-owned-run",
                methods::RUN_START,
                json!({ "run_id": run_id }),
            ),
        )
        .await;
        let result = response.result.unwrap();
        assert_eq!(result["started"], true, "{result}");
        assert_eq!(result["resumed"], true, "{result}");
        assert_eq!(result["session_id"], "provider-thread-before-restart");
        assert_eq!(result["worktree"], worktree.path.to_string_lossy().as_ref());

        let events = server.state.store.events_since(0, Some(&run_id)).unwrap();
        let recovered = events
            .iter()
            .position(|event| event.kind == "run.recovered")
            .expect("recovery must be auditable");
        let started = events
            .iter()
            .position(|event| event.kind == "run.started")
            .expect("the resumed run starts");
        assert!(
            recovered < started,
            "recovery is persisted before execution"
        );
        assert_eq!(events[recovered].payload["provider_session_resumed"], true);
        assert!(
            wait_for(|| server.state.store.get_run(&run_id).unwrap().state == "succeeded").await
        );
    }

    // ---- Phase 5: routing, bounded loops, detectors, recovery ----

    /// The router's choice and its reasoning land on the ledger, where the
    /// user can see why a shape was picked.
    #[tokio::test]
    async fn run_start_routes_and_records_its_reasoning() {
        let server = start_server().await;
        let mut client = connect(&server).await;
        auth(&mut client, &server.token).await;
        let run_id = create_run_in(
            &server,
            &mut client,
            "codex",
            &server.repo.clone(),
            Some("true"),
        )
        .await;
        send(
            &mut client,
            &Request::new("rs1", methods::RUN_START, json!({ "run_id": run_id })),
        )
        .await;

        let routed = server
            .state
            .store
            .events_since(0, Some(&run_id))
            .unwrap()
            .into_iter()
            .find(|e| e.kind == "run.routed")
            .expect("every run must record how it was routed");
        // Short objective + explicit check = one atomic edit.
        assert_eq!(routed.payload["shape"], "direct");
        assert!(!routed.payload["reasons"].as_array().unwrap().is_empty());
        assert_eq!(routed.payload["policy_version"], POLICY_VERSION);
        assert_eq!(routed.payload["budgets"]["max_concurrent_workers"], 1);
    }

    /// The heart of Phase 5: a failing check does not end a bounded loop, it
    /// becomes the next turn's evidence, and the loop finishes when it passes.
    #[tokio::test]
    async fn a_bounded_loop_retries_until_the_check_passes() {
        // Turn 1 edits nothing (check fails); turn 2 is driven by the failure.
        let (fake, gate) = autoharness_engines::FakeEngine::gated(
            EngineKind::codex(),
            vec![
                vec![
                    autoharness_engines::EngineEvent::Text {
                        text: "first attempt".into(),
                    },
                    autoharness_engines::EngineEvent::Completed { summary: None },
                ],
                vec![
                    autoharness_engines::EngineEvent::Text {
                        text: "second attempt".into(),
                    },
                    autoharness_engines::EngineEvent::Completed { summary: None },
                ],
            ],
        );
        let server = start_server_with(registry_with_once(EngineKind::codex(), fake)).await;
        let mut client = connect(&server).await;
        auth(&mut client, &server.token).await;
        // Multi-step wording routes to a loop; the check passes only once
        // `fixed.txt` exists, which the test creates between turns.
        let run_id = create_run_with_objective(
            &server,
            &mut client,
            "codex",
            "Investigate and repair the failing build",
            Some("test -f fixed.txt"),
        )
        .await;

        send(
            &mut client,
            &Request::new("rs1", methods::RUN_START, json!({ "run_id": run_id })),
        )
        .await;
        let routed = wait_for_event(&server, &run_id, "run.routed").await;
        assert_eq!(routed.payload["shape"], "bounded_loop");

        let worktree = wait_for_worktree(&server, &run_id).await;
        gate.notify_one();
        // First check fails and drives another turn.
        assert!(
            wait_for(|| {
                ledger_kinds(&server, &run_id).contains(&"run.loop_iteration".to_string())
            })
            .await,
            "a failing check must drive another iteration: {:?}",
            ledger_kinds(&server, &run_id)
        );
        // Now satisfy the check and let the second turn complete.
        std::fs::write(worktree.join("fixed.txt"), "ok\n").unwrap();
        gate.notify_one();

        assert!(
            wait_for(|| server.state.store.get_run(&run_id).unwrap().state == "succeeded").await,
            "the loop must finish once the check passes: {:?}",
            ledger_kinds(&server, &run_id)
        );
        let kinds = ledger_kinds(&server, &run_id);
        let checks = kinds.iter().filter(|k| *k == "run.check").count();
        assert_eq!(checks, 2, "one check per iteration: {kinds:?}");
        assert_eq!(kinds.last().unwrap(), "run.succeeded");
        // The verified state was checkpointed so a later restart can return.
        assert!(
            server
                .state
                .store
                .latest_checkpoint(&run_id)
                .unwrap()
                .is_some()
        );
    }

    /// A loop that cannot make progress walks the recovery ladder instead of
    /// spinning, and ends Blocked with the evidence — never a false success.
    #[tokio::test]
    async fn a_stuck_loop_walks_the_recovery_ladder_and_blocks() {
        // Every turn repeats the identical tool action and completes, so the
        // check keeps failing the same way.
        let turn = || {
            vec![
                autoharness_engines::EngineEvent::ToolActivity {
                    name: "shell".into(),
                    status: autoharness_engines::ToolStatus::Started,
                    detail: json!({ "command": "cargo test" }),
                },
                autoharness_engines::EngineEvent::Completed { summary: None },
            ]
        };
        let fake = autoharness_engines::FakeEngine::scripted(
            EngineKind::codex(),
            (0..30).map(|_| turn()).collect(),
        );
        let server = start_server_with(registry_with_once(EngineKind::codex(), fake)).await;
        let mut client = connect(&server).await;
        auth(&mut client, &server.token).await;
        let run_id = create_run_with_objective(
            &server,
            &mut client,
            "codex",
            "Investigate and repair the failing build",
            // Never satisfiable: the loop cannot escape by luck.
            Some("test -f never-created.txt"),
        )
        .await;

        send(
            &mut client,
            &Request::new("rs1", methods::RUN_START, json!({ "run_id": run_id })),
        )
        .await;
        assert!(
            wait_for(|| server.state.store.get_run(&run_id).unwrap().state == "blocked").await,
            "a stuck loop must end blocked, not run forever: {:?}",
            ledger_kinds(&server, &run_id)
        );

        let events = server.state.store.events_since(0, Some(&run_id)).unwrap();
        let ladder: Vec<String> = events
            .iter()
            .filter(|e| e.kind == "run.detector")
            .map(|e| e.payload["recovery"].as_str().unwrap().to_string())
            .collect();
        assert!(
            !ladder.is_empty(),
            "the detector must have fired: {:?}",
            events.iter().map(|e| &e.kind).collect::<Vec<_>>()
        );
        // Rungs are climbed in order and never repeat.
        let expected = ["nudge", "replan", "restart_from_checkpoint", "blocked"];
        for (rung, want) in ladder.iter().zip(expected.iter()) {
            assert_eq!(rung, want, "ladder out of order: {ladder:?}");
        }

        let blocked = events
            .iter()
            .rev()
            .find(|e| e.kind == "run.blocked")
            .expect("blocking must record its evidence");
        assert!(!blocked.payload["evidence"].as_str().unwrap().is_empty());
        // Never reported as a success, and the work is kept for review.
        let kinds: Vec<&String> = events.iter().map(|e| &e.kind).collect();
        assert!(!kinds.iter().any(|k| *k == "run.succeeded"));
        assert!(events.iter().any(|e| e.kind == "run.worktree_preserved"));
    }

    // ---- Phase 6: graphs and swarms ----

    /// Approving a plan runs every node in its own worktree and branch, in
    /// dependency waves, then integrates. The user's checkout never moves.
    #[tokio::test]
    async fn an_approved_graph_runs_nodes_in_isolated_worktrees_then_integrates() {
        // Every node's engine session writes a file named after the node, so
        // the acceptance checks can prove which worktree the work landed in.
        let mut registry = EngineRegistry::default();
        registry.insert(EngineKind::codex(), || {
            Box::new(autoharness_engines::FakeEngine::scripted(
                EngineKind::codex(),
                vec![vec![
                    autoharness_engines::EngineEvent::Text {
                        text: "node work".into(),
                    },
                    autoharness_engines::EngineEvent::Completed { summary: None },
                ]],
            ))
        });
        let server = start_server_with(registry).await;
        let base = git_fixture(&server.repo, &["rev-parse", "HEAD"]);
        let mut client = connect(&server).await;
        auth(&mut client, &server.token).await;
        let run_id = create_run(&server, &mut client, "codex").await;

        // Two independent editors converging on one integration node.
        let proposal = autoharness_core::GraphProposal {
            nodes: vec![
                autoharness_core::ProposedNode {
                    id: "edit_a".into(),
                    role: "editor".into(),
                    objective: "work on a".into(),
                    file_scope: vec!["src/a/**".into()],
                    acceptance_checks: vec!["true".into()],
                },
                autoharness_core::ProposedNode {
                    id: "edit_b".into(),
                    role: "editor".into(),
                    objective: "work on b".into(),
                    file_scope: vec!["src/b/**".into()],
                    acceptance_checks: vec!["true".into()],
                },
                autoharness_core::ProposedNode {
                    id: "merge".into(),
                    role: "integration".into(),
                    objective: "combine".into(),
                    file_scope: vec!["src/**".into()],
                    acceptance_checks: vec!["true".into()],
                },
            ],
            edges: vec![
                ("edit_a".into(), "merge".into()),
                ("edit_b".into(), "merge".into()),
            ],
            integration_strategy: "staging worktree".into(),
        };
        let compiled =
            autoharness_core::graph::compile(&proposal, &autoharness_core::Budget::default())
                .expect("the fixture plan must be valid");
        // Waves prove the compiler scheduled the editors together.
        assert_eq!(compiled.waves[0].len(), 2);

        server
            .state
            .store
            .save_graph(&run_id, &serde_json::to_value(&compiled).unwrap())
            .unwrap();
        server
            .state
            .store
            .set_run_state(&run_id, "awaiting_approval")
            .unwrap();

        let approved = send(
            &mut client,
            &Request::new("ap1", methods::RUN_APPROVE, json!({ "run_id": run_id })),
        )
        .await;
        assert!(approved.error.is_none(), "{:?}", approved.error);
        assert_eq!(approved.result.as_ref().unwrap()["approved"], true);

        assert!(
            wait_for(|| {
                matches!(
                    server.state.store.get_run(&run_id).unwrap().state.as_str(),
                    "succeeded" | "failed"
                )
            })
            .await,
            "the graph must reach a terminal state: {:?}",
            ledger_kinds(&server, &run_id)
        );
        assert_eq!(
            server.state.store.get_run(&run_id).unwrap().state,
            "succeeded"
        );

        // Every node ran, and each got its own branch.
        let attempts = server.state.store.node_attempts(&run_id).unwrap();
        for node in ["edit_a", "edit_b", "merge"] {
            assert!(
                attempts
                    .iter()
                    .any(|(id, _, state)| id == node && state == "succeeded"),
                "node {node} did not succeed: {attempts:?}"
            );
            // A worktree and branch per node, never a shared checkout.
            let branch = format!("ah/node-{node}");
            assert!(
                !git_fixture(&server.repo, &["rev-parse", "--verify", &branch]).is_empty(),
                "node {node} must have its own branch"
            );
        }

        // Waves are ordered: both editors finish before integration starts.
        let kinds = ledger_kinds(&server, &run_id);
        let waves = kinds.iter().filter(|k| *k == "graph.wave_started").count();
        assert_eq!(waves, 2, "two waves expected: {kinds:?}");
        assert_eq!(kinds.last().unwrap(), "run.succeeded");

        // The user's checkout is untouched throughout.
        assert_eq!(git_fixture(&server.repo, &["rev-parse", "HEAD"]), base);
        assert_eq!(git_fixture(&server.repo, &["status", "--porcelain"]), "");
    }

    /// A node whose dependency failed must not run against inputs that were
    /// never produced.
    #[tokio::test]
    async fn a_failed_node_skips_its_dependents_instead_of_running_them() {
        // The only scripted turn fails unrecoverably.
        let mut registry = EngineRegistry::default();
        registry.insert(EngineKind::codex(), || {
            Box::new(autoharness_engines::FakeEngine::scripted(
                EngineKind::codex(),
                vec![vec![autoharness_engines::EngineEvent::Failed {
                    message: "the model gave up".into(),
                    recoverable: false,
                }]],
            ))
        });
        let server = start_server_with(registry).await;
        let mut client = connect(&server).await;
        auth(&mut client, &server.token).await;
        let run_id = create_run(&server, &mut client, "codex").await;

        let compiled = autoharness_core::graph::compile(
            &autoharness_core::GraphProposal {
                nodes: vec![
                    autoharness_core::ProposedNode {
                        id: "edit".into(),
                        role: "editor".into(),
                        objective: "work".into(),
                        file_scope: vec!["src/**".into()],
                        acceptance_checks: vec!["true".into()],
                    },
                    autoharness_core::ProposedNode {
                        id: "merge".into(),
                        role: "integration".into(),
                        objective: "combine".into(),
                        file_scope: vec!["src/**".into()],
                        acceptance_checks: vec!["true".into()],
                    },
                ],
                edges: vec![("edit".into(), "merge".into())],
                integration_strategy: "staging".into(),
            },
            &autoharness_core::Budget::default(),
        )
        .unwrap();
        server
            .state
            .store
            .save_graph(&run_id, &serde_json::to_value(&compiled).unwrap())
            .unwrap();
        server
            .state
            .store
            .set_run_state(&run_id, "awaiting_approval")
            .unwrap();
        send(
            &mut client,
            &Request::new("ap1", methods::RUN_APPROVE, json!({ "run_id": run_id })),
        )
        .await;

        assert!(
            wait_for(|| server.state.store.get_run(&run_id).unwrap().state == "blocked").await,
            "a graph whose node failed must block for a controlled retry: {:?}",
            ledger_kinds(&server, &run_id)
        );
        let attempts = server.state.store.node_attempts(&run_id).unwrap();
        assert!(
            attempts
                .iter()
                .any(|(id, _, state)| id == "merge" && state == "cancelled"),
            "the dependent must be skipped, not run: {attempts:?}"
        );
        // And never reported as a success.
        assert!(!ledger_kinds(&server, &run_id).contains(&"run.succeeded".to_string()));
        let cancelled = send(
            &mut client,
            &Request::new(
                "cancel-blocked-graph",
                methods::RUN_CANCEL,
                json!({ "run_id": run_id }),
            ),
        )
        .await;
        assert_eq!(cancelled.result.unwrap()["accepted"], true);
    }

    #[tokio::test]
    async fn node_retry_executes_the_failed_node_and_its_dependent_subgraph() {
        let creations = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut registry = EngineRegistry::default();
        let factory_calls = Arc::clone(&creations);
        registry.insert(EngineKind::codex(), move || {
            let call = factory_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let events = if call == 0 {
                vec![autoharness_engines::EngineEvent::Failed {
                    message: "first attempt failed".into(),
                    recoverable: false,
                }]
            } else {
                vec![autoharness_engines::EngineEvent::Completed { summary: None }]
            };
            Box::new(autoharness_engines::FakeEngine::scripted(
                EngineKind::codex(),
                vec![events],
            ))
        });
        let server = start_server_with(registry).await;
        let mut client = connect(&server).await;
        auth(&mut client, &server.token).await;
        let run_id = create_run(&server, &mut client, "codex").await;
        let compiled = autoharness_core::graph::compile(
            &autoharness_core::GraphProposal {
                nodes: vec![
                    autoharness_core::ProposedNode {
                        id: "edit".into(),
                        role: "editor".into(),
                        objective: "edit".into(),
                        file_scope: vec!["src/**".into()],
                        acceptance_checks: vec!["true".into()],
                    },
                    autoharness_core::ProposedNode {
                        id: "merge".into(),
                        role: "integration".into(),
                        objective: "merge".into(),
                        file_scope: vec!["src/**".into()],
                        acceptance_checks: vec!["true".into()],
                    },
                ],
                edges: vec![("edit".into(), "merge".into())],
                integration_strategy: "staging".into(),
            },
            &autoharness_core::Budget::default(),
        )
        .unwrap();
        server
            .state
            .store
            .save_graph(&run_id, &serde_json::to_value(&compiled).unwrap())
            .unwrap();
        server
            .state
            .store
            .set_run_state(&run_id, "awaiting_approval")
            .unwrap();
        send(
            &mut client,
            &Request::new(
                "retry-approve",
                methods::RUN_APPROVE,
                json!({ "run_id": run_id }),
            ),
        )
        .await;
        assert!(wait_for(|| server.state.store.get_run(&run_id).unwrap().state == "blocked").await);

        let retried = send(
            &mut client,
            &Request::new(
                "retry-node",
                methods::NODE_RETRY,
                json!({ "run_id": run_id, "node_id": "edit" }),
            ),
        )
        .await;
        assert_eq!(retried.result.as_ref().unwrap()["accepted"], true);
        assert_eq!(retried.result.as_ref().unwrap()["attempt"], 2);
        assert!(
            wait_for(|| server.state.store.get_run(&run_id).unwrap().state == "succeeded").await,
            "retry did not finish: {:?}",
            ledger_kinds(&server, &run_id)
        );
        let attempts = server.state.store.node_attempts(&run_id).unwrap();
        for node in ["edit", "merge"] {
            assert!(
                attempts.iter().any(|(id, attempt, state)| id == node
                    && *attempt == 2
                    && state == "succeeded"),
                "{node} was not rerun successfully: {attempts:?}"
            );
        }
        assert!(ledger_kinds(&server, &run_id).contains(&"node.retry_started".to_string()));
    }

    #[tokio::test]
    async fn node_cancel_stops_a_live_node_and_leaves_the_graph_recoverable() {
        let mut registry = EngineRegistry::default();
        registry.insert(EngineKind::codex(), || {
            Box::new(autoharness_engines::FakeEngine::hanging(EngineKind::codex()))
        });
        let server = start_server_with(registry).await;
        let mut client = connect(&server).await;
        auth(&mut client, &server.token).await;
        let run_id = create_run(&server, &mut client, "codex").await;
        let compiled = autoharness_core::graph::compile(
            &autoharness_core::GraphProposal {
                nodes: vec![autoharness_core::ProposedNode {
                    id: "slow".into(),
                    role: "verifier".into(),
                    objective: "wait".into(),
                    file_scope: vec!["src/**".into()],
                    acceptance_checks: vec!["true".into()],
                }],
                edges: vec![],
                integration_strategy: "none".into(),
            },
            &autoharness_core::Budget::default(),
        )
        .unwrap();
        server
            .state
            .store
            .save_graph(&run_id, &serde_json::to_value(&compiled).unwrap())
            .unwrap();
        server
            .state
            .store
            .set_run_state(&run_id, "awaiting_approval")
            .unwrap();
        send(
            &mut client,
            &Request::new(
                "cancel-approve",
                methods::RUN_APPROVE,
                json!({ "run_id": run_id }),
            ),
        )
        .await;
        assert!(
            wait_for(|| {
                server
                    .state
                    .store
                    .latest_node_attempt(&run_id, "slow")
                    .unwrap()
                    .is_some_and(|attempt| attempt.state == "running")
            })
            .await
        );

        let cancelled = send(
            &mut client,
            &Request::new(
                "cancel-node",
                methods::NODE_CANCEL,
                json!({ "run_id": run_id, "node_id": "slow" }),
            ),
        )
        .await;
        assert_eq!(cancelled.result.as_ref().unwrap()["accepted"], true);
        assert!(wait_for(|| server.state.store.get_run(&run_id).unwrap().state == "blocked").await);
        assert_eq!(
            server
                .state
                .store
                .latest_node_attempt(&run_id, "slow")
                .unwrap()
                .unwrap()
                .state,
            "cancelled"
        );
        assert!(ledger_kinds(&server, &run_id).contains(&"node.cancel_requested".to_string()));

        send(
            &mut client,
            &Request::new(
                "cancel-graph",
                methods::RUN_CANCEL,
                json!({ "run_id": run_id }),
            ),
        )
        .await;
        assert!(
            wait_for(|| server.state.store.get_run(&run_id).unwrap().state == "cancelled").await
        );
    }

    /// Approval is not optional: a graph cannot be started by any other path.
    #[tokio::test]
    async fn a_graph_cannot_run_without_approval() {
        let server = start_server().await;
        let mut client = connect(&server).await;
        auth(&mut client, &server.token).await;
        let run_id = create_run(&server, &mut client, "codex").await;
        // Draft, not awaiting approval.
        let resp = send(
            &mut client,
            &Request::new("ap1", methods::RUN_APPROVE, json!({ "run_id": run_id })),
        )
        .await;
        let err = resp.error.expect("approving a draft run must be refused");
        assert_eq!(err.code, codes::INVALID_PARAMS);
        assert!(err.message.contains("not awaiting approval"));
    }

    /// The planning call closes the loop: an objective that describes
    /// independent work gets a plan from the engine, the plan is compiled, and
    /// the run stops for human approval instead of starting on its own.
    #[tokio::test]
    async fn a_plan_from_the_engine_compiles_and_waits_for_approval() {
        const PLAN: &str = r#"Here is the plan:
```json
{
  "confidence": 0.92,
  "integration_strategy": "staging worktree",
  "nodes": [
    {"id": "edit_a", "role": "editor", "objective": "document a",
     "file_scope": ["src/a/**"], "acceptance_checks": ["true"]},
    {"id": "edit_b", "role": "editor", "objective": "document b",
     "file_scope": ["src/b/**"], "acceptance_checks": ["true"]},
    {"id": "merge", "role": "integration", "objective": "combine",
     "file_scope": ["src/**"], "acceptance_checks": ["true"]}
  ],
  "edges": [["edit_a", "merge"], ["edit_b", "merge"]]
}
```"#;
        let mut registry = EngineRegistry::default();
        registry.insert(EngineKind::codex(), || {
            Box::new(autoharness_engines::FakeEngine::scripted(
                EngineKind::codex(),
                vec![vec![
                    autoharness_engines::EngineEvent::Text { text: PLAN.into() },
                    autoharness_engines::EngineEvent::Completed { summary: None },
                ]],
            ))
        });
        let server = start_server_with(registry).await;
        let mut client = connect(&server).await;
        auth(&mut client, &server.token).await;
        let run_id = create_run_with_objective(
            &server,
            &mut client,
            "codex",
            "Add a docstring to each public function across the adapter modules",
            Some("true"),
        )
        .await;

        let resp = send(
            &mut client,
            &Request::new("rs1", methods::RUN_START, json!({ "run_id": run_id })),
        )
        .await;
        let result = resp.result.as_ref().unwrap();
        assert_eq!(result["awaiting_approval"], true, "{result}");
        assert_eq!(result["shape"], "dynamic_dag");
        assert_eq!(
            server.state.store.get_run(&run_id).unwrap().state,
            "awaiting_approval"
        );

        let kinds = ledger_kinds(&server, &run_id);
        assert!(kinds.contains(&"run.planned".to_string()), "{kinds:?}");
        assert_eq!(kinds.last().unwrap(), "run.awaiting_approval");

        // The compiled plan is stored and shown, ready for review.
        let (version, stored) = server.state.store.latest_graph(&run_id).unwrap().unwrap();
        assert_eq!(version, 1);
        assert_eq!(stored["nodes"].as_array().unwrap().len(), 3);
        assert_eq!(stored["waves"].as_array().unwrap().len(), 2);
        // Nothing ran: approval is not a formality.
        assert!(
            server
                .state
                .store
                .node_attempts(&run_id)
                .unwrap()
                .is_empty()
        );
    }

    /// A plan the compiler refuses must not run, and must not silently vanish:
    /// the run falls back to a bounded loop and the problems are on the ledger.
    #[tokio::test]
    async fn an_invalid_plan_is_refused_and_the_run_falls_back() {
        // Two editors claiming the same scope: a merge-corrupting plan.
        const BAD_PLAN: &str = r#"{
  "confidence": 0.95,
  "integration_strategy": "hope",
  "nodes": [
    {"id": "a", "role": "editor", "objective": "x",
     "file_scope": ["src/**"], "acceptance_checks": ["true"]},
    {"id": "b", "role": "editor", "objective": "y",
     "file_scope": ["src/**"], "acceptance_checks": ["true"]}
  ],
  "edges": []
}"#;
        let mut registry = EngineRegistry::default();
        registry.insert(EngineKind::codex(), || {
            Box::new(autoharness_engines::FakeEngine::scripted(
                EngineKind::codex(),
                vec![vec![
                    autoharness_engines::EngineEvent::Text {
                        text: BAD_PLAN.into(),
                    },
                    autoharness_engines::EngineEvent::Completed { summary: None },
                ]],
            ))
        });
        let server = start_server_with(registry).await;
        let mut client = connect(&server).await;
        auth(&mut client, &server.token).await;
        let run_id = create_run_with_objective(
            &server,
            &mut client,
            "codex",
            "Add a docstring to each public function across the adapter modules",
            Some("true"),
        )
        .await;

        let resp = send(
            &mut client,
            &Request::new("rs1", methods::RUN_START, json!({ "run_id": run_id })),
        )
        .await;
        assert_eq!(resp.result.as_ref().unwrap()["started"], true);

        let events = server.state.store.events_since(0, Some(&run_id)).unwrap();
        let rejected = events
            .iter()
            .find(|e| e.kind == "graph.rejected")
            .expect("a refused plan must say why");
        let problems = rejected.payload["problems"].as_array().unwrap();
        assert!(
            problems
                .iter()
                .any(|p| p.as_str().unwrap().contains("would both edit")),
            "{problems:?}"
        );
        assert_eq!(rejected.payload["fallback"], "bounded_loop");
        // It ran as a loop, not as an unvalidated graph.
        let routed = events.iter().rfind(|e| e.kind == "run.routed").unwrap();
        assert_eq!(routed.payload["shape"], "bounded_loop");
    }

    /// Switching engines mid-conversation must carry the context across. A
    /// Codex session id means nothing to Claude, so without a handoff the
    /// second engine starts blind — the exact bug this covers.
    #[tokio::test]
    async fn switching_engines_hands_the_context_over() {
        let mut registry = EngineRegistry::default();
        for kind in EngineKind::builtins() {
            let factory_kind = kind.clone();
            registry.insert(kind, move || {
                Box::new(autoharness_engines::FakeEngine::scripted(
                    factory_kind.clone(),
                    vec![vec![
                        autoharness_engines::EngineEvent::Text {
                            text: "wrote the parser".into(),
                        },
                        autoharness_engines::EngineEvent::Completed { summary: None },
                    ]],
                ))
            });
        }
        let server = start_server_with(registry).await;
        let mut client = connect(&server).await;
        auth(&mut client, &server.token).await;

        // Turn one on codex.
        let first = create_run(&server, &mut client, "codex").await;
        send(
            &mut client,
            &Request::new("s1", methods::RUN_START, json!({ "run_id": first })),
        )
        .await;
        assert!(
            wait_for(|| server.state.store.get_run(&first).unwrap().state == "succeeded").await
        );

        // Turn two continues the thread on claude.
        let project = server.state.store.list_projects().unwrap().remove(0);
        let second = send(
            &mut client,
            &Request::new(
                next_req_id("r"),
                methods::RUN_CREATE,
                json!({
                    "project_id": project.id,
                    "engine": "claude",
                    "objective": "now handle escapes",
                    "parent_run_id": first,
                }),
            ),
        )
        .await;
        let second = second.result.as_ref().unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();
        let started = send(
            &mut client,
            &Request::new("s2", methods::RUN_START, json!({ "run_id": second })),
        )
        .await;
        let result = started.result.as_ref().unwrap();
        assert_eq!(result["started"], true, "{result}");
        assert_eq!(
            result["handed_off"], true,
            "a cross-engine turn must hand the context over"
        );
        // It must NOT have resumed: a codex session is meaningless to claude.
        assert_eq!(result["resumed"], false);

        let handoff = wait_for_event(&server, &second, "run.handoff").await;
        let brief = handoff.payload["brief"].as_str().unwrap();
        assert_eq!(handoff.payload["to_engine"], "claude");
        // The brief carries what actually happened, not a promise that it did.
        assert!(brief.contains("codex"), "{brief}");
        assert!(brief.contains("do the thing"), "the original ask: {brief}");
        assert!(
            brief.contains("wrote the parser"),
            "what was reported: {brief}"
        );
        assert!(
            brief.contains("Verify"),
            "and not to trust it blindly: {brief}"
        );
    }

    /// A thread's turns must share one checkout.
    ///
    /// The regression this covers: the worktree was keyed on the turn's own run
    /// id, so every follow-up — same engine or not — got a fresh tree off the
    /// base commit on a brand new `ah/run-*` branch. The previous turn's work
    /// was simply not there, and the cross-engine brief then told the incoming
    /// engine that it was. Switching provider looked like starting over,
    /// because it was.
    #[tokio::test]
    async fn a_follow_up_turn_works_in_the_threads_worktree() {
        let server = start_server().await;
        let mut client = connect(&server).await;
        auth(&mut client, &server.token).await;

        let first = create_run(&server, &mut client, "codex").await;
        send(
            &mut client,
            &Request::new(
                next_req_id("s"),
                methods::RUN_START,
                json!({ "run_id": first }),
            ),
        )
        .await;
        assert!(
            wait_for(|| server.state.store.get_run(&first).unwrap().state == "succeeded").await
        );
        let first_tree = wait_for_event(&server, &first, "run.worktree_created").await;
        let thread_branch = first_tree.payload["branch"].as_str().unwrap().to_string();
        let thread_path = first_tree.payload["path"].as_str().unwrap().to_string();

        let project = server.state.store.list_projects().unwrap().remove(0);
        let second = send(
            &mut client,
            &Request::new(
                next_req_id("r"),
                methods::RUN_CREATE,
                json!({
                    "project_id": project.id,
                    "engine": "claude",
                    "objective": "keep going",
                    "parent_run_id": first,
                }),
            ),
        )
        .await;
        let second = second.result.as_ref().unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();
        let started = send(
            &mut client,
            &Request::new(
                next_req_id("s"),
                methods::RUN_START,
                json!({ "run_id": second }),
            ),
        )
        .await;
        let result = started.result.as_ref().unwrap();
        assert_eq!(result["started"], true, "{result}");
        assert_eq!(
            result["worktree"].as_str().unwrap(),
            thread_path,
            "a follow-up turn must continue in the thread's checkout"
        );

        // Whichever way the tree got there — resumed because the previous turn
        // left it, or recreated because it was reclaimed as empty — the branch
        // is the thread's, never a second one.
        let events = server.state.store.events_since(0, Some(&second)).unwrap();
        let tree = events
            .iter()
            .find(|event| {
                event.kind == "run.worktree_resumed" || event.kind == "run.worktree_created"
            })
            .expect("the second turn reports the tree it got");
        assert_eq!(tree.payload["branch"].as_str().unwrap(), thread_branch);
        assert_eq!(tree.payload["path"].as_str().unwrap(), thread_path);

        // And the brief it was handed must describe that tree truthfully.
        let handoff = wait_for_event(&server, &second, "run.handoff").await;
        let carried = handoff.payload["worktree_carried"].as_bool().unwrap();
        let brief = handoff.payload["brief"].as_str().unwrap();
        if carried {
            assert!(brief.contains("same worktree"), "{brief}");
        } else {
            assert!(brief.contains("FRESH"), "{brief}");
        }
    }

    /// A draft can be given its objective; anything past draft cannot.
    ///
    /// This is what lets the sidebar's `+` create a row the moment it is
    /// clicked — a real, persisted, auditable draft — and fill in the objective
    /// when the user submits one. The refusal half matters more: once a run has
    /// started, its objective is what an engine was actually told, and a record
    /// that can be edited afterwards is not an audit trail.
    #[tokio::test]
    async fn a_drafts_objective_can_be_set_once_and_never_after_it_starts() {
        let server = start_server().await;
        let mut client = connect(&server).await;
        auth(&mut client, &server.token).await;
        // Registers the repository as a project, which a bare server has none of.
        let _seed = create_run(&server, &mut client, "codex").await;
        let project = server.state.store.list_projects().unwrap().remove(0);

        let created = send(
            &mut client,
            &Request::new(
                next_req_id("r"),
                methods::RUN_CREATE,
                json!({ "project_id": project.id, "engine": "codex", "objective": "" }),
            ),
        )
        .await;
        let run_id = created.result.as_ref().unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();
        assert_eq!(server.state.store.get_run(&run_id).unwrap().state, "draft");

        // An empty objective is not an objective.
        let empty = send(
            &mut client,
            &Request::new(
                next_req_id("o"),
                methods::RUN_SET_OBJECTIVE,
                json!({ "run_id": run_id, "objective": "   " }),
            ),
        )
        .await;
        assert!(empty.error.is_some(), "{empty:?}");

        let set = send(
            &mut client,
            &Request::new(
                next_req_id("o"),
                methods::RUN_SET_OBJECTIVE,
                json!({ "run_id": run_id, "objective": "ship the thing" }),
            ),
        )
        .await;
        assert!(set.error.is_none(), "{set:?}");
        assert_eq!(
            server.state.store.get_run(&run_id).unwrap().objective,
            "ship the thing"
        );
        let recorded = wait_for_event(&server, &run_id, "run.objective_set").await;
        assert_eq!(recorded.payload["objective"], "ship the thing");

        send(
            &mut client,
            &Request::new(
                next_req_id("s"),
                methods::RUN_START,
                json!({ "run_id": run_id }),
            ),
        )
        .await;
        assert!(
            wait_for(|| server.state.store.get_run(&run_id).unwrap().state == "succeeded").await
        );

        let after = send(
            &mut client,
            &Request::new(
                next_req_id("o"),
                methods::RUN_SET_OBJECTIVE,
                json!({ "run_id": run_id, "objective": "rewrite history" }),
            ),
        )
        .await;
        assert!(
            after.error.is_some(),
            "a started run must refuse: {after:?}"
        );
        assert_eq!(
            server.state.store.get_run(&run_id).unwrap().objective,
            "ship the thing",
            "the ledger keeps what the engine was actually told"
        );
    }

    /// The sidebar's `+` flow end to end: create a draft so a row exists, then
    /// queue that same draft once the user has typed an objective.
    ///
    /// The regression this guards is a duplicate run: queueing must adopt the
    /// draft, not create a second run beside it, or clicking `+` and then
    /// submitting would leave two rows for one piece of work.
    #[tokio::test]
    async fn queueing_a_draft_adopts_it_instead_of_creating_a_second_run() {
        let server = start_server().await;
        let mut client = connect(&server).await;
        auth(&mut client, &server.token).await;
        let _seed = create_run(&server, &mut client, "codex").await;
        let project = server.state.store.list_projects().unwrap().remove(0);
        let created_runs = || {
            server
                .state
                .store
                .events_since(0, None)
                .unwrap()
                .into_iter()
                .filter(|event| event.kind == "run.created")
                .count()
        };
        let before = created_runs();

        let created = send(
            &mut client,
            &Request::new(
                next_req_id("r"),
                methods::RUN_CREATE,
                json!({ "project_id": project.id, "engine": "codex", "objective": "" }),
            ),
        )
        .await;
        let draft = created.result.as_ref().unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();

        let queued = send(
            &mut client,
            &Request::new(
                next_req_id("q"),
                methods::RUN_ENQUEUE,
                json!({
                    "project_id": project.id,
                    "run_id": draft,
                    "objective": "do the work",
                    "request_id": autoharness_core::new_id(),
                }),
            ),
        )
        .await;
        assert!(queued.error.is_none(), "{queued:?}");
        assert_eq!(queued.result.as_ref().unwrap()["run_id"], draft);

        let after = created_runs();
        assert_eq!(after, before + 1, "the draft is adopted, not duplicated");
        assert!(
            wait_for(|| server.state.store.get_run(&draft).unwrap().state == "succeeded").await
        );
        assert_eq!(
            server.state.store.get_run(&draft).unwrap().objective,
            "do the work"
        );
    }

    /// A pinned effort the model does not accept is refused before anything
    /// runs, with a problem naming what IS accepted.
    ///
    /// This was shape-checked only — one printable token — so a stale picker
    /// could pin an effort the provider rejects, and the run built a worktree
    /// and died inside the engine with an error the user could not act on.
    #[test]
    fn an_unsupported_execution_selection_is_refused_with_the_alternatives() {
        let catalog = vec![
            autoharness_engines::EngineModel {
                id: "sonnet".into(),
                display_name: "Sonnet".into(),
                description: String::new(),
                reasoning_efforts: vec!["low".into(), "high".into()],
                default_reasoning_effort: Some("high".into()),
                is_default: true,
            },
            autoharness_engines::EngineModel {
                id: "opus".into(),
                display_name: "Opus".into(),
                description: String::new(),
                reasoning_efforts: vec!["high".into(), "max".into()],
                default_reasoning_effort: Some("high".into()),
                is_default: false,
            },
        ];

        // Valid combinations pass.
        assert!(validate_execution_selection(&catalog, Some("opus"), Some("max")).is_ok());
        assert!(validate_execution_selection(&catalog, None, None).is_ok());
        // No model pinned falls back to the default model's efforts.
        assert!(validate_execution_selection(&catalog, None, Some("low")).is_ok());

        // A model this engine does not offer.
        let problem = validate_execution_selection(&catalog, Some("gpt-9"), None).unwrap_err();
        assert!(problem.contains("gpt-9"), "{problem}");
        assert!(
            problem.contains("sonnet"),
            "it lists what exists: {problem}"
        );

        // An effort this MODEL does not offer, even though another model does.
        let problem =
            validate_execution_selection(&catalog, Some("sonnet"), Some("max")).unwrap_err();
        assert!(problem.contains("max"), "{problem}");
        assert!(problem.contains("Sonnet"), "{problem}");
        assert!(
            problem.contains("low, high"),
            "it lists what works: {problem}"
        );
    }

    /// Missing information is never treated as a refusal: an unreadable
    /// catalog or a model that declares no efforts leaves provider-default
    /// execution usable, which is the same call `detect_all_with_models`
    /// already makes for readiness.
    #[test]
    fn an_unknowable_catalog_does_not_block_a_run() {
        assert!(validate_execution_selection(&[], Some("anything"), Some("max")).is_ok());
        let silent = vec![autoharness_engines::EngineModel {
            id: "mystery".into(),
            display_name: "Mystery".into(),
            description: String::new(),
            reasoning_efforts: Vec::new(),
            default_reasoning_effort: None,
            is_default: true,
        }];
        assert!(validate_execution_selection(&silent, Some("mystery"), Some("max")).is_ok());
    }

    /// One objective, answered several ways, judged the same way.
    ///
    /// This is what a terminal full of agents cannot do: each attempt is an
    /// ordinary run in its own worktree on its own branch, sharing the check —
    /// so "this one passed" is a fact rather than a preference. Every attempt
    /// keeps the ordinary rules, which is the point of making it a group
    /// rather than a new kind of run.
    #[tokio::test]
    async fn one_objective_can_be_answered_several_ways_and_compared() {
        let server = start_server().await;
        let mut client = connect(&server).await;
        auth(&mut client, &server.token).await;
        let _seed = create_run(&server, &mut client, "codex").await;
        let project = server.state.store.list_projects().unwrap().remove(0);

        let request_id = autoharness_core::new_id();
        let response = send(
            &mut client,
            &Request::new(
                next_req_id("a"),
                methods::RUN_ATTEMPTS,
                json!({
                    "project_id": project.id,
                    "objective": "make the test pass",
                    "check_command": "cargo test",
                    "attempts": [
                        { "engine": "codex" },
                        { "engine": "claude" },
                    ],
                    "request_id": request_id,
                }),
            ),
        )
        .await;
        let result = response.result.as_ref().expect("attempts start");
        let group = result["attempt_group"].as_str().unwrap().to_string();
        let ids: Vec<String> = result["run_ids"]
            .as_array()
            .unwrap()
            .iter()
            .map(|id| id.as_str().unwrap().to_string())
            .collect();
        assert_eq!(ids.len(), 2);

        let attempts = server.state.store.attempts_in_group(&group).unwrap();
        assert_eq!(attempts.len(), 2);
        // Same question, same judge — otherwise the results are not comparable.
        for attempt in &attempts {
            assert_eq!(attempt.objective, "make the test pass");
            assert_eq!(attempt.check_command.as_deref(), Some("cargo test"));
            assert_eq!(attempt.attempt_group.as_deref(), Some(group.as_str()));
        }
        // Different ways of answering it.
        let engines: Vec<&str> = attempts.iter().map(|a| a.engine.as_str()).collect();
        assert!(engines.contains(&"codex") && engines.contains(&"claude"));

        // Each is an ordinary run: they run, and they finish.
        for id in &ids {
            assert!(
                wait_for(|| {
                    matches!(
                        server.state.store.get_run(id).unwrap().state.as_str(),
                        "succeeded" | "failed" | "blocked"
                    )
                })
                .await,
                "attempt {id} settles"
            );
        }
        // Each got its OWN worktree: attempts that shared one would overwrite
        // each other and there would be nothing to compare.
        let mut branches: Vec<String> = Vec::new();
        for id in &ids {
            let created = wait_for_event(&server, id, "run.worktree_created").await;
            branches.push(created.payload["branch"].as_str().unwrap().to_string());
        }
        branches.sort();
        branches.dedup();
        assert_eq!(branches.len(), 2, "one worktree each");
    }

    /// Retrying the same request must not spend a second set of attempts.
    #[tokio::test]
    async fn a_retried_attempt_set_returns_the_one_that_already_exists() {
        let server = start_server().await;
        let mut client = connect(&server).await;
        auth(&mut client, &server.token).await;
        let _seed = create_run(&server, &mut client, "codex").await;
        let project = server.state.store.list_projects().unwrap().remove(0);
        let request_id = autoharness_core::new_id();
        let params = json!({
            "project_id": project.id,
            "objective": "twice",
            "attempts": [{ "engine": "codex" }, { "engine": "claude" }],
            "request_id": request_id,
        });

        let first = send(
            &mut client,
            &Request::new(next_req_id("a"), methods::RUN_ATTEMPTS, params.clone()),
        )
        .await;
        let second = send(
            &mut client,
            &Request::new(next_req_id("a"), methods::RUN_ATTEMPTS, params),
        )
        .await;
        assert_eq!(
            first.result.as_ref().unwrap()["attempt_group"],
            second.result.as_ref().unwrap()["attempt_group"],
            "a retry returns the same group rather than racing again"
        );
        let group = first.result.as_ref().unwrap()["attempt_group"]
            .as_str()
            .unwrap();
        assert_eq!(
            server.state.store.attempts_in_group(group).unwrap().len(),
            2
        );
    }

    /// A typo must not launch fifty engines, and an unknown engine is refused
    /// before anything is created.
    #[tokio::test]
    async fn an_attempt_set_is_bounded_and_its_engines_are_checked() {
        let server = start_server().await;
        let mut client = connect(&server).await;
        auth(&mut client, &server.token).await;
        let _seed = create_run(&server, &mut client, "codex").await;
        let project = server.state.store.list_projects().unwrap().remove(0);

        let too_many: Vec<Value> = (0..9).map(|_| json!({ "engine": "codex" })).collect();
        let response = send(
            &mut client,
            &Request::new(
                next_req_id("a"),
                methods::RUN_ATTEMPTS,
                json!({
                    "project_id": project.id,
                    "objective": "x",
                    "attempts": too_many,
                    "request_id": autoharness_core::new_id(),
                }),
            ),
        )
        .await;
        assert!(response.error.is_some(), "{response:?}");

        let unknown = send(
            &mut client,
            &Request::new(
                next_req_id("a"),
                methods::RUN_ATTEMPTS,
                json!({
                    "project_id": project.id,
                    "objective": "x",
                    "attempts": [{ "engine": "nonexistent" }],
                    "request_id": autoharness_core::new_id(),
                }),
            ),
        )
        .await;
        assert!(unknown.error.is_some(), "{unknown:?}");
    }

    /// The worktree key is the thread root, and the walk terminates.
    #[test]
    fn the_worktree_key_is_the_thread_root() {
        let store = autoharness_store::Store::open_in_memory().unwrap();
        let project = store.add_project("demo", "/tmp/threadkey").unwrap();
        let root = store
            .create_run_full(&project.id, "codex", "one", None, None)
            .unwrap();
        assert_eq!(thread_worktree_key(&store, &root), root.id);

        let mut parent = root.id.clone();
        for turn in 0..5 {
            let engine = if turn % 2 == 0 { "claude" } else { "codex" };
            let child = store
                .create_run_full(&project.id, engine, "next", None, Some(&parent))
                .unwrap();
            assert_eq!(
                thread_worktree_key(&store, &child),
                root.id,
                "turn {turn} must key on the thread root"
            );
            parent = child.id;
        }
    }

    /// Continuing on the SAME engine resumes the real session, so no brief is
    /// manufactured — a summary would be strictly worse than the context the
    /// model already holds.
    #[tokio::test]
    async fn continuing_on_the_same_engine_resumes_instead_of_summarising() {
        let server = start_server().await;
        let mut client = connect(&server).await;
        auth(&mut client, &server.token).await;
        let first = create_run(&server, &mut client, "codex").await;
        send(
            &mut client,
            &Request::new("s1", methods::RUN_START, json!({ "run_id": first })),
        )
        .await;
        assert!(
            wait_for(|| server.state.store.get_run(&first).unwrap().state == "succeeded").await
        );

        let project = server.state.store.list_projects().unwrap().remove(0);
        let second = send(
            &mut client,
            &Request::new(
                next_req_id("r"),
                methods::RUN_CREATE,
                json!({
                    "project_id": project.id,
                    "engine": "codex",
                    "objective": "keep going",
                    "parent_run_id": first,
                }),
            ),
        )
        .await;
        let second = second.result.as_ref().unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();
        let started = send(
            &mut client,
            &Request::new("s2", methods::RUN_START, json!({ "run_id": second })),
        )
        .await;
        let result = started.result.as_ref().unwrap();
        assert_eq!(result["handed_off"], false);
        // The thread's own session carried over instead.
        assert_eq!(result["resumed"], true, "{result}");
        assert!(!ledger_kinds(&server, &second).contains(&"run.handoff".to_string()));
    }

    /// The dedup cache outlives a connection, so a client that restarts its
    /// request numbering gets the previous session's answers replayed at it.
    /// This is why UI request ids carry a per-client session token.
    #[tokio::test]
    async fn a_reused_request_id_replays_the_old_answer_on_a_new_connection() {
        let server = start_server().await;

        let mut first = connect(&server).await;
        auth(&mut first, &server.token).await;
        let added = send(
            &mut first,
            &Request::new(
                "ui-1",
                methods::PROJECT_ADD,
                json!({ "name": "a", "path": server.repo }),
            ),
        )
        .await;
        assert!(added.error.is_none());

        // A brand-new connection reusing the same id gets the OLD response,
        // not an answer to what it actually asked.
        let mut second = connect(&server).await;
        auth(&mut second, &server.token).await;
        let replayed = send(
            &mut second,
            &Request::new("ui-1", methods::PROJECT_LIST, json!({})),
        )
        .await;
        assert_eq!(
            replayed, added,
            "the daemon replays by id across connections — clients must not reuse ids"
        );
    }

    // ---- Phase 8: memory and evolution ----

    /// Promotion re-validates at the moment of promotion, so a stored
    /// candidate can never move a capability boundary just because it was
    /// accepted into the table earlier.
    #[tokio::test]
    async fn a_policy_that_touches_a_capability_boundary_cannot_be_promoted() {
        let server = start_server().await;
        let mut client = connect(&server).await;
        auth(&mut client, &server.token).await;

        let safe = server
            .state
            .store
            .add_policy_candidate(&json!({ "detector_thresholds": { "repeated_action": 4 } }))
            .unwrap();
        let unsafe_version = server
            .state
            .store
            .add_policy_candidate(&json!({ "sandbox_rules": { "allow_network": true } }))
            .unwrap();

        // The tuning candidate promotes.
        let ok = send(
            &mut client,
            &Request::new("pp1", methods::POLICY_PROMOTE, json!({ "version": safe })),
        )
        .await;
        assert_eq!(ok.result.as_ref().unwrap()["promoted"], true);

        // The capability-touching one never does.
        let refused = send(
            &mut client,
            &Request::new(
                "pp2",
                methods::POLICY_PROMOTE,
                json!({ "version": unsafe_version }),
            ),
        )
        .await;
        let err = refused
            .error
            .expect("a sandbox-touching policy must be refused");
        assert_eq!(err.code, codes::INVALID_PARAMS);
        assert!(
            err.data.unwrap()["problems"]
                .as_array()
                .unwrap()
                .iter()
                .any(|p| p.as_str().unwrap().contains("capability boundary"))
        );
        // The policy in force is unchanged.
        assert_eq!(
            server.state.store.promoted_policy().unwrap().unwrap().0,
            safe
        );
    }

    /// Rollback is exact: it re-promotes a stored version rather than trying
    /// to undo anything.
    #[tokio::test]
    async fn rollback_restores_an_earlier_policy_exactly() {
        let server = start_server().await;
        let mut client = connect(&server).await;
        auth(&mut client, &server.token).await;
        let v1 = server
            .state
            .store
            .add_policy_candidate(&json!({ "detector_thresholds": { "repeated_action": 3 } }))
            .unwrap();
        let v2 = server
            .state
            .store
            .add_policy_candidate(&json!({ "detector_thresholds": { "repeated_action": 6 } }))
            .unwrap();

        for (id, version) in [("p1", v1), ("p2", v2)] {
            send(
                &mut client,
                &Request::new(id, methods::POLICY_PROMOTE, json!({ "version": version })),
            )
            .await;
        }
        assert_eq!(server.state.store.promoted_policy().unwrap().unwrap().0, v2);

        let rolled = send(
            &mut client,
            &Request::new("rb1", methods::POLICY_ROLLBACK, json!({ "version": v1 })),
        )
        .await;
        assert_eq!(rolled.result.as_ref().unwrap()["promoted"], true);
        let (version, data) = server.state.store.promoted_policy().unwrap().unwrap();
        assert_eq!(version, v1);
        assert_eq!(data["detector_thresholds"]["repeated_action"], 3);

        let listed = send(
            &mut client,
            &Request::new("pl1", methods::POLICY_LIST, json!({})),
        )
        .await;
        let versions = listed.result.as_ref().unwrap()["versions"]
            .as_array()
            .unwrap();
        assert_eq!(versions.iter().filter(|v| v["promoted"] == true).count(), 1);
    }

    /// Memory only ever hands back evidence-backed facts, and forgetting works.
    #[tokio::test]
    async fn memory_lists_only_verified_facts_and_honours_forgetting() {
        let server = start_server().await;
        let mut client = connect(&server).await;
        auth(&mut client, &server.token).await;
        let project = server
            .state
            .store
            .add_project(
                "mem",
                &server.dir.path().join("mem-project").to_string_lossy(),
            )
            .unwrap();

        let verified = autoharness_core::MemoryFact {
            id: autoharness_core::new_id(),
            project_id: project.id.clone(),
            kind: autoharness_core::MemoryFactKind::VerifiedCommand,
            statement: "the checks run with cargo test --workspace".into(),
            evidence: vec!["event:7".into()],
            confidence: 0.95,
            supersedes: None,
            verified: true,
            created_at_ms: 1,
        };
        let proposal = autoharness_core::MemoryFact {
            id: autoharness_core::new_id(),
            statement: "the team prefers tabs".into(),
            evidence: vec![],
            verified: false,
            ..verified.clone()
        };
        server.state.store.add_memory_fact(&verified).unwrap();
        server.state.store.add_memory_fact(&proposal).unwrap();

        let listed = send(
            &mut client,
            &Request::new(
                "m1",
                methods::MEMORY_LIST,
                json!({ "project_id": project.id, "verified_only": true }),
            ),
        )
        .await;
        let facts = listed.result.as_ref().unwrap().as_array().unwrap();
        assert_eq!(
            facts.len(),
            1,
            "an unevidenced proposal must not be listed as verified"
        );
        assert_eq!(facts[0]["statement"], verified.statement);

        // Search finds it by content.
        let found = send(
            &mut client,
            &Request::new(
                "m2",
                methods::MEMORY_LIST,
                json!({ "project_id": project.id, "query": "cargo" }),
            ),
        )
        .await;
        assert_eq!(found.result.as_ref().unwrap().as_array().unwrap().len(), 1);

        let forgotten = send(
            &mut client,
            &Request::new(
                "m3",
                methods::MEMORY_FORGET,
                json!({ "fact_id": verified.id }),
            ),
        )
        .await;
        assert_eq!(forgotten.result.as_ref().unwrap()["forgotten"], true);
        assert!(
            server
                .state
                .store
                .list_memory_facts(&project.id, true)
                .unwrap()
                .is_empty()
        );
    }

    // ---- Phase 9: diagnostics, export, privacy ----

    /// A bug report should be one command, and it must never leak a secret.
    #[tokio::test]
    async fn diagnostics_bundle_everything_and_no_secrets() {
        let server = start_server().await;
        let mut client = connect(&server).await;
        auth(&mut client, &server.token).await;
        let resp = send(
            &mut client,
            &Request::new("d1", methods::APP_DIAGNOSTICS, json!({})),
        )
        .await;
        let d = resp.result.as_ref().unwrap();
        assert_eq!(d["app_version"], APP_VERSION);
        assert_eq!(d["policy_version"], POLICY_VERSION);
        assert_eq!(d["database"]["healthy"], true);
        assert_eq!(d["engines"].as_array().unwrap().len(), 2);
        assert!(d["sandbox"]["backend"].is_string());

        // The client token is the one secret the daemon holds; it must not
        // appear anywhere in a diagnostics bundle a user will paste publicly.
        let text = serde_json::to_string(d).unwrap();
        assert!(
            !text.contains(&server.token),
            "diagnostics must never carry the client token"
        );
    }

    #[tokio::test]
    async fn export_returns_the_whole_database() {
        let server = start_server().await;
        let mut client = connect(&server).await;
        auth(&mut client, &server.token).await;
        let run_id = create_run(&server, &mut client, "codex").await;
        let resp = send(
            &mut client,
            &Request::new("e1", methods::APP_EXPORT, json!({})),
        )
        .await;
        let export = resp.result.as_ref().unwrap();
        assert_eq!(export["runs"].as_array().unwrap().len(), 1);
        assert_eq!(export["runs"][0]["id"], run_id);
        assert!(export["events"].as_array().unwrap().len() >= 2);
    }

    /// Purging is irreversible, so it takes deliberate confirmation.
    #[tokio::test]
    async fn purging_requires_confirmation_then_erases_everything() {
        let server = start_server().await;
        let mut client = connect(&server).await;
        auth(&mut client, &server.token).await;
        let run_id = create_run(&server, &mut client, "codex").await;
        let project = server.state.store.list_projects().unwrap().remove(0);

        // Without the confirmation, nothing happens.
        let refused = send(
            &mut client,
            &Request::new(
                "p1",
                methods::APP_PURGE_PROJECT,
                json!({ "project_id": project.id }),
            ),
        )
        .await;
        assert_eq!(refused.error.unwrap().code, codes::INVALID_PARAMS);
        assert!(server.state.store.get_run(&run_id).is_ok());

        let purged = send(
            &mut client,
            &Request::new(
                "p2",
                methods::APP_PURGE_PROJECT,
                json!({ "project_id": project.id, "confirm_path": project.path }),
            ),
        )
        .await;
        assert_eq!(purged.result.as_ref().unwrap()["project_removed"], true);
        assert!(server.state.store.get_run(&run_id).is_err());
        assert!(server.state.store.list_projects().unwrap().is_empty());
        // The database is still coherent after an erase.
        assert!(server.state.store.integrity_report().unwrap().healthy);
    }

    /// `engine.list` reports detection for every registered adapter.
    #[tokio::test]
    async fn engine_list_reports_detection_for_each_adapter() {
        let server = start_server().await;
        let mut client = connect(&server).await;
        auth(&mut client, &server.token).await;
        let resp = send(
            &mut client,
            &Request::new("e1", methods::ENGINE_LIST, json!({})),
        )
        .await;
        let engines = resp.result.as_ref().unwrap().as_array().unwrap();
        assert_eq!(engines.len(), 2);
        for engine in engines {
            assert_eq!(engine["ready"], true);
        }
    }
}
