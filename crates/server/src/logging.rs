//! Centralized logging — port of `app/logging_config.py`.
//!
//! Writes a rotating file under `<repo>/logs/app.log` in addition to stdout
//! (systemd journal locally). Python used `RotatingFileHandler` (10MB x5);
//! `tracing-appender` doesn't do size-based rotation, so we use daily rotation
//! to the same directory — close enough for the monitoring use case, and we can
//! revisit if the benchmark deploy needs exact parity.
//!
//! Returns the appender guard, which the caller MUST keep alive for the life of
//! the process — dropping it flushes and stops the background writer.

use std::path::Path;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

pub fn configure(repo_root: &Path) -> std::io::Result<WorkerGuard> {
    let log_dir = repo_root.join("logs");
    std::fs::create_dir_all(&log_dir)?;

    let file_appender = tracing_appender::rolling::daily(&log_dir, "app.log");
    let (file_writer, guard) = tracing_appender::non_blocking(file_appender);

    // Default to INFO like the Python side; RUST_LOG overrides for debugging.
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,tower_http=info"));

    tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer().with_target(true)) // stdout / journal
        .with(
            fmt::layer()
                .with_writer(file_writer)
                .with_ansi(false)
                .with_target(true),
        )
        .init();

    tracing::info!("Logging configured -> {}/app.log (daily rotation)", log_dir.display());
    Ok(guard)
}
