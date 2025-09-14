//! Common test utilities and helpers for integration tests

use chronik_common::Result;
use std::net::{TcpListener, SocketAddr};
use std::path::PathBuf;
use std::process::{Child, Command};
use std::time::Duration;
use tempfile::TempDir;
use tokio::time::{sleep, timeout};
use tracing::{info, warn};

/// Test cluster configuration
#[derive(Debug, Clone)]
pub struct TestClusterConfig {
    pub num_servers: usize,
    pub data_dir: Option<PathBuf>,
    pub object_storage: ObjectStorageType,
    pub enable_tls: bool,
    pub enable_auth: bool,
    pub enable_wal_metadata: bool,
}

impl Default for TestClusterConfig {
    fn default() -> Self {
        Self {
            num_servers: 1,
            data_dir: None,
            object_storage: ObjectStorageType::Local,
            enable_tls: false,
            enable_auth: false,
            enable_wal_metadata: true,
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
    server_addrs: Vec<SocketAddr>,
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
        let server_addrs = allocate_ports(config.num_servers)?;

        let mut cluster = Self {
            config: config.clone(),
            _temp_dir,
            server_addrs: server_addrs.clone(),
            processes: Vec::new(),
        };

        // Start servers
        for (i, addr) in server_addrs.iter().enumerate() {
            cluster.start_server(i, *addr, &data_dir).await?;
        }

        // Wait for servers to be ready
        cluster.wait_for_cluster_ready().await?;

        info!("Test cluster started successfully");
        Ok(cluster)
    }

    /// Get server addresses
    pub fn server_addrs(&self) -> &[SocketAddr] {
        &self.server_addrs
    }

    /// Get Kafka bootstrap servers string
    pub fn bootstrap_servers(&self) -> String {
        self.server_addrs
            .iter()
            .map(|addr| addr.to_string())
            .collect::<Vec<_>>()
            .join(",")
    }

    /// Get admin endpoint for the first server
    pub fn admin_endpoint(&self) -> String {
        format!("http://{}", self.server_addrs[0])
    }

    async fn start_server(&mut self, id: usize, addr: SocketAddr, data_dir: &PathBuf) -> Result<()> {
        let node_data_dir = data_dir.join(format!("server-{}", id));
        std::fs::create_dir_all(&node_data_dir)?;

        let mut cmd = Command::new("chronik-server");
        cmd.env("RUST_LOG", "chronik=debug")
            .arg("--bind-addr").arg("0.0.0.0")
            .arg("--kafka-port").arg(addr.port().to_string())
            .arg("--data-dir").arg(node_data_dir.to_str().unwrap());

        // Enable WAL metadata if configured
        if self.config.enable_wal_metadata {
            cmd.arg("--wal-metadata");
        }

        // Configure object storage
        match &self.config.object_storage {
            ObjectStorageType::Local => {
                let segments_dir = node_data_dir.join("segments");
                std::fs::create_dir_all(&segments_dir)?;
                cmd.env("OBJECT_STORE_TYPE", "local")
                    .env("LOCAL_STORAGE_PATH", segments_dir.to_str().unwrap());
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

        info!("Starting server {} with command: {:?}", id, cmd);
        let child = cmd.spawn()?;
        self.processes.push(child);

        Ok(())
    }

    async fn wait_for_cluster_ready(&self) -> Result<()> {
        info!("Waiting for cluster to be ready...");

        // Wait for all servers to accept Kafka connections
        for addr in &self.server_addrs {
            wait_for_tcp_endpoint(addr, Duration::from_secs(30)).await?;
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