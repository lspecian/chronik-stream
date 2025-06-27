//! Common test utilities and helpers for integration tests

use chronik_common::Result;
use std::net::{TcpListener, SocketAddr};
use std::path::PathBuf;
use std::process::{Child, Command};
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;
use tokio::time::{sleep, timeout};
use tracing::{info, warn};

/// Test cluster configuration
#[derive(Debug, Clone)]
pub struct TestClusterConfig {
    pub num_controllers: usize,
    pub num_ingest_nodes: usize,
    pub num_search_nodes: usize,
    pub data_dir: Option<PathBuf>,
    pub object_storage: ObjectStorageType,
    pub enable_tls: bool,
    pub enable_auth: bool,
}

impl Default for TestClusterConfig {
    fn default() -> Self {
        Self {
            num_controllers: 1,
            num_ingest_nodes: 1,
            num_search_nodes: 1,
            data_dir: None,
            object_storage: ObjectStorageType::Local,
            enable_tls: false,
            enable_auth: false,
        }
    }
}

#[derive(Debug, Clone)]
pub enum ObjectStorageType {
    Local,
    S3(S3Config),
    InMemory,
}

#[derive(Debug, Clone)]
pub struct S3Config {
    pub endpoint: String,
    pub bucket: String,
    pub access_key: String,
    pub secret_key: String,
}

/// Test cluster instance
pub struct TestCluster {
    config: TestClusterConfig,
    _temp_dir: Option<TempDir>,
    controller_addrs: Vec<SocketAddr>,
    ingest_addrs: Vec<SocketAddr>,
    search_addrs: Vec<SocketAddr>,
    processes: Vec<Child>,
}

impl TestCluster {
    /// Create and start a new test cluster
    pub async fn start(config: TestClusterConfig) -> Result<Self> {
        info!("Starting test cluster with config: {:?}", config);
        
        // Create temporary directory if not provided
        let (_temp_dir, data_dir) = if let Some(dir) = config.data_dir.clone() {
            (None, dir)
        } else {
            let temp = TempDir::new()?;
            let path = temp.path().to_path_buf();
            (Some(temp), path)
        };
        
        // Allocate ports
        let controller_addrs = allocate_ports(config.num_controllers)?;
        let ingest_addrs = allocate_ports(config.num_ingest_nodes)?;
        let search_addrs = allocate_ports(config.num_search_nodes)?;
        
        let mut cluster = Self {
            config: config.clone(),
            _temp_dir,
            controller_addrs: controller_addrs.clone(),
            ingest_addrs: ingest_addrs.clone(),
            search_addrs: search_addrs.clone(),
            processes: Vec::new(),
        };
        
        // Start controllers
        for (i, addr) in controller_addrs.iter().enumerate() {
            cluster.start_controller(i, *addr, &data_dir).await?;
        }
        
        // Wait for controllers to be ready
        cluster.wait_for_controllers().await?;
        
        // Start ingest nodes
        for (i, addr) in ingest_addrs.iter().enumerate() {
            cluster.start_ingest_node(i, *addr, &data_dir).await?;
        }
        
        // Start search nodes
        for (i, addr) in search_addrs.iter().enumerate() {
            cluster.start_search_node(i, *addr, &data_dir).await?;
        }
        
        // Wait for all nodes to be ready
        cluster.wait_for_cluster_ready().await?;
        
        info!("Test cluster started successfully");
        Ok(cluster)
    }
    
    /// Get controller addresses
    pub fn controller_addrs(&self) -> &[SocketAddr] {
        &self.controller_addrs
    }
    
    /// Get ingest node addresses
    pub fn ingest_addrs(&self) -> &[SocketAddr] {
        &self.ingest_addrs
    }
    
    /// Get search node addresses  
    pub fn search_addrs(&self) -> &[SocketAddr] {
        &self.search_addrs
    }
    
    /// Get Kafka bootstrap servers string
    pub fn bootstrap_servers(&self) -> String {
        self.ingest_addrs
            .iter()
            .map(|addr| addr.to_string())
            .collect::<Vec<_>>()
            .join(",")
    }
    
