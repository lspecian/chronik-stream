//! Admin API for Chronik Stream.

pub mod api;
pub mod config;
pub mod error;
pub mod handlers;
pub mod state;

pub use config::AdminConfig;
pub use error::{AdminError, AdminResult};
pub use state::AppState;