use anyhow::Result;
use tracing::info;

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "buffr=info".into()),
        )
        .init();

    info!("buffr v{} starting", env!("CARGO_PKG_VERSION"));
    info!("buffr-core v{}", buffr_core::version());

    // TODO: cef_execute_process dispatch (single-binary subprocess mode).
    // TODO: cef_initialize with settings.
    // TODO: create main window + browser host.
    // TODO: install modal keybind handler.
    // TODO: cef_run_message_loop.

    Ok(())
}
