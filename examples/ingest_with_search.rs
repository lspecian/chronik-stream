//! Example demonstrating how the ingest service can integrate with the search service
//! to provide real-time indexing of Kafka messages.

use chronik_search::{SearchClient, SearchClientConfig, KafkaMessage};
use std::collections::HashMap;
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{info, error};

/// Example configuration for integrating search into the ingest service
#[derive(Debug, Clone)]
struct IngestSearchConfig {
    /// Enable real-time indexing
    pub enable_indexing: bool,
    /// Search service URL
    pub search_url: String,
    /// Batch size for bulk indexing
    pub batch_size: usize,
    /// Batch timeout
    pub batch_timeout: Duration,
    /// Maximum retries for failed indexing
    pub max_retries: u32,
}

impl Default for IngestSearchConfig {
    fn default() -> Self {
        Self {
            enable_indexing: true,
            search_url: "http://search:9200".to_string(),
            batch_size: 100,
            batch_timeout: Duration::from_secs(1),
            max_retries: 3,
        }
    }
}

/// Search indexing component for the ingest service
struct IngestSearchIndexer {
    config: IngestSearchConfig,
    client: SearchClient,
    batch_sender: mpsc::Sender<KafkaMessage>,
}

impl IngestSearchIndexer {
    /// Create a new search indexer
    async fn new(config: IngestSearchConfig) -> anyhow::Result<Self> {
        let search_config = SearchClientConfig {
            base_url: config.search_url.clone(),
            timeout: Duration::from_secs(30),
            max_retries: config.max_retries,
            retry_delay: Duration::from_millis(100),
            auth_token: None,
        };

        let client = SearchClient::new(search_config)?;
        
        // Check if search service is available
        match client.health_check().await {
            Ok(health) => {
                info!("Connected to search service: {}", health.status);
            }
            Err(e) => {
                error!("Search service not available: {}", e);
                if config.enable_indexing {
                    return Err(anyhow::anyhow!("Search service required but not available"));
                }
            }
        }

        // Create channel for batching
        let (batch_sender, batch_receiver) = mpsc::channel(1000);
        
        // Start batch processor
        let client_clone = client.clone();
        let batch_config = config.clone();
        tokio::spawn(async move {
            Self::batch_processor(client_clone, batch_config, batch_receiver).await;
        });

        Ok(Self {
            config,
            client,
            batch_sender,
        })
    }

    /// Index a single message (non-blocking)
    async fn index_message(
        &self,
        topic: &str,
        partition: i32,
        offset: i64,
        timestamp: i64,
        key: Option<&[u8]>,
        value: &[u8],
        headers: &HashMap<String, Vec<u8>>,
    ) -> anyhow::Result<()> {
        if !self.config.enable_indexing {
            return Ok(());
        }

        let message = KafkaMessage {
            topic: topic.to_string(),
            partition,
            offset,
            timestamp,
            key: key.map(|k| k.to_vec()),
            value: value.to_vec(),
            headers: headers.clone(),
        };

        // Send to batch processor (non-blocking)
        if let Err(e) = self.batch_sender.try_send(message) {
            error!("Failed to queue message for indexing: {}", e);
        }

        Ok(())
    }

    /// Batch processor for efficient bulk indexing
    async fn batch_processor(
        client: SearchClient,
        config: IngestSearchConfig,
        mut receiver: mpsc::Receiver<KafkaMessage>,
    ) {
        let mut batch = Vec::with_capacity(config.batch_size);
        let mut batch_timer = tokio::time::interval(config.batch_timeout);

        loop {
            tokio::select! {
                Some(message) = receiver.recv() => {
                    batch.push(message);
                    
                    if batch.len() >= config.batch_size {
                        Self::flush_batch(&client, &mut batch).await;
                    }
                }
                _ = batch_timer.tick() => {
                    if !batch.is_empty() {
                        Self::flush_batch(&client, &mut batch).await;
                    }
                }
            }
        }
    }

    /// Flush a batch of messages to the search service
    async fn flush_batch(client: &SearchClient, batch: &mut Vec<KafkaMessage>) {
        if batch.is_empty() {
            return;
        }

        let batch_size = batch.len();
        let messages = std::mem::take(batch);

        match client.bulk_index_kafka_messages(messages).await {
            Ok(response) => {
                if response.errors {
                    error!("Some messages failed to index in batch of {}", batch_size);
                    // Could analyze response.items for specific errors
                } else {
                    info!("Successfully indexed batch of {} messages", batch_size);
                }
            }
            Err(e) => {
                error!("Failed to index batch of {} messages: {}", batch_size, e);
                // Could implement retry logic or dead letter queue here
            }
        }
    }

