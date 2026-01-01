//! SQL Query Handler for Unified API
//!
//! Provides SQL query endpoints via DataFusion:
//! - POST `/_sql` - Execute SQL query
//! - POST `/_sql/explain` - Get query execution plan
//! - GET `/_sql/tables` - List available tables (topics)
//! - GET `/_sql/describe/:table` - Get table schema

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::collections::HashSet;
use tracing::{debug, error, info, warn};

use super::UnifiedApiState;

/// SQL query request
#[derive(Debug, Deserialize)]
pub struct SqlRequest {
    /// SQL query to execute
    pub query: String,
    /// Maximum rows to return (default: 1000)
    #[serde(default = "default_limit")]
    pub limit: usize,
    /// Query timeout in seconds (default: 30)
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
}

fn default_limit() -> usize {
    1000
}

fn default_timeout() -> u64 {
    30
}

/// SQL query response
#[derive(Debug, Serialize)]
pub struct SqlResponse {
    /// Column names
    pub columns: Vec<String>,
    /// Row data (each row is a map of column name to value)
    pub rows: Vec<HashMap<String, serde_json::Value>>,
    /// Number of rows returned
    pub row_count: usize,
    /// Query execution time in milliseconds
    pub execution_time_ms: u64,
    /// Whether results were truncated
    pub truncated: bool,
}

/// Error response
#[derive(Debug, Serialize)]
pub struct SqlErrorResponse {
    pub error: String,
    pub error_type: String,
}

/// SQL Handler (for direct usage without HTTP)
pub struct SqlHandler;

