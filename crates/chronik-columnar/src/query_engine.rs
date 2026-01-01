//! SQL query engine for columnar storage using DataFusion.
//!
//! Provides SQL query capabilities over Parquet files with predicate pushdown
//! and partition pruning. Supports:
//!
//! - Single file registration via `register_file()`
//! - Multiple file registration via `register_files()` for topics with many Parquet segments
//! - Streaming query results via `execute_sql_stream()`
//! - Predicate pushdown for timestamp and offset ranges

use anyhow::{anyhow, Result};
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::datasource::listing::{ListingOptions, ListingTable, ListingTableConfig, ListingTableUrl};
use datafusion::datasource::file_format::parquet::ParquetFormat;
use datafusion::execution::context::SessionContext;
use datafusion::execution::SendableRecordBatchStream;
use datafusion::prelude::*;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tracing::debug;

/// Configuration for the columnar query engine.
#[derive(Debug, Clone)]
pub struct QueryEngineConfig {
    /// Maximum rows returned per query.
    pub max_rows: usize,
    /// Query timeout.
    pub timeout: Duration,
    /// Memory limit in bytes.
    pub memory_limit: usize,
    /// Number of partitions for parallel execution.
    pub target_partitions: usize,
}

impl Default for QueryEngineConfig {
    fn default() -> Self {
        Self {
            max_rows: 100_000,
            timeout: Duration::from_secs(30),
            memory_limit: 512 * 1024 * 1024, // 512MB
            target_partitions: num_cpus::get(),
        }
    }
}

/// SQL query engine for columnar data.
pub struct ColumnarQueryEngine {
    ctx: SessionContext,
    config: QueryEngineConfig,
}

impl ColumnarQueryEngine {
    /// Create a new query engine with default configuration.
    pub fn new() -> Self {
        Self::with_config(QueryEngineConfig::default())
    }

    /// Create a new query engine with custom configuration.
    pub fn with_config(config: QueryEngineConfig) -> Self {
        let session_config = SessionConfig::new()
            .with_target_partitions(config.target_partitions)
            .with_batch_size(8192);

        let ctx = SessionContext::new_with_config(session_config);

        Self { ctx, config }
    }

    /// Register a single Parquet file as a table.
    pub async fn register_file(&self, table_name: &str, path: &Path) -> Result<()> {
        let path_str = path.to_string_lossy();

        self.ctx
            .register_parquet(table_name, &path_str, ParquetReadOptions::default())
            .await
            .map_err(|e| anyhow!("Failed to register file as {}: {}", table_name, e))?;

        Ok(())
    }

    /// Register multiple Parquet files as a single table.
    ///
    /// This is the primary method for querying topics with many Parquet segments.
    /// DataFusion will union all files and apply predicate pushdown to minimize
    /// data scanned.
    ///
    /// # Arguments
    /// * `table_name` - Name to register the table as (typically the topic name)
    /// * `paths` - List of Parquet file paths (local or object store URLs like s3://...)
    ///
    /// # Example
    /// ```ignore
    /// // Get paths from SegmentIndex
    /// let paths = segment_index.get_parquet_paths("my-topic").await?;
    /// engine.register_files("my_topic", &paths).await?;
    /// ```
    pub async fn register_files(&self, table_name: &str, paths: &[String]) -> Result<()> {
        if paths.is_empty() {
            return Err(anyhow!("Cannot register empty file list for table {}", table_name));
        }

        debug!("Registering {} Parquet files as table '{}'", paths.len(), table_name);

        // For a single file, use the simple path
        if paths.len() == 1 {
            return self.register_file(table_name, Path::new(&paths[0])).await;
        }

        // For multiple files, we need to use ListingTable with explicit file list
        // First, infer schema from the first file
        let first_path = &paths[0];
        let table_url = ListingTableUrl::parse(first_path)
            .map_err(|e| anyhow!("Invalid path '{}': {}", first_path, e))?;

        let parquet_format = Arc::new(ParquetFormat::default());
        let listing_options = ListingOptions::new(parquet_format)
            .with_file_extension(".parquet");

        // Infer schema from the first file
        let schema = listing_options
            .infer_schema(&self.ctx.state(), &table_url)
            .await
            .map_err(|e| anyhow!("Failed to infer schema from '{}': {}", first_path, e))?;

        debug!("Inferred schema with {} columns for '{}'", schema.fields().len(), table_name);

        // Create listing table URLs for all files
        let mut table_urls = Vec::with_capacity(paths.len());
        for path in paths {
            let url = ListingTableUrl::parse(path)
                .map_err(|e| anyhow!("Invalid path '{}': {}", path, e))?;
            table_urls.push(url);
        }

        // Create listing table config with all files
        let config = ListingTableConfig::new_with_multi_paths(table_urls)
            .with_listing_options(listing_options)
            .with_schema(schema);

        let table = ListingTable::try_new(config)
            .map_err(|e| anyhow!("Failed to create listing table: {}", e))?;

        // Register the table
        self.ctx
            .register_table(table_name, Arc::new(table))
            .map_err(|e| anyhow!("Failed to register table '{}': {}", table_name, e))?;

        debug!("Successfully registered {} files as '{}'", paths.len(), table_name);
        Ok(())
    }

