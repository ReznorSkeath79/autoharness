//! PTY ownership: spawning a child on a pseudo-terminal and driving it.
//!
//! This is the sharpest platform seam in the engine. Unix is implemented here
//! against `openpty`; Windows needs ConPTY, which has a different shape
//! (`CreatePseudoConsole` + a pipe pair, no fd inheritance, no
//! `TIOCSCTTY`). The API in this module is deliberately written so that a
//! Windows implementation can satisfy it without the callers changing:
//! `spawn`, `resize`, `size`, `reader`, `writer`, `wait`, `kill`.
//!
//! Ported from `CDirijorPTY/cdirijor_pty.c` and `HolderPTY.swift`. Several
//! details there are load-bearing and are preserved exactly — see `spawn`.

use std::path::PathBuf;

/// What to launch on a PTY.
#[derive(Clone, Debug)]
pub struct PtySpec {
    /// argv[0] is the executable path.
    pub argv: Vec<String>,
    /// The child's complete environment. The parent's is not inherited: the
    /// daemon's environment leaks things like `NO_COLOR` that silently
    /// monochrome an agent's output.
    pub env: Vec<(String, String)>,
    pub cwd: PathBuf,
    pub cols: u16,
    pub rows: u16,
    /// Optional `RLIMIT_NOFILE` / `RLIMIT_NPROC` for the child.
    ///
    /// Not a PTY concern, which is why it is opt-in: this crate owns terminals,
    /// not policy. Callers that spawn somebody else's agent want them, and set
    /// them to the same values the rest of their engine spawns use.
    pub limits: Option<(u64, u64)>,
}

impl PtySpec {
    pub fn new(argv: Vec<String>, cwd: impl Into<PathBuf>) -> Self {
        Self {
            argv,
            env: Vec::new(),
            cwd: cwd.into(),
            cols: 80,
            rows: 24,
            limits: None,
        }
    }

    pub fn env(mut self, key: &str, value: &str) -> Self {
        self.env.push((key.to_string(), value.to_string()));
        self
    }

    pub fn size(mut self, cols: u16, rows: u16) -> Self {
        self.cols = cols;
        self.rows = rows;
        self
    }

    /// Cap open descriptors and processes for the child.
    pub fn limits(mut self, nofile: u64, nproc: u64) -> Self {
        self.limits = Some((nofile, nproc));
        self
    }
}

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub use unix::{Pty, PtyStream};

#[cfg(not(unix))]
mod unsupported;
#[cfg(not(unix))]
pub use unsupported::Pty;

/// How a child ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Exit {
    Code(i32),
    Signal(i32),
}

impl Exit {
    pub fn is_success(self) -> bool {
        matches!(self, Exit::Code(0))
    }
}
