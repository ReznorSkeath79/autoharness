//! Fail-closed installation helper shared by the UI and the helper binary.
//!
//! The UI may download and stage an update, but it never replaces its own
//! bundle. This crate defines the mode-0600 request handed to the separately
//! signed helper and independently re-verifies every security-relevant fact
//! immediately before the atomic sibling swap.

use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::Duration;

use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

pub const APP_NAME: &str = "AutoHarness.app";
pub const BUNDLE_ID: &str = "dev.autoharness.app";
const TOKEN_BYTES: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstallRequest {
    pub version: String,
    pub expected_bundle_id: String,
    pub expected_team_id: String,
    pub archive_path: PathBuf,
    pub archive_sha256: String,
    pub staged_bundle: PathBuf,
    pub current_bundle: PathBuf,
    pub database_path: PathBuf,
    pub parent_pid: u32,
    pub token: String,
}

#[derive(Debug, Error)]
pub enum InstallError {
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    #[error("request token mismatch")]
    TokenMismatch,
    #[error("active runs prevent update installation: {0}")]
    RunsActive(usize),
    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("SQLite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("bundle verification failed: {0}")]
    Verification(String),
    #[error("bundle swap failed: {0}")]
    Swap(String),
    #[error("relaunch failed and the previous bundle was restored: {0}")]
    Relaunch(String),
    #[error("parent process did not exit: {0}")]
    ParentStillRunning(u32),
}

pub type Result<T> = std::result::Result<T, InstallError>;

pub trait BundleInspector {
    fn bundle_id(&self, path: &Path) -> std::result::Result<String, String>;
    fn bundle_version(&self, path: &Path) -> std::result::Result<String, String>;
    fn codesign_verify(&self, path: &Path) -> std::result::Result<(), String>;
    fn team_id(&self, path: &Path) -> std::result::Result<String, String>;
    fn gatekeeper_assess(&self, path: &Path) -> std::result::Result<(), String>;
}

pub struct MacBundleInspector;

fn run_tool(program: &str, args: &[&str]) -> std::result::Result<Output, String> {
    Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|error| format!("{program}: {error}"))
}

fn path_text(path: &Path) -> std::result::Result<&str, String> {
    path.to_str()
        .ok_or_else(|| "bundle path is not valid UTF-8".to_string())
}

fn plist_value(bundle: &Path, key: &str) -> std::result::Result<String, String> {
    let plist = bundle.join("Contents/Info.plist");
    let plist = path_text(&plist)?;
    let command = format!("Print :{key}");
    let output = run_tool("/usr/libexec/PlistBuddy", &["-c", &command, plist])?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

impl BundleInspector for MacBundleInspector {
    fn bundle_id(&self, path: &Path) -> std::result::Result<String, String> {
        plist_value(path, "CFBundleIdentifier")
    }

    fn bundle_version(&self, path: &Path) -> std::result::Result<String, String> {
        plist_value(path, "CFBundleShortVersionString")
    }

    fn codesign_verify(&self, path: &Path) -> std::result::Result<(), String> {
        let path = path_text(path)?;
        let output = run_tool(
            "/usr/bin/codesign",
            &["--verify", "--deep", "--strict", path],
        )?;
        if output.status.success() {
            Ok(())
        } else {
            Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
        }
    }

    fn team_id(&self, path: &Path) -> std::result::Result<String, String> {
        let path = path_text(path)?;
        let output = run_tool("/usr/bin/codesign", &["-dv", "--verbose=4", path])?;
        let report = String::from_utf8_lossy(&output.stderr);
        report
            .lines()
            .find_map(|line| line.strip_prefix("TeamIdentifier="))
            .map(|team| team.trim().to_string())
            .ok_or_else(|| "codesign reported no TeamIdentifier".to_string())
    }

    fn gatekeeper_assess(&self, path: &Path) -> std::result::Result<(), String> {
        let path = path_text(path)?;
        let output = run_tool("/usr/sbin/spctl", &["--assess", "--type", "execute", path])?;
        if output.status.success() {
            Ok(())
        } else {
            Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
        }
    }
}

pub fn sha256_file(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn token_shape_is_valid(token: &str) -> bool {
    token.len() == TOKEN_BYTES && token.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn tokens_match(left: &str, right: &str) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.bytes()
        .zip(right.bytes())
        .fold(0_u8, |difference, (a, b)| difference | (a ^ b))
        == 0
}

pub fn write_request(path: &Path, request: &InstallRequest) -> Result<()> {
    if !token_shape_is_valid(&request.token) {
        return Err(InstallError::InvalidRequest(
            "token must be 64 hexadecimal characters".into(),
        ));
    }
    let bytes = serde_json::to_vec(request)?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    Ok(())
}

pub fn load_request(path: &Path, supplied_token: &str) -> Result<InstallRequest> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(InstallError::InvalidRequest(
            "request path must be a regular file, not a symlink".into(),
        ));
    }
    if metadata.mode() & 0o777 != 0o600 {
        return Err(InstallError::InvalidRequest(format!(
            "request mode is {:o}, expected 600",
            metadata.mode() & 0o777
        )));
    }
    let request: InstallRequest = serde_json::from_slice(&fs::read(path)?)?;
    if !token_shape_is_valid(&request.token)
        || !token_shape_is_valid(supplied_token)
        || !tokens_match(&request.token, supplied_token)
    {
        return Err(InstallError::TokenMismatch);
    }
    Ok(request)
}

