//! Engine adapter contract and adapters for Codex and Claude CLIs, plus a
//! deterministic FakeEngine for contract and integration tests.
//!
//! - [`adapter::EngineAdapter`]: the async contract (detection, session
//!   start/resume, turns, normalized streaming, pause/interrupt/cancel).
//! - [`event::EngineEvent`]: the normalized event model persisted to the
//!   daemon's ledger.
//! - [`fake::FakeEngine`]: in-process scripted adapter; all contract tests
//!   run against it.
//! - [`codex::CodexAdapter`]: `codex app-server` (v2 JSON-RPC stdio).
//! - [`claude::ClaudeAdapter`]: `claude --input-format stream-json
//!   --output-format stream-json`.
//!
//! V1 never switches engines automatically: an unavailable engine surfaces a
//! structured block, and switching means creating a new derived run.

pub mod adapter;
pub mod claude;
pub mod codex;
pub mod contract;
pub mod diagnostics;
pub mod event;
pub mod fake;
pub mod probe;
pub mod process;
pub mod pty_adapter;

pub use adapter::{EngineAdapter, EngineError, EngineModel, SessionSpec};
pub use autoharness_core::Answer;
pub use claude::ClaudeAdapter;
pub use codex::CodexAdapter;
pub use diagnostics::EngineDiagnostics;
pub use event::{EngineEvent, FileChangeKind, ToolStatus};
pub use fake::FakeEngine;
pub use pty_adapter::{Confinement, PtyAdapter};

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use autoharness_core::EngineKind;

/// Factory for engine adapters, keyed by engine kind. Production wires the
/// real CLIs; tests inject scripted fakes.
#[derive(Clone, Default)]
pub struct EngineRegistry {
    factories: HashMap<EngineKind, Arc<dyn Fn() -> Box<dyn EngineAdapter> + Send + Sync>>,
}

impl EngineRegistry {
    /// Real Codex and Claude adapters.
    /// Codex and Claude over their structured protocols, plus every agent a
    /// bundled manifest describes, driven through a terminal.
    ///
    /// The two built-ins keep their hand-written adapters: they report what
    /// they did rather than painting it, and that fidelity is worth having
    /// where it exists. Everything else is a `PtyAdapter`, which is why adding
    /// an agent is a JSON file — there is no code path here that names one.
    pub fn production() -> Self {
        Self::production_with(None, None, None)
    }

    /// [`Self::production`] with a manifest override directory and the
    /// daemon's network broker.
    pub fn production_with(
        manifest_overrides: Option<PathBuf>,
        proxy_addr: Option<String>,
        confinement: Option<Arc<dyn crate::pty_adapter::Confinement>>,
    ) -> Self {
        let mut registry = Self::default();
        registry.insert(EngineKind::codex(), || Box::new(CodexAdapter::new()));
        registry.insert(EngineKind::claude(), || Box::new(ClaudeAdapter::new()));

        let (manifests, failed) = autoharness_pty::manifests::load(manifest_overrides.as_deref());
        for problem in failed {
            tracing::warn!(%problem, "agent manifest could not be loaded");
        }
        for id in manifests.ids() {
            let kind = EngineKind::new(id);
            let Some(descriptor) = manifests.manifest(id).and_then(|m| m.agent.as_ref()) else {
                continue;
            };
            // A manifest that names a built-in — by id or by alias — describes
            // the same agent. `claude-code` is Claude Code, which is our
            // `claude`, and registering both would put two entries for one
            // agent in front of the user. The structured adapter is also
            // strictly better informed, so it wins.
            let names_a_builtin = kind.is_builtin()
                || descriptor
                    .aliases
                    .iter()
                    .any(|alias| EngineKind::new(alias).is_builtin());
            if names_a_builtin {
                continue;
            }
            // No binary means nothing to launch. `shell` and `generic` take
            // their command from the caller, so offering them as engines would
            // be a menu entry that can never work.
            if descriptor.binary.is_none() {
                continue;
            }
            let overrides = manifest_overrides.clone();
            let proxy = proxy_addr.clone();
            let confine = confinement.clone();
            let factory_kind = kind.clone();
            registry.insert(kind, move || {
                Box::new(
                    PtyAdapter::new(factory_kind.clone(), overrides.clone())
                        .with_proxy(proxy.clone())
                        .with_confinement(confine.clone()),
                )
            });
        }
        registry
    }

    pub fn insert(
        &mut self,
        kind: EngineKind,
        factory: impl Fn() -> Box<dyn EngineAdapter> + Send + Sync + 'static,
    ) {
        self.factories.insert(kind, Arc::new(factory));
    }

    /// Every engine this registry can build, in a stable order so a derived
    /// offer does not change between two identical situations.
    pub fn kinds(&self) -> Vec<EngineKind> {
        let mut kinds: Vec<EngineKind> = self.factories.keys().cloned().collect();
        kinds.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        kinds
    }

    /// Build a fresh adapter for `kind`. One adapter serves one run.
    pub fn create(&self, kind: EngineKind) -> Option<Box<dyn EngineAdapter>> {
        self.factories.get(&kind).map(|f| f())
    }

    /// Detection across all registered engines, for setup diagnostics.
    pub async fn detect_all(&self) -> Vec<EngineDiagnostics> {
        let mut results = Vec::new();
        for factory in self.factories.values() {
            results.push(factory().detect().await);
        }
        results
    }

    /// Diagnostics plus token-free model catalogs. A catalog error never
    /// changes readiness: provider-default runs must remain usable.
    pub async fn detect_all_with_models(
        &self,
        data_dir: &std::path::Path,
    ) -> Vec<EngineDiagnostics> {
        let mut results = Vec::new();
        for factory in self.factories.values() {
            let mut adapter = factory();
            let mut diagnostics = adapter.detect().await;
            if diagnostics.ready {
                match tokio::time::timeout(
                    std::time::Duration::from_secs(10),
                    adapter.available_models(data_dir),
                )
                .await
                {
                    Ok(Ok(models)) => diagnostics.models = models,
                    Ok(Err(error)) => diagnostics.model_load_error = Some(error.to_string()),
                    Err(_) => {
                        diagnostics.model_load_error =
                            Some("model catalog timed out after 10 seconds".into())
                    }
                }
            }
            results.push(diagnostics);
        }
        results
    }
}
