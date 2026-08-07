//! Always-current Parquet table provider.
//!
//! The cold half of a topic's SQL table is a set of Parquet segments that the
//! WalIndexer keeps appending to. Registering that set once — as a fixed file
//! list — freezes the table at its registration moment: every segment written
//! afterwards is invisible to SQL, so `COUNT(*)` silently under-reports and
//! never converges (issue #19).
//!
//! [`LiveParquetTableProvider`] is registered **once** per topic and re-resolves
//! the file set at scan time, throttled by a refresh interval. The DataFusion
//! catalog entry (and any VIEW built on it) therefore stays valid while the data
//! underneath it grows.

use std::fmt;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use datafusion::arrow::datatypes::SchemaRef;
use datafusion::catalog::Session;
use datafusion::datasource::file_format::parquet::ParquetFormat;
use datafusion::datasource::listing::{
    ListingOptions, ListingTable, ListingTableConfig, ListingTableUrl,
};
use datafusion::datasource::{TableProvider, TableType};
use datafusion::error::{DataFusionError, Result as DFResult};
use datafusion::logical_expr::{Expr, TableProviderFilterPushDown};
use datafusion::physical_plan::memory::MemoryExec;
use datafusion::physical_plan::ExecutionPlan;
use tokio::sync::RwLock;
use tracing::{debug, warn};

/// Default interval between file-set re-resolutions, in milliseconds.
pub const DEFAULT_PARQUET_REFRESH_MS: u64 = 1_000;

/// Resolves the current Parquet file paths backing one topic.
///
/// Implemented by the server (metadata store + filesystem fallback); kept as a
/// trait here so `chronik-columnar` stays independent of the metadata layer.
#[async_trait::async_trait]
pub trait ParquetPathSource: Send + Sync + fmt::Debug {
    /// Current Parquet paths, in a stable order. Returning an empty list means
    /// "no cold data right now" and is not an error.
    async fn parquet_paths(&self) -> Vec<String>;
}

/// A fixed list of paths — useful for tests and for callers that manage
/// refresh themselves.
#[derive(Debug)]
pub struct StaticParquetPaths(pub Vec<String>);

#[async_trait::async_trait]
impl ParquetPathSource for StaticParquetPaths {
    async fn parquet_paths(&self) -> Vec<String> {
        self.0.clone()
    }
}

/// Cached listing state, rebuilt only when the resolved path set changes.
struct Listing {
    paths: Vec<String>,
    table: Option<Arc<ListingTable>>,
    last_refresh_ms: u64,
}

/// See module docs.
pub struct LiveParquetTableProvider {
    source: Arc<dyn ParquetPathSource>,
    /// Schema inferred once at registration. Parquet segments of a topic share
    /// a schema; inferring per refresh would let a late-arriving file silently
    /// change the table's shape underneath already-planned views.
    schema: SchemaRef,
    refresh_interval_ms: u64,
    listing: RwLock<Listing>,
}

impl fmt::Debug for LiveParquetTableProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LiveParquetTableProvider")
            .field("source", &self.source)
            .field("refresh_interval_ms", &self.refresh_interval_ms)
            .finish()
    }
}

impl LiveParquetTableProvider {
    /// Create a provider with an already-inferred schema.
    pub fn new(
        source: Arc<dyn ParquetPathSource>,
        schema: SchemaRef,
        refresh_interval_ms: u64,
    ) -> Self {
        Self {
            source,
            schema,
            refresh_interval_ms,
            listing: RwLock::new(Listing {
                paths: Vec::new(),
                table: None,
                last_refresh_ms: 0,
            }),
        }
    }

    fn now_ms() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }

    fn listing_options() -> ListingOptions {
        ListingOptions::new(Arc::new(ParquetFormat::default())).with_file_extension(".parquet")
    }

    /// Build a `ListingTable` over an explicit file list using the fixed schema.
    fn build_table(&self, paths: &[String]) -> DFResult<Option<Arc<ListingTable>>> {
        if paths.is_empty() {
            return Ok(None);
        }

        let mut urls = Vec::with_capacity(paths.len());
        for path in paths {
            match ListingTableUrl::parse(path) {
                Ok(url) => urls.push(url),
                Err(e) => {
                    // One unusable path must not take down the whole table.
                    warn!("Skipping unparseable Parquet path '{}': {}", path, e);
                }
            }
        }
        if urls.is_empty() {
            return Ok(None);
        }

        let config = ListingTableConfig::new_with_multi_paths(urls)
            .with_listing_options(Self::listing_options())
            .with_schema(self.schema.clone());

        Ok(Some(Arc::new(ListingTable::try_new(config)?)))
    }

    /// Return the current listing table, re-resolving the path set if the
    /// refresh interval has elapsed.
    async fn current_table(&self) -> DFResult<Option<Arc<ListingTable>>> {
        let now = Self::now_ms();

        {
            let listing = self.listing.read().await;
            if listing.last_refresh_ms != 0
                && now.saturating_sub(listing.last_refresh_ms) < self.refresh_interval_ms
            {
                return Ok(listing.table.clone());
            }
        }

        let paths = self.source.parquet_paths().await;

        let mut listing = self.listing.write().await;
        // Another task may have refreshed while we were resolving.
        if listing.last_refresh_ms != 0
            && Self::now_ms().saturating_sub(listing.last_refresh_ms) < self.refresh_interval_ms
            && listing.paths == paths
        {
            return Ok(listing.table.clone());
        }

        if listing.table.is_some() && listing.paths == paths {
            listing.last_refresh_ms = now;
            return Ok(listing.table.clone());
        }

        let table = self.build_table(&paths)?;
        debug!(
            "Refreshed live Parquet listing: {} files (was {})",
            paths.len(),
            listing.paths.len()
        );
        listing.paths = paths;
        listing.table = table.clone();
        listing.last_refresh_ms = now;

        Ok(table)
    }

    /// Number of files currently backing the table (after the last refresh).
    pub async fn file_count(&self) -> usize {
        self.listing.read().await.paths.len()
    }
}

#[async_trait::async_trait]
impl TableProvider for LiveParquetTableProvider {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn schema(&self) -> SchemaRef {
        self.schema.clone()
    }

    fn table_type(&self) -> TableType {
        TableType::Base
    }

    async fn scan(
        &self,
        state: &dyn Session,
        projection: Option<&Vec<usize>>,
        filters: &[Expr],
        limit: Option<usize>,
    ) -> DFResult<Arc<dyn ExecutionPlan>> {
        match self.current_table().await? {
            Some(table) => table.scan(state, projection, filters, limit).await,
            None => MemoryExec::try_new(&[vec![]], self.schema.clone(), projection.cloned())
                .map(|e| Arc::new(e) as Arc<dyn ExecutionPlan>)
                .map_err(DataFusionError::from),
        }
    }

    fn supports_filters_pushdown(
        &self,
        filters: &[&Expr],
    ) -> DFResult<Vec<TableProviderFilterPushDown>> {
        // Mirror ListingTable: predicates are re-applied after the scan, so
        // Inexact is always safe (and correct for the empty case).
        Ok(filters
            .iter()
            .map(|_| TableProviderFilterPushDown::Inexact)
            .collect())
    }
}