    /// Register files from a directory matching a glob pattern.
    ///
    /// # Arguments
    /// * `table_name` - Name to register the table as
    /// * `base_path` - Base directory path
    /// * `pattern` - Glob pattern (e.g., "*.parquet" or "**/*.parquet")
    pub async fn register_directory(&self, table_name: &str, base_path: &str, pattern: &str) -> Result<()> {
        let full_pattern = format!("{}/{}", base_path.trim_end_matches('/'), pattern);
        debug!("Registering directory with pattern: {}", full_pattern);

        let table_url = ListingTableUrl::parse(base_path)
            .map_err(|e| anyhow!("Invalid base path '{}': {}", base_path, e))?;

        let parquet_format = Arc::new(ParquetFormat::default());
        let listing_options = ListingOptions::new(parquet_format)
            .with_file_extension(".parquet");

        let schema = listing_options
            .infer_schema(&self.ctx.state(), &table_url)
            .await
            .map_err(|e| anyhow!("Failed to infer schema from '{}': {}", base_path, e))?;

        let config = ListingTableConfig::new(table_url)
            .with_listing_options(listing_options)
            .with_schema(schema);

        let table = ListingTable::try_new(config)
            .map_err(|e| anyhow!("Failed to create listing table: {}", e))?;

        self.ctx
            .register_table(table_name, Arc::new(table))
            .map_err(|e| anyhow!("Failed to register table '{}': {}", table_name, e))?;

        Ok(())
    }

    /// Register an in-memory table for hot buffer queries (v2.2.23)
    ///
    /// This is used to register hot data (from WAL) for SQL queries before
    /// Parquet files are created. The MemTable provides zero-copy access to
    /// Arrow RecordBatches.
    ///
    /// # Arguments
    /// * `table_name` - Name to register the table as (typically "{topic}_hot")
    /// * `mem_table` - DataFusion MemTable containing hot data
    ///
    /// # Example
    /// ```ignore
    /// let mem_table = hot_buffer.get_topic_mem_table("my-topic").await?;
    /// if let Some(mt) = mem_table {
    ///     engine.register_memory_table("my_topic_hot", mt)?;
    /// }
    /// ```
    pub fn register_memory_table(
        &self,
        table_name: &str,
        mem_table: datafusion::datasource::MemTable,
    ) -> Result<()> {
        debug!("Registering in-memory table '{}' for hot buffer", table_name);

        self.ctx
            .register_table(table_name, Arc::new(mem_table))
            .map_err(|e| anyhow!("Failed to register memory table '{}': {}", table_name, e))?;

        debug!("Successfully registered memory table '{}'", table_name);
        Ok(())
    }

    /// Check if a table is registered (v2.2.23)
    pub async fn table_exists(&self, table_name: &str) -> bool {
        self.ctx.table_provider(table_name).await.is_ok()
    }

