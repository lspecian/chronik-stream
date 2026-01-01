//! Topic Query Service - SQL queries over topics with partition pruning.
//!
//! This module provides a high-level service for executing SQL queries over
//! Kafka topics stored in Parquet format. It integrates with SegmentIndex
//! to enable efficient partition pruning based on:
//!
//! - Offset ranges (skip files that don't contain relevant offsets)
//! - Timestamp ranges (skip files outside the time window)
//! - Time partition keys (for hourly/daily partitioned data)
//!
//! # Example
//!
//! ```ignore
//! let service = TopicQueryService::new(segment_index);
//!
//! // Query with automatic partition pruning
//! let results = service
//!     .query_topic("orders", "SELECT * FROM orders WHERE _offset > 1000")
//!     .await?;
//!
//! // Query with explicit time range (more efficient pruning)
//! let results = service
//!     .query_topic_with_time_range(
//!         "orders",
//!         "SELECT SUM(amount) FROM orders",
//!         Some(start_ts),
//!         Some(end_ts),
//!     )
//!     .await?;
//! ```

use anyhow::{anyhow, Result};
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::execution::SendableRecordBatchStream;
use std::sync::Arc;
use tracing::{debug, info, warn};

use crate::query_engine::{ColumnarQueryEngine, QueryEngineConfig};

/// Trait for segment index operations needed by TopicQueryService.
/// This abstracts over the actual SegmentIndex implementation from chronik-storage.
#[async_trait::async_trait]
pub trait SegmentIndexProvider: Send + Sync {
    /// Get all Parquet file paths for a topic.
    async fn get_parquet_paths(&self, topic: &str) -> Result<Vec<String>>;

    /// Get Parquet file paths filtered by offset range.
    async fn get_parquet_paths_by_offset_range(
        &self,
        topic: &str,
        min_offset: Option<i64>,
        max_offset: Option<i64>,
    ) -> Result<Vec<String>>;

    /// Get Parquet file paths filtered by timestamp range.
    async fn get_parquet_paths_by_timestamp_range(
        &self,
        topic: &str,
        min_timestamp: Option<i64>,
        max_timestamp: Option<i64>,
    ) -> Result<Vec<String>>;

    /// Get Parquet file paths filtered by time partition keys.
    async fn get_parquet_paths_by_time_partitions(
        &self,
        topic: &str,
        time_partition_keys: &[String],
    ) -> Result<Vec<String>>;

    /// Check if a topic has any Parquet segments.
    async fn has_parquet_segments(&self, topic: &str) -> Result<bool>;
}

/// Query filters for partition pruning.
#[derive(Debug, Clone, Default)]
pub struct QueryFilters {
    /// Minimum offset (inclusive).
    pub min_offset: Option<i64>,
    /// Maximum offset (exclusive).
    pub max_offset: Option<i64>,
    /// Minimum timestamp in milliseconds (inclusive).
    pub min_timestamp: Option<i64>,
    /// Maximum timestamp in milliseconds (exclusive).
    pub max_timestamp: Option<i64>,
    /// Specific time partition keys to query.
    pub time_partitions: Option<Vec<String>>,
}

impl QueryFilters {
    /// Create filters with offset range.
    pub fn with_offset_range(min: Option<i64>, max: Option<i64>) -> Self {
        Self {
            min_offset: min,
            max_offset: max,
            ..Default::default()
        }
    }

    /// Create filters with timestamp range.
    pub fn with_timestamp_range(min: Option<i64>, max: Option<i64>) -> Self {
        Self {
            min_timestamp: min,
            max_timestamp: max,
            ..Default::default()
        }
    }

    /// Create filters with time partitions.
    pub fn with_time_partitions(partitions: Vec<String>) -> Self {
        Self {
            time_partitions: Some(partitions),
            ..Default::default()
        }
    }

    /// Check if any filters are set.
    pub fn has_filters(&self) -> bool {
        self.min_offset.is_some()
            || self.max_offset.is_some()
            || self.min_timestamp.is_some()
            || self.max_timestamp.is_some()
            || self.time_partitions.is_some()
    }
}

