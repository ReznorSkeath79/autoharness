//! Replaceable sandbox backend. `/usr/bin/sandbox-exec` is deprecated by
//! Apple, so all Seatbelt execution goes through this trait.

use std::path::{Path, PathBuf};

use thiserror::Error;

#[derive(Debug, Error)]
pub enum SandboxError {
    #[error("sandbox backend unavailable: {0}")]
    Unavailable(String),
    #[error("external-write policy violation: {0}")]
    PolicyViolation(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// Result of a sandboxed command.
#[derive(Debug)]
pub struct CommandResult {
    pub status: std::process::ExitStatus,
    pub stdout: String,
    pub stderr: String,
}

impl CommandResult {
    pub fn success(&self) -> bool {
        self.status.success()
    }
}

/// Executes commands inside a kernel sandbox. Implementations must honor
/// deny-by-default profiles; there is deliberately no "permissive" backend.
#[async_trait::async_trait]
pub trait SandboxBackend: Send + Sync {
    fn name(&self) -> &'static str;

    /// Cheap availability probe: the backend exists and can execute a
    /// trivial profiled command.
    async fn is_available(&self) -> bool;

    /// Run `program args` under `profile` with a pre-sanitized environment.
    async fn run(
        &self,
        profile: &str,
        program: &Path,
        args: &[String],
        env: &[(String, String)],
        cwd: &Path,
    ) -> Result<CommandResult, SandboxError>;

    /// The executable that wraps a long-lived interactive child in a profile,
    /// for backends that can do that. `None` means this backend confines only
    /// the commands it runs itself, and a terminal agent must be refused
    /// rather than run outside the sandbox.
    fn agent_program(&self) -> Option<PathBuf> {
        None
    }
}

/// macOS Seatbelt via `/usr/bin/sandbox-exec -p <profile>`.
pub struct SeatbeltBackend {
    path: PathBuf,
}

impl Default for SeatbeltBackend {
    fn default() -> Self {
        Self {
            path: PathBuf::from("/usr/bin/sandbox-exec"),
        }
    }
}

impl SeatbeltBackend {
    pub fn with_path(path: PathBuf) -> Self {
        Self { path }
    }
}

#[async_trait::async_trait]
impl SandboxBackend for SeatbeltBackend {
    fn name(&self) -> &'static str {
        "seatbelt(sandbox-exec)"
    }

    fn agent_program(&self) -> Option<PathBuf> {
        self.path.is_file().then(|| self.path.clone())
    }

    async fn is_available(&self) -> bool {
        if !self.path.is_file() {
            return false;
        }
        // Execute a trivial command under a permissive profile to prove the
        // backend actually works (not merely exists).
        let result = tokio::process::Command::new(&self.path)
            .args(["-p", "(version 1)(allow default)", "/usr/bin/true"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await;
        matches!(result, Ok(status) if status.success())
    }

    async fn run(
        &self,
        profile: &str,
        program: &Path,
        args: &[String],
        env: &[(String, String)],
        cwd: &Path,
    ) -> Result<CommandResult, SandboxError> {
        if !self.path.is_file() {
            return Err(SandboxError::Unavailable(format!(
                "{} not found",
                self.path.display()
            )));
        }
        let mut command = tokio::process::Command::new(&self.path);
        command
            .arg("-p")
            .arg(profile)
            .arg(program)
            .args(args)
            .current_dir(cwd)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        autoharness_engines::process::apply_isolation(&mut command, env);
        let output = command.output().await?;
        Ok(CommandResult {
            status: output.status,
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}
