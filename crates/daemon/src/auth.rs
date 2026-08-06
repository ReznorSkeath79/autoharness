//! One reconciled daemon-client secret across Keychain and the mode-0600
//! fallback file.

use std::fs::{File, OpenOptions};
use std::io::{Error, ErrorKind, Result};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenSource {
    Keychain,
    File,
}

pub(crate) trait TokenBackend: Send + Sync {
    fn read(&self) -> Result<Option<String>>;
    fn write(&self, token: &str) -> Result<()>;
}

#[cfg(not(debug_assertions))]
struct SystemKeychain;

#[cfg(not(debug_assertions))]
impl TokenBackend for SystemKeychain {
    fn read(&self) -> Result<Option<String>> {
        const SERVICE: &str = "dev.autoharness.app";
        const ACCOUNT: &str = "daemon-client-token";
        // Authentication is internal plumbing and must never interrupt launch
        // with a Keychain repair/access dialog. An unavailable or locked
        // Keychain becomes an ordinary backend error and reconciliation uses
        // the private mode-0600 file instead.
        let _interaction_lock =
            security_framework::os::macos::keychain::SecKeychain::disable_user_interaction()
                .map_err(|error| {
                    Error::other(format!("Keychain UI suppression failed: {error}"))
                })?;
        match security_framework::passwords::get_generic_password(SERVICE, ACCOUNT) {
            Ok(bytes) => {
                let token = String::from_utf8(bytes)
                    .map_err(|error| Error::new(ErrorKind::InvalidData, error))?;
                Ok(normalized(&token).map(str::to_string))
            }
            Err(error) => {
                // `errSecItemNotFound` is the expected first-launch state.
                if error.code() == -25_300 {
                    Ok(None)
                } else {
                    Err(Error::other(format!("Keychain read failed: {error}")))
                }
            }
        }
    }

    fn write(&self, token: &str) -> Result<()> {
        const SERVICE: &str = "dev.autoharness.app";
        const ACCOUNT: &str = "daemon-client-token";
        let _interaction_lock =
            security_framework::os::macos::keychain::SecKeychain::disable_user_interaction()
                .map_err(|error| {
                    Error::other(format!("Keychain UI suppression failed: {error}"))
                })?;
        security_framework::passwords::set_generic_password(SERVICE, ACCOUNT, token.as_bytes())
            .map_err(|error| Error::other(format!("Keychain write failed: {error}")))
    }
}

struct InterprocessLock(File);

impl InterprocessLock {
    fn acquire(data_dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(data_dir)?;
        let path = data_dir.join("client-token.lock");
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(path)?;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        loop {
            let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
            if result == 0 {
                return Ok(Self(file));
            }
            let error = Error::last_os_error();
            if error.kind() != ErrorKind::Interrupted {
                return Err(error);
            }
        }
    }
}

