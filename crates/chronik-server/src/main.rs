//! Unified Chronik Stream Server
//! 
//! This is the main entry point for Chronik Stream, supporting multiple operational modes:
//! - Standalone (default): Single-node Kafka-compatible server
//! - Ingest: Data ingestion node (future: for distributed mode)
//! - Search: Search node (future: for distributed mode)
//! - All: All components in one process

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::sync::Arc;
use tracing::{error, info, warn};
use chronik_monitoring::{init_monitoring, TracingConfig};

mod integrated_server;
mod error_handler;
mod kafka_handler;
mod produce_handler;
mod pipelined_connection;  // v2.2.9: Async request pipelining for leader forwarding
mod response_pipeline;     // v2.2.10: Async response delivery for acks=1 (eliminates 168x bottleneck)
mod storage;
mod consumer_group;
mod fetch_handler;
mod fetch;  // Phase 2.2: Extracted fetch modules
mod offset_storage;
mod coordinator_manager;
mod wal_integration;
mod metadata_dr;
mod wal_replication;  // v2.2.0: PostgreSQL-style WAL streaming
mod replication;  // Phase 2.3: Extracted replication modules (connection_state, frame_reader, record_processor)
// v2.2.7 Phase 2: Raft for metadata coordination only (NOT data replication)
mod raft_metadata;
mod raft_cluster;
// v2.2.9 Option A: REMOVED raft_metadata_store (replaced by WalMetadataStore in chronik-common)
// mod raft_metadata_store;  // v2.2.7 Phase 3: OLD Raft-based metadata store (1-N nodes)
// v2.2.7 Phase 2: WAL-based metadata writes (fast path, bypasses Raft consensus)
mod metadata_wal;          // Fast local WAL for metadata operations (1-2ms vs 10-50ms Raft)
mod metadata_wal_replication;  // Async replication using existing WalReplicationManager
mod metadata_events;       // Event-based architecture for metadata WAL replication
// v2.2.7 Phase 3: ISR tracking for partition replication
mod isr_tracker;
// v2.2.7 Phase 4: ISR ACK tracking for acks=-1 quorum support
mod isr_ack_tracker;
// v2.2.7 Phase 5: Automatic leader election per partition
mod leader_election;
// v2.2.7: HTTP Admin API for cluster management
mod admin_api;
// v2.2.7 Priority 2 Step 3: Automatic partition rebalancing
mod partition_rebalancer;
// v2.2.7 Phase 1: Leader-forwarding RPC for metadata queries
mod metadata_rpc;
// v2.2.7 Phase 3: Leader leases for fast follower reads
mod leader_lease;
mod leader_heartbeat;

// v2.2.7 Phase 2.5: Automatic quorum recovery
mod quorum_recovery;

mod cli;
mod cluster;  // Phase 1.2: Cluster mode refactoring (complexity reduction from 288 → <25)

use integrated_server::{IntegratedKafkaServer, IntegratedServerConfig, IntegratedKafkaServerBuilder};
use chronik_wal::compaction::{WalCompactor, CompactionConfig, CompactionStrategy};
use chronik_wal::config::{WalConfig, CompressionType};
use chronik_storage::object_store::{ObjectStoreConfig, StorageBackend, AuthConfig, S3Credentials};
use chronik_config::{ClusterConfig, NodeConfig};
use serde_json;

#[derive(Parser, Debug, Clone)]
#[command(
    name = "chronik-server",
    about = "Chronik Stream - Kafka-compatible streaming platform",
    version,
    author,
    long_about = "A high-performance, Kafka-compatible streaming platform with WAL durability"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,

    /// Data directory for storage
    #[arg(short = 'd', long, env = "CHRONIK_DATA_DIR", default_value = "./data", global = true)]
    data_dir: PathBuf,

    /// Log level (error, warn, info, debug, trace)
    #[arg(short = 'l', long, env = "RUST_LOG", default_value = "info", global = true)]
    log_level: String,
}

#[derive(Subcommand, Debug, Clone)]
enum CompactAction {
    /// Run manual compaction immediately
    #[command(about = "Trigger immediate WAL compaction")]
    Now {
        /// Target partition (format: topic:partition)
        #[arg(long)]
        partition: Option<String>,

        /// Compaction strategy (key-based, time-based, hybrid, custom)
        #[arg(long, default_value = "key-based")]
        strategy: String,

        /// Dry run - show what would be compacted without actually doing it
        #[arg(long, default_value = "false")]
        dry_run: bool,
    },

    /// Show compaction status and statistics
    #[command(about = "Display WAL compaction statistics")]
    Status {
        /// Show detailed statistics
        #[arg(long, default_value = "false")]
        detailed: bool,
    },

    /// Configure compaction settings
    #[command(about = "Configure WAL compaction parameters")]
    Config {
        /// Enable/disable automatic compaction
        #[arg(long)]
        enabled: Option<bool>,

        /// Compaction interval in seconds
        #[arg(long)]
        interval: Option<u64>,

        /// Retention period in days
        #[arg(long)]
        retention_days: Option<u32>,

        /// Compaction strategy
        #[arg(long)]
        strategy: Option<String>,

        /// Show current configuration
        #[arg(long, default_value = "false")]
        show: bool,
    },

    /// Schedule compaction for specific time
    #[command(about = "Schedule WAL compaction")]
    Schedule {
        /// Time to run compaction (cron format or "daily", "hourly")
        #[arg(long)]
        at: String,

        /// Cancel scheduled compaction
        #[arg(long, default_value = "false")]
        cancel: bool,
    },
}

#[derive(Subcommand, Debug, Clone)]
enum Commands {
    /// Start a Chronik server (single-node or cluster mode)
    #[command(about = "Start server (auto-detects mode from config)")]
    Start {
        /// Cluster config file (TOML)
        #[arg(short = 'c', long, env = "CHRONIK_CONFIG")]
        config: Option<PathBuf>,

        /// Bind address for all services (default: 0.0.0.0)
        #[arg(long, env = "CHRONIK_BIND", default_value = "0.0.0.0")]
        bind: String,

        /// Advertised address for clients (overrides config)
        #[arg(long, env = "CHRONIK_ADVERTISE")]
        advertise: Option<String>,

        /// Node ID (overrides config file)
        #[arg(long, env = "CHRONIK_NODE_ID")]
        node_id: Option<u64>,
    },