impl SqlHandler {
    /// v2.2.23: Ensure topics are registered as SQL tables with hot/cold union
    ///
    /// This function lists all topics from the metadata store and registers:
    /// - `{topic}_cold`: Parquet files (historical data)
    /// - `{topic}_hot`: Hot buffer from WAL (recent data, sub-second latency)
    /// - `{topic}`: Union view of hot + cold for seamless queries
    ///
    /// Table names are sanitized (replacing - and . with _) for SQL compatibility.
    async fn ensure_topics_registered(
        state: &UnifiedApiState,
        engine: &chronik_columnar::ColumnarQueryEngine,
    ) {
        // Get list of already registered tables
        let registered = engine.list_tables().unwrap_or_default();
        let registered_set: HashSet<_> = registered.into_iter().collect();

        // List all topics from metadata store
        let topics = match state.metadata_store.list_topics().await {
            Ok(topics) => topics,
            Err(e) => {
                warn!("Failed to list topics for SQL registration: {}", e);
                return;
            }
        };

        // For each topic, register hot and cold tables
        for topic_meta in topics {
            let topic = &topic_meta.name;
            let base_table_name = Self::sanitize_table_name(topic);
            let cold_table_name = format!("{}_cold", base_table_name);
            let hot_table_name = format!("{}_hot", base_table_name);

            let mut has_cold = false;
            let mut has_hot = false;

            // ============================================================
            // Register COLD table (Parquet files)
            // ============================================================
            if !registered_set.contains(&cold_table_name) {
                // Get Parquet paths for this topic
                let paths = match state.metadata_store.get_parquet_paths(topic).await {
                    Ok(paths) => paths,
                    Err(e) => {
                        debug!("No Parquet data for topic '{}': {}", topic, e);
                        Vec::new()
                    }
                };

                if !paths.is_empty() {
                    // v2.2.22: Filter out non-existent files (stale entries from previous runs)
                    let valid_paths: Vec<String> = paths
                        .into_iter()
                        .filter(|p| std::path::Path::new(p).exists())
                        .collect();

                    if !valid_paths.is_empty() {
                        if let Err(e) = engine.register_files(&cold_table_name, &valid_paths).await {
                            warn!("Failed to register cold table '{}': {}", cold_table_name, e);
                        } else {
                            info!(
                                topic = %topic,
                                table_name = %cold_table_name,
                                num_files = valid_paths.len(),
                                "Registered cold (Parquet) table"
                            );
                            has_cold = true;
                        }
                    }
                }
            } else {
                has_cold = true;
            }

            // ============================================================
            // Register HOT table (in-memory from WAL)
            // ============================================================
            if !registered_set.contains(&hot_table_name) {
                if let Some(hot_buffer) = &state.hot_buffer {
                    match hot_buffer.get_topic_mem_table(topic).await {
                        Ok(Some(mem_table)) => {
                            if let Err(e) = engine.register_memory_table(&hot_table_name, mem_table) {
                                debug!("Failed to register hot table '{}': {}", hot_table_name, e);
                            } else {
                                info!(
                                    topic = %topic,
                                    table_name = %hot_table_name,
                                    "Registered hot (WAL) table"
                                );
                                has_hot = true;
                            }
                        }
                        Ok(None) => {
                            debug!("No hot data available for topic '{}'", topic);
                        }
                        Err(e) => {
                            debug!("Failed to get hot buffer for topic '{}': {}", topic, e);
                        }
                    }
                }
            } else {
                has_hot = true;
            }

            // ============================================================
            // Create unified VIEW (hot UNION ALL cold)
            // ============================================================
            // Only create view if base table doesn't exist and we have at least one source
            if !registered_set.contains(&base_table_name) && (has_hot || has_cold) {
                // v2.2.23: Use explicit columns for UNION to handle schema differences
                // Hot buffer (MemTable) and cold (Parquet) may have different schemas:
                // - Hot: Utf8, Binary types without _headers
                // - Cold: Utf8View, BinaryView types with _headers
                // We select only the common core columns to ensure UNION compatibility
                let common_cols = "_topic, _partition, _offset, _timestamp, _timestamp_type, _key, _value";

                let view_sql = if has_hot && has_cold {
                    // Both hot and cold available - union them with explicit columns
                    // Hot data has priority (more recent), cold provides historical
                    format!(
                        "SELECT {} FROM {} UNION ALL SELECT {} FROM {}",
                        common_cols, hot_table_name, common_cols, cold_table_name
                    )
                } else if has_hot {
                    // Only hot data
                    format!("SELECT {} FROM {}", common_cols, hot_table_name)
                } else {
                    // Only cold data
                    format!("SELECT {} FROM {}", common_cols, cold_table_name)
                };

                if let Err(e) = engine.register_view(&base_table_name, &view_sql).await {
                    debug!(
                        "Failed to register unified view '{}': {} (will use individual tables)",
                        base_table_name, e
                    );
                    // Fallback: if view creation fails, at least register cold as base name
                    // This maintains backward compatibility
                    if has_cold && !has_hot {
                        // Re-register cold as the base name for backward compat
                        if let Ok(paths) = state.metadata_store.get_parquet_paths(topic).await {
                            let valid_paths: Vec<String> = paths
                                .into_iter()
                                .filter(|p| std::path::Path::new(p).exists())
                                .collect();
                            if !valid_paths.is_empty() {
                                let _ = engine.register_files(&base_table_name, &valid_paths).await;
                            }
                        }
                    }
                } else {
                    info!(
                        topic = %topic,
                        view_name = %base_table_name,
                        has_hot = has_hot,
                        has_cold = has_cold,
                        "Registered unified hot/cold view"
                    );
                }
            }
        }
    }

    /// Sanitize topic name to be a valid SQL table name
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

    /// Execute a SQL query directly
    pub async fn execute(
        state: &UnifiedApiState,
        query: &str,
        limit: usize,
    ) -> Result<SqlResponse, String> {
        let start = std::time::Instant::now();

        let engine = state
            .query_engine
            .as_ref()
            .ok_or("SQL query engine not available")?;

        // v2.2.22: Dynamically register topics with Parquet data before query execution
        // This ensures all columnar-enabled topics are available as SQL tables
        Self::ensure_topics_registered(state, engine).await;

        // Execute query
        let batches = engine
            .execute_sql(query)
            .await
            .map_err(|e| format!("Query execution failed: {}", e))?;

        // Convert to response format
        let mut columns: Vec<String> = Vec::new();
        let mut rows: Vec<HashMap<String, serde_json::Value>> = Vec::new();
        let mut truncated = false;

        for batch in batches {
            // Get column names from first batch
            if columns.is_empty() {
                columns = batch
                    .schema()
                    .fields()
                    .iter()
                    .map(|f| f.name().clone())
                    .collect();
            }

            // Convert rows
            let num_rows = batch.num_rows();
            for row_idx in 0..num_rows {
                if rows.len() >= limit {
                    truncated = true;
                    break;
                }

                let mut row: HashMap<String, serde_json::Value> = HashMap::new();
                for (col_idx, col_name) in columns.iter().enumerate() {
                    let column = batch.column(col_idx);
                    let value = arrow_value_to_json(column, row_idx);
                    row.insert(col_name.clone(), value);
                }
                rows.push(row);
            }

            if truncated {
                break;
            }
        }

        let row_count = rows.len();
        let execution_time_ms = start.elapsed().as_millis() as u64;

        Ok(SqlResponse {
            columns,
            rows,
            row_count,
            execution_time_ms,
            truncated,
        })
    }
}

