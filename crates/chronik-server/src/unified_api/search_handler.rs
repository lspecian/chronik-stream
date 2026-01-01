//! Search Handler for Unified API
//!
//! Integrates the existing chronik-search API into the unified API.
//! This module provides backward-compatible Elasticsearch-style search endpoints:
//! - POST/GET `/_search` - Search across all indices
//! - POST/GET `/:index/_search` - Search specific index
//! - `/:index/_doc/:id` - Document CRUD operations
//! - `/:index` - Index management
//! - `/_cat/indices` - List indices
//!
//! All existing search API functionality is preserved for backward compatibility.

#[cfg(feature = "search")]
use std::sync::Arc;

#[cfg(feature = "search")]
use chronik_search::SearchApi;

#[cfg(feature = "search")]
use chronik_storage::WalIndexer;

/// Create a SearchApi instance for the unified API
///
/// This function creates a SearchApi configured with the WalIndexer
/// for accessing WAL-created Tantivy indexes.
///
/// # Arguments
/// * `wal_indexer` - The WAL indexer for accessing disk-based indexes
/// * `index_base_path` - Full path to tantivy_indexes directory (e.g., "{data_dir}/tantivy_indexes")
///
/// # Returns
/// An Arc-wrapped SearchApi ready for use with Axum
///
/// # Errors
/// Returns an error if the SearchApi cannot be created
#[cfg(feature = "search")]
pub fn create_search_api(
    wal_indexer: Arc<WalIndexer>,
    index_base_path: &str,
) -> Result<Arc<SearchApi>, chronik_common::Error> {
    // Note: index_base_path is already the full path to tantivy_indexes from main.rs
    // The handler will also search "{base_path}" with "tantivy_indexes" replaced by "index"
    // to find RealtimeIndexer's indices
    Ok(Arc::new(SearchApi::new_with_wal_indexer(wal_indexer, index_base_path.to_string())?))
}

/// Get the search API router for mounting in the unified API
///
/// Returns the search API router without /health endpoint
/// (since the unified API provides its own /health).
#[cfg(feature = "search")]
pub fn search_router(search_api: Arc<SearchApi>) -> axum::Router {
    search_api.router_for_embedding()
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_module_compiles() {
        // Basic compile test
        assert!(true);
    }
}
