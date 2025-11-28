//! WAL replication handler module
//!
//! Extracted from `handle_connection()` to reduce complexity from 136 to <25 per function.
//! Handles follower WAL replication from leader nodes.
//!
//! **Module Structure:**
//! - `connection_state` - Connection setup with timeout monitoring and election workers
//! - `frame_reader` - Frame reading, header parsing, and heartbeat handling
//! - `record_processor` - WAL record processing (metadata + normal records) and ACK sending
//!
//! **Refactoring Goal:**
//! Extract 233-line monolithic `handle_connection()` into focused, testable modules.

pub mod connection_state;
pub mod frame_reader;
pub mod record_processor;

// Re-export key types for convenience
pub use connection_state::ConnectionState;
pub use frame_reader::{FrameReader, FrameReadResult};
pub use record_processor::RecordProcessor;
