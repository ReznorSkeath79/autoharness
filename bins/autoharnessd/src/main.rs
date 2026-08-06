//! AutoHarness daemon entry point.

use autoharness_daemon::{Daemon, DaemonConfig};

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "autoharness_daemon=info".into()),
        )
        .init();

    let config = DaemonConfig::default_paths();
    let daemon = match Daemon::new(config) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("autoharnessd: failed to initialize: {e}");
            std::process::exit(1);
        }
    };
    if let Err(e) = daemon.run().await {
        eprintln!("autoharnessd: {e}");
        std::process::exit(1);
    }
}