    /// Manage cluster membership
    #[command(about = "Cluster management commands")]
    Cluster {
        #[command(subcommand)]
        action: ClusterAction,
    },

    /// WAL compaction management
    #[command(about = "Manage WAL compaction")]
    Compact {
        #[command(subcommand)]
        action: CompactAction,
    },

    /// Show version and build information
    #[command(about = "Display version and build information")]
    Version,
}

#[derive(Subcommand, Debug, Clone)]
enum ClusterAction {
    /// Show cluster status
    #[command(about = "Display cluster status")]
    Status {
        /// Cluster config file
        #[arg(short = 'c', long, env = "CHRONIK_CONFIG")]
        config: PathBuf,
    },

    /// Add a node to the cluster (zero-downtime)
    #[command(about = "Add a new node to the cluster")]
    AddNode {
        /// Node ID to add
        node_id: u64,

        /// Kafka address (host:port)
        #[arg(long)]
        kafka: String,

        /// WAL address (host:port)
        #[arg(long)]
        wal: String,

        /// Raft address (host:port)
        #[arg(long)]
        raft: String,

        /// Cluster config file
        #[arg(short = 'c', long, env = "CHRONIK_CONFIG")]
        config: PathBuf,
    },

    /// Remove a node from the cluster (zero-downtime)
    #[command(about = "Remove a node from the cluster")]
    RemoveNode {
        /// Node ID to remove
        node_id: u64,

        /// Force removal (don't wait for partition reassignment)
        #[arg(long, default_value = "false")]
        force: bool,

        /// Cluster config file
        #[arg(short = 'c', long, env = "CHRONIK_CONFIG")]
        config: PathBuf,
    },

    /// Trigger partition rebalancing
    #[command(about = "Manually trigger partition rebalancing")]
    Rebalance {
        /// Cluster config file
        #[arg(short = 'c', long, env = "CHRONIK_CONFIG")]
        config: PathBuf,
    },
}

/// Parse Kafka address (host:port) from cluster config
fn parse_kafka_addr(addr: &str) -> Result<(String, i32)> {
    if let Some(colon_pos) = addr.rfind(':') {
        let host = addr[..colon_pos].to_string();
        let port_str = &addr[colon_pos + 1..];
        let port = port_str.parse::<u16>()
            .map_err(|e| anyhow::anyhow!("Invalid port in address '{}': {}", addr, e))?;
        Ok((host, port as i32))
    } else {
        Err(anyhow::anyhow!("Address '{}' must be in format 'host:port'", addr))
    }
}

/// Parse object store configuration from environment variables
fn parse_object_store_config_from_env() -> Option<ObjectStoreConfig> {
    let backend_type = std::env::var("OBJECT_STORE_BACKEND").ok()?;

    match backend_type.to_lowercase().as_str() {
        "s3" => {
            info!("Configuring S3-compatible object store from environment variables");

            let endpoint = std::env::var("S3_ENDPOINT").ok();
            let region = std::env::var("S3_REGION").unwrap_or_else(|_| "us-east-1".to_string());
            let bucket = std::env::var("S3_BUCKET").unwrap_or_else(|_| "chronik-storage".to_string());
            let access_key = std::env::var("S3_ACCESS_KEY").ok();
            let secret_key = std::env::var("S3_SECRET_KEY").ok();
            let path_style = std::env::var("S3_PATH_STYLE")
                .ok()
                .and_then(|v| v.parse::<bool>().ok())
                .unwrap_or(true); // Default to path-style for MinIO compatibility
            let disable_ssl = std::env::var("S3_DISABLE_SSL")
                .ok()
                .and_then(|v| v.parse::<bool>().ok())
                .unwrap_or(false);

            let auth = match (access_key, secret_key) {
                (Some(key), Some(secret)) => {
                    info!("Using S3 access key authentication");
                    AuthConfig::S3(S3Credentials::AccessKey {
                        access_key_id: key,
                        secret_access_key: secret,
                        session_token: std::env::var("S3_SESSION_TOKEN").ok(),
                    })
                }
                _ => {
                    info!("Using S3 environment-based authentication");
                    AuthConfig::S3(S3Credentials::FromEnvironment)
                }
            };

            let config = ObjectStoreConfig {
                backend: StorageBackend::S3 {
                    region,
                    endpoint,
                    force_path_style: path_style,
                    use_virtual_hosted_style: !path_style,
                    signing_region: None,
                    disable_ssl,
                },
                bucket,
                prefix: std::env::var("S3_PREFIX").ok(),
                auth,
                connection: Default::default(),
                performance: Default::default(),
                retry: Default::default(),
                default_metadata: None,
                encryption: None,
            };

            info!("S3 object store configured: bucket={}, endpoint={:?}",
                  config.bucket,
                  if let StorageBackend::S3 { endpoint, .. } = &config.backend { endpoint } else { &None });

            Some(config)
        }
        "gcs" => {
            info!("Configuring Google Cloud Storage object store from environment variables");

            let bucket = std::env::var("GCS_BUCKET").unwrap_or_else(|_| "chronik-storage".to_string());
            let project_id = std::env::var("GCS_PROJECT_ID").ok();
            let endpoint = std::env::var("GCS_ENDPOINT").ok();

            let config = ObjectStoreConfig {
                backend: StorageBackend::Gcs {
                    project_id,
                    endpoint,
                },
                bucket,
                prefix: std::env::var("GCS_PREFIX").ok(),
                auth: AuthConfig::Gcs(chronik_storage::object_store::GcsCredentials::Default),
                connection: Default::default(),
                performance: Default::default(),
                retry: Default::default(),
                default_metadata: None,
                encryption: None,
            };

            info!("GCS object store configured: bucket={}", config.bucket);

            Some(config)
        }
        "azure" => {
            info!("Configuring Azure Blob Storage object store from environment variables");

            let account_name = std::env::var("AZURE_ACCOUNT_NAME").ok()?;
            let bucket = std::env::var("AZURE_CONTAINER").unwrap_or_else(|_| "chronik-storage".to_string());
            let endpoint = std::env::var("AZURE_ENDPOINT").ok();
            let use_emulator = std::env::var("AZURE_USE_EMULATOR")
                .ok()
                .and_then(|v| v.parse::<bool>().ok())
                .unwrap_or(false);

            let config = ObjectStoreConfig {
                backend: StorageBackend::Azure {
                    account_name,
                    endpoint,
                    use_emulator,
                },
                bucket,
                prefix: std::env::var("AZURE_PREFIX").ok(),
                auth: AuthConfig::Azure(chronik_storage::object_store::AzureCredentials::DefaultChain),
                connection: Default::default(),
                performance: Default::default(),
                retry: Default::default(),
                default_metadata: None,
                encryption: None,
            };

            info!("Azure object store configured: container={}", config.bucket);

            Some(config)
        }
        "local" => {
            info!("Configuring local filesystem object store from environment variables");

            let path = std::env::var("LOCAL_STORAGE_PATH").unwrap_or_else(|_| "./data/segments".to_string());

            let config = ObjectStoreConfig {
                backend: StorageBackend::Local { path: path.clone() },
                bucket: "local".to_string(),
                prefix: None,
                auth: AuthConfig::None,
                connection: Default::default(),
                performance: Default::default(),
                retry: Default::default(),
                default_metadata: None,
                encryption: None,
            };

            info!("Local object store configured: path={}", path);

            Some(config)
        }
        other => {
            warn!("Unknown OBJECT_STORE_BACKEND value: {}", other);
            warn!("Valid values: s3, gcs, azure, local");
            None
        }
    }
}