    /// Get search endpoint
    pub fn search_endpoint(&self) -> String {
        format!("http://{}", self.search_addrs[0])
    }
    
    async fn start_controller(&mut self, id: usize, addr: SocketAddr, data_dir: &PathBuf) -> Result<()> {
        let node_data_dir = data_dir.join(format!("controller-{}", id));
        std::fs::create_dir_all(&node_data_dir)?;
        
        let mut cmd = Command::new("chronik-controller");
        cmd.env("RUST_LOG", "chronik=debug")
            .env("NODE_ID", id.to_string())
            .env("RAFT_ADDR", addr.to_string())
            .env("ADMIN_ADDR", format!("127.0.0.1:{}", addr.port() + 1000))
            .env("DATA_DIR", node_data_dir.to_str().unwrap());
        
        // Add peer addresses
        if self.controller_addrs.len() > 1 {
            let peers: Vec<String> = self.controller_addrs
                .iter()
                .enumerate()
                .filter(|(i, _)| *i != id)
                .map(|(i, addr)| format!("{}={}", i, addr))
                .collect();
            cmd.env("PEERS", peers.join(","));
        }
        
        info!("Starting controller {} with command: {:?}", id, cmd);
        let child = cmd.spawn()?;
        self.processes.push(child);
        
        Ok(())
    }
    
    async fn start_ingest_node(&mut self, id: usize, addr: SocketAddr, data_dir: &PathBuf) -> Result<()> {
        let node_data_dir = data_dir.join(format!("ingest-{}", id));
        std::fs::create_dir_all(&node_data_dir)?;
        
        let mut cmd = Command::new("chronik-ingest");
        cmd.env("RUST_LOG", "chronik=debug")
            .env("BIND_ADDRESS", addr.to_string())
            .env("STORAGE_PATH", node_data_dir.to_str().unwrap())
            .env("CONTROLLER_ADDRS", self.controller_addrs[0].to_string());
        
        // Configure object storage
        match &self.config.object_storage {
            ObjectStorageType::Local => {
                cmd.env("OBJECT_STORE_TYPE", "local")
                    .env("LOCAL_STORAGE_PATH", node_data_dir.join("segments").to_str().unwrap());
            }
            ObjectStorageType::S3(s3) => {
                cmd.env("OBJECT_STORE_TYPE", "s3")
                    .env("S3_ENDPOINT", &s3.endpoint)
                    .env("S3_BUCKET", &s3.bucket)
                    .env("AWS_ACCESS_KEY_ID", &s3.access_key)
                    .env("AWS_SECRET_ACCESS_KEY", &s3.secret_key);
            }
            ObjectStorageType::InMemory => {
                cmd.env("OBJECT_STORE_TYPE", "memory");
            }
        }
        
        info!("Starting ingest node {} with command: {:?}", id, cmd);
        let child = cmd.spawn()?;
        self.processes.push(child);
        
        Ok(())
    }
    
    async fn start_search_node(&mut self, id: usize, addr: SocketAddr, data_dir: &PathBuf) -> Result<()> {
        let node_data_dir = data_dir.join(format!("search-{}", id));
        std::fs::create_dir_all(&node_data_dir)?;
        
        let mut cmd = Command::new("chronik-search");
        cmd.env("RUST_LOG", "chronik=debug")
            .env("BIND_ADDRESS", addr.to_string())
            .env("INDEX_DIR", node_data_dir.join("indices").to_str().unwrap())
            .env("CONTROLLER_ADDRS", self.controller_addrs[0].to_string());
        
        // Configure object storage (same as ingest)
        match &self.config.object_storage {
            ObjectStorageType::Local => {
                cmd.env("OBJECT_STORE_TYPE", "local")
                    .env("LOCAL_STORAGE_PATH", data_dir.join("segments").to_str().unwrap());
            }
            ObjectStorageType::S3(s3) => {
                cmd.env("OBJECT_STORE_TYPE", "s3")
                    .env("S3_ENDPOINT", &s3.endpoint)
                    .env("S3_BUCKET", &s3.bucket)
                    .env("AWS_ACCESS_KEY_ID", &s3.access_key)
                    .env("AWS_SECRET_ACCESS_KEY", &s3.secret_key);
            }
            ObjectStorageType::InMemory => {
                cmd.env("OBJECT_STORE_TYPE", "memory");
            }
        }
        
        info!("Starting search node {} with command: {:?}", id, cmd);
        let child = cmd.spawn()?;
        self.processes.push(child);
        
        Ok(())
    }
    
