//! Update checking, in the user-interface process.
//!
//! # Why the daemon does not do this
//!
//! The daemon runs sandboxed work on the user's repositories. Giving it the
//! ability to fetch and execute new code would make it the largest attack
//! surface in the product. Checking for updates is a user-facing errand, so
//! it lives in the user-interface process and touches nothing the daemon owns.
//!
//! # What is enforced before an update is even offered
//!
//! Every one of these must pass. A failure is a `Blocked` status with the
//! reason, never a silent downgrade to "install anyway":
//!
//! 1. The feed URL matches the pinned host and path exactly, over HTTPS.
//! 2. The feed parses into a version, a minimum macOS version, and an asset.
//! 3. This machine meets the minimum macOS version.
//! 4. The offered version is strictly newer (see `autoharness_core::version`).
//! 5. The downloaded asset's SHA-256 equals the digest in the feed.
//! 6. The bundle identifier and bundle version match what the feed declares.
//! 7. `codesign --verify --deep --strict` accepts the bundle.
//! 8. The signing Team ID equals the pinned Team ID.
//! 9. `spctl --assess` accepts the bundle.
//! 10. No run is active, so a restart cannot interrupt work.
//!
//! # Installation boundary
//!
//! The UI downloads and verifies into a private staging directory, then hands
//! a mode-0600, random-token-bound request to the separately signed
//! `autoharness-updater` helper. The helper waits for this UI process to exit,
//! independently repeats digest/signature/Team ID/Gatekeeper and active-run
//! checks, performs an atomic sibling swap, rolls back on failure, and
//! relaunches through `/usr/bin/open`.
//!
//! Release identity is compiled into release builds with
//! `AUTOHARNESS_RELEASE_FEED_URL` and `AUTOHARNESS_RELEASE_TEAM_ID`. Missing
//! inputs are a visible fail-closed status, never a permissive default.

use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;

use autoharness_core::version::{self, UpdateVerdict};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// The only host an update may come from.
pub const FEED_HOST: &str = "releases.autoharness.dev";
/// The only path on that host.
pub const FEED_PATH: &str = "/stable/appcast.json";
/// The only Team ID whose signature is accepted.
///
/// Release packaging supplies the owner's Team ID at compile time. The
/// fallback exists only so pure evaluator tests have a well-formed value;
/// [`ReleaseConfig::compiled`] refuses the live path when the input is absent.
pub const EXPECTED_TEAM_ID: &str = match option_env!("AUTOHARNESS_RELEASE_TEAM_ID") {
    Some(team_id) => team_id,
    // Test-only fallback for the pure evaluator. The live checker calls
    // `ReleaseConfig::compiled` first and refuses when the environment input
    // was absent, so this value can never enable an installation.
    None => "TESTTEAM01",
};
/// The only bundle identifier that may replace this one. Matches the
/// development bundle identifier recorded in PLAN.md.
pub const EXPECTED_BUNDLE_ID: &str = "dev.autoharness.app";

const FEED_MAX_BYTES: u64 = 1024 * 1024;
const ARCHIVE_MAX_BYTES: u64 = 2 * 1024 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseConfig {
    pub feed_url: String,
    pub team_id: String,
}

impl ReleaseConfig {
    pub fn compiled() -> Result<Self, Vec<&'static str>> {
        let mut missing = Vec::new();
        let feed_url = option_env!("AUTOHARNESS_RELEASE_FEED_URL")
            .filter(|value| !value.trim().is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| {
                missing.push("AUTOHARNESS_RELEASE_FEED_URL");
                String::new()
            });
        let team_id = option_env!("AUTOHARNESS_RELEASE_TEAM_ID")
            .filter(|value| !value.trim().is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| {
                missing.push("AUTOHARNESS_RELEASE_TEAM_ID");
                String::new()
            });
        if missing.is_empty() {
            Ok(Self { feed_url, team_id })
        } else {
            Err(missing)
        }
    }

    fn validate(&self) -> Result<(), Blocker> {
        let feed = parse_https_url(&self.feed_url)?;
        if feed.path_and_query.contains('?')
            || !feed.path_and_query.to_ascii_lowercase().ends_with(".json")
        {
            return Err(Blocker::ReleaseConfiguration(
                "feed URL must be a fixed .json path without a query".into(),
            ));
        }
        if self.team_id.len() != 10
            || !self
                .team_id
                .bytes()
                .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit())
        {
            return Err(Blocker::ReleaseConfiguration(
                "Team ID must be 10 uppercase letters or digits".into(),
            ));
        }
        Ok(())
    }
}

/// One release, as the feed describes it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeedRelease {
    pub version: String,
    /// Lowest macOS version this build runs on, e.g. "14.0".
    pub minimum_macos: String,
    pub url: String,
    /// Lowercase hex SHA-256 of the asset at `url`.
    pub sha256: String,
    pub bundle_id: String,
    pub team_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Feed {
    pub releases: Vec<FeedRelease>,
}

/// Why an offered update was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Blocker {
    ReleaseConfiguration(String),
    FeedHostNotPinned,
    FeedPathNotPinned,
    FeedNotHttps,
    FeedUnreadable(String),
    AssetOriginNotPinned,
    AssetNotZip,
    DownloadFailed(String),
    ExtractionFailed(String),
    StagingInvalid(String),
    HelperUnavailable(String),
    InstallPreparation(String),
    MacosTooOld { needs: String, found: String },
    NotNewer,
    DigestMismatch,
    BundleIdMismatch,
    BundleVersionMismatch,
    CodesignRejected(String),
    TeamIdMismatch { expected: String, found: String },
    GatekeeperRejected(String),
    RunsActive(usize),
}

