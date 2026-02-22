use tracing_subscriber::{fmt, EnvFilter};

/// Initialize tracing with the given log level.
///
/// Uses `RUST_LOG` env var if set, otherwise falls back to the provided level.
pub fn init(log_level: &str) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(log_level));

    fmt()
        .with_env_filter(filter)
        .with_target(true)
        .with_thread_ids(false)
        .with_file(false)
        .with_line_number(false)
        .init();
}