/// Execute SQL query endpoint
pub async fn execute_sql(
    State(state): State<UnifiedApiState>,
    Json(request): Json<SqlRequest>,
) -> impl IntoResponse {
    info!(query = %request.query, limit = request.limit, "Executing SQL query");

    match SqlHandler::execute(&state, &request.query, request.limit).await {
        Ok(response) => {
            info!(
                rows = response.row_count,
                time_ms = response.execution_time_ms,
                "SQL query completed"
            );
            (StatusCode::OK, Json(response)).into_response()
        }
        Err(e) => {
            error!(error = %e, "SQL query failed");
            let error_response = SqlErrorResponse {
                error: e,
                error_type: "QueryError".to_string(),
            };
            (StatusCode::BAD_REQUEST, Json(error_response)).into_response()
        }
    }
}

/// Explain query request
#[derive(Debug, Deserialize)]
pub struct ExplainRequest {
    /// SQL query to explain
    pub query: String,
    /// Include physical plan
    #[serde(default)]
    pub physical: bool,
}

/// Explain query response
#[derive(Debug, Serialize)]
pub struct ExplainResponse {
    /// Logical query plan
    pub logical_plan: String,
    /// Physical query plan (if requested)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub physical_plan: Option<String>,
}

/// Explain SQL query endpoint
pub async fn explain_sql(
    State(state): State<UnifiedApiState>,
    Json(request): Json<ExplainRequest>,
) -> impl IntoResponse {
    info!(query = %request.query, "Explaining SQL query");

    let engine = match &state.query_engine {
        Some(e) => e,
        None => {
            let error_response = SqlErrorResponse {
                error: "SQL query engine not available".to_string(),
                error_type: "ServiceUnavailable".to_string(),
            };
            return (StatusCode::SERVICE_UNAVAILABLE, Json(error_response)).into_response();
        }
    };

    match engine.explain(&request.query).await {
        Ok(plan) => {
            let response = ExplainResponse {
                logical_plan: plan,
                physical_plan: None, // TODO: Add physical plan support
            };
            (StatusCode::OK, Json(response)).into_response()
        }
        Err(e) => {
            error!(error = %e, "Explain failed");
            let error_response = SqlErrorResponse {
                error: e.to_string(),
                error_type: "ExplainError".to_string(),
            };
            (StatusCode::BAD_REQUEST, Json(error_response)).into_response()
        }
    }
}

/// List tables response
#[derive(Debug, Serialize)]
pub struct ListTablesResponse {
    pub tables: Vec<TableInfo>,
}

/// Table information
#[derive(Debug, Serialize)]
pub struct TableInfo {
    pub name: String,
    pub table_type: String,
}

/// List available tables (topics)
pub async fn list_tables(State(state): State<UnifiedApiState>) -> impl IntoResponse {
    debug!("Listing SQL tables");

    let engine = match &state.query_engine {
        Some(e) => e,
        None => {
            let error_response = SqlErrorResponse {
                error: "SQL query engine not available".to_string(),
                error_type: "ServiceUnavailable".to_string(),
            };
            return (StatusCode::SERVICE_UNAVAILABLE, Json(error_response)).into_response();
        }
    };

    let tables = engine.list_registered_topics().await;

    let response = ListTablesResponse {
        tables: tables
            .into_iter()
            .map(|name| TableInfo {
                name,
                table_type: "TOPIC".to_string(),
            })
            .collect(),
    };

    (StatusCode::OK, Json(response)).into_response()
}