impl Blocker {
    pub fn label(&self) -> String {
        match self {
            Self::ReleaseConfiguration(why) => {
                format!("release update configuration is invalid: {why}")
            }
            Self::FeedHostNotPinned => "the feed host is not the pinned one".into(),
            Self::FeedPathNotPinned => "the feed path is not the pinned one".into(),
            Self::FeedNotHttps => "the feed is not served over HTTPS".into(),
            Self::FeedUnreadable(why) => format!("the feed could not be read: {why}"),
            Self::AssetOriginNotPinned => {
                "the release asset is not on the compiled feed origin".into()
            }
            Self::AssetNotZip => "the release asset is not a .zip archive".into(),
            Self::DownloadFailed(why) => format!("the update download failed: {why}"),
            Self::ExtractionFailed(why) => format!("the update could not be extracted: {why}"),
            Self::StagingInvalid(why) => format!("the staged update is invalid: {why}"),
            Self::HelperUnavailable(why) => format!("the updater helper is unavailable: {why}"),
            Self::InstallPreparation(why) => {
                format!("the install request could not be prepared: {why}")
            }
            Self::MacosTooOld { needs, found } => {
                format!("this release needs macOS {needs}; this machine runs {found}")
            }
            Self::NotNewer => "the offered version is not newer".into(),
            Self::DigestMismatch => "the download does not match the published SHA-256".into(),
            Self::BundleIdMismatch => "the bundle identifier does not match".into(),
            Self::BundleVersionMismatch => "the bundle version does not match the feed".into(),
            Self::CodesignRejected(why) => format!("codesign rejected the bundle: {why}"),
            Self::TeamIdMismatch { expected, found } => {
                format!("the signing Team ID is {found}, not {expected}")
            }
            Self::GatekeeperRejected(why) => format!("Gatekeeper rejected the bundle: {why}"),
            Self::RunsActive(count) => {
                format!("{count} run(s) are active; a restart would interrupt them")
            }
        }
    }
}

/// What the Settings panel shows.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum UpdateStatus {
    #[default]
    Idle,
    Checking,
    /// This build can never self-update: unsigned, unbundled, or ad-hoc.
    UnavailableForThisBuild(String),
    UpToDate,
    /// Verified and newer, but installation is still gated.
    Available {
        version: String,
        url: String,
    },
    Blocked(Blocker),
    Unknown(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateAction {
    CheckNow,
    InstallVerified,
}

impl UpdateStatus {
    pub fn summary(&self) -> String {
        match self {
            Self::Idle => "Not checked yet".into(),
            Self::Checking => "Checking…".into(),
            Self::UnavailableForThisBuild(why) => {
                format!("Updates unavailable for this build: {why}")
            }
            Self::UpToDate => "Up to date".into(),
            Self::Available { version, .. } => format!("{version} verified — ready to install"),
            Self::Blocked(blocker) => format!("Blocked: {}", blocker.label()),
            Self::Unknown(why) => format!("Unknown: {why}"),
        }
    }
}

/// A pinned HTTPS feed URL, and nothing else.
///
/// Pinning is checked by parsing rather than by substring: a URL like
/// `https://evil.example/releases.autoharness.dev/stable/appcast.json`
/// contains the host as text but is not served by it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct HttpsUrl<'a> {
    authority: &'a str,
    path_and_query: &'a str,
}

fn parse_https_url(url: &str) -> Result<HttpsUrl<'_>, Blocker> {
    let Some(rest) = url.strip_prefix("https://") else {
        return Err(Blocker::FeedNotHttps);
    };
    if rest.contains('#') {
        return Err(Blocker::ReleaseConfiguration(
            "URL fragments are not allowed".into(),
        ));
    }
    let (authority, path_and_query) = match rest.find('/') {
        Some(index) => (&rest[..index], &rest[index..]),
        None => (rest, ""),
    };
    if authority.is_empty() || authority.contains('@') {
        return Err(Blocker::FeedHostNotPinned);
    }
    Ok(HttpsUrl {
        authority,
        path_and_query,
    })
}

pub fn check_feed_url(url: &str) -> Result<(), Blocker> {
    let parsed = parse_https_url(url)?;
    if parsed.authority != FEED_HOST {
        return Err(Blocker::FeedHostNotPinned);
    }
    if parsed.path_and_query != FEED_PATH {
        return Err(Blocker::FeedPathNotPinned);
    }
    Ok(())
}

fn check_compiled_feed_url(actual: &str, expected: &str) -> Result<(), Blocker> {
    let actual = parse_https_url(actual)?;
    let expected = parse_https_url(expected)?;
    if actual.authority != expected.authority {
        return Err(Blocker::FeedHostNotPinned);
    }
    if actual.path_and_query != expected.path_and_query {
        return Err(Blocker::FeedPathNotPinned);
    }
    Ok(())
}

pub fn check_asset_url(feed_url: &str, asset_url: &str) -> Result<(), Blocker> {
    let feed = parse_https_url(feed_url).map_err(|_| Blocker::AssetOriginNotPinned)?;
    let asset = parse_https_url(asset_url).map_err(|_| Blocker::AssetOriginNotPinned)?;
    if feed.authority != asset.authority {
        return Err(Blocker::AssetOriginNotPinned);
    }
    let asset_path = asset
        .path_and_query
        .split('?')
        .next()
        .unwrap_or(asset.path_and_query);
    if !asset_path.to_ascii_lowercase().ends_with(".zip") {
        return Err(Blocker::AssetNotZip);
    }
    Ok(())
}

pub fn parse_feed(body: &str) -> Result<Feed, Blocker> {
    serde_json::from_str::<Feed>(body).map_err(|e| Blocker::FeedUnreadable(e.to_string()))
}

/// Compare two dotted macOS versions numerically.
fn at_least(found: &str, needs: &str) -> bool {
    let parse = |text: &str| -> Vec<u64> {
        text.split('.')
            .map(|part| part.trim().parse::<u64>().unwrap_or(0))
            .collect()
    };
    let found = parse(found);
    let needs = parse(needs);
    for index in 0..needs.len().max(found.len()) {
        let a = found.get(index).copied().unwrap_or(0);
        let b = needs.get(index).copied().unwrap_or(0);
        if a != b {
            return a > b;
        }
    }
    true
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

/// What a bundle says about itself, and what the system says about it.
///
/// Everything here comes from an adapter so the whole flow can be exercised
/// with fakes. The real adapter shells out to `codesign` and `spctl` with
/// fixed absolute paths and fixed arguments — no command is ever built from
/// feed data.
pub trait BundleInspector {
    fn bundle_id(&self, path: &str) -> Result<String, String>;
    fn bundle_version(&self, path: &str) -> Result<String, String>;
    /// `codesign --verify --deep --strict`.
    fn codesign_verify(&self, path: &str) -> Result<(), String>;
    /// Team ID from the signing certificate.
    fn team_id(&self, path: &str) -> Result<String, String>;
    /// `spctl --assess --type execute`.
    fn gatekeeper_assess(&self, path: &str) -> Result<(), String>;
}

/// Everything the checker needs to know about the running system.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostFacts {
    pub current_version: String,
    pub macos_version: String,
    pub active_runs: usize,
    /// False for a development, ad-hoc, or unbundled build.
    pub signed_release_build: bool,
}

