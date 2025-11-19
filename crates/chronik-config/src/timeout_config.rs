//! Timeout Configuration for I/O Operations (Phase 0 - P0.1)
//!
//! This module provides centralized timeout configuration for all I/O operations
//! in Chronik to prevent indefinite hangs and improve system reliability.
//!
//! ## Design Philosophy
//!
//! Timeouts are applied at different layers based on their nature:
//!
//! 1. **Network I/O**: Hard timeouts (connections can be dropped)
//! 2. **Disk I/O**: Soft timeouts (alerts, but can't cancel disk operations)
//! 3. **Lock Acquisitions**: Hard timeouts (prevent deadlocks)
//! 4. **Raft Operations**: Hard timeouts (prevent stuck consensus)
//!
//! ## Configuration via Environment Variables
//!
//! ```bash
//! # Network timeouts
//! CHRONIK_NETWORK_CONNECT_TIMEOUT_SECS=5
//! CHRONIK_NETWORK_READ_TIMEOUT_SECS=60
//! CHRONIK_NETWORK_WRITE_TIMEOUT_SECS=30
//!
//! # Disk I/O timeouts (soft - warning only)
//! CHRONIK_DISK_WRITE_TIMEOUT_SECS=30
//! CHRONIK_DISK_SYNC_TIMEOUT_SECS=60
//!
//! # Raft operation timeouts
//! CHRONIK_RAFT_LOCK_TIMEOUT_SECS=5
//! CHRONIK_RAFT_PROPOSAL_TIMEOUT_SECS=10
//!
//! # Client operation timeouts
//! CHRONIK_CLIENT_READ_TIMEOUT_SECS=30
//! CHRONIK_CLIENT_WRITE_TIMEOUT_SECS=30
//! ```

use std::time::Duration;
use serde::{Deserialize, Serialize};

/// Centralized timeout configuration for all I/O operations
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimeoutConfig {
    /// Network operation timeouts
    pub network: NetworkTimeoutConfig,

    /// Disk I/O operation timeouts
    pub disk: DiskTimeoutConfig,

    /// Raft operation timeouts
    pub raft: RaftTimeoutConfig,

    /// Client connection timeouts
    pub client: ClientTimeoutConfig,
}

/// Network operation timeouts
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkTimeoutConfig {
    /// TCP connection establishment timeout (default: 5s)
    pub connect_timeout_secs: u64,

    /// TCP read operation timeout (default: 60s)
    pub read_timeout_secs: u64,

    /// TCP write operation timeout (default: 30s)
    pub write_timeout_secs: u64,

    /// Frame completion timeout (default: 30s)
    pub frame_timeout_secs: u64,
}

/// Disk I/O operation timeouts
///
/// NOTE: Disk operations cannot be truly cancelled once started.
/// These timeouts serve as **alerts** to detect slow/failing disks,
/// not as hard limits. The operation will continue in the background.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiskTimeoutConfig {
    /// Disk write operation timeout (default: 30s)
    /// WARNING: Timeout does not cancel write, only alerts
    pub write_timeout_secs: u64,

    /// Disk fsync operation timeout (default: 60s)
    /// WARNING: Timeout does not cancel fsync, only alerts
    pub sync_timeout_secs: u64,

    /// Whether to fail the operation on timeout (default: false)
    /// If false, log warning and continue waiting
    /// If true, return error (may cause data loss!)
    pub fail_on_timeout: bool,
}

/// Raft operation timeouts
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RaftTimeoutConfig {
    /// Raft lock acquisition timeout (default: 5s)
    /// Prevents deadlocks in metadata operations
    pub lock_timeout_secs: u64,

    /// Raft proposal timeout (default: 10s)
    /// Maximum time to wait for proposal to be committed
    pub proposal_timeout_secs: u64,

    /// Raft apply timeout (default: 5s)
    /// Maximum time to wait for state machine application
    pub apply_timeout_secs: u64,
}

/// Client connection timeouts
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientTimeoutConfig {
    /// Client read timeout (default: 30s)
    pub read_timeout_secs: u64,

    /// Client write timeout (default: 30s)
    pub write_timeout_secs: u64,

    /// Request processing timeout (default: 60s)
    pub request_timeout_secs: u64,
}

impl Default for TimeoutConfig {
    fn default() -> Self {
        Self {
            network: NetworkTimeoutConfig::default(),
            disk: DiskTimeoutConfig::default(),
            raft: RaftTimeoutConfig::default(),
            client: ClientTimeoutConfig::default(),
        }
    }
}

impl Default for NetworkTimeoutConfig {
    fn default() -> Self {
        Self {
            connect_timeout_secs: 5,
            read_timeout_secs: 60,
            write_timeout_secs: 30,
            frame_timeout_secs: 30,
        }
    }
}

impl Default for DiskTimeoutConfig {
    fn default() -> Self {
        Self {
            write_timeout_secs: 30,
            sync_timeout_secs: 60,
            fail_on_timeout: false, // Safe default - don't fail, just warn
        }
    }
}

impl Default for RaftTimeoutConfig {
    fn default() -> Self {
        Self {
            lock_timeout_secs: 5,
            proposal_timeout_secs: 10,
            apply_timeout_secs: 5,
        }
    }
}

impl Default for ClientTimeoutConfig {
    fn default() -> Self {
        Self {
            read_timeout_secs: 30,
            write_timeout_secs: 30,
            request_timeout_secs: 60,
        }
    }
}

