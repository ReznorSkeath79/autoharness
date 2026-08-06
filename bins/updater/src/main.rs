use std::path::PathBuf;
use std::time::Duration;

fn main() {
    if let Err(error) = run() {
        eprintln!("AutoHarness updater refused installation: {error}");
        std::process::exit(1);
    }
}

fn run() -> autoharness_updater::Result<()> {
    let mut args = std::env::args_os().skip(1);
    if args.next().as_deref() != Some(std::ffi::OsStr::new("--request")) {
        return Err(autoharness_updater::InstallError::InvalidRequest(
            "expected --request <path> --token <token>".into(),
        ));
    }
    let request_path = PathBuf::from(args.next().ok_or_else(|| {
        autoharness_updater::InstallError::InvalidRequest("missing request path".into())
    })?);
    if args.next().as_deref() != Some(std::ffi::OsStr::new("--token")) {
        return Err(autoharness_updater::InstallError::InvalidRequest(
            "expected --token after request path".into(),
        ));
    }
    let token = args
        .next()
        .and_then(|value| value.into_string().ok())
        .ok_or_else(|| autoharness_updater::InstallError::InvalidRequest("missing token".into()))?;
    if args.next().is_some() {
        return Err(autoharness_updater::InstallError::InvalidRequest(
            "unexpected trailing arguments".into(),
        ));
    }

    let request = autoharness_updater::load_request(&request_path, &token)?;
    // Consume the capability before waiting or mutating the app. A crash or a
    // later refusal must not leave a reusable install request on disk.
    std::fs::remove_file(&request_path)?;
    autoharness_updater::wait_for_parent(request.parent_pid, 300, Duration::from_millis(100))?;
    autoharness_updater::install(
        &request,
        &autoharness_updater::MacBundleInspector,
        &autoharness_updater::SystemInstallOps,
    )?;
    Ok(())
}