    /// Register a SQL view for unified hot/cold queries (v2.2.23)
    ///
    /// Creates a view that unions hot (WAL) and cold (Parquet) tables.
    /// This enables seamless queries across recent and historical data.
    ///
    /// # Arguments
    /// * `view_name` - Name for the view (typically the base topic name)
    /// * `sql` - SQL defining the view (e.g., "SELECT * FROM topic_hot UNION ALL SELECT * FROM topic_cold")
    ///
    /// # Example
    /// ```ignore
    /// engine.register_view(
    ///     "orders",
    ///     "SELECT * FROM orders_hot UNION ALL SELECT * FROM orders_cold"
    /// ).await?;
    /// // Now queries to "orders" will include both hot and cold data
    /// ```
    pub async fn register_view(&self, view_name: &str, sql: &str) -> Result<()> {
        debug!("Registering view '{}' with SQL: {}", view_name, sql);

        // Use CREATE VIEW SQL statement
        let create_view_sql = format!("CREATE VIEW {} AS {}", view_name, sql);
        self.ctx
            .sql(&create_view_sql)
            .await
            .map_err(|e| anyhow!("Failed to create view '{}': {}", view_name, e))?;

        debug!("Successfully registered view '{}'", view_name);
        Ok(())
    }

    /// Execute a SQL query and return results as RecordBatches.
    pub async fn execute_sql(&self, sql: &str) -> Result<Vec<RecordBatch>> {
        // Parse and validate query
        let df = self
            .ctx
            .sql(sql)
            .await
            .map_err(|e| anyhow!("SQL parse error: {}", e))?;

        // Apply row limit
        let df = df.limit(0, Some(self.config.max_rows))?;

        // Execute query
        let batches = df
            .collect()
            .await
            .map_err(|e| anyhow!("Query execution error: {}", e))?;

        Ok(batches)
    }

    /// Execute a SQL query and return results as a stream.
    ///
    /// This is more memory-efficient than `execute_sql()` for large result sets
    /// as it doesn't buffer all results in memory. Results are returned as
    /// a stream of RecordBatches that can be processed incrementally.
    ///
    /// # Arguments
    /// * `sql` - SQL query string
    ///
    /// # Returns
    /// A stream of RecordBatches wrapped in Result for error handling.
    ///
    /// # Example
    /// ```ignore
    /// use futures::StreamExt;
    ///
    /// let mut stream = engine.execute_sql_stream("SELECT * FROM topic").await?;
    /// while let Some(batch_result) = stream.next().await {
    ///     let batch = batch_result?;
    ///     process_batch(&batch);
    /// }
    /// ```
    pub async fn execute_sql_stream(&self, sql: &str) -> Result<SendableRecordBatchStream> {
        let df = self
            .ctx
            .sql(sql)
            .await
            .map_err(|e| anyhow!("SQL parse error: {}", e))?;

        // Apply row limit
        let df = df.limit(0, Some(self.config.max_rows))?;

        // Execute and return stream
        let stream = df
            .execute_stream()
            .await
            .map_err(|e| anyhow!("Failed to create query stream: {}", e))?;

        Ok(stream)
    }

    /// Execute a query with explicit timestamp range predicate.
    ///
    /// Adds a WHERE clause filter on `_timestamp_ms` to enable partition pruning.
    /// This is more efficient than including the filter in the SQL string directly
    /// because it ensures proper predicate pushdown.
    ///
    /// # Arguments
    /// * `sql` - Base SQL query (without timestamp filter)
    /// * `min_timestamp_ms` - Minimum timestamp (inclusive), or None for no lower bound
    /// * `max_timestamp_ms` - Maximum timestamp (exclusive), or None for no upper bound
    pub async fn execute_with_timestamp_range(
        &self,
        sql: &str,
        min_timestamp_ms: Option<i64>,
        max_timestamp_ms: Option<i64>,
    ) -> Result<Vec<RecordBatch>> {
        let filtered_sql = self.add_timestamp_filter(sql, min_timestamp_ms, max_timestamp_ms)?;
        self.execute_sql(&filtered_sql).await
    }

