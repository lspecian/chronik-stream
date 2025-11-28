//! Integrated server module with builder pattern
//!
//! This module contains the IntegratedKafkaServer and its builder.
//! The builder pattern was introduced to reduce complexity from 764 to < 25.

pub mod builder;
mod server;

// Re-export main types
pub use builder::IntegratedKafkaServerBuilder;
pub use server::{IntegratedKafkaServer, IntegratedServerConfig};
