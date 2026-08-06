//! Seatbelt profile generation. Deny-by-default with targeted allows.
//!
//! Verified semantics on macOS (live probes, Phase 3):
//! - dyld touches paths far beyond system dirs at process startup, so reads
//!   must be allowed BROADLY and sensitive paths explicitly denied after
//!   (SBPL: last matching rule wins).
//! - Every path in a rule must be canonical (`/tmp` is a symlink to
//!   `/private/tmp`; non-canonical paths silently don't match).
//! - Network rules must use `localhost`, never `127.0.0.1`.
//!
//! Result: writes only inside assigned worktree/session dirs; network only
//! to the daemon's local proxy; `~/.ssh`, git credentials, Keychain files,
//! cloud/docker credentials, engine credentials, and the daemon's own
//! secrets are explicitly unreadable.

use std::path::{Path, PathBuf};

/// Inputs for one worker's sandbox profile.
#[derive(Debug, Clone)]
pub struct SandboxSpec {
    /// Directories the worker may write (worktree + session dirs).
    pub writable_roots: Vec<PathBuf>,
    /// Local read-only proxy port; the only permitted network destination.
    pub proxy_port: Option<u16>,
    /// Daemon data dir holding client-token and the ledger; its secret files
    /// are explicitly denied (session subdirs stay writable via the write
    /// rules — reads of them are allowed, writes too, but token/db are not).
    pub data_dir: Option<PathBuf>,
}

/// Sensitive paths under the real home dir that must be unreadable.
const HOME_DENIES: &[&str] = &[
    ".ssh",
    ".gnupg",
    ".git-credentials",
    ".netrc",
    ".aws",
    ".kube",
    ".docker",
    ".config/gh",
    ".config/gcloud",
    "Library/Keychains",
    ".codex",
    ".claude",
    ".claude.json",
];

/// Sensitive absolute paths that must be unreadable.
const ABSOLUTE_DENIES: &[&str] = &["/var/run/docker.sock", "/private/var/run/docker.sock"];

fn canonical(path: &Path) -> String {
    std::fs::canonicalize(path)
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

fn subpath_rules(paths: impl Iterator<Item = String>) -> String {
    paths
        .map(|p| format!("(subpath \"{p}\")"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Sensitive paths, canonicalized, as deny subpaths/literals.
fn sensitive_denies(data_dir: Option<&PathBuf>) -> Vec<String> {
    sensitive_denies_for_homes(data_dir, &super::home::protected_homes())
}

fn sensitive_denies_for_homes(data_dir: Option<&PathBuf>, homes: &[PathBuf]) -> Vec<String> {
    let mut denies = Vec::new();
    for home in homes {
        for rel in HOME_DENIES {
            denies.push(format!("(subpath \"{}\")", canonical(&home.join(rel))));
        }
    }
    for abs in ABSOLUTE_DENIES {
        denies.push(format!("(subpath \"{}\")", canonical(Path::new(abs))));
    }
    if let Some(dir) = data_dir {
        let dir = PathBuf::from(canonical(dir));
        for file in [
            "client-token",
            "autoharness.db",
            "autoharness.db-wal",
            "autoharness.db-shm",
        ] {
            denies.push(format!("(literal \"{}\")", canonical(&dir.join(file))));
        }
    }
    denies
}

/// Generate the `.sb` profile text for a worker.
pub fn generate(spec: &SandboxSpec) -> String {
    let writable: Vec<String> = spec.writable_roots.iter().map(|p| canonical(p)).collect();

    let mut profile = String::from("(version 1)\n(deny default)\n");
    profile.push_str("(allow process*)\n");
    profile.push_str("(allow sysctl-read)\n");

    // Broad reads (dyld requires them), then explicit sensitive denies.
    profile.push_str("(allow file-read*)\n");
    profile.push_str(&format!(
        "(deny file-read* {})\n",
        sensitive_denies(spec.data_dir.as_ref()).join(" ")
    ));

    // Writes: assigned roots only, plus /dev/null.
    profile.push_str(&format!(
        "(allow file-write* (literal \"/dev/null\"){})\n",
        if writable.is_empty() {
            String::new()
        } else {
            format!(" {}", subpath_rules(writable.into_iter()))
        }
    ));

    // Network: only the daemon's local read-only proxy. Seatbelt requires
    // the host to be `*` or `localhost` in network addresses.
    if let Some(port) = spec.proxy_port {
        profile.push_str(&format!(
            "(allow network-outbound (remote tcp \"localhost:{port}\"))\n"
        ));
    }
    profile
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> SandboxSpec {
        SandboxSpec {
            writable_roots: vec![
                PathBuf::from("/tmp/ah-worktree"),
                PathBuf::from("/tmp/ah-session/home"),
                PathBuf::from("/tmp/ah-session/tmp"),
            ],
            proxy_port: Some(8787),
            data_dir: Some(PathBuf::from("/tmp/ah-data")),
        }
    }

    #[test]
    fn profile_snapshot() {
        let home = crate::sandbox::home::primary_home().unwrap();
        let hc = canonical(&home);
        let expected = format!(
            r#"(version 1)
(deny default)
(allow process*)
(allow sysctl-read)
(allow file-read*)
(deny file-read* (subpath "{hc}/.ssh") (subpath "{hc}/.gnupg") (subpath "{hc}/.git-credentials") (subpath "{hc}/.netrc") (subpath "{hc}/.aws") (subpath "{hc}/.kube") (subpath "{hc}/.docker") (subpath "{hc}/.config/gh") (subpath "{hc}/.config/gcloud") (subpath "{hc}/Library/Keychains") (subpath "{hc}/.codex") (subpath "{hc}/.claude") (subpath "{hc}/.claude.json") (subpath "/var/run/docker.sock") (subpath "/private/var/run/docker.sock") (literal "/tmp/ah-data/client-token") (literal "/tmp/ah-data/autoharness.db") (literal "/tmp/ah-data/autoharness.db-wal") (literal "/tmp/ah-data/autoharness.db-shm"))
(allow file-write* (literal "/dev/null") (subpath "/tmp/ah-worktree") (subpath "/tmp/ah-session/home") (subpath "/tmp/ah-session/tmp"))
(allow network-outbound (remote tcp "localhost:8787"))
"#
        );
        assert_eq!(generate(&spec()), expected);
    }

    #[test]
    fn profile_omits_network_rule_without_proxy() {
        let mut spec = spec();
        spec.proxy_port = None;
        let profile = generate(&spec);
        assert!(!profile.contains("network-outbound"));
    }

    #[test]
    fn sensitive_paths_appear_only_in_deny_rules() {
        let profile = generate(&spec());
        for line in profile.lines() {
            let is_deny = line.starts_with("(deny");
            for sensitive in [".ssh", "git-credentials", "docker.sock", "Keychains"] {
                if line.contains(sensitive) {
                    assert!(is_deny, "{sensitive} outside a deny rule: {line}");
                }
            }
        }
        assert!(profile.starts_with("(version 1)\n(deny default)"));
    }

    #[test]
    fn account_and_overridden_homes_are_both_protected() {
        let denies = sensitive_denies_for_homes(
            None,
            &[
                PathBuf::from("/Users/real-account"),
                PathBuf::from("/tmp/clean-home"),
            ],
        )
        .join(" ");

        assert!(denies.contains("/Users/real-account/Library/Keychains"));
        assert!(denies.contains("/tmp/clean-home/Library/Keychains"));
        assert!(denies.contains("/Users/real-account/.codex"));
        assert!(denies.contains("/tmp/clean-home/.claude"));
    }
}