fn canonical_without_symlink(path: &Path, label: &str) -> Result<PathBuf> {
    if fs::symlink_metadata(path)?.file_type().is_symlink() {
        return Err(InstallError::InvalidRequest(format!(
            "{label} must not be a symlink"
        )));
    }
    let canonical = path.canonicalize()?;
    if canonical != path {
        return Err(InstallError::InvalidRequest(format!(
            "{label} must use its canonical absolute path"
        )));
    }
    Ok(canonical)
}

fn reject_tree_symlinks(root: &Path) -> Result<()> {
    let mut pending = vec![root.to_path_buf()];
    while let Some(path) = pending.pop() {
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() {
            return Err(InstallError::InvalidRequest(format!(
                "staged bundle contains symlink: {}",
                path.display()
            )));
        }
        if metadata.is_dir() {
            for entry in fs::read_dir(path)? {
                pending.push(entry?.path());
            }
        }
    }
    Ok(())
}

fn verify_bundle(
    path: &Path,
    request: &InstallRequest,
    inspector: &dyn BundleInspector,
) -> Result<()> {
    match inspector.bundle_id(path) {
        Ok(found) if found == request.expected_bundle_id && found == BUNDLE_ID => {}
        Ok(found) => {
            return Err(InstallError::Verification(format!(
                "bundle identifier is {found}"
            )));
        }
        Err(error) => return Err(InstallError::Verification(error)),
    }
    match inspector.bundle_version(path) {
        Ok(found) if found == request.version => {}
        Ok(found) => {
            return Err(InstallError::Verification(format!(
                "bundle version is {found}, expected {}",
                request.version
            )));
        }
        Err(error) => return Err(InstallError::Verification(error)),
    }
    inspector
        .codesign_verify(path)
        .map_err(InstallError::Verification)?;
    match inspector.team_id(path) {
        Ok(found) if found == request.expected_team_id => {}
        Ok(found) => {
            return Err(InstallError::Verification(format!(
                "Team ID is {found}, expected {}",
                request.expected_team_id
            )));
        }
        Err(error) => return Err(InstallError::Verification(error)),
    }
    inspector
        .gatekeeper_assess(path)
        .map_err(InstallError::Verification)
}