/// Service for executing SQL queries over topics with partition pruning.
pub struct TopicQueryService<S: SegmentIndexProvider> {
    segment_index: Arc<S>,
    engine_config: QueryEngineConfig,
}

impl<S: SegmentIndexProvider> TopicQueryService<S> {
    /// Create a new TopicQueryService with the given segment index.
    pub fn new(segment_index: Arc<S>) -> Self {
        Self {
            segment_index,
            engine_config: QueryEngineConfig::default(),
        }
    }

    /// Create a new TopicQueryService with custom engine configuration.
    pub fn with_config(segment_index: Arc<S>, engine_config: QueryEngineConfig) -> Self {
        Self {
            segment_index,
            engine_config,
        }
    }

    /// Execute a SQL query over a topic with automatic table registration.
    ///
    /// The topic is registered as a table with the same name (sanitized for SQL).
    /// All Parquet segments for the topic are included.
    pub async fn query_topic(&self, topic: &str, sql: &str) -> Result<Vec<RecordBatch>> {
        self.query_topic_with_filters(topic, sql, QueryFilters::default())
            .await
    }

    /// Execute a SQL query with explicit filters for partition pruning.
    ///
    /// Filters are applied at the segment level before query execution,
    /// reducing the number of Parquet files that need to be scanned.
    pub async fn query_topic_with_filters(
        &self,
        topic: &str,
        sql: &str,
        filters: QueryFilters,
    ) -> Result<Vec<RecordBatch>> {
        let table_name = sanitize_table_name(topic);

        // Get relevant Parquet paths based on filters
        let paths = self.get_filtered_paths(topic, &filters).await?;

        if paths.is_empty() {
            debug!(topic = %topic, "No Parquet segments found for topic");
            return Ok(Vec::new());
        }

        info!(
            topic = %topic,
            table_name = %table_name,
            num_files = paths.len(),
            has_filters = filters.has_filters(),
            "Querying topic with partition pruning"
        );

        // Create engine and register files
        let engine = ColumnarQueryEngine::with_config(self.engine_config.clone());
        engine.register_files(&table_name, &paths).await?;

        // Build SQL with additional filters if needed
        let final_sql = self.apply_sql_filters(sql, &table_name, &filters)?;

        // Execute query
        engine.execute_sql(&final_sql).await
    }

    /// Execute a SQL query and return results as a stream.
    pub async fn query_topic_stream(
        &self,
        topic: &str,
        sql: &str,
        filters: QueryFilters,
    ) -> Result<SendableRecordBatchStream> {
        let table_name = sanitize_table_name(topic);
        let paths = self.get_filtered_paths(topic, &filters).await?;

        if paths.is_empty() {
            return Err(anyhow!("No Parquet segments found for topic '{}'", topic));
        }

        let engine = ColumnarQueryEngine::with_config(self.engine_config.clone());
        engine.register_files(&table_name, &paths).await?;

        let final_sql = self.apply_sql_filters(sql, &table_name, &filters)?;
        engine.execute_sql_stream(&final_sql).await
    }

    /// Query with offset range filter.
    pub async fn query_topic_with_offset_range(
        &self,
        topic: &str,
        sql: &str,
        min_offset: Option<i64>,
        max_offset: Option<i64>,
    ) -> Result<Vec<RecordBatch>> {
        let filters = QueryFilters::with_offset_range(min_offset, max_offset);
        self.query_topic_with_filters(topic, sql, filters).await
    }

    /// Query with timestamp range filter.
    pub async fn query_topic_with_timestamp_range(
        &self,
        topic: &str,
        sql: &str,
        min_timestamp: Option<i64>,
        max_timestamp: Option<i64>,
    ) -> Result<Vec<RecordBatch>> {
        let filters = QueryFilters::with_timestamp_range(min_timestamp, max_timestamp);
        self.query_topic_with_filters(topic, sql, filters).await
    }

    /// Query specific time partitions (e.g., last 24 hours).
    pub async fn query_topic_time_partitions(
        &self,
        topic: &str,
        sql: &str,
        time_partitions: Vec<String>,
    ) -> Result<Vec<RecordBatch>> {
        let filters = QueryFilters::with_time_partitions(time_partitions);
        self.query_topic_with_filters(topic, sql, filters).await
    }

