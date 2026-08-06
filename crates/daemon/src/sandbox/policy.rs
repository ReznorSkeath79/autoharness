//! V1's "no external writes" invariant, enforced BEFORE execution: any
//! command resembling a git push, package publish, PR/release creation,
//! deployment, or other remote mutation is rejected and (by the caller)
//! recorded as an audit event.

/// Returns `Some(reason)` when the command would perform an external write.
pub fn external_write_violation(program: &str, args: &[String]) -> Option<String> {
    let base = program.rsplit('/').next().unwrap_or(program);
    let first = args.first().map(String::as_str).unwrap_or("");
    let second = args.get(1).map(String::as_str).unwrap_or("");

    let deny = |what: &str| {
        Some(format!(
            "{base} {what} performs an external write (denied in V1)"
        ))
    };

    match base {
        "git" => match first {
            "push" => deny("push"),
            "remote" if matches!(second, "add" | "set-url") => deny("remote mutation"),
            "config"
                if args.iter().any(|a| a.contains("url"))
                    && args.iter().any(|a| a == "--global") =>
            {
                deny("global remote config")
            }
            _ => None,
        },
        "gh" => match (first, second) {
            ("pr", "create" | "merge" | "close" | "edit" | "comment" | "review") => {
                deny("pr mutation")
            }
            ("release", "create" | "upload" | "delete" | "edit") => deny("release mutation"),
            ("repo", "create" | "fork" | "delete" | "edit") => deny("repo mutation"),
            ("issue", "create" | "close" | "edit" | "comment" | "delete") => deny("issue mutation"),
            ("gist", "create" | "edit" | "delete") => deny("gist mutation"),
            ("workflow", "enable" | "disable" | "run") => deny("workflow mutation"),
            _ => None,
        },
        "npm" | "yarn" | "pnpm" | "bun" if first == "publish" => deny("publish"),
        "npm" if first == "dist-tag" => deny("dist-tag mutation"),
        "cargo" if first == "publish" => deny("publish"),
        "docker" | "podman" if first == "push" => deny("image push"),
        "ssh" | "scp" | "sftp" => deny("remote shell/copy"),
        "rsync" if args.iter().any(|a| a.contains(':') && !a.starts_with('/')) => {
            deny("remote copy")
        }
        "curl" | "wget" => {
            let mutating_method = args.iter().any(|a| {
                let a = a.to_uppercase();
                matches!(a.as_str(), "-XPOST" | "-XPUT" | "-XDELETE" | "-XPATCH")
            }) || args.windows(2).any(|w| {
                w[0] == "-X"
                    && matches!(
                        w[1].to_uppercase().as_str(),
                        "POST" | "PUT" | "DELETE" | "PATCH"
                    )
            });
            let has_body = args.iter().any(|a| {
                a == "-d"
                    || a.starts_with("--data")
                    || a == "-T"
                    || a.starts_with("--upload-file")
                    || a == "-F"
                    || a.starts_with("--form")
            });
            if mutating_method || has_body {
                deny("HTTP mutation (use the read-only proxy)")
            } else {
                None
            }
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(program: &str, args: &[&str]) -> Option<String> {
        external_write_violation(
            program,
            &args.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
        )
    }

    #[test]
    fn external_writes_are_detected() {
        assert!(v("git", &["push", "origin", "main"]).is_some());
        assert!(v("/usr/bin/git", &["push"]).is_some());
        assert!(v("git", &["remote", "add", "origin", "x"]).is_some());
        assert!(v("gh", &["pr", "create", "--fill"]).is_some());
        assert!(v("gh", &["release", "create", "v1.0"]).is_some());
        assert!(v("npm", &["publish"]).is_some());
        assert!(v("cargo", &["publish"]).is_some());
        assert!(v("docker", &["push", "img"]).is_some());
        assert!(v("ssh", &["host", "ls"]).is_some());
        assert!(v("curl", &["-X", "POST", "https://x"]).is_some());
        assert!(v("curl", &["-d", "a=b", "https://x"]).is_some());
        assert!(v("curl", &["--upload-file", "f", "https://x"]).is_some());
        assert!(v("rsync", &["-a", "src/", "host:dst/"]).is_some());
    }

    #[test]
    fn local_operations_are_allowed() {
        assert!(v("git", &["status"]).is_none());
        assert!(v("git", &["commit", "-m", "x"]).is_none());
        assert!(v("git", &["clone", "https://x"]).is_none());
        assert!(v("gh", &["pr", "list"]).is_none());
        assert!(v("gh", &["auth", "status"]).is_none());
        assert!(v("curl", &["https://x"]).is_none());
        assert!(v("curl", &["-X", "GET", "https://x"]).is_none());
        assert!(v("cargo", &["test"]).is_none());
        assert!(v("npm", &["install"]).is_none());
        assert!(v("rsync", &["-a", "a/", "b/"]).is_none());
        assert!(v("sh", &["-c", "echo hi"]).is_none());
    }
}
