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

use datafusion::arrow::array::{new_null_array, ArrayRef, RecordBatch, RecordBatchOptions};
use datafusion::arrow::compute::{can_cast_types, cast};
use datafusion::arrow::datatypes::{Schema, SchemaRef};
use datafusion::datasource::physical_plan::ParquetExec;
use datafusion::datasource::schema_adapter::{SchemaAdapter, SchemaAdapterFactory, SchemaMapper};
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
    /// Derives projected columns for segments written before them.
    adapter: Option<Arc<ProjectingSchemaAdapterFactory>>,
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
    pub fn with_projected_columns(mut self, projected: Vec<crate::projection::ColumnSpec>) -> Self {
        if !projected.is_empty() {
            self.adapter = Some(Arc::new(ProjectingSchemaAdapterFactory::new(projected)));
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
            Some(table) => {
                let plan = table.scan(state, projection, filters, limit).await?;
                match (&self.adapter, plan.as_any().downcast_ref::<ParquetExec>()) {
                    (Some(adapter), Some(exec)) => Ok(Arc::new(
                        exec.clone().with_schema_adapter_factory(adapter.clone() as Arc<dyn SchemaAdapterFactory>),
                    ) as Arc<dyn ExecutionPlan>),
                    _ => Ok(plan),
                }
            }
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

/// Reads projected columns (issue #73) from segments written before the column
/// existed.
///
/// A projected column is written into every segment from the moment it is
/// configured; segments written earlier lack it. Reading those as NULL would
/// make every filter or aggregate on the column quietly short over the topic's
/// history — present and wrong. Instead, for a segment that lacks a projected
/// column, this adapter also reads that segment's `_value` and computes the
/// column exactly as the writer would have (`project_binary_array`). A segment
/// that has the column is read as usual, so only old segments pay for decoding
/// `_value`, and only when the query uses the column.
#[derive(Debug)]
pub struct ProjectingSchemaAdapterFactory {
    projected: Arc<Vec<crate::projection::ColumnSpec>>,
}

impl ProjectingSchemaAdapterFactory {
    pub fn new(projected: Vec<crate::projection::ColumnSpec>) -> Self {
        Self { projected: Arc::new(projected) }
    }
}

impl SchemaAdapterFactory for ProjectingSchemaAdapterFactory {
    fn create(&self, projected_table_schema: SchemaRef, table_schema: SchemaRef) -> Box<dyn SchemaAdapter> {
        Box::new(ProjectingSchemaAdapter {
            projected_table_schema,
            table_schema,
            projected: self.projected.clone(),
        })
    }
}

struct ProjectingSchemaAdapter {
    projected_table_schema: SchemaRef,
    table_schema: SchemaRef,
    projected: Arc<Vec<crate::projection::ColumnSpec>>,
}

impl SchemaAdapter for ProjectingSchemaAdapter {
    fn map_column_index(&self, index: usize, file_schema: &Schema) -> Option<usize> {
        let field = self.projected_table_schema.field(index);
        Some(file_schema.fields.find(field.name())?.0)
    }

    fn map_schema(&self, file_schema: &Schema) -> DFResult<(Arc<dyn SchemaMapper>, Vec<usize>)> {
        let mut projection = Vec::with_capacity(file_schema.fields().len());
        let mut sources = vec![Source::Null; self.projected_table_schema.fields().len()];
        for (file_idx, file_field) in file_schema.fields().iter().enumerate() {
            if let Some((table_idx, table_field)) = self.projected_table_schema.fields().find(file_field.name()) {
                if !can_cast_types(file_field.data_type(), table_field.data_type()) {
                    return Err(DataFusionError::Plan(format!(
                        "Cannot cast file schema field {} of type {:?} to table schema field of type {:?}",
                        file_field.name(),
                        file_field.data_type(),
                        table_field.data_type()
                    )));
                }
                sources[table_idx] = Source::File(projection.len());
                projection.push(file_idx);
            }
        }
        // Projected columns this file lacks: computed from its `_value`.
        let value_idx = file_schema.index_of("_value").ok();
        let mut value_pos = None;
        for (table_idx, field) in self.projected_table_schema.fields().iter().enumerate() {
            if !matches!(sources[table_idx], Source::Null) {
                continue;
            }
            let (Some(spec), Some(file_value)) =
                (self.projected.iter().find(|s| s.name == *field.name()), value_idx)
            else {
                continue;
            };
            let pos = *value_pos.get_or_insert_with(|| {
                projection.iter().position(|&i| i == file_value).unwrap_or_else(|| {
                    projection.push(file_value);
                    projection.len() - 1
                })
            });
            sources[table_idx] = Source::Derived(pos, spec.clone());
        }
        Ok((
            Arc::new(ProjectingSchemaMapper {
                projected_table_schema: self.projected_table_schema.clone(),
                table_schema: self.table_schema.clone(),
                sources,
                projected: self.projected.clone(),
            }),
            projection,
        ))
    }
}

#[derive(Debug, Clone)]
enum Source {
    /// The column at this position of the file projection.
    File(usize),
    /// Computed from `_value` at this position of the file projection.
    Derived(usize, crate::projection::ColumnSpec),
    /// Not in the file and not derivable: NULL.
    Null,
}

#[derive(Debug)]
struct ProjectingSchemaMapper {
    projected_table_schema: SchemaRef,
    table_schema: SchemaRef,
    sources: Vec<Source>,
    projected: Arc<Vec<crate::projection::ColumnSpec>>,
}

impl SchemaMapper for ProjectingSchemaMapper {
    fn map_batch(&self, batch: RecordBatch) -> DFResult<RecordBatch> {
        let rows = batch.num_rows();
        let cols = self
            .projected_table_schema
            .fields()
            .iter()
            .zip(&self.sources)
            .map(|(field, source)| match source {
                Source::File(i) => cast(batch.column(*i), field.data_type()).map_err(DataFusionError::from),
                Source::Derived(i, spec) => derive(batch.column(*i).as_ref(), spec, rows),
                Source::Null => Ok(new_null_array(field.data_type(), rows)),
            })
            .collect::<DFResult<Vec<_>>>()?;
        let options = RecordBatchOptions::new().with_row_count(Some(rows));
        Ok(RecordBatch::try_new_with_options(self.projected_table_schema.clone(), cols, &options)?)
    }

    /// Used by Parquet filter pushdown, on a batch holding whichever file
    /// columns a predicate needs: each is cast to its table type, and a
    /// projected column the file lacks is derived when `_value` is present.
    fn map_partial_batch(&self, batch: RecordBatch) -> DFResult<RecordBatch> {
        let schema = batch.schema();
        let rows = batch.num_rows();
        let mut fields = Vec::new();
        let mut cols = Vec::new();
        for (field, col) in schema.fields().iter().zip(batch.columns()) {
            if let Ok(table_field) = self.table_schema.field_with_name(field.name()) {
                cols.push(cast(col, table_field.data_type())?);
                fields.push(table_field.clone());
            }
        }
        if let Ok(value_idx) = schema.index_of("_value") {
            for spec in self.projected.iter() {
                if schema.index_of(&spec.name).is_ok() {
                    continue;
                }
                if let Ok(table_field) = self.table_schema.field_with_name(&spec.name) {
                    cols.push(derive(batch.column(value_idx).as_ref(), spec, rows)?);
                    fields.push(table_field.clone());
                }
            }
        }
        let options = RecordBatchOptions::new().with_row_count(Some(rows));
        Ok(RecordBatch::try_new_with_options(Arc::new(Schema::new(fields)), cols, &options)?)
    }
}

fn derive(values: &dyn datafusion::arrow::array::Array, spec: &crate::projection::ColumnSpec, rows: usize) -> DFResult<ArrayRef> {
    match crate::udfs::project_binary_array(values, spec) {
        Some(array) => Ok(array),
        None => Ok(new_null_array(&spec.ty.sql_arrow_type(), rows)),
    }
}