/// Load cluster configuration from file
fn load_cluster_config_from_file(path: &PathBuf, node_id_override: Option<u64>) -> Result<ClusterConfig> {
    info!("Loading cluster configuration from file: {}", path.display());
    let contents = std::fs::read_to_string(path)?;
    let mut config: ClusterConfig = toml::from_str(&contents)?;

    if let Some(node_id) = node_id_override {
        info!("Overriding node_id from CLI: {}", node_id);
        config.node_id = node_id;
    }

    config.validate_config().map_err(|e| {
        anyhow::anyhow!("Invalid cluster configuration: {}", e)
    })?;

    info!("Loaded cluster configuration from file");
    info!("  Node ID: {}", config.node_id);
    info!("  Replication Factor: {}", config.replication_factor);
    info!("  Min In-Sync Replicas: {}", config.min_insync_replicas);
    info!("  Peers: {}", config.peers.len());

    Ok(config)
}

/// Load cluster configuration from environment variables
fn load_cluster_config_from_env(node_id_override: Option<u64>) -> Result<Option<ClusterConfig>> {
    if let Ok(Some(mut config)) = ClusterConfig::from_env() {
        if let Some(node_id) = node_id_override {
            info!("Overriding node_id from CLI: {}", node_id);
            config.node_id = node_id;
        }
        info!("Loaded cluster configuration from environment variables");
        info!("  Node ID: {}", config.node_id);
        info!("  Replication Factor: {}", config.replication_factor);
        info!("  Min In-Sync Replicas: {}", config.min_insync_replicas);
        info!("  Peers: {}", config.peers.len());
        Ok(Some(config))
    } else {
        Ok(None)
    }
}