impl Drop for InterprocessLock {
    fn drop(&mut self) {
        unsafe {
            libc::flock(self.0.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

fn normalized(token: &str) -> Option<&str> {
    let token = token.trim();
    (!token.is_empty()).then_some(token)
}

fn read_file(path: &Path) -> Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(token) => Ok(normalized(&token).map(str::to_string)),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn write_file(path: &Path, token: &str) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| Error::other("token path has no parent"))?;
    std::fs::create_dir_all(parent)?;
    let temporary = parent.join("client-token.tmp");
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true).mode(0o600);
    let mut file = options.open(&temporary)?;
    use std::io::Write as _;
    file.write_all(token.as_bytes())?;
    file.sync_all()?;
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    std::fs::rename(&temporary, path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

fn file_needs_repair(path: &Path, current: Option<&str>, chosen: &str) -> bool {
    if current != Some(chosen) {
        return true;
    }
    std::fs::metadata(path)
        .map(|metadata| metadata.permissions().mode() & 0o777 != 0o600)
        .unwrap_or(true)
}

pub(crate) fn reconcile_token(
    data_dir: &Path,
    backend: Option<&dyn TokenBackend>,
) -> Result<(String, TokenSource)> {
    let _lock = InterprocessLock::acquire(data_dir)?;
    let token_path = data_dir.join("client-token");
    let file_token = read_file(&token_path)?;

    let (keychain_token, keychain_readable) = match backend {
        Some(backend) => match backend.read() {
            Ok(token) => (
                token.and_then(|token| normalized(&token).map(str::to_string)),
                true,
            ),
            Err(_) => (None, false),
        },
        None => (None, false),
    };

    // An existing Keychain item is authoritative. If it is absent, preserve
    // the fallback token rather than rotating a live daemon out from under a
    // reconnecting UI. Only a true first launch generates a new secret.
    let token = keychain_token
        .clone()
        .or_else(|| file_token.clone())
        .unwrap_or_else(generate_token);

    let keychain_persisted = if keychain_readable {
        match backend {
            Some(_) if keychain_token.as_deref() == Some(token.as_str()) => true,
            Some(backend) => backend.write(&token).is_ok(),
            None => false,
        }
    } else {
        false
    };

    if file_needs_repair(&token_path, file_token.as_deref(), &token) {
        write_file(&token_path, &token)?;
    }

    Ok((
        token,
        if keychain_persisted {
            TokenSource::Keychain
        } else {
            TokenSource::File
        },
    ))
}

/// Generate a fresh 256-bit hex token from UUID entropy.
pub fn generate_token() -> String {
    format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}

pub fn load_or_create_token(data_dir: &Path) -> Result<(String, TokenSource)> {
    #[cfg(not(debug_assertions))]
    {
        reconcile_token(data_dir, Some(&SystemKeychain))
    }
    #[cfg(debug_assertions)]
    {
        // Ad-hoc debug signatures have unstable Keychain identity and may
        // trigger an interactive ACL prompt. The same lock/reconciliation
        // path still protects concurrent debug daemon and UI launches.
        reconcile_token(data_dir, None)
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::sync::{Arc, Mutex};

    use super::*;

    #[derive(Default)]
    struct FakeKeychain {
        value: Mutex<Option<String>>,
        fail_read: bool,
        fail_write: bool,
    }

    impl TokenBackend for FakeKeychain {
        fn read(&self) -> Result<Option<String>> {
            if self.fail_read {
                return Err(Error::other("keychain read unavailable"));
            }
            Ok(self.value.lock().unwrap().clone())
        }

        fn write(&self, token: &str) -> Result<()> {
            if self.fail_write {
                return Err(Error::other("keychain write unavailable"));
            }
            *self.value.lock().unwrap() = Some(token.to_string());
            Ok(())
        }
    }

    #[test]
    fn keychain_wins_a_mismatch_and_repairs_the_fallback_copy() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("client-token"), "stale-file-token").unwrap();
        let backend = FakeKeychain {
            value: Mutex::new(Some("authoritative-keychain-token".into())),
            ..Default::default()
        };

        let (token, source) = reconcile_token(temp.path(), Some(&backend)).unwrap();

        assert_eq!(token, "authoritative-keychain-token");
        assert_eq!(source, TokenSource::Keychain);
        assert_eq!(
            std::fs::read_to_string(temp.path().join("client-token")).unwrap(),
            token
        );
        assert_eq!(
            std::fs::metadata(temp.path().join("client-token"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[test]
    fn a_keychain_failure_uses_the_existing_file_without_rotating_it() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("client-token"), "stable-fallback-token").unwrap();
        let backend = FakeKeychain {
            fail_read: true,
            ..Default::default()
        };

        let (token, source) = reconcile_token(temp.path(), Some(&backend)).unwrap();

        assert_eq!(token, "stable-fallback-token");
        assert_eq!(source, TokenSource::File);
    }

    #[test]
    fn a_keychain_write_failure_creates_an_exact_mode_0600_fallback() {
        let temp = tempfile::tempdir().unwrap();
        let backend = FakeKeychain {
            fail_write: true,
            ..Default::default()
        };

        let (token, source) = reconcile_token(temp.path(), Some(&backend)).unwrap();

        assert_eq!(token.len(), 64);
        assert_eq!(source, TokenSource::File);
        assert_eq!(
            std::fs::metadata(temp.path().join("client-token"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[test]
    fn concurrent_first_launch_converges_on_one_token() {
        let temp = tempfile::tempdir().unwrap();
        let data_dir = Arc::new(temp.path().to_path_buf());
        let backend = Arc::new(FakeKeychain::default());
        let mut threads = Vec::new();
        for _ in 0..12 {
            let data_dir = data_dir.clone();
            let backend = backend.clone();
            threads.push(std::thread::spawn(move || {
                reconcile_token(&data_dir, Some(backend.as_ref()))
                    .unwrap()
                    .0
            }));
        }
        let tokens = threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect::<Vec<_>>();

        assert!(tokens.windows(2).all(|pair| pair[0] == pair[1]));
        assert_eq!(
            backend.value.lock().unwrap().as_deref(),
            Some(tokens[0].as_str())
        );
        assert_eq!(
            std::fs::read_to_string(temp.path().join("client-token")).unwrap(),
            tokens[0]
        );
    }
}