fn verify_current_bundle(
    path: &Path,
    request: &InstallRequest,
    inspector: &dyn BundleInspector,
) -> Result<()> {
    match inspector.bundle_id(path) {
        Ok(found) if found == request.expected_bundle_id && found == BUNDLE_ID => {}
        Ok(found) => {
            return Err(InstallError::Verification(format!(
                "current bundle identifier is {found}"
            )));
        }
        Err(error) => return Err(InstallError::Verification(error)),
    }
    inspector
        .codesign_verify(path)
        .map_err(InstallError::Verification)?;
    match inspector.team_id(path) {
        Ok(found) if found == request.expected_team_id => {}
        Ok(found) => {
            return Err(InstallError::Verification(format!(
                "current bundle Team ID is {found}, expected {}",
                request.expected_team_id
            )));
        }
        Err(error) => return Err(InstallError::Verification(error)),
    }
    inspector
        .gatekeeper_assess(path)
        .map_err(InstallError::Verification)
}

pub fn active_run_count(database: &Path) -> Result<usize> {
    if !database.exists() {
        return Ok(0);
    }
    if fs::symlink_metadata(database)?.file_type().is_symlink() {
        return Err(InstallError::InvalidRequest(
            "database path must not be a symlink".into(),
        ));
    }
    let connection = Connection::open_with_flags(
        database,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    let count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM runs WHERE state IN ('running', 'paused', 'awaiting_approval')",
        [],
        |row| row.get(0),
    )?;
    usize::try_from(count).map_err(|_| {
        InstallError::InvalidRequest("active run count could not fit in memory".into())
    })
}

fn validate_request(request: &InstallRequest) -> Result<()> {
    if request.expected_bundle_id != BUNDLE_ID {
        return Err(InstallError::InvalidRequest(
            "request has the wrong bundle identifier".into(),
        ));
    }
    if request.expected_team_id.trim().is_empty() || request.version.trim().is_empty() {
        return Err(InstallError::InvalidRequest(
            "version and Team ID must be present".into(),
        ));
    }
    if request.parent_pid <= 1 {
        return Err(InstallError::InvalidRequest(
            "parent PID must identify the running UI".into(),
        ));
    }
    if !token_shape_is_valid(&request.token) {
        return Err(InstallError::InvalidRequest(
            "token shape is invalid".into(),
        ));
    }
    for (label, path) in [
        ("archive", &request.archive_path),
        ("staged bundle", &request.staged_bundle),
        ("current bundle", &request.current_bundle),
        ("database", &request.database_path),
    ] {
        if !path.is_absolute() {
            return Err(InstallError::InvalidRequest(format!(
                "{label} path must be absolute"
            )));
        }
    }
    if request
        .current_bundle
        .file_name()
        .and_then(|name| name.to_str())
        != Some(APP_NAME)
        || request
            .staged_bundle
            .file_name()
            .and_then(|name| name.to_str())
            != Some(APP_NAME)
    {
        return Err(InstallError::InvalidRequest(format!(
            "both bundles must be named {APP_NAME}"
        )));
    }
    Ok(())
}

pub trait InstallOps {
    fn prepare_candidate(&self, staged: &Path, candidate: &Path)
    -> std::result::Result<(), String>;
    fn rename(&self, from: &Path, to: &Path) -> std::result::Result<(), String>;
    fn remove_dir_all(&self, path: &Path) -> std::result::Result<(), String>;
    fn relaunch(&self, bundle: &Path) -> std::result::Result<(), String>;
}

pub struct SystemInstallOps;

impl InstallOps for SystemInstallOps {
    fn prepare_candidate(
        &self,
        staged: &Path,
        candidate: &Path,
    ) -> std::result::Result<(), String> {
        match fs::rename(staged, candidate) {
            Ok(()) => Ok(()),
            Err(error) if error.raw_os_error() == Some(libc_exdev()) => {
                let from = path_text(staged)?;
                let to = path_text(candidate)?;
                let output = run_tool("/usr/bin/ditto", &[from, to])?;
                if output.status.success() {
                    Ok(())
                } else {
                    Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
                }
            }
            Err(error) => Err(error.to_string()),
        }
    }