/// Run the whole verification. Returns the status the panel displays.
///
/// The order matters: the cheap, offline refusals come first, so a bad feed
/// never causes a download, and a download is never inspected before its
/// digest is confirmed.
pub fn evaluate(
    feed_url: &str,
    feed_body: &str,
    downloaded: &[u8],
    bundle_path: &str,
    inspector: &dyn BundleInspector,
    facts: &HostFacts,
) -> UpdateStatus {
    evaluate_with_config(
        &ReleaseConfig {
            feed_url: format!("https://{FEED_HOST}{FEED_PATH}"),
            team_id: EXPECTED_TEAM_ID.into(),
        },
        feed_url,
        feed_body,
        downloaded,
        bundle_path,
        inspector,
        facts,
    )
}

pub fn evaluate_with_config(
    config: &ReleaseConfig,
    feed_url: &str,
    feed_body: &str,
    downloaded: &[u8],
    bundle_path: &str,
    inspector: &dyn BundleInspector,
    facts: &HostFacts,
) -> UpdateStatus {
    if !facts.signed_release_build {
        return UpdateStatus::UnavailableForThisBuild(
            "this build is not a signed, notarized release".into(),
        );
    }
    if let Err(blocker) = config.validate() {
        return UpdateStatus::Blocked(blocker);
    }
    if let Err(blocker) = check_compiled_feed_url(feed_url, &config.feed_url) {
        return UpdateStatus::Blocked(blocker);
    }
    let feed = match parse_feed(feed_body) {
        Ok(feed) => feed,
        Err(blocker) => return UpdateStatus::Blocked(blocker),
    };
    let Some(release) = feed.releases.first() else {
        return UpdateStatus::UpToDate;
    };
    if let Err(blocker) = check_asset_url(&config.feed_url, &release.url) {
        return UpdateStatus::Blocked(blocker);
    }
    if release.team_id != config.team_id {
        return UpdateStatus::Blocked(Blocker::TeamIdMismatch {
            expected: config.team_id.clone(),
            found: release.team_id.clone(),
        });
    }

    match version::check(&facts.current_version, &release.version, &release.url) {
        UpdateVerdict::UpToDate => return UpdateStatus::UpToDate,
        UpdateVerdict::Unknown { reason } => return UpdateStatus::Unknown(reason),
        UpdateVerdict::Available { .. } => {}
    }

    if !at_least(&facts.macos_version, &release.minimum_macos) {
        return UpdateStatus::Blocked(Blocker::MacosTooOld {
            needs: release.minimum_macos.clone(),
            found: facts.macos_version.clone(),
        });
    }
    // The digest gates everything after it: an unverified download is never
    // handed to codesign, and never mounted or opened.
    if sha256_hex(downloaded) != release.sha256.trim().to_ascii_lowercase() {
        return UpdateStatus::Blocked(Blocker::DigestMismatch);
    }
    match inspector.bundle_id(bundle_path) {
        Ok(id) if id == release.bundle_id && id == EXPECTED_BUNDLE_ID => {}
        Ok(_) => return UpdateStatus::Blocked(Blocker::BundleIdMismatch),
        Err(why) => return UpdateStatus::Unknown(why),
    }
    match inspector.bundle_version(bundle_path) {
        Ok(found)
            if version::Version::parse(&found) == version::Version::parse(&release.version) => {}
        Ok(_) => return UpdateStatus::Blocked(Blocker::BundleVersionMismatch),
        Err(why) => return UpdateStatus::Unknown(why),
    }
    if let Err(why) = inspector.codesign_verify(bundle_path) {
        return UpdateStatus::Blocked(Blocker::CodesignRejected(why));
    }
    match inspector.team_id(bundle_path) {
        Ok(found) if found == config.team_id && found == release.team_id => {}
        Ok(found) => {
            return UpdateStatus::Blocked(Blocker::TeamIdMismatch {
                expected: config.team_id.clone(),
                found,
            });
        }
        Err(why) => return UpdateStatus::Unknown(why),
    }
    if let Err(why) = inspector.gatekeeper_assess(bundle_path) {
        return UpdateStatus::Blocked(Blocker::GatekeeperRejected(why));
    }
    // Last, because it is the only condition that can change while the user
    // reads the result: never restart out from under a running agent.
    if facts.active_runs > 0 {
        return UpdateStatus::Blocked(Blocker::RunsActive(facts.active_runs));
    }
    UpdateStatus::Available {
        version: release.version.clone(),
        url: release.url.clone(),
    }
}

/// The real macOS inspector.
///
/// Every command is a fixed absolute path with fixed arguments plus one path.
/// No part of a command line comes from the feed, so a hostile feed cannot
/// turn a verification step into an execution primitive.
pub struct MacBundleInspector;

fn run_tool(program: &str, args: &[&str]) -> Result<std::process::Output, String> {
    std::process::Command::new(program)
        .args(args)
        .output()
        .map_err(|e| format!("{program}: {e}"))
}