    /// List all topics that have Parquet segments.
    pub async fn list_queryable_topics(&self, topics: &[String]) -> Result<Vec<String>> {
        let mut queryable = Vec::new();
        for topic in topics {
            if self.segment_index.has_parquet_segments(topic).await? {
                queryable.push(topic.clone());
            }
        }
        Ok(queryable)
    }

    /// Get the number of Parquet files for a topic.
    pub async fn get_segment_count(&self, topic: &str) -> Result<usize> {
        let paths = self.segment_index.get_parquet_paths(topic).await?;
        Ok(paths.len())
    }

    /// Get filtered Parquet paths based on query filters.
    async fn get_filtered_paths(&self, topic: &str, filters: &QueryFilters) -> Result<Vec<String>> {
        // If specific time partitions are requested, use those
        if let Some(ref partitions) = filters.time_partitions {
            return self
                .segment_index
                .get_parquet_paths_by_time_partitions(topic, partitions)
                .await;
        }

        // If timestamp range is provided, use timestamp filtering
        if filters.min_timestamp.is_some() || filters.max_timestamp.is_some() {
            return self
                .segment_index
                .get_parquet_paths_by_timestamp_range(
                    topic,
                    filters.min_timestamp,
                    filters.max_timestamp,
                )
                .await;
        }

        // If offset range is provided, use offset filtering
        if filters.min_offset.is_some() || filters.max_offset.is_some() {
            return self
                .segment_index
                .get_parquet_paths_by_offset_range(topic, filters.min_offset, filters.max_offset)
                .await;
        }

        // No filters - return all paths
        self.segment_index.get_parquet_paths(topic).await
    }

    /// Apply SQL-level filters to the query.
    /// This adds WHERE clauses that DataFusion can push down to Parquet.
    fn apply_sql_filters(
        &self,
        sql: &str,
        _table_name: &str,
        filters: &QueryFilters,
    ) -> Result<String> {
        let mut result = sql.to_string();

        // Add offset filter to SQL (DataFusion will push this down)
        if filters.min_offset.is_some() || filters.max_offset.is_some() {
            let engine = ColumnarQueryEngine::new();
            result = engine.add_offset_filter(&result, filters.min_offset, filters.max_offset)?;
        }

        // Note: Timestamp filtering is done at segment level using time_partition_key
        // Adding SQL-level timestamp filter can help with intra-segment filtering
        // but requires arrow_cast which adds complexity

        Ok(result)
    }
}