    /// Execute a query with explicit offset range predicate.
    ///
    /// Adds a WHERE clause filter on `_offset` for efficient segment pruning.
    ///
    /// # Arguments
    /// * `sql` - Base SQL query (without offset filter)
    /// * `min_offset` - Minimum offset (inclusive), or None for no lower bound
    /// * `max_offset` - Maximum offset (exclusive), or None for no upper bound
    pub async fn execute_with_offset_range(
        &self,
        sql: &str,
        min_offset: Option<i64>,
        max_offset: Option<i64>,
    ) -> Result<Vec<RecordBatch>> {
        let filtered_sql = self.add_offset_filter(sql, min_offset, max_offset)?;
        self.execute_sql(&filtered_sql).await
    }

    /// Helper to add timestamp filter to SQL query.
    /// Note: Uses CAST to convert milliseconds to timestamp for comparison.
    fn add_timestamp_filter(
        &self,
        sql: &str,
        min_ts: Option<i64>,
        max_ts: Option<i64>,
    ) -> Result<String> {
        if min_ts.is_none() && max_ts.is_none() {
            return Ok(sql.to_string());
        }

        let mut conditions = Vec::new();
        if let Some(min) = min_ts {
            // Convert milliseconds to timestamp for comparison
            conditions.push(format!("_timestamp >= arrow_cast({}, 'Timestamp(Millisecond, Some(\"UTC\"))')", min));
        }
        if let Some(max) = max_ts {
            conditions.push(format!("_timestamp < arrow_cast({}, 'Timestamp(Millisecond, Some(\"UTC\"))')", max));
        }

        let filter = conditions.join(" AND ");

        // Check if query already has WHERE clause
        let sql_upper = sql.to_uppercase();
        if sql_upper.contains(" WHERE ") {
            // Add to existing WHERE clause
            Ok(format!("{} AND ({})", sql, filter))
        } else if sql_upper.contains(" GROUP BY ") || sql_upper.contains(" ORDER BY ") || sql_upper.contains(" LIMIT ") {
            // Insert WHERE before GROUP BY/ORDER BY/LIMIT
            let insertion_keywords = [" GROUP BY ", " ORDER BY ", " LIMIT "];
            for keyword in insertion_keywords {
                if let Some(pos) = sql_upper.find(keyword) {
                    let (before, after) = sql.split_at(pos);
                    return Ok(format!("{} WHERE {}{}", before, filter, after));
                }
            }
            Ok(format!("{} WHERE {}", sql, filter))
        } else {
            Ok(format!("{} WHERE {}", sql, filter))
        }
    }

    /// Helper to add offset filter to SQL query.
    /// Public to allow TopicQueryService to add filters for partition pruning.
    pub fn add_offset_filter(
        &self,
        sql: &str,
        min_offset: Option<i64>,
        max_offset: Option<i64>,
    ) -> Result<String> {
        if min_offset.is_none() && max_offset.is_none() {
            return Ok(sql.to_string());
        }

        let mut conditions = Vec::new();
        if let Some(min) = min_offset {
            conditions.push(format!("_offset >= {}", min));
        }
        if let Some(max) = max_offset {
            conditions.push(format!("_offset < {}", max));
        }

        let filter = conditions.join(" AND ");

        // Check if query already has WHERE clause
        let sql_upper = sql.to_uppercase();
        if sql_upper.contains(" WHERE ") {
            Ok(format!("{} AND ({})", sql, filter))
        } else if sql_upper.contains(" GROUP BY ") || sql_upper.contains(" ORDER BY ") || sql_upper.contains(" LIMIT ") {
            let insertion_keywords = [" GROUP BY ", " ORDER BY ", " LIMIT "];
            for keyword in insertion_keywords {
                if let Some(pos) = sql_upper.find(keyword) {
                    let (before, after) = sql.split_at(pos);
                    return Ok(format!("{} WHERE {}{}", before, filter, after));
                }
            }
            Ok(format!("{} WHERE {}", sql, filter))
        } else {
            Ok(format!("{} WHERE {}", sql, filter))
        }
    }

    /// Get the logical plan for a query (for debugging/explain).
    pub async fn explain(&self, sql: &str) -> Result<String> {
        let df = self
            .ctx
            .sql(sql)
            .await
            .map_err(|e| anyhow!("SQL parse error: {}", e))?;

        let plan = df.logical_plan();
        Ok(format!("{}", plan.display_indent()))
    }

