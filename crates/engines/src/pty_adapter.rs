//! Driving an agent that only paints a terminal.
//!
//! Codex and Claude speak structured protocols and have hand-written adapters.
//! Most coding CLIs do not: they draw a TUI, ask questions in it, and expect a
//! human. This adapter runs one of those on a real pseudo-terminal and turns
//! what it *paints* into the same [`EngineEvent`]s every other adapter emits.
//!
//! That last part is the whole design. Nothing downstream — the ledger, the
//! checks, the worktree, the router, the cross-engine handoff — learns that a
//! terminal exists. A PTY agent is a third `EngineAdapter`, not a second way
//! for a run to happen, so there is still exactly one vocabulary describing
//! what a run did.
//!
//! # What is and is not knowable here
//!
//! A structured adapter is *told* what the agent did. This one infers it from
//! a screen, and the difference is not cosmetic:
//!
//! - **Questions are reliable.** Manifest rules match a prompt box that is
//!   still visible after all the redraws, which is exactly what a blocker is.
//! - **Completion is a judgement.** "Went idle and stayed idle" is the best
//!   available signal, debounced by the reducer against redraw flicker.
//! - **File changes are not observable at all.** A terminal does not report
//!   them. The daemon already learns them from the worktree diff, which is
//!   ground truth rather than a claim, so this adapter emits none instead of
//!   guessing from scrollback.
//! - **Token usage is not observable.** No `Usage` is emitted rather than a
//!   fabricated number.
//!
//! Everything scraped off a screen passes through redaction before it leaves
//! this module: it is shown to the user and written to the ledger, and an
//! agent can print anything at all.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use autoharness_core::EngineKind;
use autoharness_pty::SessionStatus;
use autoharness_pty::agent::AgentDescriptor;
use autoharness_pty::detect::redact;
use autoharness_pty::pty::{Exit, Pty, PtySpec, PtyStream};
use autoharness_pty::screen::HeadlessScreen;
use autoharness_pty::status::{StatusReducer, StatusSignal};
use tokio::sync::mpsc;

use crate::adapter::{EngineAdapter, EngineError, SessionSpec};
use crate::diagnostics::EngineDiagnostics;
use crate::event::EngineEvent;
use crate::process::SessionDirs;

/// How often the pump wakes when the agent is quiet. The reducer's debounce
/// windows are tenths of a second, so a slower tick would delay every
/// working→idle decision by the difference.
const POLL: Duration = Duration::from_millis(100);

/// Bytes read from the PTY in one go.
const READ_CHUNK: usize = 8 * 1024;

/// A terminal the agent believes it has. Wide enough that a TUI does not
/// choose its cramped layout, which is what a manifest's rules were written
/// against.
const COLS: u16 = 120;
const ROWS: u16 = 40;

/// How long to let a cancelled agent exit before the process group is killed.
const TERMINATE_GRACE: Duration = Duration::from_secs(3);

/// Supplies the Seatbelt confinement an agent runs inside.
///
/// A profile's writable roots are per-session — the run's worktree, its fake
/// HOME, its TMPDIR — so this is asked at session start rather than handed
/// over once. The daemon implements it because the daemon owns the policy;
/// this crate only carries the answer to the spawn, which keeps the dependency
/// pointing the right way: engines does not know what a run is, let alone what
/// a sandbox policy should permit.
pub trait Confinement: Send + Sync {
    /// Usually `/usr/bin/sandbox-exec`.
    fn program(&self) -> PathBuf;
    /// A profile permitting writes to exactly `writable_roots`.
    fn profile_for(&self, writable_roots: &[PathBuf]) -> String;
}

