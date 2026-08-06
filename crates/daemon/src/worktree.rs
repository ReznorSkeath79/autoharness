//! Daemon-managed git worktrees for runs (PLAN.md "Worktrees and
//! integration", direct-mode slice).
//!
//! Every editing run gets its own worktree + branch under daemon storage.
//! All git operations go through `Sandbox::run_bookkeeping` — the repo's
//! `.git` dir and daemon worktree storage are declared writable roots, and
//! the external-write policy still rejects pushes. The user's checked-out
//! branch is never mutated: every command runs with `-C <worktree>` or
//! read-only plumbing against the repo.

use std::path::{Path, PathBuf};

use autoharness_engines::process::SessionDirs;
use thiserror::Error;

use crate::sandbox::Sandbox;

#[derive(Debug, Error)]
pub enum WorktreeError {
    #[error("git failed: {cmd}: {stderr}")]
    Git { cmd: String, stderr: String },
    #[error("not a git repository: {0}")]
    NotARepo(String),
    #[error("sandbox: {0}")]
    Sandbox(#[from] crate::sandbox::backend::SandboxError),
    #[error("worktree index: {0}")]
    Index(String),
    #[error("no indexed worktree exists for run {0}")]
    ResumeMissing(String),
    #[error("worktree for run {0} was already reclaimed")]
    ResumeReclaimed(String),
    #[error("worktree path mismatch: indexed {indexed}, expected {expected}")]
    ResumePathMismatch { indexed: String, expected: String },
    #[error("worktree repository mismatch: indexed {indexed}, expected {expected}")]
    ResumeRepoMismatch { indexed: String, expected: String },
    #[error("worktree branch mismatch: indexed {indexed}, actual {actual}")]
    ResumeBranchMismatch { indexed: String, actual: String },
    #[error("worktree git common directory mismatch: actual {actual}, expected {expected}")]
    ResumeCommonDirMismatch { actual: String, expected: String },
    #[error("multiple active worktrees are indexed for run {0}")]
    ResumeAmbiguous(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, WorktreeError>;

// `git worktree add` writes shared repository administration files. Independent
// graph nodes may prepare concurrently, but those bookkeeping writes cannot:
// Git otherwise fails one node transiently on its own lock file.
static WORKTREE_MUTATION_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Return the Git administration directory that owns `repo_path`.
///
/// A normal checkout has a `.git` directory. A linked worktree instead has a
/// `.git` *file* containing a `gitdir:` pointer into the primary checkout's
/// `.git/worktrees/<name>` directory. Git writes refs, locks, and worktree
/// metadata in that shared administration directory, so granting the sandbox
/// only `repo_path/.git` makes every nested `git worktree add` fail for linked
/// worktrees. Resolve the pointer before constructing writable roots.
fn git_common_dir(repo_path: &Path) -> PathBuf {
    let dot_git = repo_path.join(".git");
    if dot_git.is_dir() {
        return dot_git;
    }
    let Ok(contents) = std::fs::read_to_string(&dot_git) else {
        return dot_git;
    };
    let Some(raw) = contents
        .lines()
        .find_map(|line| line.strip_prefix("gitdir:"))
    else {
        return dot_git;
    };
    let git_dir = PathBuf::from(raw.trim());
    let git_dir = if git_dir.is_absolute() {
        git_dir
    } else {
        dot_git
            .parent()
            .map(|parent| parent.join(&git_dir))
            .unwrap_or(git_dir)
    };
    let git_dir = git_dir.canonicalize().unwrap_or(git_dir);
    // Linked worktrees always point at <common>/.git/worktrees/<name>.
    git_dir
        .parent()
        .filter(|parent| parent.file_name().is_some_and(|name| name == "worktrees"))
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .unwrap_or(git_dir)
}

/// Who a worktree belongs to. Graph-node worktrees are named by session key,
/// which is not the run id, so ownership is carried explicitly rather than
/// inferred from the directory name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeOwner {
    pub run_id: String,
    pub node_id: Option<String>,
}

impl WorktreeOwner {
    pub fn run(run_id: impl Into<String>) -> Self {
        Self {
            run_id: run_id.into(),
            node_id: None,
        }
    }

    pub fn node(run_id: impl Into<String>, node_id: impl Into<String>) -> Self {
        Self {
            run_id: run_id.into(),
            node_id: Some(node_id.into()),
        }
    }

    fn kind(&self) -> &'static str {
        if self.node_id.is_some() {
            "graph_node"
        } else {
            "run"
        }
    }
}

/// A run's isolated checkout.
#[derive(Debug, Clone)]
pub struct RunWorktree {
    /// Directory name key: the run id for run worktrees, the session key for
    /// graph-node worktrees.
    pub run_id: String,
    pub owner: WorktreeOwner,
    pub repo_path: PathBuf,
    pub path: PathBuf,
    pub branch: String,
    pub base_commit: String,
    /// Whether the user's checkout was dirty when the run started (captured
    /// for audit; never modified).
    pub repo_dirty_at_start: bool,
}

impl RunWorktree {
    pub fn record(&self) -> autoharness_store::WorktreeRecord {
        autoharness_store::WorktreeRecord {
            path: self.path.to_string_lossy().into_owned(),
            kind: self.owner.kind().to_string(),
            run_id: self.owner.run_id.clone(),
            node_id: self.owner.node_id.clone(),
            repo_path: self.repo_path.to_string_lossy().into_owned(),
            branch: self.branch.clone(),
            base_commit: self.base_commit.clone(),
            created_at_ms: 0,
            removed_at_ms: None,
        }
    }
}

impl RunWorktree {
    /// Writable roots for git bookkeeping: the repo's `.git` dir (objects,
    /// refs, per-worktree metadata), the daemon's worktree storage (so the
    /// worktree directory itself can be created and removed), and the
    /// worktree. Nothing else in the repository is writable.
    fn roots(&self) -> Vec<PathBuf> {
        let mut roots = vec![git_common_dir(&self.repo_path)];
        if let Some(storage) = self.path.parent() {
            roots.push(storage.to_path_buf());
        }
        roots.push(self.path.clone());
        roots
    }
}

fn git_binary() -> PathBuf {
    autoharness_engines::process::find_binary("git")
        .unwrap_or_else(|| PathBuf::from("/usr/bin/git"))
}

/// Run a git command via sandbox bookkeeping. Returns trimmed stdout.
async fn git(
    sandbox: &Sandbox,
    dirs: &SessionDirs,
    extra_writable: &[PathBuf],
    cwd: &Path,
    args: &[&str],
) -> Result<String> {
    let cmd = format!("git {}", args.join(" "));
    let arg_strings: Vec<String> = args.iter().map(|a| a.to_string()).collect();
    let result = sandbox
        .run_bookkeeping(extra_writable, dirs, &git_binary(), &arg_strings, cwd)
        .await?;
    if !result.success() {
        return Err(WorktreeError::Git {
            cmd,
            stderr: result.stderr.trim().to_string(),
        });
    }
    Ok(result.stdout.trim().to_string())
}

/// Initialize a brand-new repository at `path` with an initial empty commit.
///
/// The commit is not decoration: run worktrees branch from HEAD, so a
/// repository without one cannot host a run at all. Identity is pinned with
/// `-c` because bookkeeping runs under a sanitized HOME with no global git
/// config to fall back on.
pub async fn init_repository(sandbox: &Sandbox, dirs: &SessionDirs, path: &Path) -> Result<()> {
    let writable = [path.to_path_buf()];
    git(
        sandbox,
        dirs,
        &writable,
        path,
        &["init", "--initial-branch=main"],
    )
    .await?;
    git(
        sandbox,
        dirs,
        &writable,
        path,
        &[
            "-c",
            "user.name=AutoHarness",
            "-c",
            "user.email=autoharness@localhost",
            "commit",
            "--allow-empty",
            "-m",
            "Initial commit",
        ],
    )
    .await?;
    Ok(())
}

/// Create a run worktree at `data_dir/worktrees/<run_id>` on branch
/// `ah/run-<id8>`, capturing base commit and repo status.
pub async fn create(
    sandbox: &Sandbox,
    dirs: &SessionDirs,
    store: &autoharness_store::Store,
    repo_path: &Path,
    run_id: &str,
    data_dir: &Path,
) -> Result<RunWorktree> {
    let short: String = run_id.chars().take(8).collect();
    create_named(
        sandbox,
        dirs,
        store,
        repo_path,
        run_id,
        &format!("ah/run-{short}"),
        data_dir,
        WorktreeOwner::run(run_id),
    )
    .await
}

/// Create a worktree with an explicit branch name. Graph nodes need
/// deterministic branch names so the integration node can find and merge them.
#[allow(clippy::too_many_arguments)]
pub async fn create_named(
    sandbox: &Sandbox,
    dirs: &SessionDirs,
    store: &autoharness_store::Store,
    repo_path: &Path,
    name: &str,
    branch: &str,
    data_dir: &Path,
    owner: WorktreeOwner,
) -> Result<RunWorktree> {
    let run_id = name;
    let storage = data_dir.join("worktrees");
    std::fs::create_dir_all(&storage)?;
    let roots = vec![git_common_dir(repo_path), storage.clone()];

    let base_commit = git(sandbox, dirs, &roots, repo_path, &["rev-parse", "HEAD"])
        .await
        .map_err(|e| match e {
            WorktreeError::Git { stderr, .. } if stderr.contains("not a git repository") => {
                WorktreeError::NotARepo(repo_path.display().to_string())
            }
            other => other,
        })?;
    if base_commit.is_empty() {
        return Err(WorktreeError::NotARepo(repo_path.display().to_string()));
    }
    let status = git(sandbox, dirs, &roots, repo_path, &["status", "--porcelain"]).await?;

    let path = storage.join(run_id);
    let branch = branch.to_string();
    let _mutation = WORKTREE_MUTATION_LOCK.lock().await;
    git(
        sandbox,
        dirs,
        &roots,
        repo_path,
        &[
            "worktree",
            "add",
            &path.to_string_lossy(),
            "-b",
            &branch,
            "HEAD",
        ],
    )
    .await?;
    drop(_mutation);

    // Store the canonical path. On macOS the data dir arrives as /var/... and
    // resolves to /private/var/..., and an index keyed on the unresolved form
    // would never match a reclaim request.
    let path = path.canonicalize().unwrap_or(path);
    let worktree = RunWorktree {
        run_id: run_id.to_string(),
        owner,
        repo_path: repo_path.to_path_buf(),
        path,
        branch,
        base_commit,
        repo_dirty_at_start: !status.is_empty(),
    };
    // Index it before anyone can ask about it. A worktree that exists on disk
    // but not here is one reclaim will refuse to touch, which is the safe
    // direction to fail.
    store
        .record_worktree(&worktree.record())
        .map_err(|e| WorktreeError::Index(e.to_string()))?;
    Ok(worktree)
}

/// Re-open a daemon-owned run worktree after a blocked run or daemon restart.
///
/// The index alone is not trusted: the canonical path, storage root,
/// repository, common git directory, branch, and base ancestry all have to
/// describe the same checkout before an engine process is allowed back in.
pub async fn resume_existing(
    sandbox: &Sandbox,
    dirs: &SessionDirs,
    store: &autoharness_store::Store,
    repo_path: &Path,
    run_id: &str,
    data_dir: &Path,
) -> Result<RunWorktree> {
    let owned = store
        .list_worktrees(true)
        .map_err(|error| WorktreeError::Index(error.to_string()))?
        .into_iter()
        .filter(|record| {
            record.run_id == run_id && record.kind == "run" && record.node_id.is_none()
        })
        .collect::<Vec<_>>();
    let mut active = owned.iter().filter(|record| record.removed_at_ms.is_none());
    let Some(record) = active.next() else {
        return Err(if owned.is_empty() {
            WorktreeError::ResumeMissing(run_id.to_string())
        } else {
            WorktreeError::ResumeReclaimed(run_id.to_string())
        });
    };
    if active.next().is_some() {
        return Err(WorktreeError::ResumeAmbiguous(run_id.to_string()));
    }

    let storage = data_dir.join("worktrees").canonicalize()?;
    let expected = storage.join(run_id).canonicalize()?;
    let indexed = PathBuf::from(&record.path).canonicalize()?;
    if !indexed.starts_with(&storage) || indexed != expected {
        return Err(WorktreeError::ResumePathMismatch {
            indexed: indexed.display().to_string(),
            expected: expected.display().to_string(),
        });
    }

    let expected_repo = repo_path.canonicalize()?;
    let indexed_repo = PathBuf::from(&record.repo_path).canonicalize()?;
    if indexed_repo != expected_repo {
        return Err(WorktreeError::ResumeRepoMismatch {
            indexed: indexed_repo.display().to_string(),
            expected: expected_repo.display().to_string(),
        });
    }

    let worktree = RunWorktree {
        run_id: run_id.to_string(),
        owner: WorktreeOwner::run(run_id),
        repo_path: expected_repo.clone(),
        path: indexed.clone(),
        branch: record.branch.clone(),
        base_commit: record.base_commit.clone(),
        repo_dirty_at_start: false,
    };
    let roots = worktree.roots();
    let actual_root = git(
        sandbox,
        dirs,
        &roots,
        &indexed,
        &["rev-parse", "--show-toplevel"],
    )
    .await?;
    let actual_root = PathBuf::from(actual_root).canonicalize()?;
    if actual_root != indexed {
        return Err(WorktreeError::ResumePathMismatch {
            indexed: actual_root.display().to_string(),
            expected: indexed.display().to_string(),
        });
    }

    let actual_common = git(
        sandbox,
        dirs,
        &roots,
        &indexed,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )
    .await?;
    let actual_common = PathBuf::from(actual_common).canonicalize()?;
    let expected_common = expected_repo.join(".git").canonicalize()?;
    if actual_common != expected_common {
        return Err(WorktreeError::ResumeCommonDirMismatch {
            actual: actual_common.display().to_string(),
            expected: expected_common.display().to_string(),
        });
    }

    let actual_branch = git(
        sandbox,
        dirs,
        &roots,
        &indexed,
        &["rev-parse", "--abbrev-ref", "HEAD"],
    )
    .await?;
    if actual_branch != record.branch {
        return Err(WorktreeError::ResumeBranchMismatch {
            indexed: record.branch.clone(),
            actual: actual_branch,
        });
    }
    git(
        sandbox,
        dirs,
        &roots,
        &indexed,
        &["merge-base", "--is-ancestor", &record.base_commit, "HEAD"],
    )
    .await?;
    Ok(worktree)
}

/// True when the worktree has no uncommitted changes.
pub async fn is_clean(sandbox: &Sandbox, dirs: &SessionDirs, wt: &RunWorktree) -> Result<bool> {
    let roots = wt.roots();
    let status = git(sandbox, dirs, &roots, &wt.path, &["status", "--porcelain"]).await?;
    Ok(status.is_empty())
}

/// Count of commits on the run branch beyond the base commit.
pub async fn commits_beyond_base(
    sandbox: &Sandbox,
    dirs: &SessionDirs,
    wt: &RunWorktree,
) -> Result<i64> {
    let roots = wt.roots();
    let out = git(
        sandbox,
        dirs,
        &roots,
        &wt.path,
        &["rev-list", "--count", &format!("{}..HEAD", wt.base_commit)],
    )
    .await?;
    Ok(out.parse().unwrap_or(0))
}

/// Stage everything and commit on the run branch with a daemon identity.
/// Returns the commit hash, or None when there was nothing to commit.
pub async fn commit_all(
    sandbox: &Sandbox,
    dirs: &SessionDirs,
    wt: &RunWorktree,
    message: &str,
) -> Result<Option<String>> {
    if is_clean(sandbox, dirs, wt).await? {
        return Ok(None);
    }
    let roots = wt.roots();
    git(sandbox, dirs, &roots, &wt.path, &["add", "-A"]).await?;
    git(
        sandbox,
        dirs,
        &roots,
        &wt.path,
        &[
            "-c",
            "user.name=AutoHarness",
            "-c",
            "user.email=autoharness@localhost",
            "commit",
            "-m",
            message,
        ],
    )
    .await?;
    let hash = git(sandbox, dirs, &roots, &wt.path, &["rev-parse", "HEAD"]).await?;
    Ok(Some(hash))
}

/// The full patch for the run branch, truncated to `max_bytes`.
///
/// Bounded on purpose: a run that rewrites a lockfile can produce megabytes,
/// and the ledger is not a place to put them. The UI shows what fits and says
/// the rest was elided.
pub async fn diff_patch(
    sandbox: &Sandbox,
    dirs: &SessionDirs,
    wt: &RunWorktree,
    max_bytes: usize,
) -> Result<String> {
    let roots = wt.roots();
    let patch = git(
        sandbox,
        dirs,
        &roots,
        &wt.path,
        &["diff", "--no-color", &format!("{}..HEAD", wt.base_commit)],
    )
    .await?;
    if patch.len() <= max_bytes {
        return Ok(patch);
    }
    // Cut on a line boundary so the UI never parses half a hunk header.
    let cut = patch[..max_bytes].rfind('\n').unwrap_or(max_bytes);
    Ok(format!(
        "{}\n… diff truncated at {} bytes; open the worktree to see all of it",
        &patch[..cut],
        max_bytes
    ))
}

/// Worktree-relative paths changed on the run branch since its base commit.
pub async fn changed_files(
    sandbox: &Sandbox,
    dirs: &SessionDirs,
    wt: &RunWorktree,
) -> Result<Vec<String>> {
    let roots = wt.roots();
    let out = git(
        sandbox,
        dirs,
        &roots,
        &wt.path,
        &["diff", "--name-only", &format!("{}..HEAD", wt.base_commit)],
    )
    .await?;
    Ok(out
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect())
}

/// `git diff --stat base..HEAD` (empty when no changes).
pub async fn diff_stat(sandbox: &Sandbox, dirs: &SessionDirs, wt: &RunWorktree) -> Result<String> {
    let roots = wt.roots();
    git(
        sandbox,
        dirs,
        &roots,
        &wt.path,
        &["diff", "--stat", &format!("{}..HEAD", wt.base_commit)],
    )
    .await
}

/// Remove the worktree and its branch — only called when it is CLEAN
/// (no uncommitted changes, no commits beyond base). Returns what happened.
pub enum CleanupOutcome {
    Removed,
    Preserved(String),
}

pub async fn cleanup(
    sandbox: &Sandbox,
    dirs: &SessionDirs,
    store: &autoharness_store::Store,
    wt: &RunWorktree,
) -> Result<CleanupOutcome> {
    if !is_clean(sandbox, dirs, wt).await? {
        return Ok(CleanupOutcome::Preserved(
            "uncommitted changes present".into(),
        ));
    }
    if commits_beyond_base(sandbox, dirs, wt).await? > 0 {
        return Ok(CleanupOutcome::Preserved(
            "run branch contains commits".into(),
        ));
    }
    remove(sandbox, dirs, wt).await?;
    let _ = store.mark_worktree_removed(&wt.path.to_string_lossy());
    Ok(CleanupOutcome::Removed)
}

/// Remove the worktree directory and its branch. Deliberately never passes
/// `--force`: if git thinks the worktree is still in use or has changes, the
/// answer is to leave it alone, not to overrule git.
pub async fn remove(sandbox: &Sandbox, dirs: &SessionDirs, wt: &RunWorktree) -> Result<()> {
    let roots = wt.roots();
    if wt.path.exists() {
        git(
            sandbox,
            dirs,
            &roots,
            &wt.repo_path,
            &["worktree", "remove", &wt.path.to_string_lossy()],
        )
        .await?;
    }
    // Prune the administrative entry for a directory that vanished under us.
    git(sandbox, dirs, &roots, &wt.repo_path, &["worktree", "prune"]).await?;
    // The branch is unmerged by construction, so -D is the only delete that
    // applies. It is not `worktree remove --force`: no working tree is
    // discarded here, and the commits-beyond-base check already ran.
    if !wt.branch.is_empty() {
        let _ = git(
            sandbox,
            dirs,
            &roots,
            &wt.repo_path,
            &["branch", "-D", &wt.branch],
        )
        .await;
    }
    Ok(())
}

/// Commits on `branch` beyond `base_commit`, asked of the repository rather
/// than the worktree, so it still answers when the directory is gone.
pub async fn branch_commits_beyond_base(
    sandbox: &Sandbox,
    dirs: &SessionDirs,
    wt: &RunWorktree,
) -> Result<Option<i64>> {
    let roots = wt.roots();
    let exists = git(
        sandbox,
        dirs,
        &roots,
        &wt.repo_path,
        &["rev-parse", "--verify", "--quiet", &wt.branch],
    )
    .await;
    if exists.is_err() {
        // The branch is already gone: there is nothing left to lose.
        return Ok(None);
    }
    let out = git(
        sandbox,
        dirs,
        &roots,
        &wt.repo_path,
        &[
            "rev-list",
            "--count",
            &format!("{}..{}", wt.base_commit, wt.branch),
        ],
    )
    .await?;
    Ok(Some(out.parse().unwrap_or(0)))
}

/// Merge another run branch into this worktree. Conflicts are NOT an error
/// here: the conflicted state is left in place for the integration node's
/// engine session to resolve, which is the whole point of a staging worktree.
pub async fn merge_branch(
    sandbox: &Sandbox,
    dirs: &SessionDirs,
    wt: &RunWorktree,
    branch: &str,
) -> Result<()> {
    let roots = wt.roots();
    git(
        sandbox,
        dirs,
        &roots,
        &wt.path,
        &[
            "-c",
            "user.name=AutoHarness",
            "-c",
            "user.email=autoharness@localhost",
            "merge",
            "--no-edit",
            branch,
        ],
    )
    .await
    .map(|_| ())
}

/// Whether any process holds a file (or its cwd) under `path`.
///
/// `Unknown` is a distinct answer on purpose: "we could not tell" must block
/// a reclaim exactly like "yes", never fall through to "no".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessUse {
    None,
    InUse,
    Unknown,
}