    fn rename(&self, from: &Path, to: &Path) -> std::result::Result<(), String> {
        fs::rename(from, to).map_err(|error| error.to_string())
    }

    fn remove_dir_all(&self, path: &Path) -> std::result::Result<(), String> {
        if path.exists() {
            fs::remove_dir_all(path).map_err(|error| error.to_string())
        } else {
            Ok(())
        }
    }

    fn relaunch(&self, bundle: &Path) -> std::result::Result<(), String> {
        let status = Command::new("/usr/bin/open")
            .arg(bundle)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map_err(|error| error.to_string())?;
        if status.success() {
            Ok(())
        } else {
            Err(format!("/usr/bin/open exited with {status}"))
        }
    }
}

// EXDEV is 18 on Darwin and every Unix target supported by this macOS-only
// workspace. Keeping the number local avoids adding an FFI call or unsafe.
const fn libc_exdev() -> i32 {
    18
}

pub fn install(
    request: &InstallRequest,
    inspector: &dyn BundleInspector,
    operations: &dyn InstallOps,
) -> Result<()> {
    validate_request(request)?;
    let archive = canonical_without_symlink(&request.archive_path, "archive")?;
    let staged = canonical_without_symlink(&request.staged_bundle, "staged bundle")?;
    let current = canonical_without_symlink(&request.current_bundle, "current bundle")?;
    let stage_root = archive
        .parent()
        .ok_or_else(|| InstallError::InvalidRequest("archive has no staging parent".into()))?;
    if !staged.starts_with(stage_root) {
        return Err(InstallError::InvalidRequest(
            "staged bundle is outside the private staging directory".into(),
        ));
    }
    reject_tree_symlinks(&staged)?;
    if sha256_file(&archive)? != request.archive_sha256.to_ascii_lowercase() {
        return Err(InstallError::Verification(
            "archive SHA-256 changed after staging".into(),
        ));
    }
    verify_bundle(&staged, request, inspector)?;
    verify_current_bundle(&current, request, inspector)?;
    let active = active_run_count(&request.database_path)?;
    if active > 0 {
        return Err(InstallError::RunsActive(active));
    }

    let parent = current.parent().ok_or_else(|| {
        InstallError::InvalidRequest("current bundle has no parent directory".into())
    })?;
    let candidate = parent.join(format!(".{APP_NAME}.update-{}", request.token));
    let backup = parent.join(format!(".{APP_NAME}.previous-{}", request.token));
    if candidate.exists() || backup.exists() {
        return Err(InstallError::InvalidRequest(
            "candidate or rollback path already exists".into(),
        ));
    }

    operations
        .prepare_candidate(&staged, &candidate)
        .map_err(InstallError::Swap)?;
    reject_tree_symlinks(&candidate)?;
    verify_bundle(&candidate, request, inspector)?;

    operations
        .rename(&current, &backup)
        .map_err(InstallError::Swap)?;
    if let Err(error) = operations.rename(&candidate, &current) {
        let rollback = operations.rename(&backup, &current);
        let _ = operations.remove_dir_all(&candidate);
        return Err(InstallError::Swap(match rollback {
            Ok(()) => format!("{error}; previous bundle restored"),
            Err(rollback_error) => {
                format!("{error}; CRITICAL rollback also failed: {rollback_error}")
            }
        }));
    }

    if let Err(error) = operations.relaunch(&current) {
        let failed = parent.join(format!(".{APP_NAME}.failed-{}", request.token));
        let moved_failed = operations.rename(&current, &failed);
        let restored = operations.rename(&backup, &current);
        if moved_failed.is_ok() && restored.is_ok() {
            let _ = operations.relaunch(&current);
            let _ = operations.remove_dir_all(&failed);
            return Err(InstallError::Relaunch(error));
        }
        return Err(InstallError::Swap(format!(
            "relaunch failed ({error}); rollback failed: move={moved_failed:?}, restore={restored:?}"
        )));
    }

    operations
        .remove_dir_all(&backup)
        .map_err(InstallError::Swap)?;
    Ok(())
}