/// An agent driven through a pseudo-terminal.
pub struct PtyAdapter {
    kind: EngineKind,
    /// Manifests are compiled in; this only adds or replaces them, so a rule
    /// can be corrected without a rebuild.
    manifest_overrides: Option<PathBuf>,
    /// The daemon's read-only network broker, when one is running. Direct
    /// egress is denied by the sandbox profile; anything allowed goes here.
    proxy_addr: Option<String>,
    /// Only a test may run an agent outside the sandbox, and it has to say so.
    allow_unconfined: bool,
    /// Seatbelt confinement for the agent process.
    ///
    /// `None` runs the agent UNCONFINED, which is only ever right in a test.
    /// The daemon always supplies one: a structured engine's commands go
    /// through `Sandbox::run_command`, and an agent driven on a terminal is no
    /// more trusted for being interactive.
    confinement: Option<Arc<dyn Confinement>>,
    session_id: Option<String>,
    /// `None` until `start_session`; closed when the agent exits.
    events: Option<mpsc::UnboundedReceiver<Result<EngineEvent, EngineError>>>,
    /// Writing here is typing at the agent.
    input: Option<PtyStream>,
    /// Shared with the pump so cancellation can reach the child.
    pty: Option<Arc<Mutex<Pty>>>,
    /// The keystrokes this agent's manifest says mean yes and no. Read once at
    /// session start, because answering must not depend on re-reading a file
    /// while the agent is waiting.
    approve: Option<autoharness_pty::agent::ApproveSpec>,
    deny: Option<autoharness_pty::agent::ApproveSpec>,
}

impl PtyAdapter {
    pub fn new(kind: EngineKind, manifest_overrides: Option<PathBuf>) -> Self {
        Self {
            kind,
            manifest_overrides,
            proxy_addr: None,
            confinement: None,
            allow_unconfined: false,
            session_id: None,
            events: None,
            input: None,
            pty: None,
            approve: None,
            deny: None,
        }
    }

    /// Route the agent's allowed network access through the daemon's broker.
    pub fn with_proxy(mut self, proxy_addr: Option<String>) -> Self {
        self.proxy_addr = proxy_addr;
        self
    }

    /// Run WITHOUT a sandbox. Tests only, and named so that a production call
    /// site reads as obviously wrong.
    pub fn allow_unconfined_for_tests(mut self) -> Self {
        self.allow_unconfined = true;
        self
    }

    /// Confine the agent with a Seatbelt profile, the same way daemon-owned
    /// commands are confined.
    pub fn with_confinement(mut self, confinement: Option<Arc<dyn Confinement>>) -> Self {
        self.confinement = confinement;
        self
    }

    /// The manifest describing this agent, or a structured error naming the id
    /// that was not found — a missing manifest is a setup problem the user can
    /// fix, not an internal fault.
    fn descriptor(&self) -> Result<AgentDescriptor, EngineError> {
        let (engine, _failed) =
            autoharness_pty::manifests::load(self.manifest_overrides.as_deref());
        engine
            .manifest(self.kind.as_str())
            .and_then(|manifest| manifest.agent.clone())
            .ok_or_else(|| EngineError::Unsupported(format!("no agent manifest for {}", self.kind)))
    }

    fn writer(&mut self) -> Result<&mut PtyStream, EngineError> {
        self.input
            .as_mut()
            .ok_or_else(|| EngineError::Protocol("no live PTY session".into()))
    }

    /// Type bytes at the agent exactly as a keyboard would.
    fn type_bytes(&mut self, bytes: &[u8]) -> Result<(), EngineError> {
        use std::io::Write;
        let writer = self.writer()?;
        writer
            .write_all(bytes)
            .and_then(|()| writer.flush())
            .map_err(|error| EngineError::Protocol(format!("writing to the agent failed: {error}")))
    }
}

#[async_trait::async_trait]
impl EngineAdapter for PtyAdapter {
    fn kind(&self) -> EngineKind {
        self.kind.clone()
    }

    async fn detect(&self) -> EngineDiagnostics {
        let descriptor = match self.descriptor() {
            Ok(descriptor) => descriptor,
            Err(error) => {
                return EngineDiagnostics::not_installed(self.kind.clone())
                    .with_problem(error.to_string());
            }
        };
        let Some(binary) = descriptor.binary.clone() else {
            // `shell` and `generic` take their command from the caller, so
            // there is no binary to look for and nothing to verify.
            return EngineDiagnostics::not_installed(self.kind.clone())
                .with_problem(format!("{} has no binary to launch", self.kind));
        };
        match which(&binary) {
            Some(path) => {
                let mut diagnostics =
                    EngineDiagnostics::ready(self.kind.clone(), path, "terminal".into());
                // A terminal agent is the definition of unstructured: its
                // status is inferred from a screen, not reported. Saying
                // otherwise would let the UI promise a fidelity that is not
                // there.
                diagnostics.structured_mode = false;
                // Nothing here can verify a login. An agent that is installed
                // but signed out looks identical until it says so on screen.
                diagnostics.authenticated = None;
                diagnostics
            }
            None => EngineDiagnostics::not_installed(self.kind.clone())
                .with_problem(format!("{binary} not found on PATH")),
        }
    }