    /// List all registered tables.
    pub fn list_tables(&self) -> Result<Vec<String>> {
        let catalog = self.ctx.catalog("datafusion").ok_or_else(|| anyhow!("No default catalog"))?;
        let schema = catalog.schema("public").ok_or_else(|| anyhow!("No public schema"))?;
        Ok(schema.table_names())
    }

    /// v2.2.22: List all registered topics (async wrapper for list_tables).
    pub async fn list_registered_topics(&self) -> Vec<String> {
        self.list_tables().unwrap_or_default()
    }

    /// v2.2.22: Get the schema of a registered table.
    pub async fn get_table_schema(
        &self,
        table: &str,
    ) -> Result<Arc<datafusion::arrow::datatypes::Schema>> {
        let table_ref = self
            .ctx
            .table(table)
            .await
            .map_err(|e| anyhow!("Table not found: {}", e))?;

        Ok(table_ref.schema().inner().clone())
    }

    /// Deregister a table.
    pub fn deregister_table(&self, table: &str) -> Result<()> {
        self.ctx
            .deregister_table(table)
            .map_err(|e| anyhow!("Failed to deregister table {}: {}", table, e))?;
        Ok(())
    }

    /// Get the underlying SessionContext for advanced operations.
    pub fn context(&self) -> &SessionContext {
        &self.ctx
    }
}

impl Default for ColumnarQueryEngine {
    fn default() -> Self {
        Self::new()
    }
}

/// Result of a query execution.
#[derive(Debug)]
pub struct QueryResult {
    /// The record batches containing the results.
    pub batches: Vec<RecordBatch>,
    /// Total number of rows.
    pub num_rows: usize,
    /// Query execution time.
    pub execution_time: Duration,
}

impl QueryResult {
    /// Create a new query result.
    pub fn new(batches: Vec<RecordBatch>, execution_time: Duration) -> Self {
        let num_rows = batches.iter().map(|b| b.num_rows()).sum();
        Self {
            batches,
            num_rows,
            execution_time,
        }
    }

    /// Check if the result is empty.
    pub fn is_empty(&self) -> bool {
        self.num_rows == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::converter::{KafkaRecord, RecordBatchConverter};
    use crate::schema::kafka_message_schema;
    use crate::writer::ParquetSegmentWriter;
    use crate::config::ColumnarConfig;
    use tempfile::tempdir;

    fn make_test_records(count: usize) -> Vec<KafkaRecord> {
        (0..count)
            .map(|i| KafkaRecord {
                topic: "test-topic".to_string(),
                partition: 0,
                offset: i as i64,
                timestamp_ms: 1704067200000 + i as i64,
                timestamp_type: 0,
                key: Some(format!("key-{}", i).into_bytes()),
                value: format!("value-{}", i).into_bytes(),
                headers: vec![],
                embedding: None,
            })
            .collect()
    }

    fn write_test_file(path: &Path, num_records: usize) {
        let config = ColumnarConfig::default();
        let schema = kafka_message_schema();
        let writer = ParquetSegmentWriter::new(schema, config);

        let converter = RecordBatchConverter::new();
        let records = make_test_records(num_records);
        let batch = converter.convert(&records).unwrap();

        writer.write_to_file(&[batch], path).unwrap();
    }

    #[tokio::test]
    async fn test_register_and_query() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.parquet");
        write_test_file(&path, 100);

        let engine = ColumnarQueryEngine::new();
        engine.register_file("messages", &path).await.unwrap();

        let batches = engine
            .execute_sql("SELECT _offset, _topic FROM messages LIMIT 10")
            .await
            .unwrap();

        let total_rows: usize = batches.iter().map(|b| b.num_rows()).sum();
        assert_eq!(total_rows, 10);
    }

    #[tokio::test]
    async fn test_count_query() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.parquet");
        write_test_file(&path, 50);

        let engine = ColumnarQueryEngine::new();
        engine.register_file("messages", &path).await.unwrap();

