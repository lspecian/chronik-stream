//! Admin Handler for Unified API
//!
//! Integrates the existing admin API and Schema Registry into the unified API.
//! This module provides:
//! - POST `/admin/add-node` - Add new node to cluster
//! - POST `/admin/remove-node` - Remove node from cluster
//! - GET `/admin/status` - Get cluster status
//! - POST `/admin/rebalance` - Trigger partition rebalancing
//! - GET `/admin/health` - Health check (no auth required)
//!
//! Schema Registry (Confluent-compatible):
//! - `/subjects/*` - Subject management
//! - `/schemas/*` - Schema by ID
//! - `/config/*` - Compatibility configuration
//!
//! All existing admin and schema registry functionality is preserved.

use std::sync::Arc;

use axum::Router;
use tracing::{info, warn};

use crate::admin_api::{create_admin_router, AdminApiState};
use crate::isr_tracker::IsrTracker;
use crate::raft_cluster::RaftCluster;
use crate::schema_registry::SchemaRegistry;
use chronik_common::metadata::traits::MetadataStore;

/// Create AdminApiState for the unified API
///
/// This function creates the shared state needed for admin endpoints.
///
/// # Arguments
/// * `raft_cluster` - The Raft cluster for consensus operations
/// * `metadata_store` - The metadata store for topic/partition info
/// * `api_key` - Optional API key (falls back to CHRONIK_ADMIN_API_KEY env var)
/// * `isr_tracker` - Optional ISR tracker for replica status
/// * `schema_registry` - The schema registry for Confluent-compatible schema management
///
/// # Returns
/// An AdminApiState ready for use with the unified API router
pub fn create_admin_state(
    raft_cluster: Arc<RaftCluster>,
    metadata_store: Arc<dyn MetadataStore>,
    api_key: Option<String>,
    isr_tracker: Option<Arc<IsrTracker>>,
    schema_registry: Arc<SchemaRegistry>,
) -> AdminApiState {
    let effective_key = match api_key.or_else(|| std::env::var("CHRONIK_ADMIN_API_KEY").ok()) {
        Some(key) => {
            info!("Unified API: Admin authentication enabled");
            Some(key)
        }
        None => {
            warn!("Unified API: CHRONIK_ADMIN_API_KEY not set - Admin endpoints will run WITHOUT authentication!");
            warn!("This is INSECURE for production. Set CHRONIK_ADMIN_API_KEY to enable auth.");
            None
        }
    };

    AdminApiState {
        raft_cluster,
        metadata_store,
        api_key: effective_key,
        isr_tracker,
        schema_registry,
    }
}

/// Get the admin API router for mounting in the unified API
///
/// Returns the complete admin API router including:
/// - Protected admin routes (require X-API-Key)
/// - Public health endpoint
/// - Schema Registry routes (optional HTTP Basic Auth)
///
/// # Arguments
/// * `state` - The AdminApiState created via `create_admin_state()`
///
/// # Returns
/// An Axum router that can be merged into the unified API router
pub fn admin_router(state: AdminApiState) -> Router {
    create_admin_router(state)
}

/// Convenience function to create state and router in one call
///
/// # Arguments
/// * `raft_cluster` - The Raft cluster for consensus operations
/// * `metadata_store` - The metadata store for topic/partition info
/// * `api_key` - Optional API key (falls back to CHRONIK_ADMIN_API_KEY env var)
/// * `isr_tracker` - Optional ISR tracker for replica status
/// * `schema_registry` - The schema registry for Confluent-compatible schema management
///
/// # Returns
/// An Axum router ready for merging into the unified API
pub fn create_admin_router_for_unified_api(
    raft_cluster: Arc<RaftCluster>,
    metadata_store: Arc<dyn MetadataStore>,
    api_key: Option<String>,
    isr_tracker: Option<Arc<IsrTracker>>,
    schema_registry: Arc<SchemaRegistry>,
) -> Router {
    let state = create_admin_state(
        raft_cluster,
        metadata_store,
        api_key,
        isr_tracker,
        schema_registry,
    );
    admin_router(state)
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_module_compiles() {
        // Basic compile test
        assert!(true);
    }
}
