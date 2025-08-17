use anyhow::Result;
use chronik_common::metadata::{MetadataStore, TopicMetadata, ConsumerGroupMetadata, BrokerMetadata};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::RwLock;
use async_trait::async_trait;

/// Embedded metadata store using RocksDB or in-memory storage
pub struct EmbeddedMetadataStore {
    topics: Arc<RwLock<HashMap<String, TopicMetadata>>>,
    groups: Arc<RwLock<HashMap<String, ConsumerGroupMetadata>>>,
    brokers: Arc<RwLock<HashMap<i32, BrokerMetadata>>>,
    db: Option<rocksdb::DB>,
}

impl EmbeddedMetadataStore {
    /// Create a new embedded store with RocksDB persistence
    pub fn new(data_dir: &Path) -> Result<Self> {
        let db_path = data_dir.join("metadata.db");
        let db = rocksdb::DB::open_default(&db_path)?;
        
        let mut store = Self {
            topics: Arc::new(RwLock::new(HashMap::new())),
            groups: Arc::new(RwLock::new(HashMap::new())),
            brokers: Arc::new(RwLock::new(HashMap::new())),
            db: Some(db),
        };
        
        // Load existing data from RocksDB
        store.load_from_disk()?;
        
        // Register self as broker 1
        let broker = BrokerMetadata {
            id: 1,
            host: "localhost".to_string(),
            port: 9092,
            rack: None,
        };
        
        tokio::runtime::Handle::current().block_on(async {
            store.register_broker(broker).await
        })?;
        
        Ok(store)
    }
    
    /// Create an in-memory only store (for standalone mode)
    pub fn new_in_memory() -> Result<Self> {
        let mut store = Self {
            topics: Arc::new(RwLock::new(HashMap::new())),
            groups: Arc::new(RwLock::new(HashMap::new())),
            brokers: Arc::new(RwLock::new(HashMap::new())),
            db: None,
        };
        
        // Register self as broker 1
        let broker = BrokerMetadata {
            id: 1,
            host: "localhost".to_string(),
            port: 9092,
            rack: None,
        };
        
        tokio::runtime::Handle::current().block_on(async {
            store.register_broker(broker).await
        })?;
        
        Ok(store)
    }
    
    fn load_from_disk(&mut self) -> Result<()> {
        if let Some(db) = &self.db {
            // Load topics
            let iter = db.prefix_iterator(b"topic:");
            for item in iter {
                let (key, value) = item?;
                if let Ok(key_str) = std::str::from_utf8(&key) {
                    if let Some(topic_name) = key_str.strip_prefix("topic:") {
                        if let Ok(metadata) = serde_json::from_slice::<TopicMetadata>(&value) {
                            tokio::runtime::Handle::current().block_on(async {
                                self.topics.write().await.insert(topic_name.to_string(), metadata);
                            });
                        }
                    }
                }
            }
            
            // Load consumer groups
            let iter = db.prefix_iterator(b"group:");
            for item in iter {
                let (key, value) = item?;
                if let Ok(key_str) = std::str::from_utf8(&key) {
                    if let Some(group_id) = key_str.strip_prefix("group:") {
                        if let Ok(metadata) = serde_json::from_slice::<ConsumerGroupMetadata>(&value) {
                            tokio::runtime::Handle::current().block_on(async {
                                self.groups.write().await.insert(group_id.to_string(), metadata);
                            });
                        }
                    }
                }
            }
        }
        Ok(())
    }
    
    async fn persist_topic(&self, name: &str, metadata: &TopicMetadata) -> Result<()> {
        if let Some(db) = &self.db {
            let key = format!("topic:{}", name);
            let value = serde_json::to_vec(metadata)?;
            db.put(key.as_bytes(), &value)?;
        }
        Ok(())
    }
    
    async fn persist_group(&self, id: &str, metadata: &ConsumerGroupMetadata) -> Result<()> {
        if let Some(db) = &self.db {
            let key = format!("group:{}", id);
            let value = serde_json::to_vec(metadata)?;
            db.put(key.as_bytes(), &value)?;
        }
        Ok(())
    }
}

#[async_trait]
impl MetadataStore for EmbeddedMetadataStore {
    async fn create_topic(&self, metadata: TopicMetadata) -> Result<()> {
        let name = metadata.name.clone();
        self.topics.write().await.insert(name.clone(), metadata.clone());
        self.persist_topic(&name, &metadata).await?;
        Ok(())
    }
    
    async fn get_topic(&self, name: &str) -> Result<Option<TopicMetadata>> {
        Ok(self.topics.read().await.get(name).cloned())
    }
    
    async fn list_topics(&self) -> Result<Vec<TopicMetadata>> {
        Ok(self.topics.read().await.values().cloned().collect())
    }
    
    async fn delete_topic(&self, name: &str) -> Result<()> {
        self.topics.write().await.remove(name);
        if let Some(db) = &self.db {
            let key = format!("topic:{}", name);
            db.delete(key.as_bytes())?;
        }
        Ok(())
    }
    
    async fn register_broker(&self, metadata: BrokerMetadata) -> Result<()> {
        self.brokers.write().await.insert(metadata.id, metadata);
        Ok(())
    }
    
    async fn get_broker(&self, id: i32) -> Result<Option<BrokerMetadata>> {
        Ok(self.brokers.read().await.get(&id).cloned())
    }
    
    async fn list_brokers(&self) -> Result<Vec<BrokerMetadata>> {
        Ok(self.brokers.read().await.values().cloned().collect())
    }
    
    async fn create_consumer_group(&self, metadata: ConsumerGroupMetadata) -> Result<()> {
        let id = metadata.group_id.clone();
        self.groups.write().await.insert(id.clone(), metadata.clone());
        self.persist_group(&id, &metadata).await?;
        Ok(())
    }
    
    async fn get_consumer_group(&self, group_id: &str) -> Result<Option<ConsumerGroupMetadata>> {
        Ok(self.groups.read().await.get(group_id).cloned())
    }
    
    async fn update_consumer_group(&self, metadata: ConsumerGroupMetadata) -> Result<()> {
        let id = metadata.group_id.clone();
        self.groups.write().await.insert(id.clone(), metadata.clone());
        self.persist_group(&id, &metadata).await?;
        Ok(())
    }
    
    async fn list_consumer_groups(&self) -> Result<Vec<ConsumerGroupMetadata>> {
        Ok(self.groups.read().await.values().cloned().collect())
    }
}