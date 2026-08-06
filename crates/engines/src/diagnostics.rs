//! Typed capability diagnostics for engine setup.
//!
//! Detection must never panic and never crash on missing, outdated, or
//! unauthenticated CLIs — it reports structured problems instead.

use std::path::PathBuf;

use autoharness_core::EngineKind;
use serde::{Deserialize, Serialize};

use crate::adapter::EngineModel;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EngineDiagnostics {
    pub engine: EngineKind,
    /// Binary found on PATH or a known location.
    pub installed: bool,
    pub binary_path: Option<PathBuf>,
    pub version: Option<String>,
    /// `None` when auth state could not be determined.
    pub authenticated: Option<bool>,
    /// The structured control protocol this adapter needs is available
    /// (codex `app-server`, claude `--input-format stream-json`).
    pub structured_mode: bool,
    /// Everything required to accept a run is in place.
    pub ready: bool,
    /// Human-readable setup problems, empty when ready.
    pub problems: Vec<String>,
    /// Token-free provider catalog. Empty when the engine is unavailable or
    /// the provider does not expose any usable choice.
    #[serde(default)]
    pub models: Vec<EngineModel>,
    /// Catalog failure is non-fatal to provider-default execution.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_load_error: Option<String>,
}

impl EngineDiagnostics {
    pub fn ready(engine: EngineKind, binary_path: PathBuf, version: String) -> Self {
        Self {
            engine,
            installed: true,
            binary_path: Some(binary_path),
            version: Some(version),
            authenticated: Some(true),
            structured_mode: true,
            ready: true,
            problems: Vec::new(),
            models: Vec::new(),
            model_load_error: None,
        }
    }

    pub fn not_installed(engine: EngineKind) -> Self {
        Self {
            engine: engine.clone(),
            installed: false,
            binary_path: None,
            version: None,
            authenticated: None,
            structured_mode: false,
            ready: false,
            problems: vec![format!(
                "{engine} CLI not found on PATH or in known install locations"
            )],
            models: Vec::new(),
            model_load_error: None,
        }
    }

    fn finalize(mut self) -> Self {
        self.ready = self.installed
            && self.version.is_some()
            && self.authenticated != Some(false)
            && self.structured_mode;
        if !self.ready && self.problems.is_empty() {
            self.problems.push(format!(
                "{engine:?} engine is not ready",
                engine = self.engine
            ));
        }
        self
    }

    pub fn with_problem(mut self, problem: impl Into<String>) -> Self {
        self.problems.push(problem.into());
        self.ready = false;
        self
    }
}

/// Assemble diagnostics from probed parts. Used by both real adapters.
pub(crate) fn assemble(
    engine: EngineKind,
    binary_path: Option<PathBuf>,
    version: Option<String>,
    authenticated: Option<bool>,
    structured_mode: bool,
) -> EngineDiagnostics {
    let installed = binary_path.is_some();
    let mut problems = Vec::new();
    if !installed {
        problems.push(format!("{engine} CLI not found on PATH"));
    }
    if installed && version.is_none() {
        problems.push(format!("{engine} CLI found but `--version` did not parse"));
    }
    if authenticated == Some(false) {
        problems.push(format!(
            "{engine} CLI does not appear to be authenticated; sign in first"
        ));
    }
    if installed && !structured_mode {
        problems.push(format!(
            "{engine} CLI lacks the required structured control mode"
        ));
    }
    EngineDiagnostics {
        engine,
        installed,
        binary_path,
        version,
        authenticated,
        structured_mode,
        ready: false,
        problems,
        models: Vec::new(),
        model_load_error: None,
    }
    .finalize()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assemble_marks_ready_only_when_all_checks_pass() {
        let d = assemble(
            EngineKind::codex(),
            Some(PathBuf::from("/bin/codex")),
            Some("1.0".into()),
            Some(true),
            true,
        );
        assert!(d.ready);
        assert!(d.problems.is_empty());

        let d = assemble(EngineKind::codex(), None, None, None, false);
        assert!(!d.ready);
        assert!(!d.problems.is_empty());

        let d = assemble(
            EngineKind::claude(),
            Some(PathBuf::from("/bin/claude")),
            Some("2.0".into()),
            Some(false),
            true,
        );
        assert!(!d.ready);
        assert!(d.problems.iter().any(|p| p.contains("authenticated")));

        // Unknown auth state does not block readiness.
        let d = assemble(
            EngineKind::claude(),
            Some(PathBuf::from("/bin/claude")),
            Some("2.0".into()),
            None,
            true,
        );
        assert!(d.ready);
    }
}