    /// Create indices for topics
    async fn ensure_topic_index(&self, topic: &str) -> anyhow::Result<()> {
        use chronik_search::client::{IndexMapping, FieldMapping};
        
        let index_name = format!("kafka-{}", topic.replace('.', "_").to_lowercase());
        
        // Check if index exists
        match self.client.get_mapping(&index_name).await {
            Ok(_) => {
                info!("Index {} already exists", index_name);
                return Ok(());
            }
            Err(_) => {
                // Index doesn't exist, create it
            }
        }

        // Define mapping for Kafka messages
        let mut properties = HashMap::new();
        
        properties.insert("topic".to_string(), FieldMapping {
            field_type: "keyword".to_string(),
            analyzer: None,
            store: Some(true),
            index: Some(true),
        });
        
        properties.insert("partition".to_string(), FieldMapping {
            field_type: "long".to_string(),
            analyzer: None,
            store: Some(true),
            index: Some(true),
        });
        
        properties.insert("offset".to_string(), FieldMapping {
            field_type: "long".to_string(),
            analyzer: None,
            store: Some(true),
            index: Some(true),
        });
        
        properties.insert("timestamp".to_string(), FieldMapping {
            field_type: "long".to_string(),
            analyzer: None,
            store: Some(true),
            index: Some(true),
        });
        
        properties.insert("key".to_string(), FieldMapping {
            field_type: "text".to_string(),
            analyzer: Some("standard".to_string()),
            store: Some(true),
            index: Some(true),
        });
        
        properties.insert("value".to_string(), FieldMapping {
            field_type: "text".to_string(),
            analyzer: Some("standard".to_string()),
            store: Some(true),
            index: Some(true),
        });
        
        properties.insert("json_content".to_string(), FieldMapping {
            field_type: "object".to_string(),
            analyzer: None,
            store: Some(true),
            index: Some(true),
        });
        
        properties.insert("headers".to_string(), FieldMapping {
            field_type: "object".to_string(),
            analyzer: None,
            store: Some(true),
            index: Some(true),
        });
        
        let mapping = IndexMapping { properties };
        
        match self.client.create_index(&index_name, Some(mapping)).await {
            Ok(response) => {
                info!("Created index {}: acknowledged={}", index_name, response.acknowledged);
                Ok(())
            }
            Err(e) => {
                error!("Failed to create index {}: {}", index_name, e);
                Err(e.into())
            }
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Initialize logging
    tracing_subscriber::fmt::init();

    // Example: How the ingest service would integrate search
    info!("Starting ingest service with search integration");

    // Create search indexer
    let search_config = IngestSearchConfig {
        search_url: "http://localhost:9200".to_string(),
        batch_size: 50,
        batch_timeout: Duration::from_millis(500),
        ..Default::default()
    };

    let indexer = IngestSearchIndexer::new(search_config).await?;

    // Ensure indices exist for topics
    indexer.ensure_topic_index("orders").await?;
    indexer.ensure_topic_index("payments").await?;

    // Simulate processing messages
    info!("Simulating message processing with indexing");

    // Order messages
    for i in 0..100 {
        let key = format!("order-{}", i);
        let value = serde_json::json!({
            "order_id": format!("order-{}", i),
            "customer_id": format!("customer-{}", i % 10),
            "amount": 100.0 + (i as f64),
            "status": if i % 3 == 0 { "completed" } else { "pending" },
            "timestamp": chrono::Utc::now().to_rfc3339(),
        });

        let mut headers = HashMap::new();
        headers.insert("type".to_string(), b"order_created".to_vec());
        headers.insert("version".to_string(), b"1.0".to_vec());

        indexer.index_message(
            "orders",
            i % 3,  // partition
            i as i64,  // offset
            chrono::Utc::now().timestamp_millis(),
            Some(key.as_bytes()),
            value.to_string().as_bytes(),
            &headers,
        ).await?;
    }

    // Payment messages
    for i in 0..50 {
        let key = format!("payment-{}", i);
        let value = serde_json::json!({
            "payment_id": format!("payment-{}", i),
            "order_id": format!("order-{}", i * 2),
            "amount": 100.0 + (i as f64 * 2.0),
            "method": if i % 2 == 0 { "credit_card" } else { "paypal" },
            "status": "success",
            "timestamp": chrono::Utc::now().to_rfc3339(),
        });

        let mut headers = HashMap::new();
        headers.insert("type".to_string(), b"payment_processed".to_vec());

        indexer.index_message(
            "payments",
            i % 2,  // partition
            i as i64,  // offset
            chrono::Utc::now().timestamp_millis(),
            Some(key.as_bytes()),
            value.to_string().as_bytes(),
            &headers,
        ).await?;
    }

    // Wait for batches to flush
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Perform some searches
    info!("Performing search queries");

    // Search for completed orders
    let results = indexer.client.search_simple(
        "kafka-orders",
        "status:completed",
        Some(10),
    ).await?;
    
    info!("Found {} completed orders", results.hits.total.value);

    // Search for high-value orders
    let query = serde_json::json!({
        "query": {
            "range": {
                "json_content.amount": {
                    "gte": 150.0
                }
            }
        },
        "size": 10,
        "sort": [
            { "json_content.amount": { "order": "desc" } }
        ]
    });

    let results = indexer.client.search(
        Some("kafka-orders"),
        serde_json::from_value(query)?,
    ).await?;

    info!("Found {} high-value orders", results.hits.total.value);
    
    for hit in results.hits.hits.iter().take(5) {
        if let Some(amount) = hit._source.get("json_content")
            .and_then(|c| c.get("amount"))
            .and_then(|a| a.as_f64()) {
            info!("Order {}: ${:.2}", hit._id, amount);
        }
    }

    // Search across both indices
    let results = indexer.client.search_simple(
        "kafka-*",
        "success",
        Some(20),
    ).await?;

    info!("Found {} documents with 'success' across all indices", results.hits.total.value);

    info!("Ingest with search integration example completed");
    Ok(())
}