/// How long `lsof` gets before the answer is treated as unknown. A recursive
/// scan over a network mount can hang indefinitely, and a hung probe must not
/// hold the request open.
const LSOF_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

pub async fn processes_using(
    sandbox: &Sandbox,
    dirs: &SessionDirs,
    path: &Path,
) -> Result<ProcessUse> {
    let Some(lsof) = autoharness_engines::process::find_binary("lsof") else {
        return Ok(ProcessUse::Unknown);
    };
    let args = vec![
        "-t".to_string(),
        "+D".to_string(),
        path.to_string_lossy().into_owned(),
    ];
    let cwd = path.parent().unwrap_or(path);
    let probe = sandbox.run_bookkeeping(&[], dirs, &lsof, &args, cwd);
    match tokio::time::timeout(LSOF_TIMEOUT, probe).await {
        Err(_) => Ok(ProcessUse::Unknown),
        Ok(Err(_)) => Ok(ProcessUse::Unknown),
        Ok(Ok(result)) => Ok(classify_lsof(&result.stdout, result.status.code())),
    }
}

/// Read an `lsof -t` result.
///
/// lsof exits 1 with no output when nothing matched, which is the only
/// non-zero status that means "no". Every other status — including a signal
/// death, where `code()` is `None` — leaves the question unanswered.
pub fn classify_lsof(stdout: &str, status: Option<i32>) -> ProcessUse {
    if !stdout.trim().is_empty() {
        return ProcessUse::InUse;
    }
    match status {
        Some(0) | Some(1) => ProcessUse::None,
        _ => ProcessUse::Unknown,
    }
}