/// Describe table response
#[derive(Debug, Serialize)]
pub struct DescribeTableResponse {
    pub table: String,
    pub columns: Vec<ColumnInfo>,
}

/// Column information
#[derive(Debug, Serialize)]
pub struct ColumnInfo {
    pub name: String,
    pub data_type: String,
    pub nullable: bool,
}

/// Describe a table's schema
pub async fn describe_table(
    State(state): State<UnifiedApiState>,
    Path(table): Path<String>,
) -> impl IntoResponse {
    debug!(table = %table, "Describing table");

    let engine = match &state.query_engine {
        Some(e) => e,
        None => {
            let error_response = SqlErrorResponse {
                error: "SQL query engine not available".to_string(),
                error_type: "ServiceUnavailable".to_string(),
            };
            return (StatusCode::SERVICE_UNAVAILABLE, Json(error_response)).into_response();
        }
    };

    match engine.get_table_schema(&table).await {
        Ok(schema) => {
            let columns: Vec<ColumnInfo> = schema
                .fields()
                .iter()
                .map(|f| ColumnInfo {
                    name: f.name().clone(),
                    data_type: format!("{:?}", f.data_type()),
                    nullable: f.is_nullable(),
                })
                .collect();

            let response = DescribeTableResponse {
                table,
                columns,
            };
            (StatusCode::OK, Json(response)).into_response()
        }
        Err(e) => {
            error!(table = %table, error = %e, "Describe table failed");
            let error_response = SqlErrorResponse {
                error: e.to_string(),
                error_type: "TableNotFound".to_string(),
            };
            (StatusCode::NOT_FOUND, Json(error_response)).into_response()
        }
    }
}

/// Convert an Arrow array value at a given index to JSON
fn arrow_value_to_json(
    column: &chronik_columnar::datafusion::arrow::array::ArrayRef,
    row_idx: usize,
) -> serde_json::Value {
    use chronik_columnar::datafusion::arrow::array::*;

    if column.is_null(row_idx) {
        return serde_json::Value::Null;
    }

    // Handle different array types
    if let Some(arr) = column.as_any().downcast_ref::<Int64Array>() {
        return serde_json::Value::Number(arr.value(row_idx).into());
    }
    if let Some(arr) = column.as_any().downcast_ref::<Int32Array>() {
        return serde_json::Value::Number(arr.value(row_idx).into());
    }
    if let Some(arr) = column.as_any().downcast_ref::<Float64Array>() {
        return serde_json::json!(arr.value(row_idx));
    }
    if let Some(arr) = column.as_any().downcast_ref::<Float32Array>() {
        return serde_json::json!(arr.value(row_idx) as f64);
    }
    if let Some(arr) = column.as_any().downcast_ref::<StringArray>() {
        return serde_json::Value::String(arr.value(row_idx).to_string());
    }
    if let Some(arr) = column.as_any().downcast_ref::<BooleanArray>() {
        return serde_json::Value::Bool(arr.value(row_idx));
    }
    if let Some(arr) = column.as_any().downcast_ref::<BinaryArray>() {
        // Return binary as base64
        return serde_json::Value::String(base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            arr.value(row_idx),
        ));
    }
    if let Some(arr) = column.as_any().downcast_ref::<TimestampMillisecondArray>() {
        return serde_json::Value::Number(arr.value(row_idx).into());
    }

    // Fallback: convert to debug string
    serde_json::Value::String(format!("{:?}", column))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sql_request_defaults() {
        let json = r#"{"query": "SELECT * FROM test"}"#;
        let request: SqlRequest = serde_json::from_str(json).unwrap();
        assert_eq!(request.query, "SELECT * FROM test");
        assert_eq!(request.limit, 1000);
        assert_eq!(request.timeout_secs, 30);
    }

    #[test]
    fn test_sql_request_custom_limit() {
        let json = r#"{"query": "SELECT * FROM test", "limit": 100}"#;
        let request: SqlRequest = serde_json::from_str(json).unwrap();
        assert_eq!(request.limit, 100);
    }
}