pub fn wait_for_parent(parent_pid: u32, attempts: usize, delay: Duration) -> Result<()> {
    if parent_pid <= 1 {
        return Err(InstallError::InvalidRequest("invalid parent PID".into()));
    }
    for _ in 0..attempts {
        let status = Command::new("/bin/kill")
            .arg("-0")
            .arg(parent_pid.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        if status.is_ok_and(|status| !status.success()) {
            return Ok(());
        }
        std::thread::sleep(delay);
    }
    Err(InstallError::ParentStillRunning(parent_pid))
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::rc::Rc;

    use super::*;

    #[derive(Default)]
    struct FakeInspector;

    impl BundleInspector for FakeInspector {
        fn bundle_id(&self, _path: &Path) -> std::result::Result<String, String> {
            Ok(BUNDLE_ID.into())
        }
        fn bundle_version(&self, _path: &Path) -> std::result::Result<String, String> {
            Ok("0.2.0".into())
        }
        fn codesign_verify(&self, _path: &Path) -> std::result::Result<(), String> {
            Ok(())
        }
        fn team_id(&self, _path: &Path) -> std::result::Result<String, String> {
            Ok("ABCDEFGHIJ".into())
        }
        fn gatekeeper_assess(&self, _path: &Path) -> std::result::Result<(), String> {
            Ok(())
        }
    }

    struct TestOps {
        rename_count: Cell<usize>,
        fail_rename: Option<usize>,
        fail_launch: bool,
        launches: Rc<Cell<usize>>,
    }

    impl TestOps {
        fn successful() -> (Self, Rc<Cell<usize>>) {
            let launches = Rc::new(Cell::new(0));
            (
                Self {
                    rename_count: Cell::new(0),
                    fail_rename: None,
                    fail_launch: false,
                    launches: launches.clone(),
                },
                launches,
            )
        }
    }

    impl InstallOps for TestOps {
        fn prepare_candidate(
            &self,
            staged: &Path,
            candidate: &Path,
        ) -> std::result::Result<(), String> {
            fs::rename(staged, candidate).map_err(|error| error.to_string())
        }

        fn rename(&self, from: &Path, to: &Path) -> std::result::Result<(), String> {
            let next = self.rename_count.get() + 1;
            self.rename_count.set(next);
            if self.fail_rename == Some(next) {
                return Err(format!("injected rename {next}"));
            }
            fs::rename(from, to).map_err(|error| error.to_string())
        }

        fn remove_dir_all(&self, path: &Path) -> std::result::Result<(), String> {
            if path.exists() {
                fs::remove_dir_all(path).map_err(|error| error.to_string())
            } else {
                Ok(())
            }
        }

        fn relaunch(&self, _bundle: &Path) -> std::result::Result<(), String> {
            self.launches.set(self.launches.get() + 1);
            if self.fail_launch {
                Err("injected launch failure".into())
            } else {
                Ok(())
            }
        }
    }

    fn fixture() -> (tempfile::TempDir, InstallRequest) {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let stage = root.join("stage");
        let current = root.join(APP_NAME);
        let staged = stage.join("extracted").join(APP_NAME);
        fs::create_dir_all(current.join("Contents")).unwrap();
        fs::create_dir_all(staged.join("Contents")).unwrap();
        fs::write(current.join("old"), b"old").unwrap();
        fs::write(staged.join("new"), b"new").unwrap();
        let archive = stage.join("update.zip");
        fs::write(&archive, b"archive").unwrap();
        let token = "a".repeat(TOKEN_BYTES);
        let request = InstallRequest {
            version: "0.2.0".into(),
            expected_bundle_id: BUNDLE_ID.into(),
            expected_team_id: "ABCDEFGHIJ".into(),
            archive_sha256: sha256_file(&archive).unwrap(),
            archive_path: archive,
            staged_bundle: staged,
            current_bundle: current,
            database_path: root.join("missing.db"),
            parent_pid: 42,
            token,
        };
        (temp, request)
    }

    #[test]
    fn request_file_is_private_and_token_bound() {
        let (_temp, request) = fixture();
        let path = request.archive_path.parent().unwrap().join("request.json");
        write_request(&path, &request).unwrap();
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
        assert_eq!(load_request(&path, &request.token).unwrap(), request);
        assert!(matches!(
            load_request(&path, &"b".repeat(TOKEN_BYTES)),
            Err(InstallError::TokenMismatch)
        ));
    }

    #[test]
    fn successful_install_swaps_relaunches_and_removes_backup() {
        let (_temp, request) = fixture();
        let (operations, launches) = TestOps::successful();
        install(&request, &FakeInspector, &operations).unwrap();
        assert!(request.current_bundle.join("new").exists());
        assert!(!request.current_bundle.join("old").exists());
        assert_eq!(launches.get(), 1);
        assert!(
            !request
                .current_bundle
                .parent()
                .unwrap()
                .join(format!(".{APP_NAME}.previous-{}", request.token))
                .exists()
        );
    }

    #[test]
    fn second_swap_rename_failure_restores_the_previous_bundle() {
        let (_temp, request) = fixture();
        let launches = Rc::new(Cell::new(0));
        let operations = TestOps {
            rename_count: Cell::new(0),
            fail_rename: Some(2),
            fail_launch: false,
            launches,
        };
        assert!(matches!(
            install(&request, &FakeInspector, &operations),
            Err(InstallError::Swap(_))
        ));
        assert!(request.current_bundle.join("old").exists());
        assert!(!request.current_bundle.join("new").exists());
    }

    #[test]
    fn relaunch_failure_rolls_back_and_attempts_to_open_the_previous_bundle() {
        let (_temp, request) = fixture();
        let launches = Rc::new(Cell::new(0));
        let operations = TestOps {
            rename_count: Cell::new(0),
            fail_rename: None,
            fail_launch: true,
            launches: launches.clone(),
        };
        assert!(matches!(
            install(&request, &FakeInspector, &operations),
            Err(InstallError::Relaunch(_))
        ));
        assert!(request.current_bundle.join("old").exists());
        assert_eq!(
            launches.get(),
            2,
            "new and restored bundles were both opened"
        );
    }

    #[test]
    fn archive_tampering_and_staged_symlinks_are_refused() {
        let (_temp, request) = fixture();
        fs::write(&request.archive_path, b"tampered").unwrap();
        let (operations, _) = TestOps::successful();
        assert!(matches!(
            install(&request, &FakeInspector, &operations),
            Err(InstallError::Verification(_))
        ));

        let (_temp, request) = fixture();
        std::os::unix::fs::symlink("new", request.staged_bundle.join("link")).unwrap();
        let (operations, _) = TestOps::successful();
        assert!(matches!(
            install(&request, &FakeInspector, &operations),
            Err(InstallError::InvalidRequest(_))
        ));
    }

    #[test]
    fn authoritative_active_run_state_refuses_installation() {
        let (_temp, mut request) = fixture();
        request.database_path = request
            .current_bundle
            .parent()
            .unwrap()
            .join("autoharness.db");
        let connection = Connection::open(&request.database_path).unwrap();
        connection
            .execute("CREATE TABLE runs (state TEXT NOT NULL)", [])
            .unwrap();
        connection
            .execute("INSERT INTO runs (state) VALUES ('running')", [])
            .unwrap();
        drop(connection);
        let (operations, _) = TestOps::successful();
        assert!(matches!(
            install(&request, &FakeInspector, &operations),
            Err(InstallError::RunsActive(1))
        ));
        assert!(request.current_bundle.join("old").exists());
    }
}