    async fn start_session(&mut self, spec: &SessionSpec) -> Result<(), EngineError> {
        let descriptor = self.descriptor()?;
        let binary = descriptor
            .binary
            .clone()
            .ok_or_else(|| EngineError::Unsupported(format!("{} has no binary", self.kind)))?;
        let resolved = which(&binary)
            .ok_or_else(|| EngineError::NotInstalled(format!("{binary} not found on PATH")))?;

        // The session id is ours, not the agent's: a terminal agent reports no
        // identity, and a run still needs something stable to key on.
        let session_id = format!("pty-{}", autoharness_core::new_id());

        // The same per-session fake HOME and TMPDIR a structured engine gets.
        // Claude reaches its Keychain login through HOME, so this is also what
        // lets a terminal-driven Claude find the user's account rather than
        // starting logged out.
        let dirs = SessionDirs::create_for_engine(&spec.data_dir, &session_id)
            .map_err(|error| EngineError::Protocol(format!("session dirs failed: {error}")))?;

        // Fail closed. A terminal agent with no confinement would be the one
        // process here that runs outside the sandbox, and it is the least
        // trusted thing in the product: somebody else's CLI, driven by a model,
        // holding a real terminal. Running it loose is worse than not running.
        let argv = match (&self.confinement, self.allow_unconfined) {
            (Some(confinement), _) => {
                // The agent runs INSIDE `sandbox-exec`, so the confinement owns
                // the terminal and the agent cannot fork its way out of it.
                let roots = vec![
                    spec.working_dir.clone(),
                    dirs.home.clone(),
                    dirs.tmp.clone(),
                ];
                vec![
                    confinement.program().display().to_string(),
                    "-p".to_string(),
                    confinement.profile_for(&roots),
                    resolved.display().to_string(),
                ]
            }
            (None, true) => vec![resolved.display().to_string()],
            (None, false) => {
                return Err(EngineError::Unsupported(format!(
                    "{} cannot run: no sandbox confinement is available for terminal agents",
                    self.kind
                )));
            }
        };

        let mut pty_spec = PtySpec::new(argv, spec.working_dir.clone())
            .size(COLS, ROWS)
            // The same caps `apply_isolation` gives a structured engine.
            .limits(1024, 512);
        for (key, value) in
            agent_environment(&descriptor, &dirs, self.proxy_addr.as_deref(), &resolved)
        {
            pty_spec = pty_spec.env(&key, &value);
        }

        let pty = Pty::spawn(&pty_spec)
            .map_err(|error| EngineError::Protocol(format!("PTY spawn failed: {error}")))?;
        let reader = pty
            .reader()
            .map_err(|error| EngineError::Protocol(format!("PTY reader failed: {error}")))?;
        let writer = pty
            .writer()
            .map_err(|error| EngineError::Protocol(format!("PTY writer failed: {error}")))?;

        self.session_id = Some(session_id.clone());
        self.input = Some(writer);
        self.approve = descriptor.approve.clone();
        self.deny = descriptor.deny.clone();

        let pty = Arc::new(Mutex::new(pty));
        self.pty = Some(Arc::clone(&pty));

        let (tx, rx) = mpsc::unbounded_channel();
        self.events = Some(rx);
        let _ = tx.send(Ok(EngineEvent::SessionIdentity { session_id }));

        let manifest_id = self.kind.as_str().to_string();
        let manifest_overrides = self.manifest_overrides.clone();
        let authority = descriptor.authority();
        // A dedicated OS thread, not a tokio task: every call in the loop
        // below blocks on a file descriptor, and blocking a runtime worker
        // would stall every other run in the daemon.
        std::thread::Builder::new()
            .name(format!("pty-pump-{manifest_id}"))
            .spawn(move || {
                pump(PumpContext {
                    manifest_id,
                    manifest_overrides,
                    authority,
                    reader,
                    pty,
                    tx,
                })
            })
            .map_err(|error| EngineError::Protocol(format!("pump thread failed: {error}")))?;
        Ok(())
    }

