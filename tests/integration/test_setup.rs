//! Test setup and initialization utilities

use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
use std::sync::Once;

static INIT: Once = Once::new();

/// Initialize test environment (logging, etc.)
pub fn init() {
    INIT.call_once(|| {
        // Set up logging for tests
        let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn,chronik=debug"));
        
        tracing_subscriber::registry()
            .with(env_filter)
            .with(tracing_subscriber::fmt::layer())
            .try_init()
            .ok();
        
        // Set test environment variables if needed
        std::env::set_var("RUST_BACKTRACE", "1");
    });
}