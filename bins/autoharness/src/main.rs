//! AutoHarness desktop application entry point.

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "autoharness=info".into()),
        )
        .init();
    autoharness_ui_gpui::run();
}