/// Current HEAD of the repository's checked-out branch (for base-branch
/// immutability checks in tests).
pub async fn repo_head(sandbox: &Sandbox, dirs: &SessionDirs, repo_path: &Path) -> Result<String> {
    let roots = vec![git_common_dir(repo_path)];
    git(sandbox, dirs, &roots, repo_path, &["rev-parse", "HEAD"]).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn common_dir_for_normal_checkout_is_dot_git() {
        let root = tempfile::tempdir().unwrap();
        let dot_git = root.path().join(".git");
        std::fs::create_dir(&dot_git).unwrap();
        assert_eq!(git_common_dir(root.path()), dot_git);
    }

    #[test]
    fn common_dir_for_linked_worktree_resolves_shared_admin_dir() {
        let root = tempfile::tempdir().unwrap();
        let common = root.path().join("primary").join(".git");
        let worktree = root.path().join("linked");
        let linked_admin = common.join("worktrees").join("linked");
        std::fs::create_dir_all(&linked_admin).unwrap();
        std::fs::create_dir_all(&worktree).unwrap();
        std::fs::write(
            worktree.join(".git"),
            format!("gitdir: {}\n", linked_admin.display()),
        )
        .unwrap();

        assert_eq!(git_common_dir(&worktree), common.canonicalize().unwrap());
    }

    #[test]
    fn common_dir_for_relative_linked_worktree_pointer_is_supported() {
        let root = tempfile::tempdir().unwrap();
        let common = root.path().join("primary").join(".git");
        let worktree = root.path().join("linked");
        let linked_admin = common.join("worktrees").join("linked");
        std::fs::create_dir_all(&linked_admin).unwrap();
        std::fs::create_dir_all(&worktree).unwrap();
        std::fs::write(
            worktree.join(".git"),
            "gitdir: ../primary/.git/worktrees/linked\n",
        )
        .unwrap();

        assert_eq!(git_common_dir(&worktree), common.canonicalize().unwrap());
    }
}
