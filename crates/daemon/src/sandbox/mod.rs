//! macOS sandbox layer: Seatbelt runner, environment isolation (via
//! `autoharness_engines::process`), read-only brokered proxy, external-write
//! policy, and fail-closed startup canaries.
//!
//! Two confinement layers (PLAN.md "macOS sandbox and authority"):
//! 1. Engine CONTROL processes (codex app-server, claude) run unsandboxed by
//!    us but with sanitized env + the engine's NATIVE sandbox for their tool
//!    subprocesses (codex `workspace-write`, claude `acceptEdits` +
//!    disallowed network tools).
//! 2. Daemon-owned worker commands (shell/build/verification/integration)
//!    run through generated Seatbelt profiles with sanitized env, a process
//!    group, resource limits, and proxy-only networking.

pub mod backend;
pub mod canaries;
mod home;
pub mod policy;
pub mod profile;
pub mod proxy;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use autoharness_engines::process::{SessionDirs, worker_env};
use serde::{Deserialize, Serialize};

use backend::{CommandResult, SandboxBackend, SandboxError, SeatbeltBackend};
use canaries::CanaryReport;
use proxy::{AuditFn, FetchProxy, ProxyConfig};

/// Everything the daemon needs to confine worker commands.
pub struct Sandbox {
    backend: Arc<dyn SandboxBackend>,
    proxy: Option<FetchProxy>,
    data_dir: PathBuf,
}

impl Sandbox {
    pub fn proxy_port(&self) -> Option<u16> {
        self.proxy.as_ref().map(|p| p.addr.port())
    }

    /// The confinement a terminal-driven agent should run inside, or `None`
    /// when this backend cannot confine a long-lived interactive process. A
    /// `None` here must block the run rather than fall back to unconfined.
    pub fn agent_confinement(&self) -> Option<AgentConfinement> {
        let program = self.backend.agent_program()?;
        Some(AgentConfinement::new(
            program,
            self.proxy_port(),
            self.data_dir.clone(),
        ))
    }

    pub fn backend_name(&self) -> &'static str {
        self.backend.name()
    }

    /// Run a daemon-owned worker command: external-write policy check first,
    /// then Seatbelt profile + sanitized env + process group + rlimits.
    pub async fn run_command(
        &self,
        worktree: &Path,
        dirs: &SessionDirs,
        program: &Path,
        args: &[String],
        cwd: &Path,
    ) -> Result<CommandResult, SandboxError> {
        let writable = vec![worktree.to_path_buf(), dirs.home.clone(), dirs.tmp.clone()];
        self.run_with_roots(writable, dirs, program, args, cwd, true)
            .await
    }

    /// Run a daemon bookkeeping command (git worktree/branch/commit). These
    /// legitimately write OUTSIDE the worktree — into the repository's
    /// `.git` directory and the daemon's worktree storage — so their extra
    /// writable roots are declared explicitly. The external-write policy
    /// still applies (no push, ever).
    pub async fn run_bookkeeping(
        &self,
        extra_writable: &[PathBuf],
        dirs: &SessionDirs,
        program: &Path,
        args: &[String],
        cwd: &Path,
    ) -> Result<CommandResult, SandboxError> {
        let mut writable = extra_writable.to_vec();
        writable.push(dirs.home.clone());
        writable.push(dirs.tmp.clone());
        self.run_with_roots(writable, dirs, program, args, cwd, true)
            .await
    }

    async fn run_with_roots(
        &self,
        writable: Vec<PathBuf>,
        dirs: &SessionDirs,
        program: &Path,
        args: &[String],
        cwd: &Path,
        check_policy: bool,
    ) -> Result<CommandResult, SandboxError> {
        if check_policy
            && let Some(reason) = policy::external_write_violation(&program.to_string_lossy(), args)
        {
            return Err(SandboxError::PolicyViolation(reason));
        }
        let profile = profile::generate(&profile::SandboxSpec {
            writable_roots: writable,
            proxy_port: self.proxy_port(),
            data_dir: Some(self.data_dir.clone()),
        });
        let proxy_addr = self.proxy.as_ref().map(|p| p.addr.to_string());
        let env = worker_env(dirs, proxy_addr.as_deref());
        self.backend.run(&profile, program, args, &env, cwd).await
    }

    /// Run without the policy check (canaries probe forbidden actions on
    /// purpose; the kernel-level profile is what's being proven).
    async fn run_raw(
        &self,
        worktree: &Path,
        dirs: &SessionDirs,
        program: &Path,
        args: &[String],
        cwd: &Path,
    ) -> Result<CommandResult, SandboxError> {
        let writable = vec![worktree.to_path_buf(), dirs.home.clone(), dirs.tmp.clone()];
        self.run_with_roots(writable, dirs, program, args, cwd, false)
            .await
    }

    /// Canary probe entry point (raw, no policy check).
    pub async fn probe(
        &self,
        worktree: &Path,
        program: &str,
        args: &[String],
    ) -> Result<CommandResult, SandboxError> {
        let dirs = SessionDirs::create(&self.data_dir, "canary-session")?;
        self.run_raw(worktree, &dirs, Path::new(program), args, worktree)
            .await
    }
}

