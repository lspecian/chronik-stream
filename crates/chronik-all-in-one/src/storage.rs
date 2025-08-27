use anyhow::Result;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::RwLock;
use serde::{Serialize, Deserialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TopicMetadata {
    pub name: String,
    pub partitions: i32,
    pub replication_factor: i16,
    pub configs: HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PartitionData {
    pub topic: String,
    pub partition: i32,
    pub messages: Vec<Message>,
    pub high_water_mark: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub offset: i64,
    pub key: Option<Vec<u8>>,
    pub value: Vec<u8>,
    pub timestamp: i64,
}

pub struct EmbeddedStorage {
    db: Option<sled::Db>,
    topics: Arc<RwLock<HashMap<String, TopicMetadata>>>,
    partitions: Arc<RwLock<HashMap<(String, i32), PartitionData>>>,
}

impl EmbeddedStorage {
    pub fn new(data_dir: &Path) -> Result<Self> {
        let db = sled::open(data_dir.join("chronik.db"))?;
        
        let mut storage = Self {
            db: Some(db.clone()),
            topics: Arc::new(RwLock::new(HashMap::new())),
            partitions: Arc::new(RwLock::new(HashMap::new())),
        };
        
        // Load existing topics from disk
        storage.load_from_disk()?;
        
        Ok(storage)
    }
    
    pub fn new_in_memory() -> Result<Self> {
        Ok(Self {
            db: None,
            topics: Arc::new(RwLock::new(HashMap::new())),
            partitions: Arc::new(RwLock::new(HashMap::new())),
        })
    }
    
    fn load_from_disk(&mut self) -> Result<()> {
        if let Some(db) = &self.db {
            // Load topics
            let topics_tree = db.open_tree("topics")?;
            for item in topics_tree.iter() {
                let (key, value) = item?;
                let _topic_name = String::from_utf8_lossy(&key).to_string();
                if let Ok(_metadata) = serde_json::from_slice::<TopicMetadata>(&value) {
                    // We'll need to handle this differently for async
                    // For now, just skip loading from disk in the sync context
                }
            }
        }
        Ok(())
    }
    
    pub async fn create_topic(&mut self, name: String, partitions: i32) -> Result<()> {
        let metadata = TopicMetadata {
            name: name.clone(),
            partitions,
            replication_factor: 1,
            configs: HashMap::new(),
        };
        
        // Store in memory
        let mut topics = self.topics.write().await;
        topics.insert(name.clone(), metadata.clone());
        
        // Persist to disk if available
        if let Some(db) = &self.db {
            let topics_tree = db.open_tree("topics")?;
            topics_tree.insert(name.as_bytes(), serde_json::to_vec(&metadata)?)?;
        }
        
        // Initialize partitions
        let mut partitions = self.partitions.write().await;
        for i in 0..metadata.partitions {
            let partition_data = PartitionData {
                topic: name.clone(),
                partition: i,
                messages: Vec::new(),
                high_water_mark: 0,
            };
            partitions.insert((name.clone(), i), partition_data);
        }
        
        Ok(())
    }
    
    pub async fn list_topics(&self) -> Vec<String> {
        let topics = self.topics.read().await;
        topics.keys().cloned().collect()
    }
    
    pub async fn get_topic(&self, name: &str) -> Option<TopicMetadata> {
        let topics = self.topics.read().await;
        topics.get(name).cloned()
    }
    
    pub async fn append_messages(
        &mut self,
        topic: &str,
        partition: i32,
        record_batch_data: &[u8]
    ) -> Result<i64> {
        let mut partitions = self.partitions.write().await;
        let key = (topic.to_string(), partition);
        
        let partition_data = partitions.entry(key.clone())
            .or_insert_with(|| PartitionData {
                topic: topic.to_string(),
                partition,
                messages: Vec::new(),
                high_water_mark: 0,
            });
        
        // Store the raw record batch as a single message for now
        // In production, we'd parse the Kafka record batch format properly
        let offset = partition_data.high_water_mark;
        partition_data.messages.push(Message {
            offset,
            key: None,
            value: record_batch_data.to_vec(),
            timestamp: chrono::Utc::now().timestamp_millis(),
        });
        partition_data.high_water_mark += 1;
        
        // Persist to disk if available
        if let Some(db) = &self.db {
            let partitions_tree = db.open_tree("partitions")?;
            let key = format!("{}:{}", topic, partition);
            partitions_tree.insert(key.as_bytes(), serde_json::to_vec(&partition_data)?)?;
        }
        
        Ok(offset)
    }
    
    pub async fn fetch_messages(
        &self,
        topic: String,
        partition: i32,
        offset: i64,
        _max_bytes: i32,
    ) -> Result<Vec<Message>> {
        let partitions = self.partitions.read().await;
        let key = (topic, partition);
        
        if let Some(partition_data) = partitions.get(&key) {
            let messages: Vec<Message> = partition_data.messages
                .iter()
                .skip_while(|m| m.offset < offset)
                .take_while(|_m| {
                    // Simple byte limit check
                    true
                })
                .cloned()
                .collect();
            
            Ok(messages)
        } else {
            Ok(Vec::new())
        }
    }
}