    async fn resume_session(
        &mut self,
        _session_id: &str,
        _spec: &SessionSpec,
    ) -> Result<(), EngineError> {
        // A PTY session dies with its process. Some agents can resume their own
        // conversation from a transcript, but that is the agent's resume flag
        // producing a NEW terminal, not this session coming back — and the
        // caller must know the difference, so it is refused rather than faked.
        Err(EngineError::Unsupported(
            "a terminal session cannot be reattached after its process ends".into(),
        ))
    }

    async fn send_turn(&mut self, prompt: &str) -> Result<(), EngineError> {
        // Typing at a TUI: the text, then Return. Newlines inside the prompt
        // are sent as Return too, because that is what pasting does — and an
        // agent that treats the first line as the whole prompt would silently
        // drop the rest either way.
        let mut bytes = prompt.replace('\n', "\r").into_bytes();
        bytes.push(b'\r');
        self.type_bytes(&bytes)
    }

    async fn next_event(&mut self) -> Result<Option<EngineEvent>, EngineError> {
        let Some(events) = self.events.as_mut() else {
            return Ok(None);
        };
        match events.recv().await {
            Some(Ok(event)) => Ok(Some(event)),
            Some(Err(error)) => Err(error),
            None => Ok(None),
        }
    }

    async fn pause(&mut self) -> Result<(), EngineError> {
        // The trait's soft pause: interrupt the turn in flight and leave the
        // session usable, so a later `send_turn` just continues.
        self.interrupt().await
    }

    async fn interrupt(&mut self) -> Result<(), EngineError> {
        // Ctrl-C at the terminal, which is what a user would press.
        self.type_bytes(&[0x03])
    }

    async fn answer(&mut self, answer: autoharness_core::Answer) -> Result<(), EngineError> {
        use autoharness_core::Answer;
        // Approving and refusing are whatever this agent's manifest declares,
        // because "1" means yes to Claude Code and "y" means yes to aider, and
        // guessing wrong is a keystroke sent to a waiting agent.
        let (spec, what) = match &answer {
            Answer::Approve => (self.approve.clone(), "approve"),
            Answer::Deny => (self.deny.clone(), "deny"),
            Answer::Text(text) => {
                let mut bytes = text.replace('\n', "\r").into_bytes();
                bytes.push(b'\r');
                return self.type_bytes(&bytes);
            }
        };
        let spec = spec.ok_or_else(|| {
            // Said plainly rather than silently doing nothing: an agent waiting
            // on a prompt nobody can answer looks exactly like a hung run.
            EngineError::Unsupported(format!(
                "{} declares no keystrokes to {what} a prompt",
                self.kind
            ))
        })?;
        let mut bytes = spec.text.unwrap_or_default().into_bytes();
        if spec.submit {
            bytes.push(b'\r');
        }
        if bytes.is_empty() {
            return Err(EngineError::Unsupported(format!(
                "{} declares an empty {what} keystroke",
                self.kind
            )));
        }
        self.type_bytes(&bytes)
    }

    async fn cancel(&mut self) -> Result<(), EngineError> {
        self.input = None;
        let Some(pty) = self.pty.take() else {
            return Ok(());
        };
        let mut pty = pty
            .lock()
            .map_err(|_| EngineError::Protocol("PTY lock poisoned".into()))?;
        pty.terminate(TERMINATE_GRACE)
            .map_err(|error| EngineError::Protocol(format!("terminate failed: {error}")))?;
        Ok(())
    }

    fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }
}

struct PumpContext {
    manifest_id: String,
    manifest_overrides: Option<PathBuf>,
    authority: autoharness_pty::status::Authority,
    reader: PtyStream,
    pty: Arc<Mutex<Pty>>,
    tx: mpsc::UnboundedSender<Result<EngineEvent, EngineError>>,
}