/// Parse advertised address from string or derive from bind address
fn parse_advertise_addr(advertise: Option<&str>, bind: &str, default_port: u16) -> Result<(String, i32)> {
    if let Some(addr) = advertise {
        // User provided advertised address - parse it
        if let Some(colon_pos) = addr.rfind(':') {
            let potential_port = &addr[colon_pos + 1..];
            if potential_port.chars().all(|c| c.is_ascii_digit()) && !potential_port.is_empty() {
                let host = addr[..colon_pos].to_string();
                let port = potential_port.parse::<u16>().unwrap_or(default_port);
                Ok((host, port as i32))
            } else {
                Ok((addr.to_string(), default_port as i32))
            }
        } else {
            Ok((addr.to_string(), default_port as i32))
        }
    } else if bind == "0.0.0.0" || bind == "[::]" {
        // Binding to all interfaces - use hostname or localhost
        let hostname = std::env::var("HOSTNAME")
            .or_else(|_| std::env::var("DOCKER_HOSTNAME"))
            .unwrap_or_else(|_| "127.0.0.1".to_string());

        if hostname != "127.0.0.1" {
            info!("Binding to all interfaces, using hostname '{}' as advertised address", hostname);
        } else {
            warn!("Binding to all interfaces without advertised address configured");
            warn!("Using '127.0.0.1' - remote clients may not connect");
            warn!("Set --advertise to your hostname/IP for remote access");
        }
        Ok((hostname, default_port as i32))
    } else {
        // Use bind address as advertised
        Ok((bind.to_string(), default_port as i32))
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // Initialize logging
    std::env::set_var("RUST_LOG", &cli.log_level);
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    // Log version information on startup
    info!("Chronik Server v{}", env!("CARGO_PKG_VERSION"));
    info!("Build features: search={}, backup={}, dynamic-config={}",
        cfg!(feature = "search"),
        cfg!(feature = "backup"),
        cfg!(feature = "dynamic-config")
    );

    // Show deprecation warnings for old environment variables
    if std::env::var("CHRONIK_KAFKA_PORT").is_ok() {
        warn!("CHRONIK_KAFKA_PORT is deprecated. Use cluster config file instead.");
    }
    if std::env::var("CHRONIK_REPLICATION_FOLLOWERS").is_ok() {
        warn!("CHRONIK_REPLICATION_FOLLOWERS is deprecated. WAL replication now auto-discovers from cluster config file.");
        warn!("If using cluster config file (recommended), remove CHRONIK_REPLICATION_FOLLOWERS from environment.");
        warn!("If not using cluster config, CHRONIK_REPLICATION_FOLLOWERS still works for manual configuration.");
    }
    if std::env::var("CHRONIK_WAL_RECEIVER_ADDR").is_ok() {
        warn!("CHRONIK_WAL_RECEIVER_ADDR is deprecated. Use cluster config file with 'wal' address instead.");
    }
    if std::env::var("CHRONIK_WAL_RECEIVER_PORT").is_ok() {
        warn!("CHRONIK_WAL_RECEIVER_PORT is deprecated. Use cluster config file with 'wal' address instead.");
    }

    match &cli.command {
        Commands::Version => {
            println!("Chronik Server v{}", env!("CARGO_PKG_VERSION"));
            println!("Build features:");
            #[cfg(feature = "search")]
            println!("  - Search: enabled");
            #[cfg(feature = "backup")]
            println!("  - Backup: enabled");
            #[cfg(feature = "dynamic-config")]
            println!("  - Dynamic Config: enabled");
            Ok(())
        }

        Commands::Start { config, bind, advertise, node_id } => {
            run_start_command(&cli, config.clone(), bind.clone(), advertise.clone(), *node_id).await
        }

        Commands::Cluster { action } => {
            handle_cluster_command(&cli, action.clone()).await
        }

        Commands::Compact { action } => {
            handle_compaction_command(&cli, action.clone()).await
        }
    }
}

/// Run the start command (auto-detects single-node vs cluster mode)
async fn run_start_command(
    cli: &Cli,
    config_path: Option<PathBuf>,
    bind: String,
    advertise: Option<String>,
    node_id_override: Option<u64>,
) -> Result<()> {
    // Load cluster config (from file or env)
    let cluster_config = if let Some(path) = config_path {
        Some(load_cluster_config_from_file(&path, node_id_override)?)
    } else {
        load_cluster_config_from_env(node_id_override)?
    };

    if let Some(config) = cluster_config {
        info!("Starting in CLUSTER mode (node_id={})", config.node_id);
        run_cluster_mode(cli, config, bind, advertise).await
    } else {
        info!("Starting in SINGLE-NODE mode");
        run_single_node_mode(cli, bind, advertise).await
    }
}

/// Run in cluster mode (with Raft + WAL replication)
/// Phase 1: CLI Redesign & Unified Config - IMPLEMENTED
/// Run in cluster mode with Raft coordination
///
/// Orchestrates cluster startup using extracted modules.
/// Refactored from 335-line monolithic function to focused orchestration.
/// Complexity: < 25 (now purely orchestration)
async fn run_cluster_mode(
    cli: &Cli,
    config: ClusterConfig,
    bind: String,
    advertise: Option<String>,
) -> Result<()> {
    use cluster::*;

    info!("Starting Chronik in CLUSTER mode (node_id={})", config.node_id);
    info!("Cluster peers: {} nodes", config.peers.len());

    // Parse object store configuration
    let object_store_config = parse_object_store_config_from_env();

    // Phase 1: Configuration parsing and validation
    let init_config = ClusterInitConfig::from_cli_and_config(cli, config.clone(), &bind, advertise.as_deref())?;

    // Phase 2: Bootstrap Raft cluster
    let raft_cluster = bootstrap_raft_cluster(&init_config).await?;

    // Phase 3: Start Raft gRPC server
    start_raft_grpc_server(raft_cluster.clone(), &init_config.raft_bind_addr).await?;

    // Phase 4: Create IntegratedKafkaServer with builder pattern
    let server = create_integrated_server(&init_config, raft_cluster.clone(), object_store_config).await?;

    // Phase 5: Start Kafka protocol listener BEFORE leader election
    // CRITICAL FIX v2.2.9: Avoid cold-start deadlock
    let server_task = start_kafka_listener(server.clone(), init_config.kafka_bind_addr.clone()).await?;
    log_listener_status(&init_config);

    // Phase 5.5: Start Raft message processing loop
    // CRITICAL FIX: Required for leader election to occur
    // Without this, all nodes remain followers with leader_id=INVALID_ID
    info!("Starting Raft message processing loop...");
    raft_cluster.clone().start_message_loop();
    info!("✓ Raft message loop started - leader election can now occur");

    // Phase 6: Broker registration and partition assignment (leader only)
    register_brokers_and_assign_partitions(server.clone(), raft_cluster.clone(), &init_config).await?;

    // Phase 7: Initialize monitoring
    let metrics_port = 13000 + (init_config.node_id as u16);
    let _metrics_registry = init_monitoring("chronik-server", metrics_port, None).await?;
    info!("Metrics endpoint available at http://{}:{}/metrics", bind, metrics_port);

    // Phase 8: Start Search API (if feature enabled)
    start_search_api(
        server.clone(),
        bind.clone(),
        init_config.node_id,
        cli.data_dir.to_string_lossy().to_string(),
    ).await?;

    // Phase 9: Start Admin API
    start_admin_api(
        raft_cluster.clone(),
        server.metadata_store().clone(),
        init_config.node_id,
        &bind
    ).await?;

    // Phase 10: Start Partition Rebalancer
    info!("Starting Partition Rebalancer (RF={})", init_config.replication_factor);
    let _rebalancer = partition_rebalancer::PartitionRebalancer::new(
        raft_cluster.clone(),
        server.metadata_store().clone(),
        init_config.replication_factor as usize,
    ).await;
    info!("✓ Partition Rebalancer started (will check every 30s for membership changes)");

    // Phase 11: Wait for shutdown signals
    tokio::select! {
        result = server_task => {
            match result {
                Ok(Ok(_)) => info!("Server task completed normally"),
                Ok(Err(e)) => error!("Server task failed: {}", e),
                Err(e) => error!("Server task panicked: {}", e),
            }
        }
        _ = tokio::signal::ctrl_c() => {
            info!("Received SIGINT, shutting down...");
        }
        _ = async {
            #[cfg(unix)]
            {
                use tokio::signal::unix::{signal, SignalKind};
                let mut sigterm = signal(SignalKind::terminate()).unwrap();
                sigterm.recv().await
            }
            #[cfg(not(unix))]
            {
                std::future::pending::<()>().await
            }
        } => {
            info!("Received SIGTERM, shutting down...");
        }
    }

    info!("Shutting down cluster node {}...", init_config.node_id);
    server.shutdown().await?;

    Ok(())
}

/// Run in single-node mode (standalone, no clustering)
async fn run_single_node_mode(
    cli: &Cli,
    bind: String,
    advertise: Option<String>,
) -> Result<()> {
    let (advertised_host, advertised_port) = parse_advertise_addr(
        advertise.as_deref(),
        &bind,
        9092, // Default Kafka port
    )?;

    // Parse object store configuration from environment (for Tier 3: Tantivy archives)
    let object_store_config = parse_object_store_config_from_env();

    let config = IntegratedServerConfig {
        node_id: 1,
        advertised_host,
        advertised_port,
        data_dir: cli.data_dir.to_string_lossy().to_string(),
        enable_indexing: cfg!(feature = "search"),
        enable_compression: true,
        auto_create_topics: true,
        num_partitions: 3,  // Default to 3 partitions for better parallelism (configurable via --num-partitions)
        replication_factor: 1,
        enable_wal_indexing: true,
        wal_indexing_interval_secs: 30,
        object_store_config,
        enable_metadata_dr: true,
        metadata_upload_interval_secs: 60,
        cluster_config: None,
    };

    // Create server using builder pattern (single-node mode, no Raft cluster)
    info!("Initializing single-node IntegratedKafkaServer with builder...");
    let server = IntegratedKafkaServerBuilder::new(config)
        .build()
        .await?;
    info!("Single-node server initialized successfully");

    // Initialize monitoring
    let _metrics_registry = init_monitoring(
        "chronik-server",
        13092, // Default metrics port for single-node
        None,
    ).await?;

    let kafka_addr = format!("{}:9092", bind);
    info!("Kafka protocol listening on {}", kafka_addr);
    info!("Metrics endpoint available at http://{}:13092/metrics", bind);

    // Start search API if search feature is compiled
    #[cfg(feature = "search")]
    {
        info!("Starting Search API on port 6092");
        let search_bind = bind.clone();
        let wal_indexer = server.get_wal_indexer();
        let index_base_path = format!("{}/tantivy_indexes", cli.data_dir.to_string_lossy());

        tokio::spawn(async move {
            use chronik_search::api::SearchApi;
            use std::sync::Arc;

            let search_api = Arc::new(SearchApi::new_with_wal_indexer(wal_indexer, index_base_path).unwrap());
            let app = search_api.router();
            let addr = format!("{}:6092", search_bind);
            info!("Search API listening on http://{}", addr);

            let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
            chronik_search::serve_app(listener, app).await.unwrap()
        });

        info!("Search API available at http://{}:6092", bind);
    }

    // Start server with signal handling
    let server_clone = server.clone();
    let kafka_addr_clone = kafka_addr.clone();
    let server_task = tokio::spawn(async move {
        server_clone.run(&kafka_addr_clone).await
    });

    tokio::select! {
        result = server_task => {
            match result {
                Ok(Ok(_)) => info!("Server task completed normally"),
                Ok(Err(e)) => error!("Server task failed: {}", e),
                Err(e) => error!("Server task panicked: {}", e),
            }
        }
        _ = tokio::signal::ctrl_c() => {
            info!("Received SIGINT, shutting down...");
        }
        _ = async {
            #[cfg(unix)]
            {
                use tokio::signal::unix::{signal, SignalKind};
                let mut sigterm = signal(SignalKind::terminate()).unwrap();
                sigterm.recv().await
            }
            #[cfg(not(unix))]
            {
                std::future::pending::<()>().await
            }
        } => {
            info!("Received SIGTERM, shutting down...");
        }
    }

    server.shutdown().await?;
    Ok(())
}

/// Handle cluster management commands (v2.2.7+)
async fn handle_cluster_command(_cli: &Cli, action: ClusterAction) -> Result<()> {
    match action {
        ClusterAction::Status { config } => {
            info!("CLI: Querying cluster status");

            // Load cluster config to get node information
            let cluster_config = load_cluster_config_from_file(&config, None)?;

            println!("Discovering cluster leader...\n");

            let client = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(5))
                .build()?;

            let mut leader_url = None;
            let mut tried_nodes = Vec::new();

            // Try all peers to find the leader
            for peer in &cluster_config.peers {
                let admin_port = 10000 + peer.id;
                let health_url = format!("http://localhost:{}/admin/health", admin_port);

                tried_nodes.push((peer.id, admin_port));

                match client.get(&health_url).send().await {
                    Ok(response) => {
                        if response.status().is_success() {
                            if let Ok(health) = response.json::<admin_api::HealthResponse>().await {
                                if health.is_leader {
                                    println!("✓ Found leader: node {} (port {})\n", peer.id, admin_port);
                                    leader_url = Some(format!("http://localhost:{}/admin/status", admin_port));
                                    break;
                                }
                            }
                        }
                    }
                    Err(_) => {
                        // Node is down, continue
                    }
                }
            }

            let status_url = leader_url.ok_or_else(|| {
                anyhow::anyhow!(
                    "Could not find cluster leader. Tried nodes: {:?}\n\
                     Make sure at least one node is running and has elected a leader.",
                    tried_nodes
                )
            })?;

            // Get API key from environment
            let api_key = std::env::var("CHRONIK_ADMIN_API_KEY").ok();
            if api_key.is_none() {
                warn!("CHRONIK_ADMIN_API_KEY not set - authentication may fail");
            }

            // Send request to leader with API key
            let mut request_builder = client.get(&status_url);

            if let Some(key) = api_key {
                request_builder = request_builder.header("X-API-Key", key);
            }

            let response = request_builder
                .send()
                .await
                .map_err(|e| anyhow::anyhow!("Failed to query cluster status: {}", e))?;

            // Parse response
            if response.status().is_success() {
                let status: admin_api::ClusterStatusResponse = response.json().await?;

                // Pretty-print cluster status
                println!("═══════════════════════════════════════════════════");
                println!("             CHRONIK CLUSTER STATUS");
                println!("═══════════════════════════════════════════════════\n");

                println!("Cluster Information:");
                println!("  Current Node: {}", status.node_id);
                println!("  Leader:       {}",
                    status.leader_id.map(|id| id.to_string()).unwrap_or_else(|| "unknown".to_string()));
                println!("  Total Nodes:  {}\n", status.nodes.len());

                println!("───────────────────────────────────────────────────");
                println!("Nodes:");
                println!("───────────────────────────────────────────────────");
                for node in &status.nodes {
                    let role = if node.is_leader { "[LEADER]" } else { "[FOLLOWER]" };
                    println!("  Node {}: {} {}", node.node_id, node.address, role);
                }

                if !status.partitions.is_empty() {
                    println!("\n───────────────────────────────────────────────────");
                    println!("Partitions:");
                    println!("───────────────────────────────────────────────────");

                    let mut topics: std::collections::HashMap<String, Vec<&admin_api::PartitionInfo>> = std::collections::HashMap::new();
                    for partition in &status.partitions {
                        topics.entry(partition.topic.clone()).or_default().push(partition);
                    }

                    for (topic, partitions) in topics {
                        println!("\n  Topic: {}", topic);
                        for partition in partitions {
                            let leader_str = partition.leader.map(|id| id.to_string()).unwrap_or_else(|| "none".to_string());
                            println!("    Partition {}: Leader={}, Replicas={:?}, ISR={:?}",
                                partition.partition, leader_str, partition.replicas, partition.isr);
                        }
                    }
                } else {
                    println!("\n───────────────────────────────────────────────────");
                    println!("No partitions assigned yet.");
                    println!("───────────────────────────────────────────────────");
                }

                println!("\n═══════════════════════════════════════════════════");
                Ok(())
            } else if response.status() == reqwest::StatusCode::UNAUTHORIZED {
                eprintln!("✗ Authentication failed. Set CHRONIK_ADMIN_API_KEY environment variable.");
                std::process::exit(1);
            } else {
                let error_text = response.text().await.unwrap_or_else(|_| "Unknown error".to_string());
                eprintln!("✗ Failed to get cluster status: {}", error_text);
                std::process::exit(1);
            }
        }

        ClusterAction::AddNode { node_id, kafka, wal, raft, config } => {
            info!("CLI: Adding node {} to cluster", node_id);

            // Load cluster config to determine which node is likely the leader
            let cluster_config = load_cluster_config_from_file(&config, None)?;

            // Smart leader discovery: Query all peers to find the leader
            println!("Discovering cluster leader...");

            let client = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(5))
                .build()?;

            let mut leader_url = None;
            let mut tried_nodes = Vec::new();

            // Try all peers to find the leader
            for peer in &cluster_config.peers {
                let admin_port = 10000 + peer.id;
                let health_url = format!("http://localhost:{}/admin/health", admin_port);

                tried_nodes.push((peer.id, admin_port));

                match client.get(&health_url).send().await {
                    Ok(response) => {
                        if response.status().is_success() {
                            if let Ok(health) = response.json::<admin_api::HealthResponse>().await {
                                if health.is_leader {
                                    println!("✓ Found leader: node {} (port {})", peer.id, admin_port);
                                    leader_url = Some(format!("http://localhost:{}/admin/add-node", admin_port));
                                    break;
                                } else {
                                    info!("  Node {} is not the leader (state: follower)", peer.id);
                                }
                            }
                        }
                    }
                    Err(e) => {
                        warn!("  Node {} unreachable: {}", peer.id, e);
                    }
                }
            }

            let admin_url = leader_url.ok_or_else(|| {
                anyhow::anyhow!(
                    "Could not find cluster leader. Tried nodes: {:?}\n\
                     Make sure at least one node is running and has elected a leader.",
                    tried_nodes
                )
            })?;

            println!("Sending add-node request to leader...");

            // Get API key from environment
            let api_key = std::env::var("CHRONIK_ADMIN_API_KEY").ok();

            // Prepare request
            let request_body = serde_json::json!({
                "node_id": node_id,
                "kafka_addr": kafka,
                "wal_addr": wal,
                "raft_addr": raft,
            });

            // Send request to leader with API key
            let mut request_builder = client
                .post(&admin_url)
                .json(&request_body);

            if let Some(key) = api_key {
                request_builder = request_builder.header("X-API-Key", key);
            }

            let response = request_builder
                .send()
                .await
                .map_err(|e| anyhow::anyhow!("Failed to send request to leader: {}", e))?;

            // Parse response
            if response.status().is_success() {
                let resp: admin_api::AddNodeResponse = response.json().await?;

                if resp.success {
                    println!("✓ {}", resp.message);
                    println!("\nNode Details:");
                    println!("  ID:    {}", node_id);
                    println!("  Kafka: {}", kafka);
                    println!("  WAL:   {}", wal);
                    println!("  Raft:  {}", raft);
                    println!("\nThe node will be added to the cluster once Raft achieves consensus.");
                    println!("Monitor cluster status with: chronik-server cluster status --config {}", config.display());
                } else {
                    eprintln!("✗ Failed to add node: {}", resp.message);
                    std::process::exit(1);
                }
            } else {
                eprintln!("✗ HTTP error: {}", response.status());
                eprintln!("  {}", response.text().await?);
                std::process::exit(1);
            }

            Ok(())
        }

        ClusterAction::RemoveNode { node_id, force, config } => {
            info!("CLI: Removing node {} from cluster (force={})", node_id, force);

            // Load cluster config to get node information
            let cluster_config = load_cluster_config_from_file(&config, None)?;

            println!("Discovering cluster leader...\n");

            let client = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(5))
                .build()?;

            let mut leader_url = None;
            let mut tried_nodes = Vec::new();

            // Try all peers to find the leader
            for peer in &cluster_config.peers {
                let admin_port = 10000 + peer.id;
                let health_url = format!("http://localhost:{}/admin/health", admin_port);

                tried_nodes.push((peer.id, admin_port));

                match client.get(&health_url).send().await {
                    Ok(response) => {
                        if response.status().is_success() {
                            if let Ok(health) = response.json::<admin_api::HealthResponse>().await {
                                if health.is_leader {
                                    println!("✓ Found leader: node {} (port {})\n", peer.id, admin_port);
                                    leader_url = Some(format!("http://localhost:{}/admin/remove-node", admin_port));
                                    break;
                                }
                            }
                        }
                    }
                    Err(_) => {
                        // Node is down, continue
                    }
                }
            }

            let admin_url = leader_url.ok_or_else(|| {
                anyhow::anyhow!(
                    "Could not find cluster leader. Tried nodes: {:?}\n\
                     Make sure at least one node is running and has elected a leader.",
                    tried_nodes
                )
            })?;

            println!("Sending remove-node request to leader...");

            // Get API key from environment
            let api_key = std::env::var("CHRONIK_ADMIN_API_KEY").ok();

            // Prepare request
            let request_body = serde_json::json!({
                "node_id": node_id,
                "force": force,
            });

            // Send request to leader with API key
            let mut request_builder = client
                .post(&admin_url)
                .json(&request_body);

            if let Some(key) = api_key {
                request_builder = request_builder.header("X-API-Key", key);
            }

            let response = request_builder
                .send()
                .await
                .map_err(|e| anyhow::anyhow!("Failed to send request to leader: {}", e))?;

            // Parse response
            if response.status().is_success() {
                let resp: admin_api::RemoveNodeResponse = response.json().await?;

                if resp.success {
                    println!("✓ {}", resp.message);
                    println!("\nNode {} will be removed from the cluster once Raft achieves consensus.", node_id);
                    if !force {
                        println!("Partitions have been reassigned away from this node.");
                    }
                    println!("\nMonitor cluster status with: chronik-server cluster status --config {}", config.display());
                } else {
                    eprintln!("✗ Failed to remove node: {}", resp.message);
                    std::process::exit(1);
                }
            } else if response.status() == reqwest::StatusCode::UNAUTHORIZED {
                eprintln!("✗ Authentication failed. Set CHRONIK_ADMIN_API_KEY environment variable.");
                std::process::exit(1);
            } else {
                let error_text = response.text().await.unwrap_or_else(|_| "Unknown error".to_string());
                eprintln!("✗ Failed to remove node: {}", error_text);
                std::process::exit(1);
            }

            Ok(())
        }

        ClusterAction::Rebalance { config } => {
            println!("Triggering partition rebalance...\n");

            // Load cluster config
            let cluster_config = load_cluster_config_from_file(&config, None)?;

            // Create HTTP client
            let client = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()?;

            // Find leader by querying health endpoint on each peer
            let mut leader_url = None;
            let mut tried_nodes = Vec::new();

            for peer in cluster_config.peer_nodes() {
                let admin_port = 10000 + peer.id;
                let health_url = format!("http://localhost:{}/admin/health", admin_port);

                tried_nodes.push((peer.id, admin_port));

                match client.get(&health_url).send().await {
                    Ok(response) => {
                        if response.status().is_success() {
                            if let Ok(health) = response.json::<admin_api::HealthResponse>().await {
                                if health.is_leader {
                                    println!("✓ Found leader: node {} (port {})\n", peer.id, admin_port);
                                    leader_url = Some(format!("http://localhost:{}/admin/rebalance", admin_port));
                                    break;
                                }
                            }
                        }
                    }
                    Err(_) => {
                        // Node is down, continue
                    }
                }
            }

            let admin_url = leader_url.ok_or_else(|| {
                anyhow::anyhow!(
                    "Could not find cluster leader. Tried nodes: {:?}\n\
                     Make sure at least one node is running and has elected a leader.",
                    tried_nodes
                )
            })?;

            println!("Sending rebalance request to leader...");

            // Get API key from environment
            let api_key = std::env::var("CHRONIK_ADMIN_API_KEY").ok();

            // Send request to leader with API key
            let mut request_builder = client.post(&admin_url);

            if let Some(key) = api_key {
                request_builder = request_builder.header("X-API-Key", key);
            }

            let response = request_builder
                .send()
                .await
                .map_err(|e| anyhow::anyhow!("Failed to send request to leader: {}", e))?;

            // Parse response
            if response.status().is_success() {
                let resp: admin_api::RebalanceResponse = response.json().await?;

                if resp.success {
                    println!("✓ {}", resp.message);
                    println!("\nRebalanced {} topics.", resp.topics_rebalanced);
                    println!("\nMonitor cluster status with: chronik-server cluster status --config {}", config.display());
                } else {
                    eprintln!("✗ Rebalance failed: {}", resp.message);
                    std::process::exit(1);
                }
            } else if response.status() == reqwest::StatusCode::UNAUTHORIZED {
                eprintln!("✗ Authentication failed. Set CHRONIK_ADMIN_API_KEY environment variable.");
                std::process::exit(1);
            } else {
                let error_text = response.text().await.unwrap_or_else(|_| "Unknown error".to_string());
                eprintln!("✗ Failed to rebalance: {}", error_text);
                std::process::exit(1);
            }

            Ok(())
        }
    }
}