/// Startup verdict: the daemon refuses to start runs unless this is Ready.
pub enum SandboxStatus {
    Ready(Box<Sandbox>),
    Unavailable(SandboxDiagnostics),
}

impl SandboxStatus {
    pub fn ready(&self) -> bool {
        matches!(self, SandboxStatus::Ready(_))
    }

    pub fn sandbox(&self) -> Option<&Sandbox> {
        match self {
            SandboxStatus::Ready(s) => Some(s),
            SandboxStatus::Unavailable(_) => None,
        }
    }

    pub fn diagnostics(&self) -> SandboxDiagnostics {
        match self {
            SandboxStatus::Ready(s) => SandboxDiagnostics {
                ready: true,
                backend: s.backend_name().into(),
                proxy_addr: s.proxy.as_ref().map(|p| p.addr.to_string()),
                problems: vec![],
                canaries: None,
            },
            SandboxStatus::Unavailable(d) => d.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxDiagnostics {
    pub ready: bool,
    pub backend: String,
    pub proxy_addr: Option<String>,
    pub problems: Vec<String>,
    pub canaries: Option<CanaryReport>,
}

/// Fail-closed startup: backend probe → proxy bind → canaries. Any failure
/// yields `Unavailable` with structured diagnostics; there is NO permissive
/// fallback.
pub async fn init(data_dir: &Path, audit: AuditFn) -> SandboxStatus {
    let mut problems = Vec::new();

    let backend: Arc<dyn SandboxBackend> = Arc::new(SeatbeltBackend::default());
    if !backend.is_available().await {
        problems.push(format!("sandbox backend {} is unavailable", backend.name()));
    }

    let proxy = match FetchProxy::start(ProxyConfig::default(), audit).await {
        Ok(p) => Some(p),
        Err(e) => {
            problems.push(format!("read-only proxy failed to bind: {e}"));
            None
        }
    };

    if !problems.is_empty() {
        return SandboxStatus::Unavailable(SandboxDiagnostics {
            ready: false,
            backend: backend.name().into(),
            proxy_addr: None,
            problems,
            canaries: None,
        });
    }

    let sandbox = Sandbox {
        backend,
        proxy,
        data_dir: data_dir.to_path_buf(),
    };

    // Canaries: prove confinement before any run is accepted.
    let canary_worktree = data_dir.join("sessions").join("canary-worktree");
    if let Err(e) = std::fs::create_dir_all(&canary_worktree) {
        return SandboxStatus::Unavailable(SandboxDiagnostics {
            ready: false,
            backend: sandbox.backend_name().into(),
            proxy_addr: sandbox.proxy.as_ref().map(|p| p.addr.to_string()),
            problems: vec![format!("canary worktree: {e}")],
            canaries: None,
        });
    }
    let report = canaries::run_canaries(&sandbox, &canary_worktree).await;
    if !report.ok {
        return SandboxStatus::Unavailable(SandboxDiagnostics {
            ready: false,
            backend: sandbox.backend_name().into(),
            proxy_addr: sandbox.proxy.as_ref().map(|p| p.addr.to_string()),
            problems: report
                .results
                .iter()
                .filter(|r| !r.passed)
                .map(|r| format!("canary {} failed: {}", r.name, r.detail))
                .collect(),
            canaries: Some(report),
        });
    }

    SandboxStatus::Ready(Box::new(sandbox))
}

#[cfg(test)]
pub(crate) fn ready_for_tests(data_dir: &Path) -> SandboxStatus {
    // Daemon RPC tests inject readiness without burning canary time, but the
    // backend is the REAL Seatbelt runner: worktree git commands in those
    // tests execute under a genuine profile. The canaries themselves are
    // exercised for real in `sandbox::tests`.
    SandboxStatus::Ready(Box::new(Sandbox {
        backend: Arc::new(SeatbeltBackend::default()),
        proxy: None,
        data_dir: data_dir.to_path_buf(),
    }))
}

/// The Seatbelt confinement a terminal-driven agent runs inside.
///
/// A structured engine's commands reach the kernel through `run_command`,
/// which generates a profile per invocation. A PTY agent is one long-lived
/// process instead, so the profile is generated once at session start with
/// that session's writable roots and `sandbox-exec` becomes the process that
/// owns the terminal — the agent cannot outlive or fork its way out of it.
///
/// Policy stays here. `autoharness-engines` only carries the answer to the
/// spawn; it has no idea what a run is, let alone what should be permitted.
pub struct AgentConfinement {
    backend_program: PathBuf,
    proxy_port: Option<u16>,
    data_dir: PathBuf,
}

impl AgentConfinement {
    pub fn new(backend_program: PathBuf, proxy_port: Option<u16>, data_dir: PathBuf) -> Self {
        Self {
            backend_program,
            proxy_port,
            data_dir,
        }
    }
}

impl autoharness_engines::Confinement for AgentConfinement {
    fn program(&self) -> PathBuf {
        self.backend_program.clone()
    }

    fn profile_for(&self, writable_roots: &[PathBuf]) -> String {
        profile::generate(&profile::SandboxSpec {
            writable_roots: writable_roots.to_vec(),
            proxy_port: self.proxy_port,
            data_dir: Some(self.data_dir.clone()),
        })
    }
}