/// Read the terminal, decide what the agent is doing, and say so in the
/// vocabulary the rest of the product speaks.
///
/// Runs until the child exits or the receiver is dropped. Every send is
/// checked: once nobody is listening the agent is still running, and
/// continuing to emulate its screen would burn a core for nothing.
fn pump(ctx: PumpContext) {
    use std::io::Read;

    let PumpContext {
        manifest_id,
        manifest_overrides,
        authority,
        mut reader,
        pty,
        tx,
    } = ctx;

    let (engine, _failed) = autoharness_pty::manifests::load(manifest_overrides.as_deref());
    let mut screen = HeadlessScreen::new(COLS as usize, ROWS as usize);
    let mut reducer = StatusReducer::new(authority, SystemTime::now());
    let mut buffer = vec![0u8; READ_CHUNK];
    let mut last_seq = 0;
    let mut emitted_lines = 0usize;

    loop {
        let readable = reader.wait_readable(POLL).unwrap_or(false);

        let mut signals: Vec<StatusSignal> = Vec::new();
        if readable {
            match reader.read(&mut buffer) {
                Ok(0) => {}
                Ok(read) => {
                    screen.feed(&buffer[..read]);
                    signals.push(StatusSignal::PtyOutputActivity);
                }
                // The master reports EIO when the child closes its side; that
                // is an exit, not a failure, and `try_wait` below reports it.
                Err(_) => {}
            }
        }

        // Re-evaluate only when the screen actually changed. A quiet agent
        // otherwise re-runs every rule ten times a second for one answer.
        let snapshot = screen.snapshot();
        if snapshot.content_seq != last_seq {
            last_seq = snapshot.content_seq;
            if let Some(observation) = engine.evaluate(&snapshot, &manifest_id) {
                signals.push(StatusSignal::Screen(observation));
            }
            // Newly committed lines are the closest thing a terminal has to
            // the agent saying something. Emitting them as deltas is what
            // makes a PTY run watchable rather than a box that goes quiet.
            let lines = screen.lines();
            let fresh: Vec<String> = lines
                .iter()
                .skip(emitted_lines)
                .filter(|line| !line.trim().is_empty())
                .map(|line| redact(line))
                .collect();
            emitted_lines = lines.len();
            for line in fresh {
                if tx.send(Ok(EngineEvent::TextDelta { delta: line })).is_err() {
                    return;
                }
            }
        }
        signals.push(StatusSignal::Tick);

        let exit = {
            let mut pty = match pty.lock() {
                Ok(pty) => pty,
                Err(_) => return,
            };
            pty.try_wait().ok().flatten()
        };
        if let Some(exit) = exit {
            let (code, signal) = match exit {
                Exit::Code(code) => (Some(code), None),
                Exit::Signal(signal) => (None, Some(signal)),
            };
            signals.push(StatusSignal::ProcessExit { code, signal });
        }

        let now = SystemTime::now();
        for signal in signals {
            let outcome = reducer.reduce(signal, now);
            for event in translate(&outcome, &screen) {
                if tx.send(Ok(event)).is_err() {
                    return;
                }
            }
        }

        if exit.is_some() {
            return;
        }
    }
}

/// One reducer outcome, in the product's vocabulary.
fn translate(
    outcome: &autoharness_pty::status::ReducerOutcome,
    screen: &HeadlessScreen,
) -> Vec<EngineEvent> {
    let mut events = Vec::new();

    if let Some(detail) = &outcome.needs_input {
        // A blocked agent is the one case a terminal reports as well as a
        // structured protocol does: the prompt is still on screen, which is
        // what makes it a blocker rather than scrollback.
        let mut prompt = detail.summary.clone();
        if let Some(options) = &detail.options
            && !options.is_empty()
        {
            prompt.push_str("\n\n");
            prompt.push_str(&options.join("\n"));
        }
        events.push(EngineEvent::Question {
            id: format!("{:?}", detail.kind).to_lowercase(),
            prompt,
        });
    }

    match &outcome.status_change {
        Some(SessionStatus::Exited(info)) => {
            let ok = info.reason == autoharness_pty::ExitReason::Exited && info.code == Some(0);
            if ok {
                events.push(EngineEvent::Completed {
                    summary: Some(visible_summary(screen)),
                });
            } else {
                events.push(EngineEvent::Failed {
                    message: match (info.code, info.signal) {
                        (_, Some(signal)) => format!("agent killed by signal {signal}"),
                        (Some(code), _) => format!("agent exited with status {code}"),
                        _ => "agent exited abnormally".into(),
                    },
                    // A dead terminal is not something a retry inside this run
                    // can recover: the process is gone.
                    recoverable: false,
                });
            }
        }
        Some(SessionStatus::Unknown) => {
            // Running, but nothing readable has arrived long enough that any
            // claim would be a guess. Said plainly rather than reported as
            // progress.
            events.push(EngineEvent::TerminalOutput {
                line: "(no output from the agent for a while)".into(),
            });
        }
        _ => {}
    }

    // A completed turn on a live session: the agent stopped working and stayed
    // stopped. That is a judgement, not a report, and the reducer's debounce is
    // what keeps a redraw from looking like one.
    if outcome.turn_completed
        && !matches!(outcome.status_change, Some(SessionStatus::Exited(_)))
        && !matches!(
            outcome.status_change,
            Some(SessionStatus::NeedsInput(_)) | None
        )
    {
        events.push(EngineEvent::Completed {
            summary: Some(visible_summary(screen)),
        });
    }

    events
}