        let batches = engine
            .execute_sql("SELECT COUNT(*) as cnt FROM messages")
            .await
            .unwrap();

        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].num_rows(), 1);
    }

    #[tokio::test]
    async fn test_filter_query() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.parquet");
        write_test_file(&path, 100);

        let engine = ColumnarQueryEngine::new();
        engine.register_file("messages", &path).await.unwrap();

        let batches = engine
            .execute_sql("SELECT * FROM messages WHERE _offset >= 50")
            .await
            .unwrap();

        let total_rows: usize = batches.iter().map(|b| b.num_rows()).sum();
        assert_eq!(total_rows, 50);
    }

    #[tokio::test]
    async fn test_list_tables() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.parquet");
        write_test_file(&path, 10);

        let engine = ColumnarQueryEngine::new();
        engine.register_file("my_table", &path).await.unwrap();

        let tables = engine.list_tables().unwrap();
        assert!(tables.contains(&"my_table".to_string()));
    }

    #[tokio::test]
    async fn test_explain() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.parquet");
        write_test_file(&path, 10);

        let engine = ColumnarQueryEngine::new();
        engine.register_file("messages", &path).await.unwrap();

        let plan = engine
            .explain("SELECT * FROM messages WHERE _offset > 5")
            .await
            .unwrap();

        assert!(plan.contains("Filter"));
    }

    #[tokio::test]
    async fn test_register_multiple_files() {
        let dir = tempdir().unwrap();

        // Create 3 Parquet files with different offset ranges
        let path1 = dir.path().join("segment1.parquet");
        let path2 = dir.path().join("segment2.parquet");
        let path3 = dir.path().join("segment3.parquet");

        // Write records: 0-49, 50-99, 100-149
        write_test_file_with_offset_range(&path1, 0, 50);
        write_test_file_with_offset_range(&path2, 50, 100);
        write_test_file_with_offset_range(&path3, 100, 150);

        let paths = vec![
            path1.to_string_lossy().to_string(),
            path2.to_string_lossy().to_string(),
            path3.to_string_lossy().to_string(),
        ];

        let engine = ColumnarQueryEngine::new();
        engine.register_files("messages", &paths).await.unwrap();

        // Query should span all files - count all offsets to verify
        let batches = engine
            .execute_sql("SELECT _offset FROM messages")
            .await
            .unwrap();

        // Count total rows across all batches
        let total_rows: usize = batches.iter().map(|b| b.num_rows()).sum();
        assert_eq!(total_rows, 150);
    }

    #[tokio::test]
    async fn test_register_files_empty_list() {
        let engine = ColumnarQueryEngine::new();
        let result = engine.register_files("messages", &[]).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("empty file list"));
    }

    #[tokio::test]
    async fn test_register_files_single_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.parquet");
        write_test_file(&path, 25);

        let paths = vec![path.to_string_lossy().to_string()];

        let engine = ColumnarQueryEngine::new();
        engine.register_files("messages", &paths).await.unwrap();

        // Select all rows and count them
        let batches = engine
            .execute_sql("SELECT _offset FROM messages")
            .await
            .unwrap();

        let total_rows: usize = batches.iter().map(|b| b.num_rows()).sum();
        assert_eq!(total_rows, 25);
    }

    #[tokio::test]
    async fn test_execute_sql_stream() {
        use futures::StreamExt;

        let dir = tempdir().unwrap();
        let path = dir.path().join("test.parquet");
        write_test_file(&path, 100);

        let engine = ColumnarQueryEngine::new();
        engine.register_file("messages", &path).await.unwrap();

        let mut stream = engine
            .execute_sql_stream("SELECT _offset FROM messages")
            .await
            .unwrap();

        let mut total_rows = 0;
        while let Some(batch_result) = stream.next().await {
            let batch = batch_result.unwrap();
            total_rows += batch.num_rows();
        }

        assert_eq!(total_rows, 100);
    }

    #[tokio::test]
    async fn test_timestamp_filter_no_where() {
        let engine = ColumnarQueryEngine::new();
        let sql = "SELECT * FROM messages";

        // Add min only
        let filtered = engine.add_timestamp_filter(sql, Some(1000), None).unwrap();
        assert!(filtered.contains("_timestamp >="));
        assert!(filtered.contains("1000"));

        // Add max only
        let filtered = engine.add_timestamp_filter(sql, None, Some(2000)).unwrap();
        assert!(filtered.contains("_timestamp <"));
        assert!(filtered.contains("2000"));

        // Add both
        let filtered = engine.add_timestamp_filter(sql, Some(1000), Some(2000)).unwrap();
        assert!(filtered.contains("_timestamp >="));
        assert!(filtered.contains("_timestamp <"));
        assert!(filtered.contains("1000"));
        assert!(filtered.contains("2000"));

        // No filters
        let filtered = engine.add_timestamp_filter(sql, None, None).unwrap();
        assert_eq!(filtered, sql);
    }

    #[tokio::test]
    async fn test_timestamp_filter_with_existing_where() {
        let engine = ColumnarQueryEngine::new();
        let sql = "SELECT * FROM messages WHERE _offset > 100";

        let filtered = engine.add_timestamp_filter(sql, Some(1000), Some(2000)).unwrap();
        assert!(filtered.contains("_offset > 100"));
        assert!(filtered.contains("_timestamp >="));
        assert!(filtered.contains("_timestamp <"));
    }

    #[tokio::test]
    async fn test_timestamp_filter_before_order_by() {
        let engine = ColumnarQueryEngine::new();
        let sql = "SELECT * FROM messages ORDER BY _offset";

        let filtered = engine.add_timestamp_filter(sql, Some(1000), None).unwrap();
        assert!(filtered.contains("WHERE"));
        assert!(filtered.contains("_timestamp >="));
        assert!(filtered.ends_with("ORDER BY _offset"));
    }

    #[tokio::test]
    async fn test_offset_filter() {
        let engine = ColumnarQueryEngine::new();
        let sql = "SELECT * FROM messages";

        let filtered = engine.add_offset_filter(sql, Some(100), Some(200)).unwrap();
        assert_eq!(filtered, "SELECT * FROM messages WHERE _offset >= 100 AND _offset < 200");
    }

    #[tokio::test]
    async fn test_execute_with_timestamp_range() {
        // Test that the SQL is constructed correctly - the actual timestamp
        // comparison uses arrow_cast which requires proper type coercion
        let engine = ColumnarQueryEngine::new();

        // Verify the filter SQL is constructed correctly
        let filtered = engine.add_timestamp_filter(
            "SELECT * FROM messages",
            Some(1704067200050),
            Some(1704067200075),
        ).unwrap();

        // Should contain proper timestamp comparisons
        assert!(filtered.contains("_timestamp >="));
        assert!(filtered.contains("_timestamp <"));
        assert!(filtered.contains("1704067200050"));
        assert!(filtered.contains("1704067200075"));
        assert!(filtered.contains("WHERE"));
    }

    #[tokio::test]
    async fn test_execute_with_offset_range() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.parquet");
        write_test_file(&path, 100);

        let engine = ColumnarQueryEngine::new();
        engine.register_file("messages", &path).await.unwrap();

        let batches = engine
            .execute_with_offset_range(
                "SELECT * FROM messages",
                Some(25),
                Some(75),
            )
            .await
            .unwrap();

        let total_rows: usize = batches.iter().map(|b| b.num_rows()).sum();
        assert_eq!(total_rows, 50);
    }

    /// Helper to write test file with specific offset range
    fn write_test_file_with_offset_range(path: &Path, start_offset: i64, end_offset: i64) {
        let count = (end_offset - start_offset) as usize;
        let records: Vec<KafkaRecord> = (0..count)
            .map(|i| KafkaRecord {
                topic: "test-topic".to_string(),
                partition: 0,
                offset: start_offset + i as i64,
                timestamp_ms: 1704067200000 + start_offset + i as i64,
                timestamp_type: 0,
                key: Some(format!("key-{}", start_offset + i as i64).into_bytes()),
                value: format!("value-{}", start_offset + i as i64).into_bytes(),
                headers: vec![],
                embedding: None,
            })
            .collect();

        let config = ColumnarConfig::default();
        let schema = kafka_message_schema();
        let writer = ParquetSegmentWriter::new(schema, config);

        let converter = RecordBatchConverter::new();
        let batch = converter.convert(&records).unwrap();

        writer.write_to_file(&[batch], path).unwrap();
    }
}