    async fn wait_for_controllers(&self) -> Result<()> {
        info!("Waiting for controllers to be ready...");
        
        for addr in &self.controller_addrs {
            let admin_addr = format!("127.0.0.1:{}", addr.port() + 1000);
            wait_for_http_endpoint(&format!("http://{}/health", admin_addr), Duration::from_secs(30)).await?;
        }
        
        Ok(())
    }
    
    async fn wait_for_cluster_ready(&self) -> Result<()> {
        info!("Waiting for cluster to be ready...");
        
        // Wait for ingest nodes
        for addr in &self.ingest_addrs {
            wait_for_tcp_endpoint(addr, Duration::from_secs(30)).await?;
        }
        
        // Wait for search nodes
        for addr in &self.search_addrs {
            wait_for_http_endpoint(&format!("http://{}/health", addr), Duration::from_secs(30)).await?;
        }
        
        // Give cluster a moment to stabilize
        sleep(Duration::from_secs(2)).await;
        
        Ok(())
    }
}

impl Drop for TestCluster {
    fn drop(&mut self) {
        info!("Shutting down test cluster");
        
        // Terminate all processes
        for mut process in self.processes.drain(..) {
            let _ = process.kill();
            let _ = process.wait();
        }
    }
}

/// Allocate available ports for testing
fn allocate_ports(count: usize) -> Result<Vec<SocketAddr>> {
    let mut addrs = Vec::new();
    
    for _ in 0..count {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let addr = listener.local_addr()?;
        drop(listener); // Release the port
        addrs.push(addr);
    }
    
    Ok(addrs)
}

/// Wait for a TCP endpoint to be available
async fn wait_for_tcp_endpoint(addr: &SocketAddr, timeout_duration: Duration) -> Result<()> {
    timeout(timeout_duration, async {
        loop {
            match tokio::net::TcpStream::connect(addr).await {
                Ok(_) => return Ok(()),
                Err(_) => {
                    sleep(Duration::from_millis(100)).await;
                }
            }
        }
    })
    .await
    .map_err(|_| chronik_common::Error::Internal(format!("Timeout waiting for TCP endpoint: {}", addr)))?
}

/// Wait for an HTTP endpoint to be healthy
async fn wait_for_http_endpoint(url: &str, timeout_duration: Duration) -> Result<()> {
    timeout(timeout_duration, async {
        loop {
            match reqwest::get(url).await {
                Ok(resp) if resp.status().is_success() => return Ok(()),
                _ => {
                    sleep(Duration::from_millis(500)).await;
                }
            }
        }
    })
    .await
    .map_err(|_| chronik_common::Error::Internal(format!("Timeout waiting for HTTP endpoint: {}", url)))?
}

/// Create a test Kafka producer
pub fn create_test_producer(bootstrap_servers: &str) -> rdkafka::producer::FutureProducer {
    use rdkafka::ClientConfig;
    use rdkafka::producer::FutureProducer;
    
    ClientConfig::new()
        .set("bootstrap.servers", bootstrap_servers)
        .set("message.timeout.ms", "5000")
        .create::<FutureProducer>()
        .expect("Failed to create producer")
}

/// Create a test Kafka consumer
pub fn create_test_consumer(bootstrap_servers: &str, group_id: &str) -> rdkafka::consumer::StreamConsumer {
    use rdkafka::ClientConfig;
    use rdkafka::consumer::StreamConsumer;
    
    ClientConfig::new()
        .set("bootstrap.servers", bootstrap_servers)
        .set("group.id", group_id)
        .set("enable.auto.commit", "false")
        .set("auto.offset.reset", "earliest")
        .create::<StreamConsumer>()
        .expect("Failed to create consumer")
}