/// What the terminal is showing, redacted and trimmed. Used as a turn summary
/// because it is the only account of the work that exists.
fn visible_summary(screen: &HeadlessScreen) -> String {
    let lines = screen.lines();
    let tail: Vec<String> = lines
        .iter()
        .rev()
        .filter(|line| !line.trim().is_empty())
        .take(12)
        .map(|line| redact(line))
        .collect();
    tail.into_iter().rev().collect::<Vec<_>>().join("\n")
}

/// The environment an agent gets: built from scratch, exactly like a
/// structured engine's.
///
/// This inherited `std::env::vars()` minus a scrub list at first, which was
/// wrong in a way worth naming. `spawn_json_lines_child` calls `env_clear()`
/// and hands the child an explicit set, and `FORBIDDEN_ENV_VARS` documents
/// that policy as "since envs are built from scratch these never appear".
/// An inheriting PTY spawn would have quietly handed every agent the daemon's
/// `GITHUB_TOKEN`, `AWS_SECRET_ACCESS_KEY`, `SSH_AUTH_SOCK` and `BASH_ENV` —
/// the exact set that list exists to keep out. A terminal agent is no more
/// trusted than a structured one.
fn agent_environment(
    descriptor: &AgentDescriptor,
    dirs: &SessionDirs,
    proxy_addr: Option<&str>,
    binary: &std::path::Path,
) -> Vec<(String, String)> {
    let mut env = crate::process::worker_env(dirs, proxy_addr);

    // Node-based CLIs re-exec themselves and shell out to siblings by name, so
    // the directory the binary was actually found in goes on PATH — that one
    // directory, never the user's whole PATH.
    if let Some(bin_dir) = binary.parent() {
        let bin_dir = bin_dir.to_string_lossy().into_owned();
        for (key, value) in env.iter_mut() {
            if key == "PATH" && !value.split(':').any(|p| p == bin_dir) {
                *value = format!("{bin_dir}:{value}");
            }
        }
    }

    // Our PTY is a 24-bit colour xterm however the daemon itself was launched.
    // Without this an agent renders monochrome and half the manifest's rules —
    // which were written against a coloured TUI — stop matching.
    env.push(("TERM".into(), "xterm-256color".into()));
    env.push(("COLORTERM".into(), "truecolor".into()));

    // The manifest's own variables last, so an agent can override the defaults
    // it needs to (and only those).
    for (key, value) in &descriptor.env {
        env.push((key.clone(), value.clone()));
    }

    // Defence in depth: a manifest cannot reintroduce a forbidden variable.
    env.retain(|(key, _)| {
        !crate::process::FORBIDDEN_ENV_VARS
            .iter()
            .any(|forbidden| forbidden == key)
    });
    env
}

/// First match for `binary` on PATH. An absolute path is taken as given.
fn which(binary: &str) -> Option<PathBuf> {
    let candidate = PathBuf::from(binary);
    if candidate.is_absolute() {
        return candidate.is_file().then_some(candidate);
    }
    std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|dir| dir.join(binary))
            .find(|candidate| candidate.is_file())
    })
}
