//! HTTP client for interacting with the Chronik Search service.
//!
//! This module provides a client for communicating with the Elasticsearch-compatible
//! search API provided by Chronik Stream. It can be used by other backend services
//! like the ingest service to index documents and perform searches.

use chronik_common::{Result, Error};
use reqwest::{Client, RequestBuilder, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::time::Duration;
use tracing::{debug, error, info, warn};

/// Configuration for the search client
#[derive(Debug, Clone)]
pub struct SearchClientConfig {
    /// Base URL for the search service (e.g., "http://search:9200")
    pub base_url: String,
    /// Request timeout
    pub timeout: Duration,
    /// Maximum number of retries for failed requests
    pub max_retries: u32,
    /// Initial retry delay
    pub retry_delay: Duration,
    /// Authentication token (optional)
    pub auth_token: Option<String>,
}

impl Default for SearchClientConfig {
    fn default() -> Self {
        Self {
            base_url: "http://localhost:9200".to_string(),
            timeout: Duration::from_secs(30),
            max_retries: 3,
            retry_delay: Duration::from_millis(100),
            auth_token: None,
        }
    }
}

/// HTTP client for the search service
#[derive(Clone)]
pub struct SearchClient {
    client: Client,
    config: SearchClientConfig,
}

impl SearchClient {
    /// Create a new search client
    pub fn new(config: SearchClientConfig) -> Result<Self> {
        let client = Client::builder()
            .timeout(config.timeout)
            .pool_idle_timeout(Duration::from_secs(90))
            .pool_max_idle_per_host(10)
            .build()
            .map_err(|e| Error::Network(format!("Failed to create HTTP client: {}", e)))?;

        Ok(Self { client, config })
    }

    /// Apply authentication to request if configured
    fn apply_auth(&self, request: RequestBuilder) -> RequestBuilder {
        if let Some(token) = &self.config.auth_token {
            request.bearer_auth(token)
        } else {
            request
        }
    }

    /// Execute request with retries
    async fn execute_with_retry<T>(&self, mut request_fn: impl FnMut() -> RequestBuilder) -> Result<T>
    where
        T: for<'de> Deserialize<'de>,
    {
        let mut last_error = None;
        let mut delay = self.config.retry_delay;

        for attempt in 0..=self.config.max_retries {
            if attempt > 0 {
                tokio::time::sleep(delay).await;
                delay *= 2; // Exponential backoff
            }

            let request = self.apply_auth(request_fn());
            
            match request.send().await {
                Ok(response) => {
                    let status = response.status();
                    
                    if status.is_success() {
                        return response.json::<T>().await
                            .map_err(|e| Error::Internal(format!("Failed to parse response: {}", e)));
                    }
                    
                    let error_text = response.text().await.unwrap_or_else(|_| "Unknown error".to_string());
                    
                    // Don't retry on client errors (4xx)
                    if status.is_client_error() {
                        return Err(Error::InvalidInput(format!("Request failed: {}", error_text)));
                    }
                    
                    last_error = Some(format!("HTTP {}: {}", status, error_text));
                }
                Err(e) => {
                    last_error = Some(format!("Request failed: {}", e));
                }
            }
            
            if attempt < self.config.max_retries {
                warn!("Request failed (attempt {}/{}), retrying...", attempt + 1, self.config.max_retries);
            }
        }

        Err(Error::Network(format!(
            "Request failed after {} attempts: {}",
            self.config.max_retries + 1,
            last_error.unwrap_or_else(|| "Unknown error".to_string())
        )))
    }

    /// Check if the search service is healthy
    pub async fn health_check(&self) -> Result<HealthResponse> {
        let url = format!("{}/health", self.config.base_url);
        
        self.execute_with_retry(|| self.client.get(&url)).await
    }

    /// Create an index with optional mapping
    pub async fn create_index(&self, index: &str, mapping: Option<IndexMapping>) -> Result<CreateIndexResponse> {
        let url = format!("{}/{}", self.config.base_url, index);
        
        let body = if let Some(mapping) = mapping {
            json!({ "mappings": mapping })
        } else {
            json!({})
        };

        self.execute_with_retry(|| self.client.put(&url).json(&body)).await
    }

    /// Delete an index
    pub async fn delete_index(&self, index: &str) -> Result<DeleteIndexResponse> {
        let url = format!("{}/{}", self.config.base_url, index);
        
        self.execute_with_retry(|| self.client.delete(&url)).await
    }

    /// Get index mapping
    pub async fn get_mapping(&self, index: &str) -> Result<Value> {
        let url = format!("{}/{}/_mapping", self.config.base_url, index);
        
        self.execute_with_retry(|| self.client.get(&url)).await
    }

    /// Index a document
    pub async fn index_document(
        &self,
        index: &str,
        id: Option<&str>,
        document: &Value,
    ) -> Result<IndexDocumentResponse> {
        let url = if let Some(id) = id {
            format!("{}/{}/_doc/{}", self.config.base_url, index, id)
        } else {
            format!("{}/{}/_doc", self.config.base_url, index)
        };

        self.execute_with_retry(|| self.client.post(&url).json(document)).await
    }

    /// Bulk index documents
    pub async fn bulk(&self, operations: Vec<BulkOperation>) -> Result<BulkResponse> {
        let url = format!("{}/_bulk", self.config.base_url);
        
        // Build NDJSON body
        let mut body = String::new();
        for op in operations {
            match op {
                BulkOperation::Index { index, id, document } => {
                    let action = if let Some(id) = id {
                        json!({ "index": { "_index": index, "_id": id } })
                    } else {
                        json!({ "index": { "_index": index } })
                    };
                    body.push_str(&action.to_string());
                    body.push('\n');
                    body.push_str(&document.to_string());
                    body.push('\n');
                }
                BulkOperation::Delete { index, id } => {
                    let action = json!({ "delete": { "_index": index, "_id": id } });
                    body.push_str(&action.to_string());
                    body.push('\n');
                }
            }
        }

        self.execute_with_retry(|| {
            self.client
                .post(&url)
                .header("Content-Type", "application/x-ndjson")
                .body(body.clone())
        })
        .await
    }

    /// Get a document by ID
    pub async fn get_document(&self, index: &str, id: &str) -> Result<GetDocumentResponse> {
        let url = format!("{}/{}/_doc/{}", self.config.base_url, index, id);
        
        self.execute_with_retry(|| self.client.get(&url)).await
    }

    /// Delete a document
    pub async fn delete_document(&self, index: &str, id: &str) -> Result<DeleteDocumentResponse> {
        let url = format!("{}/{}/_doc/{}", self.config.base_url, index, id);
        
        self.execute_with_retry(|| self.client.delete(&url)).await
    }

    /// Search documents
    pub async fn search(&self, index: Option<&str>, query: SearchQuery) -> Result<SearchResponse> {
        let url = if let Some(index) = index {
            format!("{}/{}/_search", self.config.base_url, index)
        } else {
            format!("{}/_search", self.config.base_url)
        };

        self.execute_with_retry(|| self.client.post(&url).json(&query)).await
    }

    /// Simple search with query string
    pub async fn search_simple(
        &self,
        index: &str,
        query_string: &str,
        size: Option<usize>,
    ) -> Result<SearchResponse> {
        let query = SearchQuery {
            query: Some(json!({
                "query_string": {
                    "query": query_string
                }
            })),
            size,
            ..Default::default()
        };

        self.search(Some(index), query).await
    }

    /// List all indices
    pub async fn list_indices(&self) -> Result<Vec<CatIndexInfo>> {
        let url = format!("{}/_cat/indices?format=json", self.config.base_url);
        
        self.execute_with_retry(|| self.client.get(&url)).await
    }

    /// Get cluster stats
    pub async fn cluster_stats(&self) -> Result<Value> {
        let url = format!("{}/_cluster/stats", self.config.base_url);
        
        self.execute_with_retry(|| self.client.get(&url)).await
    }

    /// Index Kafka messages for real-time search
    pub async fn index_kafka_message(
        &self,
        topic: &str,
        partition: i32,
        offset: i64,
        timestamp: i64,
        key: Option<&[u8]>,
        value: &[u8],
        headers: &HashMap<String, Vec<u8>>,
    ) -> Result<IndexDocumentResponse> {
        let doc_id = format!("{}-{}-{}", topic, partition, offset);
        
        let mut document = serde_json::Map::new();
        document.insert("topic".to_string(), json!(topic));
        document.insert("partition".to_string(), json!(partition));
        document.insert("offset".to_string(), json!(offset));
        document.insert("timestamp".to_string(), json!(timestamp));
        
        // Convert key and value to strings if possible
        if let Some(key_bytes) = key {
            let key_str = String::from_utf8_lossy(key_bytes);
            document.insert("key".to_string(), json!(key_str));
        }
        
        let value_str = String::from_utf8_lossy(value);
        document.insert("value".to_string(), json!(value_str));
        
        // Try to parse value as JSON
        if let Ok(json_value) = serde_json::from_slice::<Value>(value) {
            document.insert("json_content".to_string(), json_value);
        }
        
        // Convert headers
        if !headers.is_empty() {
            let headers_map: HashMap<String, String> = headers
                .iter()
                .map(|(k, v)| (k.clone(), String::from_utf8_lossy(v).to_string()))
                .collect();
            document.insert("headers".to_string(), json!(headers_map));
        }
        
        // Use topic-specific index
        let index = format!("kafka-{}", topic.replace('.', "_").to_lowercase());
        
        self.index_document(&index, Some(&doc_id), &Value::Object(document)).await
    }

    /// Batch index Kafka messages
    pub async fn bulk_index_kafka_messages(
        &self,
        messages: Vec<KafkaMessage>,
    ) -> Result<BulkResponse> {
        let operations: Vec<BulkOperation> = messages
            .into_iter()
            .map(|msg| {
                let doc_id = format!("{}-{}-{}", msg.topic, msg.partition, msg.offset);
                
                let mut document = serde_json::Map::new();
                document.insert("topic".to_string(), json!(msg.topic));
                document.insert("partition".to_string(), json!(msg.partition));
                document.insert("offset".to_string(), json!(msg.offset));
                document.insert("timestamp".to_string(), json!(msg.timestamp));
                
                if let Some(key) = msg.key {
                    let key_str = String::from_utf8_lossy(&key);
                    document.insert("key".to_string(), json!(key_str));
                }
                
                let value_str = String::from_utf8_lossy(&msg.value);
                document.insert("value".to_string(), json!(value_str));
                
                // Try to parse value as JSON
                if let Ok(json_value) = serde_json::from_slice::<Value>(&msg.value) {
                    document.insert("json_content".to_string(), json_value);
                }
                
                if !msg.headers.is_empty() {
                    let headers_map: HashMap<String, String> = msg.headers
                        .iter()
                        .map(|(k, v)| (k.clone(), String::from_utf8_lossy(v).to_string()))
                        .collect();
                    document.insert("headers".to_string(), json!(headers_map));
                }
                
                let index = format!("kafka-{}", msg.topic.replace('.', "_").to_lowercase());
                
                BulkOperation::Index {
                    index,
                    id: Some(doc_id),
                    document: Value::Object(document),
                }
            })
            .collect();

        self.bulk(operations).await
    }
}

/// Kafka message to be indexed
#[derive(Debug, Clone)]
pub struct KafkaMessage {
    pub topic: String,
    pub partition: i32,
    pub offset: i64,
    pub timestamp: i64,
    pub key: Option<Vec<u8>>,
    pub value: Vec<u8>,
    pub headers: HashMap<String, Vec<u8>>,
}

/// Bulk operation type
#[derive(Debug, Clone)]
pub enum BulkOperation {
    Index {
        index: String,
        id: Option<String>,
        document: Value,
    },
    Delete {
        index: String,
        id: String,
    },
}

/// Search query structure
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SearchQuery {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub query: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sort: Option<Vec<Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aggs: Option<HashMap<String, Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub _source: Option<SourceFilter>,
}

/// Source filter for search results
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SourceFilter {
    Bool(bool),
    Fields(Vec<String>),
}

/// Index mapping
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexMapping {
    pub properties: HashMap<String, FieldMapping>,
}

/// Field mapping
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FieldMapping {
    #[serde(rename = "type")]
    pub field_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub analyzer: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub store: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index: Option<bool>,
}

/// Response types
#[derive(Debug, Clone, Deserialize)]
pub struct HealthResponse {
    pub status: String,
    pub service: String,
    pub version: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CreateIndexResponse {
    pub acknowledged: bool,
    pub shards_acknowledged: bool,
    pub index: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DeleteIndexResponse {
    pub acknowledged: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct IndexDocumentResponse {
    pub _index: String,
    pub _id: String,
    pub _version: i64,
    pub result: String,
    pub _shards: ShardInfo,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GetDocumentResponse {
    pub _index: String,
    pub _id: String,
    pub _version: i64,
    pub found: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub _source: Option<Value>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DeleteDocumentResponse {
    pub _index: String,
    pub _id: String,
    pub _version: i64,
    pub result: String,
    pub _shards: ShardInfo,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SearchResponse {
    pub took: u64,
    pub timed_out: bool,
    pub _shards: ShardInfo,
    pub hits: HitsInfo,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aggregations: Option<HashMap<String, Value>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ShardInfo {
    pub total: u32,
    pub successful: u32,
    pub skipped: u32,
    pub failed: u32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct HitsInfo {
    pub total: TotalHits,
    pub max_score: Option<f32>,
    pub hits: Vec<Hit>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TotalHits {
    pub value: u64,
    pub relation: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Hit {
    pub _index: String,
    pub _id: String,
    pub _score: Option<f32>,
    pub _source: Value,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BulkResponse {
    pub took: u64,
    pub errors: bool,
    pub items: Vec<BulkResponseItem>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BulkResponseItem {
    #[serde(flatten)]
    pub action: HashMap<String, BulkActionResult>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BulkActionResult {
    pub _index: String,
    pub _id: String,
    pub _version: Option<i64>,
    pub result: Option<String>,
    pub status: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<BulkError>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BulkError {
    #[serde(rename = "type")]
    pub error_type: String,
    pub reason: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CatIndexInfo {
    pub health: String,
    pub status: String,
    pub index: String,
    pub uuid: String,
    pub pri: String,
    pub rep: String,
    #[serde(rename = "docs.count")]
    pub docs_count: Option<String>,
    #[serde(rename = "store.size")]
    pub store_size: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_search_client_config() {
        let config = SearchClientConfig::default();
        assert_eq!(config.base_url, "http://localhost:9200");
        assert_eq!(config.timeout, Duration::from_secs(30));
        assert_eq!(config.max_retries, 3);
    }

    #[test]
    fn test_bulk_operation_serialization() {
        let op = BulkOperation::Index {
            index: "test".to_string(),
            id: Some("123".to_string()),
            document: json!({ "field": "value" }),
        };

        match op {
            BulkOperation::Index { index, id, document } => {
                assert_eq!(index, "test");
                assert_eq!(id, Some("123".to_string()));
                assert_eq!(document["field"], "value");
            }
            _ => panic!("Wrong operation type"),
        }
    }

    #[test]
    fn test_kafka_message_creation() {
        let msg = KafkaMessage {
            topic: "test-topic".to_string(),
            partition: 0,
            offset: 100,
            timestamp: 1234567890,
            key: Some(b"key".to_vec()),
            value: b"value".to_vec(),
            headers: HashMap::new(),
        };

        assert_eq!(msg.topic, "test-topic");
        assert_eq!(msg.partition, 0);
        assert_eq!(msg.offset, 100);
    }
}