/// Sanitize a topic name for use as a SQL table name.
/// Replaces invalid characters with underscores.
fn sanitize_table_name(topic: &str) -> String {
    topic
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use tokio::sync::RwLock;

    /// Mock segment index for testing.
    struct MockSegmentIndex {
        paths: RwLock<HashMap<String, Vec<MockSegment>>>,
    }

    #[derive(Clone)]
    struct MockSegment {
        path: String,
        min_offset: i64,
        max_offset: i64,
        min_timestamp: i64,
        max_timestamp: i64,
        time_partition: Option<String>,
    }

    impl MockSegmentIndex {
        fn new() -> Self {
            Self {
                paths: RwLock::new(HashMap::new()),
            }
        }

        async fn add_segment(&self, topic: &str, segment: MockSegment) {
            let mut paths = self.paths.write().await;
            paths
                .entry(topic.to_string())
                .or_insert_with(Vec::new)
                .push(segment);
        }
    }

    #[async_trait::async_trait]
    impl SegmentIndexProvider for MockSegmentIndex {
        async fn get_parquet_paths(&self, topic: &str) -> Result<Vec<String>> {
            let paths = self.paths.read().await;
            Ok(paths
                .get(topic)
                .map(|segs| segs.iter().map(|s| s.path.clone()).collect())
                .unwrap_or_default())
        }

        async fn get_parquet_paths_by_offset_range(
            &self,
            topic: &str,
            min_offset: Option<i64>,
            max_offset: Option<i64>,
        ) -> Result<Vec<String>> {
            let paths = self.paths.read().await;
            Ok(paths
                .get(topic)
                .map(|segs| {
                    segs.iter()
                        .filter(|s| {
                            let min_ok = min_offset.map(|m| s.max_offset >= m).unwrap_or(true);
                            let max_ok = max_offset.map(|m| s.min_offset < m).unwrap_or(true);
                            min_ok && max_ok
                        })
                        .map(|s| s.path.clone())
                        .collect()
                })
                .unwrap_or_default())
        }

        async fn get_parquet_paths_by_timestamp_range(
            &self,
            topic: &str,
            min_timestamp: Option<i64>,
            max_timestamp: Option<i64>,
        ) -> Result<Vec<String>> {
            let paths = self.paths.read().await;
            Ok(paths
                .get(topic)
                .map(|segs| {
                    segs.iter()
                        .filter(|s| {
                            let min_ok =
                                min_timestamp.map(|m| s.max_timestamp >= m).unwrap_or(true);
                            let max_ok =
                                max_timestamp.map(|m| s.min_timestamp < m).unwrap_or(true);
                            min_ok && max_ok
                        })
                        .map(|s| s.path.clone())
                        .collect()
                })
                .unwrap_or_default())
        }

        async fn get_parquet_paths_by_time_partitions(
            &self,
            topic: &str,
            time_partition_keys: &[String],
        ) -> Result<Vec<String>> {
            let paths = self.paths.read().await;
            Ok(paths
                .get(topic)
                .map(|segs| {
                    segs.iter()
                        .filter(|s| {
                            s.time_partition
                                .as_ref()
                                .map(|tp| time_partition_keys.contains(tp))
                                .unwrap_or(false)
                        })
                        .map(|s| s.path.clone())
                        .collect()
                })
                .unwrap_or_default())
        }

        async fn has_parquet_segments(&self, topic: &str) -> Result<bool> {
            let paths = self.paths.read().await;
            Ok(paths.get(topic).map(|s| !s.is_empty()).unwrap_or(false))
        }
    }

    #[test]
    fn test_sanitize_table_name() {
        assert_eq!(sanitize_table_name("orders"), "orders");
        assert_eq!(sanitize_table_name("my-topic"), "my_topic");
        assert_eq!(sanitize_table_name("topic.with.dots"), "topic_with_dots");
        assert_eq!(sanitize_table_name("topic_123"), "topic_123");
    }

    #[test]
    fn test_query_filters_default() {
        let filters = QueryFilters::default();
        assert!(!filters.has_filters());
    }

    #[test]
    fn test_query_filters_with_offset() {
        let filters = QueryFilters::with_offset_range(Some(100), Some(200));
        assert!(filters.has_filters());
        assert_eq!(filters.min_offset, Some(100));
        assert_eq!(filters.max_offset, Some(200));
    }

    #[test]
    fn test_query_filters_with_timestamp() {
        let filters = QueryFilters::with_timestamp_range(Some(1000), Some(2000));
        assert!(filters.has_filters());
        assert_eq!(filters.min_timestamp, Some(1000));
        assert_eq!(filters.max_timestamp, Some(2000));
    }

    #[tokio::test]
    async fn test_mock_segment_index_all_paths() {
        let index = MockSegmentIndex::new();
        index
            .add_segment(
                "topic1",
                MockSegment {
                    path: "path1.parquet".to_string(),
                    min_offset: 0,
                    max_offset: 99,
                    min_timestamp: 1000,
                    max_timestamp: 1999,
                    time_partition: Some("2024-01-01".to_string()),
                },
            )
            .await;
        index
            .add_segment(
                "topic1",
                MockSegment {
                    path: "path2.parquet".to_string(),
                    min_offset: 100,
                    max_offset: 199,
                    min_timestamp: 2000,
                    max_timestamp: 2999,
                    time_partition: Some("2024-01-02".to_string()),
                },
            )
            .await;

        let paths = index.get_parquet_paths("topic1").await.unwrap();
        assert_eq!(paths.len(), 2);
    }

    #[tokio::test]
    async fn test_mock_segment_index_offset_filter() {
        let index = MockSegmentIndex::new();
        index
            .add_segment(
                "topic1",
                MockSegment {
                    path: "path1.parquet".to_string(),
                    min_offset: 0,
                    max_offset: 99,
                    min_timestamp: 1000,
                    max_timestamp: 1999,
                    time_partition: None,
                },
            )
            .await;
        index
            .add_segment(
                "topic1",
                MockSegment {
                    path: "path2.parquet".to_string(),
                    min_offset: 100,
                    max_offset: 199,
                    min_timestamp: 2000,
                    max_timestamp: 2999,
                    time_partition: None,
                },
            )
            .await;
        index
            .add_segment(
                "topic1",
                MockSegment {
                    path: "path3.parquet".to_string(),
                    min_offset: 200,
                    max_offset: 299,
                    min_timestamp: 3000,
                    max_timestamp: 3999,
                    time_partition: None,
                },
            )
            .await;

        // Query offset 50-150 should match first two segments
        let paths = index
            .get_parquet_paths_by_offset_range("topic1", Some(50), Some(150))
            .await
            .unwrap();
        assert_eq!(paths.len(), 2);
        assert!(paths.contains(&"path1.parquet".to_string()));
        assert!(paths.contains(&"path2.parquet".to_string()));
    }

    #[tokio::test]
    async fn test_mock_segment_index_timestamp_filter() {
        let index = MockSegmentIndex::new();
        index
            .add_segment(
                "topic1",
                MockSegment {
                    path: "path1.parquet".to_string(),
                    min_offset: 0,
                    max_offset: 99,
                    min_timestamp: 1000,
                    max_timestamp: 1999,
                    time_partition: None,
                },
            )
            .await;
        index
            .add_segment(
                "topic1",
                MockSegment {
                    path: "path2.parquet".to_string(),
                    min_offset: 100,
                    max_offset: 199,
                    min_timestamp: 2000,
                    max_timestamp: 2999,
                    time_partition: None,
                },
            )
            .await;

        // Query timestamp 1500-2500 should match both
        let paths = index
            .get_parquet_paths_by_timestamp_range("topic1", Some(1500), Some(2500))
            .await
            .unwrap();
        assert_eq!(paths.len(), 2);

        // Query timestamp 2500-3500 should match only second
        let paths = index
            .get_parquet_paths_by_timestamp_range("topic1", Some(2500), Some(3500))
            .await
            .unwrap();
        assert_eq!(paths.len(), 1);
        assert!(paths.contains(&"path2.parquet".to_string()));
    }

    #[tokio::test]
    async fn test_mock_segment_index_time_partition_filter() {
        let index = MockSegmentIndex::new();
        index
            .add_segment(
                "topic1",
                MockSegment {
                    path: "path1.parquet".to_string(),
                    min_offset: 0,
                    max_offset: 99,
                    min_timestamp: 1000,
                    max_timestamp: 1999,
                    time_partition: Some("2024-01-01".to_string()),
                },
            )
            .await;
        index
            .add_segment(
                "topic1",
                MockSegment {
                    path: "path2.parquet".to_string(),
                    min_offset: 100,
                    max_offset: 199,
                    min_timestamp: 2000,
                    max_timestamp: 2999,
                    time_partition: Some("2024-01-02".to_string()),
                },
            )
            .await;

        let paths = index
            .get_parquet_paths_by_time_partitions("topic1", &["2024-01-01".to_string()])
            .await
            .unwrap();
        assert_eq!(paths.len(), 1);
        assert!(paths.contains(&"path1.parquet".to_string()));
    }

    #[tokio::test]
    async fn test_has_parquet_segments() {
        let index = MockSegmentIndex::new();
        index
            .add_segment(
                "topic1",
                MockSegment {
                    path: "path1.parquet".to_string(),
                    min_offset: 0,
                    max_offset: 99,
                    min_timestamp: 1000,
                    max_timestamp: 1999,
                    time_partition: None,
                },
            )
            .await;

        assert!(index.has_parquet_segments("topic1").await.unwrap());
        assert!(!index.has_parquet_segments("topic2").await.unwrap());
    }
}
