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

/// Serializes tests that start their own broker.
///
/// Every `TestCluster` spawns real `chronik-server` processes with their own
/// WAL, so cargo's default of running a binary's tests in parallel puts several
/// brokers on one machine at once. That starves the short poll windows these
/// tests use and produces failures that reproduce only under load — the worst
/// kind, because they read as product bugs.
///
/// Holding this for the duration of a cluster-based test makes the outcome
/// independent of how cargo was invoked, rather than relying on the caller to
/// remember `--test-threads=1`.
pub async fn exclusive() -> tokio::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(())).lock().await
}

/// Test cluster instance
pub struct TestCluster {
    config: TestClusterConfig,
    _temp_dir: Option<TempDir>,
    server_addrs: Vec<SocketAddr>,
    /// Unified API (port 6092 in production) — SQL, search, admin, schema
    /// registry. One per server, allocated separately so several servers can
    /// run on one host without colliding on the default.
    api_addrs: Vec<SocketAddr>,
    processes: Vec<Child>,
}

/// Locate the `chronik-server` binary under test.
///
/// `Command::new("chronik-server")` searched `$PATH`, which on a developer
/// machine either finds nothing or — worse — finds an installed build of a
/// different version than the one the test was compiled against.
pub fn server_binary() -> PathBuf {
    if let Ok(explicit) = std::env::var("CHRONIK_TEST_BIN") {
        return PathBuf::from(explicit);
    }
    // CARGO_BIN_EXE_* is only injected for binaries of the *same* package, and
    // this is a separate test package, so derive the path from the target dir.
    let target = std::env::var("CARGO_TARGET_DIR").unwrap_or_else(|_| {
        format!("{}/../target", env!("CARGO_MANIFEST_DIR"))
    });
    let profile = if cfg!(debug_assertions) { "debug" } else { "release" };
    PathBuf::from(format!("{}/{}/chronik-server", target, profile))
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
        let api_addrs = allocate_ports(config.num_servers)?;

        let mut cluster = Self {
            config: config.clone(),
            _temp_dir,
            server_addrs: server_addrs.clone(),
            api_addrs,
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

    /// Unified API addresses, one per server
    pub fn api_addrs(&self) -> &[SocketAddr] {
        &self.api_addrs
    }

    /// Admin endpoint for the first server (`/admin/*` on the Unified API).
    ///
    /// This used to return `http://<kafka port>`, which never served HTTP.
    pub fn admin_endpoint(&self) -> String {
        format!("http://{}", self.api_addrs[0])
    }

    /// Search endpoint for the first server (`/_search`, `/_sql`, `/_vector`).
    pub fn search_endpoint(&self) -> String {
        format!("http://{}", self.api_addrs[0])
    }

    async fn start_server(&mut self, id: usize, addr: SocketAddr, data_dir: &PathBuf) -> Result<()> {
        let node_data_dir = data_dir.join(format!("server-{}", id));
        std::fs::create_dir_all(&node_data_dir)?;

        // v2.2.0 CLI: `start`, `--bind`, `--kafka-port`. The previous form
        // (`--bind-addr`, `--wal-metadata`, and no subcommand at all) had not
        // been valid since the CLI was redesigned, so this harness could not
        // launch a server no matter which test called it. WAL metadata is the
        // default now, so `enable_wal_metadata` needs no flag.
        let mut cmd = Command::new(server_binary());
        // Broker verbosity is overridable: the hardcoded `chronik=debug` made a
        // full suite run emit tens of MB per invocation, which buries the test
        // results it is supposed to help explain.
        let broker_log = std::env::var("CHRONIK_TEST_LOG").unwrap_or_else(|_| "warn".to_string());

        cmd.arg("start")
            .env("RUST_LOG", broker_log)
            .env("CHRONIK_UNIFIED_API_PORT", self.api_addrs[id].port().to_string())
            // Distinct metrics port per server; the default is shared and two
            // brokers on one host would fight over it.
            .env("CHRONIK_METRICS_PORT", (self.api_addrs[id].port() as u32 + 10000).min(65535).to_string())
            .arg("--bind").arg("127.0.0.1")
            .arg("--advertise").arg(format!("127.0.0.1:{}", addr.port()))
            .arg("--kafka-port").arg(addr.port().to_string())
            .arg("--data-dir").arg(node_data_dir.to_str().unwrap());

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

        // And to serve the Unified API. A test that queries /_search or /admin
        // right after the Kafka port opens would otherwise race the HTTP
        // listener, which starts later in the builder.
        for addr in &self.api_addrs {
            wait_for_http_endpoint(&format!("http://{}/health", addr), Duration::from_secs(30)).await?;
        }

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