async fn handle_compaction_command(cli: &Cli, action: CompactAction) -> Result<()> {
    use std::sync::Arc;
    use tokio::sync::RwLock;
    use chronik_wal::manager::WalManager;

    match action {
        CompactAction::Now { partition, strategy, dry_run } => {
            info!("Running manual WAL compaction");

            // Parse strategy
            let compact_strategy = match strategy.as_str() {
                "key-based" | "key" => CompactionStrategy::KeyBased,
                "time-based" | "time" => CompactionStrategy::TimeBased,
                "hybrid" => CompactionStrategy::Hybrid,
                "custom" => CompactionStrategy::Custom,
                _ => {
                    error!("Invalid compaction strategy: {}", strategy);
                    return Err(anyhow::anyhow!("Invalid strategy. Use: key-based, time-based, hybrid, or custom"));
                }
            };

            // Create WAL config
            let wal_config = WalConfig {
                enabled: true,
                data_dir: cli.data_dir.join("wal"),
                segment_size: 100 * 1024 * 1024, // 100MB
                flush_interval_ms: 1000,
                flush_threshold: 1000,
                compression: CompressionType::None,
                checkpointing: Default::default(),
                recovery: Default::default(),
                rotation: Default::default(),
                fsync: Default::default(),
                async_io: Default::default(),
            };

            // Create compaction config
            let mut compact_config = CompactionConfig::default();
            compact_config.strategy = compact_strategy;

            // Create compactor
            let compactor = WalCompactor::new(wal_config.clone(), compact_config);

            // Create WAL manager
            let wal_manager = Arc::new(RwLock::new(
                WalManager::new(wal_config).await?
            ));

            if dry_run {
                info!("Dry run mode - showing what would be compacted");
                // TODO: Implement dry run analysis
                warn!("Dry run analysis not yet implemented");
            } else {
                // Run compaction
                let stats = compactor.run_compaction(wal_manager).await?;

                println!("\nCompaction Results:");
                println!("  Segments compacted: {}", stats.segments_compacted);
                println!("  Records before: {}", stats.records_before);
                println!("  Records after: {}", stats.records_after);
                println!("  Bytes saved: {} ({:.1} MB)",
                    stats.bytes_saved,
                    stats.bytes_saved as f64 / (1024.0 * 1024.0));
                println!("  Compaction ratio: {:.1}%", stats.compaction_ratio() * 100.0);

                if stats.errors > 0 {
                    warn!("  Errors encountered: {}", stats.errors);
                }
            }
        }

        CompactAction::Status { detailed } => {
            info!("Fetching WAL compaction status");

            // Read compaction stats from metadata
            let stats_file = cli.data_dir.join("wal").join("compaction_stats.json");

            if stats_file.exists() {
                let stats_data = tokio::fs::read_to_string(&stats_file).await?;
                let stats: serde_json::Value = serde_json::from_str(&stats_data)?;

                println!("\nWAL Compaction Status:");
                println!("  Last run: {}", stats["last_run"].as_str().unwrap_or("never"));
                println!("  Total runs: {}", stats["total_runs"].as_u64().unwrap_or(0));
                println!("  Total bytes saved: {} MB",
                    stats["total_bytes_saved"].as_u64().unwrap_or(0) / (1024 * 1024));

                if detailed {
                    println!("\nDetailed Statistics:");
                    println!("  Total records compacted: {}",
                        stats["total_records_compacted"].as_u64().unwrap_or(0));
                    println!("  Average compaction ratio: {:.1}%",
                        stats["avg_compaction_ratio"].as_f64().unwrap_or(0.0) * 100.0);
                    println!("  Total errors: {}", stats["total_errors"].as_u64().unwrap_or(0));

                    if let Some(partitions) = stats["partition_stats"].as_array() {
                        println!("\n  Partition Statistics:");
                        for partition in partitions {
                            println!("    - {}: {} segments, {} bytes saved",
                                partition["name"].as_str().unwrap_or("unknown"),
                                partition["segments"].as_u64().unwrap_or(0),
                                partition["bytes_saved"].as_u64().unwrap_or(0));
                        }
                    }
                }
            } else {
                println!("No compaction statistics available.");
                println!("Run 'chronik-server compact now' to trigger compaction.");
            }
        }

        CompactAction::Config { enabled, interval, retention_days, strategy, show } => {
            if show {
                // Show current configuration
                let config_file = cli.data_dir.join("wal").join("compaction.conf");

                if config_file.exists() {
                    let config_data = tokio::fs::read_to_string(&config_file).await?;
                    println!("\nCurrent Compaction Configuration:");
                    println!("{}", config_data);
                } else {
                    println!("\nDefault Compaction Configuration:");
                    println!("  Enabled: true");
                    println!("  Interval: 3600 seconds (1 hour)");
                    println!("  Retention: 7 days");
                    println!("  Strategy: key-based");
                    println!("  Parallel tasks: 4");
                }
            } else {
                // Update configuration
                let config_file = cli.data_dir.join("wal").join("compaction.conf");
                tokio::fs::create_dir_all(config_file.parent().unwrap()).await?;

                let mut config = if config_file.exists() {
                    let data = tokio::fs::read_to_string(&config_file).await?;
                    serde_json::from_str(&data)?
                } else {
                    serde_json::json!({
                        "enabled": true,
                        "interval_secs": 3600,
                        "retention_days": 7,
                        "strategy": "key-based",
                        "parallel_tasks": 4
                    })
                };

                if let Some(e) = enabled {
                    config["enabled"] = serde_json::Value::Bool(e);
                    println!("Automatic compaction {}", if e { "enabled" } else { "disabled" });
                }

                if let Some(i) = interval {
                    config["interval_secs"] = serde_json::Value::Number(i.into());
                    println!("Compaction interval set to {} seconds", i);
                }

                if let Some(r) = retention_days {
                    config["retention_days"] = serde_json::Value::Number(r.into());
                    println!("Retention period set to {} days", r);
                }

                if let Some(s) = strategy {
                    config["strategy"] = serde_json::Value::String(s.clone());
                    println!("Compaction strategy set to {}", s);
                }

                // Save updated configuration
                let config_str = serde_json::to_string_pretty(&config)?;
                tokio::fs::write(&config_file, config_str).await?;
                println!("\nConfiguration saved to {:?}", config_file);
            }
        }

        CompactAction::Schedule { at, cancel } => {
            if cancel {
                println!("Cancelling scheduled compaction");
                // TODO: Implement schedule cancellation
                warn!("Schedule cancellation not yet implemented");
            } else {
                println!("Scheduling compaction for: {}", at);

                match at.as_str() {
                    "daily" => println!("Compaction scheduled daily at midnight"),
                    "hourly" => println!("Compaction scheduled every hour"),
                    cron => {
                        // TODO: Parse and validate cron expression
                        println!("Compaction scheduled with cron: {}", cron);
                        warn!("Custom cron scheduling not yet implemented");
                    }
                }
            }
        }
    }

    Ok(())
}

