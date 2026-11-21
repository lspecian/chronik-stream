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

use datafusion::arrow::array::{new_null_array, RecordBatch};
use datafusion::arrow::datatypes::{Field, FieldRef, Schema, SchemaRef};
use datafusion::common::tree_node::{Transformed, TransformedResult, TreeNode};
use datafusion::catalog::Session;
use datafusion::datasource::file_format::parquet::ParquetFormat;
use datafusion::datasource::listing::{
    ListingOptions, ListingTable, ListingTableConfig, ListingTableUrl,
};
use datafusion::datasource::{TableProvider, TableType};
use datafusion::error::{DataFusionError, Result as DFResult};
use datafusion::datasource::memory::MemorySourceConfig;
use datafusion::logical_expr::{ColumnarValue, Expr, TableProviderFilterPushDown};
use datafusion::physical_expr::expressions::Column;
use datafusion::physical_expr::PhysicalExpr;
use datafusion::physical_expr_adapter::{
    DefaultPhysicalExprAdapterFactory, PhysicalExprAdapter, PhysicalExprAdapterFactory,
};
use datafusion::physical_plan::ExecutionPlan;
use tokio::sync::RwLock;
use tracing::{debug, warn};

use crate::projection::ColumnSpec;

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
    /// Derives projected columns for segments written before them.
    adapter: Option<Arc<ProjectingExprAdapterFactory>>,
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
            adapter: None,
        }
    }

    /// Derive these projected columns for segments that lack them (issue #73).
    pub fn with_projected_columns(mut self, projected: Vec<ColumnSpec>) -> Self {
        if !projected.is_empty() {
            self.adapter = Some(Arc::new(ProjectingExprAdapterFactory::new(projected)));
        }
        self
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

        let mut config = ListingTableConfig::new_with_multi_paths(urls)
            .with_listing_options(Self::listing_options())
            .with_schema(self.schema.clone());
        if let Some(adapter) = &self.adapter {
            config = config.with_expr_adapter_factory(adapter.clone() as Arc<dyn PhysicalExprAdapterFactory>);
        }

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
            None => MemorySourceConfig::try_new_exec(&[vec![]], self.schema.clone(), projection.cloned())
                .map(|e| e as Arc<dyn ExecutionPlan>),
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

/// Reads projected columns (issue #73) from segments written before the column
/// existed.
///
/// A projected column is written into every segment from the moment it is
/// configured; segments written earlier lack it. Reading those as NULL would
/// make every filter or aggregate on the column quietly short over the topic's
/// history — present and wrong. Instead, for a segment that lacks a projected
/// column, every reference to it (in the projection and in pushed-down
/// predicates alike) is rewritten to compute the column from that segment's
/// `_value`, exactly as the writer would have (`project_binary_array`). A
/// segment that has the column is read as usual, so only old segments pay for
/// decoding `_value`, and only when the query uses the column. Everything else
/// (casts, columns that are simply absent) is left to DataFusion's default
/// adapter.
#[derive(Debug)]
pub struct ProjectingExprAdapterFactory {
    projected: Arc<Vec<ColumnSpec>>,
}

impl ProjectingExprAdapterFactory {
    pub fn new(projected: Vec<ColumnSpec>) -> Self {
        Self { projected: Arc::new(projected) }
    }
}

impl PhysicalExprAdapterFactory for ProjectingExprAdapterFactory {
    fn create(
        &self,
        logical_file_schema: SchemaRef,
        physical_file_schema: SchemaRef,
    ) -> DFResult<Arc<dyn PhysicalExprAdapter>> {
        Ok(Arc::new(ProjectingExprAdapter {
            projected: self.projected.clone(),
            value_idx: physical_file_schema.index_of("_value").ok(),
            physical_file_schema: physical_file_schema.clone(),
            default: DefaultPhysicalExprAdapterFactory.create(logical_file_schema, physical_file_schema)?,
        }))
    }
}

#[derive(Debug)]
struct ProjectingExprAdapter {
    projected: Arc<Vec<ColumnSpec>>,
    physical_file_schema: SchemaRef,
    /// Position of `_value` in this file; without it nothing can be derived.
    value_idx: Option<usize>,
    default: Arc<dyn PhysicalExprAdapter>,
}

impl PhysicalExprAdapter for ProjectingExprAdapter {
    fn rewrite(&self, expr: Arc<dyn PhysicalExpr>) -> DFResult<Arc<dyn PhysicalExpr>> {
        let expr = match self.value_idx {
            None => expr,
            Some(value_idx) => expr
                .transform(|e| {
                    let derived = e.downcast_ref::<Column>().and_then(|col| {
                        if self.physical_file_schema.index_of(col.name()).is_ok() {
                            return None;
                        }
                        let spec = self.projected.iter().find(|s| s.name == col.name())?;
                        Some(Arc::new(DerivedColumnExpr {
                            value: Arc::new(Column::new("_value", value_idx)),
                            spec: spec.clone(),
                        }) as Arc<dyn PhysicalExpr>)
                    });
                    Ok(match derived {
                        Some(d) => Transformed::yes(d),
                        None => Transformed::no(e),
                    })
                })
                .data()?,
        };
        self.default.rewrite(expr)
    }
}

/// A projected column computed from the message value column.
#[derive(Debug, Eq)]
struct DerivedColumnExpr {
    value: Arc<dyn PhysicalExpr>,
    spec: ColumnSpec,
}

impl PartialEq for DerivedColumnExpr {
    fn eq(&self, other: &Self) -> bool {
        self.value.eq(&other.value) && self.spec == other.spec
    }
}

impl std::hash::Hash for DerivedColumnExpr {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.value.hash(state);
        self.spec.hash(state);
    }
}

impl fmt::Display for DerivedColumnExpr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}({})", crate::udfs::json_project_udf_name(self.spec.ty), self.value)
    }
}

impl PhysicalExpr for DerivedColumnExpr {
    fn return_field(&self, _input_schema: &Schema) -> DFResult<FieldRef> {
        Ok(Arc::new(Field::new(&self.spec.name, self.spec.ty.sql_arrow_type(), true)))
    }

    fn evaluate(&self, batch: &RecordBatch) -> DFResult<ColumnarValue> {
        let values = self.value.evaluate(batch)?.into_array(batch.num_rows())?;
        let array = match crate::udfs::project_binary_array(values.as_ref(), &self.spec) {
            Some(array) => array,
            None => new_null_array(&self.spec.ty.sql_arrow_type(), batch.num_rows()),
        };
        Ok(ColumnarValue::Array(array))
    }

    fn children(&self) -> Vec<&Arc<dyn PhysicalExpr>> {
        vec![&self.value]
    }

    fn with_new_children(
        self: Arc<Self>,
        mut children: Vec<Arc<dyn PhysicalExpr>>,
    ) -> DFResult<Arc<dyn PhysicalExpr>> {
        match (children.pop(), children.is_empty()) {
            (Some(value), true) => Ok(Arc::new(DerivedColumnExpr { value, spec: self.spec.clone() })),
            _ => Err(DataFusionError::Internal(format!("{self}: expected exactly one child"))),
        }
    }

    fn fmt_sql(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}
