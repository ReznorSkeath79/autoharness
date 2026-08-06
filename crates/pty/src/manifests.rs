//! The agent manifests shipped inside the binary.
//!
//! Detection rules are data, and adding an agent is meant to be one JSON file
//! rather than a code change. That only holds if the files are actually there:
//! a packaged app that lost its resource directory would silently know about
//! no agents at all, and every one of them would report "not installed" for a
//! reason nobody could see. Embedding them makes that failure impossible.
//!
//! A user-supplied directory still overrides these, so a rule can be fixed
//! without rebuilding.

use crate::detect::{Manifest, ManifestEngine};

/// Every manifest compiled into this binary, as `(id, json)`.
pub const BUNDLED: &[(&str, &str)] = &[
    ("aider", include_str!("../manifests/aider.json")),
    ("amp", include_str!("../manifests/amp.json")),
    ("antigravity", include_str!("../manifests/antigravity.json")),
    ("claude-code", include_str!("../manifests/claude-code.json")),
    ("codex", include_str!("../manifests/codex.json")),
    ("copilot", include_str!("../manifests/copilot.json")),
    ("cursor", include_str!("../manifests/cursor.json")),
    ("devin", include_str!("../manifests/devin.json")),
    ("droid", include_str!("../manifests/droid.json")),
    ("gemini", include_str!("../manifests/gemini.json")),
    ("generic", include_str!("../manifests/generic.json")),
    ("grok", include_str!("../manifests/grok.json")),
    ("hermes", include_str!("../manifests/hermes.json")),
    ("kilo", include_str!("../manifests/kilo.json")),
    ("kimi", include_str!("../manifests/kimi.json")),
    ("kiro", include_str!("../manifests/kiro.json")),
    ("opencode", include_str!("../manifests/opencode.json")),
    ("pi", include_str!("../manifests/pi.json")),
    ("qoder", include_str!("../manifests/qoder.json")),
    ("shell", include_str!("../manifests/shell.json")),
];

/// The bundled manifests, plus whatever `overrides` adds or replaces.
///
/// An override wins by id, so a user can correct one agent's rules without
/// shipping the other nineteen. Manifests that fail to parse are reported
/// rather than silently skipped: a broken override should be visible, not
/// mistaken for the agent behaving differently.
pub fn load(overrides: Option<&std::path::Path>) -> (ManifestEngine, Vec<String>) {
    let mut manifests: Vec<Manifest> = Vec::new();
    let mut failed: Vec<String> = Vec::new();

    for (id, json) in BUNDLED {
        match serde_json::from_str::<Manifest>(json) {
            Ok(manifest) => manifests.push(manifest),
            // Unreachable in a build that passes its tests, which is exactly
            // why it is reported rather than unwrapped.
            Err(error) => failed.push(format!("bundled {id}: {error}")),
        }
    }

    if let Some(dir) = overrides {
        match std::fs::read_dir(dir) {
            Ok(entries) => {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.extension().and_then(|e| e.to_str()) != Some("json") {
                        continue;
                    }
                    let raw = match std::fs::read_to_string(&path) {
                        Ok(raw) => raw,
                        Err(error) => {
                            failed.push(format!("{}: {error}", path.display()));
                            continue;
                        }
                    };
                    match serde_json::from_str::<Manifest>(&raw) {
                        Ok(manifest) => {
                            manifests.retain(|existing| existing.id != manifest.id);
                            manifests.push(manifest);
                        }
                        Err(error) => failed.push(format!("{}: {error}", path.display())),
                    }
                }
            }
            Err(error) => failed.push(format!("overrides in {}: {error}", dir.display())),
        }
    }

    (ManifestEngine::new(manifests), failed)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every embedded manifest parses. A manifest that does not is an agent
    /// that silently does not exist.
    #[test]
    fn every_bundled_manifest_parses() {
        let (engine, failed) = load(None);
        assert!(failed.is_empty(), "{failed:?}");
        assert_eq!(engine.ids().len(), BUNDLED.len());
        assert!(engine.manifest("claude-code").is_some());
        assert!(engine.manifest("codex").is_some());
    }

    /// The embedded set matches the directory the repository ships, so a new
    /// manifest cannot be added to one and forgotten in the other.
    #[test]
    fn the_embedded_set_matches_the_manifest_directory() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("manifests");
        let mut on_disk: Vec<String> = std::fs::read_dir(dir)
            .expect("manifests")
            .flatten()
            .filter(|entry| entry.path().extension().and_then(|e| e.to_str()) == Some("json"))
            .map(|entry| {
                entry
                    .path()
                    .file_stem()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        on_disk.sort();
        let mut embedded: Vec<String> = BUNDLED.iter().map(|(id, _)| (*id).to_string()).collect();
        embedded.sort();
        assert_eq!(embedded, on_disk);
    }

    /// An override replaces a bundled manifest by id rather than sitting
    /// alongside it, so a fix does not have to compete with what it fixes.
    #[test]
    fn an_override_replaces_the_bundled_manifest_of_the_same_id() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("codex.json"),
            r#"{
              "schemaVersion": 2,
              "id": "codex",
              "version": "override",
              "statusModel": "processOnly",
              "agent": { "displayName": "Patched", "statusAuthority": "process" },
              "rules": []
            }"#,
        )
        .unwrap();

        let (engine, failed) = load(Some(dir.path()));
        assert!(failed.is_empty(), "{failed:?}");
        assert_eq!(engine.ids().len(), BUNDLED.len(), "no duplicate id");
        let codex = engine.manifest("codex").expect("codex");
        assert_eq!(
            codex.agent.as_ref().unwrap().display_name.as_deref(),
            Some("Patched")
        );
    }
}