impl TimeoutConfig {
    /// Load timeout configuration from environment variables
    ///
    /// Falls back to defaults if environment variables are not set.
    pub fn from_env() -> Self {
        Self {
            network: NetworkTimeoutConfig::from_env(),
            disk: DiskTimeoutConfig::from_env(),
            raft: RaftTimeoutConfig::from_env(),
            client: ClientTimeoutConfig::from_env(),
        }
    }
}

impl NetworkTimeoutConfig {
    /// Load from environment variables
    pub fn from_env() -> Self {
        Self {
            connect_timeout_secs: std::env::var("CHRONIK_NETWORK_CONNECT_TIMEOUT_SECS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(5),
            read_timeout_secs: std::env::var("CHRONIK_NETWORK_READ_TIMEOUT_SECS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(60),
            write_timeout_secs: std::env::var("CHRONIK_NETWORK_WRITE_TIMEOUT_SECS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(30),
            frame_timeout_secs: std::env::var("CHRONIK_NETWORK_FRAME_TIMEOUT_SECS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(30),
        }
    }

    /// Convert to Duration for connect timeout
    pub fn connect_timeout(&self) -> Duration {
        Duration::from_secs(self.connect_timeout_secs)
    }

    /// Convert to Duration for read timeout
    pub fn read_timeout(&self) -> Duration {
        Duration::from_secs(self.read_timeout_secs)
    }

    /// Convert to Duration for write timeout
    pub fn write_timeout(&self) -> Duration {
        Duration::from_secs(self.write_timeout_secs)
    }

    /// Convert to Duration for frame timeout
    pub fn frame_timeout(&self) -> Duration {
        Duration::from_secs(self.frame_timeout_secs)
    }
}

impl DiskTimeoutConfig {
    /// Load from environment variables
    pub fn from_env() -> Self {
        Self {
            write_timeout_secs: std::env::var("CHRONIK_DISK_WRITE_TIMEOUT_SECS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(30),
            sync_timeout_secs: std::env::var("CHRONIK_DISK_SYNC_TIMEOUT_SECS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(60),
            fail_on_timeout: std::env::var("CHRONIK_DISK_FAIL_ON_TIMEOUT")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(false),
        }
    }

    /// Convert to Duration for write timeout
    pub fn write_timeout(&self) -> Duration {
        Duration::from_secs(self.write_timeout_secs)
    }

    /// Convert to Duration for sync timeout
    pub fn sync_timeout(&self) -> Duration {
        Duration::from_secs(self.sync_timeout_secs)
    }
}

impl RaftTimeoutConfig {
    /// Load from environment variables
    pub fn from_env() -> Self {
        Self {
            lock_timeout_secs: std::env::var("CHRONIK_RAFT_LOCK_TIMEOUT_SECS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(5),
            proposal_timeout_secs: std::env::var("CHRONIK_RAFT_PROPOSAL_TIMEOUT_SECS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(10),
            apply_timeout_secs: std::env::var("CHRONIK_RAFT_APPLY_TIMEOUT_SECS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(5),
        }
    }

    /// Convert to Duration for lock timeout
    pub fn lock_timeout(&self) -> Duration {
        Duration::from_secs(self.lock_timeout_secs)
    }

    /// Convert to Duration for proposal timeout
    pub fn proposal_timeout(&self) -> Duration {
        Duration::from_secs(self.proposal_timeout_secs)
    }

    /// Convert to Duration for apply timeout
    pub fn apply_timeout(&self) -> Duration {
        Duration::from_secs(self.apply_timeout_secs)
    }
}

impl ClientTimeoutConfig {
    /// Load from environment variables
    pub fn from_env() -> Self {
        Self {
            read_timeout_secs: std::env::var("CHRONIK_CLIENT_READ_TIMEOUT_SECS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(30),
            write_timeout_secs: std::env::var("CHRONIK_CLIENT_WRITE_TIMEOUT_SECS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(30),
            request_timeout_secs: std::env::var("CHRONIK_CLIENT_REQUEST_TIMEOUT_SECS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(60),
        }
    }

    /// Convert to Duration for read timeout
    pub fn read_timeout(&self) -> Duration {
        Duration::from_secs(self.read_timeout_secs)
    }

    /// Convert to Duration for write timeout
    pub fn write_timeout(&self) -> Duration {
        Duration::from_secs(self.write_timeout_secs)
    }

    /// Convert to Duration for request timeout
    pub fn request_timeout(&self) -> Duration {
        Duration::from_secs(self.request_timeout_secs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_timeout_config() {
        let config = TimeoutConfig::default();

        // Network
        assert_eq!(config.network.connect_timeout_secs, 5);
        assert_eq!(config.network.read_timeout_secs, 60);
        assert_eq!(config.network.write_timeout_secs, 30);

        // Disk
        assert_eq!(config.disk.write_timeout_secs, 30);
        assert_eq!(config.disk.sync_timeout_secs, 60);
        assert_eq!(config.disk.fail_on_timeout, false);

        // Raft
        assert_eq!(config.raft.lock_timeout_secs, 5);
        assert_eq!(config.raft.proposal_timeout_secs, 10);
        assert_eq!(config.raft.apply_timeout_secs, 5);

        // Client
        assert_eq!(config.client.read_timeout_secs, 30);
        assert_eq!(config.client.write_timeout_secs, 30);
        assert_eq!(config.client.request_timeout_secs, 60);
    }

    #[test]
    fn test_duration_conversion() {
        let config = TimeoutConfig::default();

        assert_eq!(config.network.connect_timeout(), Duration::from_secs(5));
        assert_eq!(config.disk.write_timeout(), Duration::from_secs(30));
        assert_eq!(config.raft.lock_timeout(), Duration::from_secs(5));
        assert_eq!(config.client.read_timeout(), Duration::from_secs(30));
    }
}