fn plist_value(bundle_path: &str, key: &str) -> Result<String, String> {
    let plist = format!("{bundle_path}/Contents/Info.plist");
    let output = run_tool(
        "/usr/libexec/PlistBuddy",
        &["-c", &format!("Print :{key}"), &plist],
    )?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

impl BundleInspector for MacBundleInspector {
    fn bundle_id(&self, path: &str) -> Result<String, String> {
        plist_value(path, "CFBundleIdentifier")
    }

    fn bundle_version(&self, path: &str) -> Result<String, String> {
        plist_value(path, "CFBundleShortVersionString")
    }

    fn codesign_verify(&self, path: &str) -> Result<(), String> {
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

    fn team_id(&self, path: &str) -> Result<String, String> {
        let output = run_tool("/usr/bin/codesign", &["-dv", "--verbose=4", path])?;
        // codesign writes its report to stderr.
        let report = String::from_utf8_lossy(&output.stderr);
        report
            .lines()
            .find_map(|line| line.strip_prefix("TeamIdentifier="))
            .map(|id| id.trim().to_string())
            .ok_or_else(|| "codesign reported no TeamIdentifier".to_string())
    }

    fn gatekeeper_assess(&self, path: &str) -> Result<(), String> {
        let output = run_tool("/usr/sbin/spctl", &["--assess", "--type", "execute", path])?;
        if output.status.success() {
            Ok(())
        } else {
            Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StagedUpdate {
    pub version: String,
    pub asset_url: String,
    pub archive_sha256: String,
    pub archive_path: PathBuf,
    pub bundle_path: PathBuf,
    pub current_bundle: PathBuf,
    pub staging_root: PathBuf,
    pub team_id: String,
}

#[derive(Debug)]
struct CheckOutcome {
    status: UpdateStatus,
    staged: Option<StagedUpdate>,
}

/// Owns at most one asynchronous check and one verified staging directory.
pub struct UpdateManager {
    sender: mpsc::Sender<CheckOutcome>,
    receiver: mpsc::Receiver<CheckOutcome>,
    staged: Option<StagedUpdate>,
    checking: bool,
    automatic_attempted: bool,
}

/// Resolve `Foo.app` from `Foo.app/Contents/MacOS/autoharness` without
/// accepting a lookalike directory layout.
pub fn running_bundle_path() -> Option<PathBuf> {
    let executable = std::env::current_exe().ok()?;
    let macos = executable.parent()?;
    if macos.file_name()?.to_str()? != "MacOS" {
        return None;
    }
    let contents = macos.parent()?;
    if contents.file_name()?.to_str()? != "Contents" {
        return None;
    }
    let bundle = contents.parent()?;
    if bundle.file_name()?.to_str()? != autoharness_updater::APP_NAME {
        return None;
    }
    Some(bundle.to_path_buf())
}

impl Default for UpdateManager {
    fn default() -> Self {
        let (sender, receiver) = mpsc::channel();
        Self {
            sender,
            receiver,
            staged: None,
            checking: false,
            automatic_attempted: false,
        }
    }
}

impl UpdateManager {
    pub fn automatic_attempted(&self) -> bool {
        self.automatic_attempted
    }

    pub fn mark_automatic_attempted(&mut self) {
        self.automatic_attempted = true;
    }

    pub fn checking(&self) -> bool {
        self.checking
    }

    pub fn has_staged_update(&self) -> bool {
        self.staged.is_some()
    }

    pub fn start_check(
        &mut self,
        current_bundle: Option<PathBuf>,
        data_dir: PathBuf,
        active_runs: usize,
    ) -> bool {
        if self.checking {
            return false;
        }
        if let Some(previous) = self.staged.take() {
            let _ = fs::remove_dir_all(previous.staging_root);
        }
        self.checking = true;
        let sender = self.sender.clone();
        let spawned = std::thread::Builder::new()
            .name("autoharness-update-check".into())
            .spawn(move || {
                let outcome = check_and_stage(current_bundle, &data_dir, active_runs);
                if let Err(mpsc::SendError(outcome)) = sender.send(outcome)
                    && let Some(staged) = outcome.staged
                {
                    let _ = fs::remove_dir_all(staged.staging_root);
                }
            })
            .is_ok();
        if !spawned {
            self.checking = false;
        }
        spawned
    }

    pub fn poll(&mut self) -> Option<UpdateStatus> {
        let mut latest = None;
        while let Ok(outcome) = self.receiver.try_recv() {
            self.checking = false;
            self.staged = outcome.staged;
            latest = Some(outcome.status);
        }
        latest
    }

    pub fn launch_installer(
        &mut self,
        data_dir: &Path,
        parent_pid: u32,
        active_runs: usize,
    ) -> Result<(), Blocker> {
        if active_runs > 0 {
            return Err(Blocker::RunsActive(active_runs));
        }
        let staged = self
            .staged
            .as_ref()
            .ok_or_else(|| Blocker::InstallPreparation("no verified update is staged".into()))?;
        let helper = staged
            .current_bundle
            .join("Contents/MacOS/autoharness-updater");
        let metadata = fs::symlink_metadata(&helper)
            .map_err(|error| Blocker::HelperUnavailable(error.to_string()))?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(Blocker::HelperUnavailable(
                "helper must be a regular file inside the signed bundle".into(),
            ));
        }

        let token = format!(
            "{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        );
        let request = autoharness_updater::InstallRequest {
            version: staged.version.clone(),
            expected_bundle_id: EXPECTED_BUNDLE_ID.into(),
            expected_team_id: staged.team_id.clone(),
            archive_path: staged.archive_path.clone(),
            archive_sha256: staged.archive_sha256.clone(),
            staged_bundle: staged.bundle_path.clone(),
            current_bundle: staged.current_bundle.clone(),
            database_path: data_dir.join("autoharness.db"),
            parent_pid,
            token: token.clone(),
        };
        let request_path = staged.staging_root.join("install-request.json");
        autoharness_updater::write_request(&request_path, &request)
            .map_err(|error| Blocker::InstallPreparation(error.to_string()))?;
        let log = open_private_update_log(data_dir)
            .map_err(|error| Blocker::InstallPreparation(error.to_string()))?;
        let stderr = log
            .try_clone()
            .map_err(|error| Blocker::InstallPreparation(error.to_string()))?;
        Command::new(&helper)
            .arg("--request")
            .arg(&request_path)
            .arg("--token")
            .arg(&token)
            .stdin(Stdio::null())
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(stderr))
            .spawn()
            .map_err(|error| Blocker::HelperUnavailable(error.to_string()))?;
        self.staged = None;
        Ok(())
    }
}

impl Drop for UpdateManager {
    fn drop(&mut self) {
        if let Some(staged) = self.staged.take() {
            let _ = fs::remove_dir_all(staged.staging_root);
        }
    }
}

fn open_private_update_log(data_dir: &Path) -> std::io::Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt;

    fs::create_dir_all(data_dir)?;
    let file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(data_dir.join("updater.log"))?;
    // `mode` only applies at creation time. Repair an older or manually
    // created log before writing release and filesystem diagnostics to it.
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    Ok(file)
}

fn current_bundle_is_release(
    bundle: &Path,
    config: &ReleaseConfig,
    inspector: &dyn BundleInspector,
) -> Result<String, String> {
    let bundle_text = bundle
        .to_str()
        .ok_or_else(|| "current bundle path is not valid UTF-8".to_string())?;
    if inspector.bundle_id(bundle_text)? != EXPECTED_BUNDLE_ID {
        return Err("current bundle identifier does not match AutoHarness".into());
    }
    inspector.codesign_verify(bundle_text)?;
    let team = inspector.team_id(bundle_text)?;
    if team != config.team_id {
        return Err(format!(
            "current bundle Team ID is {team}, expected {}",
            config.team_id
        ));
    }
    inspector.gatekeeper_assess(bundle_text)?;
    inspector.bundle_version(bundle_text)
}

fn macos_version() -> Result<String, String> {
    let output = Command::new("/usr/bin/sw_vers")
        .arg("-productVersion")
        .stdin(Stdio::null())
        .output()
        .map_err(|error| error.to_string())?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn curl_arguments(url: &str, max_bytes: u64, output: Option<&Path>) -> Vec<OsString> {
    let mut arguments: Vec<OsString> = [
        "-q",
        "--fail",
        "--silent",
        "--show-error",
        "--proto",
        "=https",
        "--tlsv1.2",
        "--max-redirs",
        "0",
        "--connect-timeout",
        "15",
        "--max-time",
        "600",
        "--max-filesize",
    ]
    .into_iter()
    .map(OsString::from)
    .collect();
    arguments.push(max_bytes.to_string().into());
    if let Some(output) = output {
        arguments.push("--output".into());
        arguments.push(output.as_os_str().to_owned());
    }
    arguments.push(url.into());
    arguments
}

fn fetch_feed(url: &str) -> Result<String, String> {
    let output = Command::new("/usr/bin/curl")
        .args(curl_arguments(url, FEED_MAX_BYTES, None))
        .stdin(Stdio::null())
        .output()
        .map_err(|error| error.to_string())?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    String::from_utf8(output.stdout).map_err(|error| error.to_string())
}

fn download_archive(url: &str, destination: &Path) -> Result<(), String> {
    let output = Command::new("/usr/bin/curl")
        .args(curl_arguments(url, ARCHIVE_MAX_BYTES, Some(destination)))
        .stdin(Stdio::null())
        .output()
        .map_err(|error| error.to_string())?;
    if output.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
}

fn create_staging_root(data_dir: &Path) -> Result<PathBuf, String> {
    let updates = data_dir.join("updates");
    fs::create_dir_all(&updates).map_err(|error| error.to_string())?;
    fs::set_permissions(&updates, fs::Permissions::from_mode(0o700))
        .map_err(|error| error.to_string())?;
    for _ in 0..8 {
        let candidate = updates.join(format!("stage-{}", uuid::Uuid::new_v4().simple()));
        let mut builder = fs::DirBuilder::new();
        builder.mode(0o700);
        match builder.create(&candidate) {
            Ok(()) => return candidate.canonicalize().map_err(|error| error.to_string()),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.to_string()),
        }
    }
    Err("could not allocate a unique staging directory".into())
}

fn extract_archive(archive: &Path, destination: &Path) -> Result<(), String> {
    fs::create_dir(destination).map_err(|error| error.to_string())?;
    fs::set_permissions(destination, fs::Permissions::from_mode(0o700))
        .map_err(|error| error.to_string())?;
    let output = Command::new("/usr/bin/ditto")
        .arg("-x")
        .arg("-k")
        .arg(archive)
        .arg(destination)
        .stdin(Stdio::null())
        .output()
        .map_err(|error| error.to_string())?;
    if output.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
}

fn reject_symlinks(root: &Path) -> Result<(), String> {
    let mut pending = vec![root.to_path_buf()];
    while let Some(path) = pending.pop() {
        let metadata = fs::symlink_metadata(&path).map_err(|error| error.to_string())?;
        if metadata.file_type().is_symlink() {
            return Err(format!("archive contains a symlink: {}", path.display()));
        }
        if metadata.is_dir() {
            for entry in fs::read_dir(path).map_err(|error| error.to_string())? {
                pending.push(entry.map_err(|error| error.to_string())?.path());
            }
        }
    }
    Ok(())
}

fn single_staged_bundle(extracted: &Path) -> Result<PathBuf, String> {
    let entries: Vec<PathBuf> = fs::read_dir(extracted)
        .map_err(|error| error.to_string())?
        .map(|entry| {
            entry
                .map(|entry| entry.path())
                .map_err(|error| error.to_string())
        })
        .collect::<Result<_, _>>()?;
    if entries.len() != 1
        || entries[0].file_name().and_then(|name| name.to_str())
            != Some(autoharness_updater::APP_NAME)
        || !entries[0].is_dir()
    {
        return Err(format!(
            "archive must contain exactly one top-level {}",
            autoharness_updater::APP_NAME
        ));
    }
    reject_symlinks(&entries[0])?;
    entries[0].canonicalize().map_err(|error| error.to_string())
}

fn latest_stable_release(feed: &Feed) -> Result<Option<FeedRelease>, String> {
    let mut latest: Option<(version::Version, FeedRelease)> = None;
    for release in &feed.releases {
        if version::Version::is_prerelease(&release.version) {
            continue;
        }
        let parsed = version::Version::parse(&release.version)
            .ok_or_else(|| format!("unparseable release version: {}", release.version))?;
        if latest.as_ref().is_none_or(|(current, _)| parsed > *current) {
            latest = Some((parsed, release.clone()));
        }
    }
    Ok(latest.map(|(_, release)| release))
}

fn verify_staged_bundle(
    bundle: &Path,
    release: &FeedRelease,
    config: &ReleaseConfig,
    inspector: &dyn BundleInspector,
) -> Result<(), Blocker> {
    let path = bundle
        .to_str()
        .ok_or_else(|| Blocker::StagingInvalid("bundle path is not valid UTF-8".into()))?;
    match inspector.bundle_id(path) {
        Ok(found) if found == EXPECTED_BUNDLE_ID && found == release.bundle_id => {}
        Ok(_) => return Err(Blocker::BundleIdMismatch),
        Err(error) => return Err(Blocker::StagingInvalid(error)),
    }
    match inspector.bundle_version(path) {
        Ok(found)
            if version::Version::parse(&found) == version::Version::parse(&release.version) => {}
        Ok(_) => return Err(Blocker::BundleVersionMismatch),
        Err(error) => return Err(Blocker::StagingInvalid(error)),
    }
    inspector
        .codesign_verify(path)
        .map_err(Blocker::CodesignRejected)?;
    match inspector.team_id(path) {
        Ok(found) if found == config.team_id && found == release.team_id => {}
        Ok(found) => {
            return Err(Blocker::TeamIdMismatch {
                expected: config.team_id.clone(),
                found,
            });
        }
        Err(error) => return Err(Blocker::StagingInvalid(error)),
    }
    inspector
        .gatekeeper_assess(path)
        .map_err(Blocker::GatekeeperRejected)
}

fn check_and_stage(
    current_bundle: Option<PathBuf>,
    data_dir: &Path,
    active_runs: usize,
) -> CheckOutcome {
    let config = match ReleaseConfig::compiled() {
        Ok(config) => config,
        Err(missing) => {
            return CheckOutcome {
                status: UpdateStatus::UnavailableForThisBuild(format!(
                    "missing compile-time release inputs: {}",
                    missing.join(", ")
                )),
                staged: None,
            };
        }
    };
    if let Err(blocker) = config.validate() {
        return CheckOutcome {
            status: UpdateStatus::Blocked(blocker),
            staged: None,
        };
    }
    let Some(current_bundle) = current_bundle else {
        return CheckOutcome {
            status: UpdateStatus::UnavailableForThisBuild(
                "the executable is not running from an app bundle".into(),
            ),
            staged: None,
        };
    };
    let current_bundle = match current_bundle.canonicalize() {
        Ok(path) => path,
        Err(error) => {
            return CheckOutcome {
                status: UpdateStatus::UnavailableForThisBuild(error.to_string()),
                staged: None,
            };
        }
    };
    let inspector = MacBundleInspector;
    let current_version = match current_bundle_is_release(&current_bundle, &config, &inspector) {
        Ok(version) => version,
        Err(error) => {
            return CheckOutcome {
                status: UpdateStatus::UnavailableForThisBuild(error),
                staged: None,
            };
        }
    };
    if active_runs > 0 {
        return CheckOutcome {
            status: UpdateStatus::Blocked(Blocker::RunsActive(active_runs)),
            staged: None,
        };
    }
    let feed_body = match fetch_feed(&config.feed_url) {
        Ok(body) => body,
        Err(error) => {
            return CheckOutcome {
                status: UpdateStatus::Blocked(Blocker::DownloadFailed(error)),
                staged: None,
            };
        }
    };
    let feed = match parse_feed(&feed_body) {
        Ok(feed) => feed,
        Err(blocker) => {
            return CheckOutcome {
                status: UpdateStatus::Blocked(blocker),
                staged: None,
            };
        }
    };
    let release = match latest_stable_release(&feed) {
        Ok(Some(release)) => release,
        Ok(None) => {
            return CheckOutcome {
                status: UpdateStatus::UpToDate,
                staged: None,
            };
        }
        Err(error) => {
            return CheckOutcome {
                status: UpdateStatus::Unknown(error),
                staged: None,
            };
        }
    };
    match version::check(&current_version, &release.version, &release.url) {
        UpdateVerdict::UpToDate => {
            return CheckOutcome {
                status: UpdateStatus::UpToDate,
                staged: None,
            };
        }
        UpdateVerdict::Unknown { reason } => {
            return CheckOutcome {
                status: UpdateStatus::Unknown(reason),
                staged: None,
            };
        }
        UpdateVerdict::Available { .. } => {}
    }
    let found_macos = match macos_version() {
        Ok(version) => version,
        Err(error) => {
            return CheckOutcome {
                status: UpdateStatus::Unknown(error),
                staged: None,
            };
        }
    };
    if !at_least(&found_macos, &release.minimum_macos) {
        return CheckOutcome {
            status: UpdateStatus::Blocked(Blocker::MacosTooOld {
                needs: release.minimum_macos,
                found: found_macos,
            }),
            staged: None,
        };
    }
    if release.bundle_id != EXPECTED_BUNDLE_ID {
        return CheckOutcome {
            status: UpdateStatus::Blocked(Blocker::BundleIdMismatch),
            staged: None,
        };
    }
    if release.team_id != config.team_id {
        return CheckOutcome {
            status: UpdateStatus::Blocked(Blocker::TeamIdMismatch {
                expected: config.team_id,
                found: release.team_id,
            }),
            staged: None,
        };
    }
    if let Err(blocker) = check_asset_url(&config.feed_url, &release.url) {
        return CheckOutcome {
            status: UpdateStatus::Blocked(blocker),
            staged: None,
        };
    }

    let staging_root = match create_staging_root(data_dir) {
        Ok(path) => path,
        Err(error) => {
            return CheckOutcome {
                status: UpdateStatus::Blocked(Blocker::StagingInvalid(error)),
                staged: None,
            };
        }
    };
    let outcome = (|| -> Result<StagedUpdate, Blocker> {
        let archive_path = staging_root.join("AutoHarness-update.zip");
        download_archive(&release.url, &archive_path).map_err(Blocker::DownloadFailed)?;
        let digest = autoharness_updater::sha256_file(&archive_path)
            .map_err(|error| Blocker::StagingInvalid(error.to_string()))?;
        if digest != release.sha256.trim().to_ascii_lowercase() {
            return Err(Blocker::DigestMismatch);
        }
        let extracted = staging_root.join("extracted");
        extract_archive(&archive_path, &extracted).map_err(Blocker::ExtractionFailed)?;
        let bundle_path = single_staged_bundle(&extracted).map_err(Blocker::StagingInvalid)?;
        verify_staged_bundle(&bundle_path, &release, &config, &inspector)?;
        Ok(StagedUpdate {
            version: release.version.clone(),
            asset_url: release.url.clone(),
            archive_sha256: digest,
            archive_path,
            bundle_path,
            current_bundle,
            staging_root: staging_root.clone(),
            team_id: config.team_id,
        })
    })();
    match outcome {
        Ok(staged) => CheckOutcome {
            status: UpdateStatus::Available {
                version: staged.version.clone(),
                url: staged.asset_url.clone(),
            },
            staged: Some(staged),
        },
        Err(blocker) => {
            let _ = fs::remove_dir_all(staging_root);
            CheckOutcome {
                status: UpdateStatus::Blocked(blocker),
                staged: None,
            }
        }
    }
}

/// Why installation is still gated, whatever the verification says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallGate {
    pub missing: Vec<&'static str>,
}

impl InstallGate {
    /// Compile-time release identity is the first gate. Runtime verification
    /// still checks the running bundle, downloaded bundle, notarization, and
    /// active run set before `Available` can be reached.
    pub fn current() -> Self {
        let missing = ReleaseConfig::compiled().err().unwrap_or_default();
        Self { missing }
    }

    pub fn for_config(config: Option<&ReleaseConfig>) -> Self {
        if config.is_some() {
            Self { missing: vec![] }
        } else {
            Self {
                missing: vec![
                    "AUTOHARNESS_RELEASE_FEED_URL",
                    "AUTOHARNESS_RELEASE_TEAM_ID",
                ],
            }
        }
    }

    pub fn allowed(&self) -> bool {
        self.missing.is_empty()
    }

    pub fn reason(&self) -> String {
        if self.allowed() {
            "installation unlocks only after runtime signature checks".into()
        } else {
            format!("installation needs {}", self.missing.join(", "))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeInspector {
        bundle_id: String,
        bundle_version: String,
        codesign: Result<(), String>,
        team_id: String,
        gatekeeper: Result<(), String>,
    }

    impl Default for FakeInspector {
        fn default() -> Self {
            Self {
                bundle_id: EXPECTED_BUNDLE_ID.into(),
                bundle_version: "0.2.0".into(),
                codesign: Ok(()),
                team_id: EXPECTED_TEAM_ID.into(),
                gatekeeper: Ok(()),
            }
        }
    }

    impl BundleInspector for FakeInspector {
        fn bundle_id(&self, _path: &str) -> Result<String, String> {
            Ok(self.bundle_id.clone())
        }
        fn bundle_version(&self, _path: &str) -> Result<String, String> {
            Ok(self.bundle_version.clone())
        }
        fn codesign_verify(&self, _path: &str) -> Result<(), String> {
            self.codesign.clone()
        }
        fn team_id(&self, _path: &str) -> Result<String, String> {
            Ok(self.team_id.clone())
        }
        fn gatekeeper_assess(&self, _path: &str) -> Result<(), String> {
            self.gatekeeper.clone()
        }
    }

    const ASSET: &[u8] = b"pretend this is AutoHarness.app";

    fn feed_body() -> String {
        serde_json::json!({
            "releases": [{
                "version": "0.2.0",
                "minimum_macos": "14.0",
                "url": "https://releases.autoharness.dev/stable/AutoHarness-0.2.0.zip",
                "sha256": sha256_hex(ASSET),
                "bundle_id": EXPECTED_BUNDLE_ID,
                "team_id": EXPECTED_TEAM_ID,
            }]
        })
        .to_string()
    }

    fn facts() -> HostFacts {
        HostFacts {
            current_version: "0.1.0".into(),
            macos_version: "15.2".into(),
            active_runs: 0,
            signed_release_build: true,
        }
    }

    fn feed_url() -> String {
        format!("https://{FEED_HOST}{FEED_PATH}")
    }

    fn run(inspector: &dyn BundleInspector, facts: &HostFacts) -> UpdateStatus {
        evaluate(
            &feed_url(),
            &feed_body(),
            ASSET,
            "/tmp/AutoHarness.app",
            inspector,
            facts,
        )
    }

    #[test]
    fn a_fully_verified_release_is_offered_and_install_requires_release_identity() {
        let status = run(&FakeInspector::default(), &facts());
        assert_eq!(
            status,
            UpdateStatus::Available {
                version: "0.2.0".into(),
                url: "https://releases.autoharness.dev/stable/AutoHarness-0.2.0.zip".into(),
            }
        );
        let gate = InstallGate::for_config(None);
        assert!(!gate.allowed());
        assert!(gate.reason().contains("AUTOHARNESS_RELEASE_FEED_URL"));
        assert!(
            InstallGate::for_config(Some(&ReleaseConfig {
                feed_url: feed_url(),
                team_id: EXPECTED_TEAM_ID.into(),
            }))
            .allowed()
        );
    }

    /// A development build says so instead of offering a button that cannot
    /// work. This is the state every build is in today.
    #[test]
    fn a_development_build_reports_updates_unavailable() {
        let facts = HostFacts {
            signed_release_build: false,
            ..facts()
        };
        assert!(matches!(
            run(&FakeInspector::default(), &facts),
            UpdateStatus::UnavailableForThisBuild(_)
        ));
    }

    #[test]
    fn only_the_pinned_https_host_and_path_are_accepted() {
        assert_eq!(check_feed_url(&feed_url()), Ok(()));
        assert_eq!(
            check_feed_url("http://releases.autoharness.dev/stable/appcast.json"),
            Err(Blocker::FeedNotHttps)
        );
        assert_eq!(
            check_feed_url("https://evil.example/stable/appcast.json"),
            Err(Blocker::FeedHostNotPinned)
        );
        // The pinned host appearing in the PATH of another host is the attack
        // a substring check would wave through.
        assert_eq!(
            check_feed_url("https://evil.example/releases.autoharness.dev/stable/appcast.json"),
            Err(Blocker::FeedHostNotPinned)
        );
        // Credentials cannot smuggle a different authority past the check.
        assert_eq!(
            check_feed_url("https://releases.autoharness.dev@evil.example/stable/appcast.json"),
            Err(Blocker::FeedHostNotPinned)
        );
        assert_eq!(
            check_feed_url("https://releases.autoharness.dev/beta/appcast.json"),
            Err(Blocker::FeedPathNotPinned)
        );
    }

    #[test]
    fn release_assets_must_be_https_same_origin_zip_archives() {
        let feed = feed_url();
        assert_eq!(
            check_asset_url(
                &feed,
                "https://releases.autoharness.dev/stable/AutoHarness-1.2.3.zip"
            ),
            Ok(())
        );
        for url in [
            "http://releases.autoharness.dev/stable/AutoHarness.zip",
            "https://evil.example/stable/AutoHarness.zip",
        ] {
            assert_eq!(
                check_asset_url(&feed, url),
                Err(Blocker::AssetOriginNotPinned)
            );
        }
        assert_eq!(
            check_asset_url(
                &feed,
                "https://releases.autoharness.dev/stable/AutoHarness.dmg"
            ),
            Err(Blocker::AssetNotZip)
        );
    }

    #[test]
    fn curl_fetch_is_config_free_https_only_and_never_follows_redirects() {
        let destination = Path::new("/private/tmp/update.zip");
        let arguments = curl_arguments(
            "https://releases.autoharness.dev/stable/AutoHarness.zip",
            42,
            Some(destination),
        );
        let text: Vec<_> = arguments
            .iter()
            .map(|value| value.to_string_lossy().to_string())
            .collect();
        assert_eq!(text.first().map(String::as_str), Some("-q"));
        assert!(text.windows(2).any(|pair| pair == ["--proto", "=https"]));
        assert!(text.windows(2).any(|pair| pair == ["--max-redirs", "0"]));
        assert!(!text.iter().any(|argument| argument == "-L"));
        assert_eq!(
            text.last().unwrap(),
            "https://releases.autoharness.dev/stable/AutoHarness.zip"
        );
        assert!(
            text.iter()
                .any(|argument| argument == destination.to_str().unwrap())
        );
    }

    #[test]
    fn latest_stable_release_is_selected_numerically_not_by_feed_order() {
        let release = |version: &str| FeedRelease {
            version: version.into(),
            minimum_macos: "14.0".into(),
            url: format!("https://releases.autoharness.dev/stable/{version}.zip"),
            sha256: "00".repeat(32),
            bundle_id: EXPECTED_BUNDLE_ID.into(),
            team_id: EXPECTED_TEAM_ID.into(),
        };
        let feed = Feed {
            releases: vec![release("0.9.0"), release("0.10.0"), release("1.0.0-beta.1")],
        };
        assert_eq!(
            latest_stable_release(&feed).unwrap().unwrap().version,
            "0.10.0"
        );
    }

    #[test]
    fn staging_is_private_and_accepts_exactly_one_symlink_free_app() {
        use std::os::unix::fs::PermissionsExt as _;

        let temp = tempfile::tempdir().unwrap();
        let stage = create_staging_root(temp.path()).unwrap();
        assert_eq!(
            fs::metadata(&stage).unwrap().permissions().mode() & 0o777,
            0o700
        );
        let extracted = stage.join("extracted");
        fs::create_dir(&extracted).unwrap();
        let bundle = extracted.join(autoharness_updater::APP_NAME);
        fs::create_dir(&bundle).unwrap();
        assert_eq!(
            single_staged_bundle(&extracted).unwrap(),
            bundle.canonicalize().unwrap()
        );

        std::os::unix::fs::symlink("missing", bundle.join("link")).unwrap();
        assert!(single_staged_bundle(&extracted).is_err());
    }

    #[test]
    fn malformed_compile_time_release_identity_is_refused() {
        for team_id in ["", "lowercase1", "TOO-LONG-TEAM"] {
            let config = ReleaseConfig {
                feed_url: feed_url(),
                team_id: team_id.into(),
            };
            assert!(matches!(
                config.validate(),
                Err(Blocker::ReleaseConfiguration(_))
            ));
        }
    }

    #[test]
    fn a_tampered_download_is_refused_before_anything_inspects_it() {
        let status = evaluate(
            &feed_url(),
            &feed_body(),
            b"a different file entirely",
            "/tmp/AutoHarness.app",
            &FakeInspector::default(),
            &facts(),
        );
        assert_eq!(status, UpdateStatus::Blocked(Blocker::DigestMismatch));
    }

    #[test]
    fn every_signature_check_can_refuse_on_its_own() {
        for (inspector, expected) in [
            (
                FakeInspector {
                    bundle_id: "com.someone.else".into(),
                    ..Default::default()
                },
                Blocker::BundleIdMismatch,
            ),
            (
                FakeInspector {
                    bundle_version: "9.9.9".into(),
                    ..Default::default()
                },
                Blocker::BundleVersionMismatch,
            ),
            (
                FakeInspector {
                    codesign: Err("code object is not signed at all".into()),
                    ..Default::default()
                },
                Blocker::CodesignRejected("code object is not signed at all".into()),
            ),
            (
                FakeInspector {
                    team_id: "AAAAAAAAAA".into(),
                    ..Default::default()
                },
                Blocker::TeamIdMismatch {
                    expected: EXPECTED_TEAM_ID.into(),
                    found: "AAAAAAAAAA".into(),
                },
            ),
            (
                FakeInspector {
                    gatekeeper: Err("rejected source=no usable signature".into()),
                    ..Default::default()
                },
                Blocker::GatekeeperRejected("rejected source=no usable signature".into()),
            ),
        ] {
            assert_eq!(
                run(&inspector, &facts()),
                UpdateStatus::Blocked(expected.clone()),
                "{expected:?} must refuse on its own"
            );
        }
    }

    #[test]
    fn an_older_or_equal_release_is_never_offered() {
        for current in ["0.2.0", "0.3.0", "1.0.0"] {
            let facts = HostFacts {
                current_version: current.into(),
                ..facts()
            };
            assert_eq!(
                run(&FakeInspector::default(), &facts),
                UpdateStatus::UpToDate
            );
        }
    }

    #[test]
    fn a_machine_below_the_minimum_macos_is_told_why() {
        let facts = HostFacts {
            macos_version: "13.6".into(),
            ..facts()
        };
        assert_eq!(
            run(&FakeInspector::default(), &facts),
            UpdateStatus::Blocked(Blocker::MacosTooOld {
                needs: "14.0".into(),
                found: "13.6".into(),
            })
        );
        // Numeric, not lexical: 9.10 is above 9.9.
        assert!(at_least("14.10", "14.9"));
        assert!(!at_least("14.9", "14.10"));
        assert!(at_least("15.0", "15"));
    }

    /// A restart would kill live agent work, so a verified update still waits.
    #[test]
    fn an_active_run_blocks_the_update_even_after_full_verification() {
        let facts = HostFacts {
            active_runs: 2,
            ..facts()
        };
        assert_eq!(
            run(&FakeInspector::default(), &facts),
            UpdateStatus::Blocked(Blocker::RunsActive(2))
        );
    }

    #[test]
    fn an_unreadable_feed_is_reported_rather_than_ignored() {
        let status = evaluate(
            &feed_url(),
            "{ this is not json",
            ASSET,
            "/tmp/AutoHarness.app",
            &FakeInspector::default(),
            &facts(),
        );
        assert!(matches!(
            status,
            UpdateStatus::Blocked(Blocker::FeedUnreadable(_))
        ));
    }

    #[test]
    fn the_digest_is_a_real_sha256() {
        // Known-answer test: SHA-256 of the empty string.
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
