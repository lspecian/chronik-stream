//! Test compilation of admin service and metadata sync modules

// Core dependencies
use chronik_common::Error;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::sync::RwLock;

// Basic types needed for our modules
#[derive(Clone, Debug)]
pub struct ControllerState {
    pub topics: HashMap<String, TopicConfig>,
    pub brokers: HashMap<u32, BrokerInfo>,
    pub consumer_groups: HashMap<String, ConsumerGroup>,
    pub controller_id: Option<u32>,
    pub leader_epoch: u64,
    pub metadata_version: u64,
}

impl Default for ControllerState {
    fn default() -> Self {
        Self {
            topics: HashMap::new(),
            brokers: HashMap::new(),
            consumer_groups: HashMap::new(),
            controller_id: None,
            leader_epoch: 0,
            metadata_version: 0,
        }
    }
}

impl ControllerState {
    pub fn is_healthy(&self) -> bool {
        !self.brokers.is_empty()
    }

    pub fn total_partitions(&self) -> usize {
        self.topics.values().map(|t| t.partition_count as usize).sum()
    }

    pub fn get_online_brokers(&self) -> Vec<&BrokerInfo> {
        self.brokers.values()
            .filter(|b| b.status.as_deref() == Some("online"))
            .collect()
    }

    pub fn get_health_issues(&self) -> Vec<String> {
        let mut issues = Vec::new();
        
        if self.brokers.is_empty() {
            issues.push("No brokers available".to_string());
        }
        
        let offline_brokers = self.brokers.values()
            .filter(|b| b.status.as_deref() != Some("online"))
            .count();
            
        if offline_brokers > 0 {
            issues.push(format!("{} brokers offline", offline_brokers));
        }
        
        issues
    }

    pub fn increment_metadata_version(&mut self) {
        self.metadata_version += 1;
    }
}

#[derive(Clone, Debug)]
pub struct TopicConfig {
    pub name: String,
    pub partition_count: u32,
    pub replication_factor: u16,
    pub config: HashMap<String, String>,
    pub created_at: SystemTime,
    pub version: u64,
}

#[derive(Clone, Debug)]
pub struct BrokerInfo {
    pub id: u32,
    pub address: std::net::SocketAddr,
    pub rack: Option<String>,
    pub status: Option<String>,
    pub version: Option<String>,
    pub metadata_version: Option<u64>,
}

#[derive(Clone, Debug)]
pub struct ConsumerGroup {
    pub group_id: String,
    pub topics: Vec<String>,
    pub members: Vec<GroupMember>,
    pub state: GroupState,
}

#[derive(Clone, Debug)]
pub struct GroupMember {
    pub member_id: String,
    pub client_id: String,
    pub host: String,
    pub session_timeout_ms: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub enum GroupState {
    Empty,
    PreparingRebalance,
    CompletingRebalance,
    Stable,
    Dead,
}

// Mock metastore implementation
pub struct ControllerMetadataStore {
    path: std::path::PathBuf,
}

impl ControllerMetadataStore {
    pub fn new<P: AsRef<std::path::Path>>(path: P) -> Result<Self, Error> {
        Ok(Self {
            path: path.as_ref().to_path_buf(),
        })
    }

    pub async fn init(&self) -> Result<(), Error> {
        Ok(())
    }

    pub async fn get_all_brokers(&self) -> Result<Vec<BrokerInfo>, Error> {
        Ok(vec![])
    }

    pub async fn create_topic(&self, _topic: &TopicConfig) -> Result<(), Error> {
        Ok(())
    }

    pub async fn delete_topic(&self, _name: &str) -> Result<(), Error> {
        Ok(())
    }

    pub async fn update_topic_config(&self, _name: &str, _config: &HashMap<String, String>) -> Result<(), Error> {
        Ok(())
    }
}

// Include the modules we want to test
pub mod metadata_sync {
    use super::*;
    include!("metadata_sync.rs");
}

pub mod admin_service {
    use super::*;
    include!("admin_service.rs");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_admin_service_compilation() {
        let temp_dir = std::env::temp_dir().join("chronik-test");
        std::fs::create_dir_all(&temp_dir).unwrap();

        let metadata_store = Arc::new(ControllerMetadataStore::new(&temp_dir).unwrap());
        let cluster_state = Arc::new(RwLock::new(ControllerState::default()));
        let sync_manager = Arc::new(metadata_sync::MetadataSyncManager::new(
            metadata_store.clone(),
            cluster_state.clone(),
            Duration::from_secs(60),
        ));

        let admin_service = Arc::new(admin_service::AdminService::new(
            metadata_store,
            cluster_state,
            sync_manager,
        ));

        // Test that we can create the router
        let _router = admin_service.router();
    }
}