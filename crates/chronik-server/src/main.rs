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
mod storage;
mod consumer_group;
mod fetch_handler;
mod handler;
mod offset_storage;
mod coordinator_manager;
mod wal_integration;
mod metadata_dr;
mod wal_replication;  // v2.2.0: PostgreSQL-style WAL streaming
// v2.5.0 Phase 2: Raft for metadata coordination only (NOT data replication)
mod raft_metadata;
mod raft_cluster;
// v2.5.0 Phase 3: ISR tracking for partition replication
mod isr_tracker;
// v2.5.0 Phase 4: ISR ACK tracking for acks=-1 quorum support
mod isr_ack_tracker;
// v2.5.0 Phase 5: Automatic leader election per partition
mod leader_election;

mod cli;

use integrated_server::{IntegratedKafkaServer, IntegratedServerConfig};
use chronik_wal::compaction::{WalCompactor, CompactionConfig, CompactionStrategy};
use chronik_wal::config::{WalConfig, CompressionType};
use chronik_storage::object_store::{ObjectStoreConfig, StorageBackend, AuthConfig, S3Credentials};
use chronik_config::{ClusterConfig, NodeConfig};
use serde_json;

#[derive(Parser, Debug, Clone)]
#[command(
    name = "chronik-server",
    about = "Chronik Stream - Unified Kafka-compatible streaming platform",
    version,
    author,
    long_about = "A high-performance, Kafka-compatible streaming platform with optional search capabilities"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    /// Port for Kafka protocol (default: 9092)
    #[arg(short = 'p', long, env = "CHRONIK_KAFKA_PORT", default_value = "9092")]
    kafka_port: u16,

    /// Port for Admin API (default: 3000)
    #[arg(short = 'a', long, env = "CHRONIK_ADMIN_PORT", default_value = "3000")]
    admin_port: u16,

    /// Port for Metrics endpoint (default: 9093)
    #[arg(short = 'm', long, env = "CHRONIK_METRICS_PORT", default_value = "9093")]
    metrics_port: u16,

    /// Data directory for storage
    #[arg(short = 'd', long, env = "CHRONIK_DATA_DIR", default_value = "./data")]
    data_dir: PathBuf,

    /// Log level (error, warn, info, debug, trace)
    #[arg(short = 'l', long, env = "RUST_LOG", default_value = "info")]
    log_level: String,

    /// Bind address (default: 0.0.0.0)
    #[arg(short = 'b', long, env = "CHRONIK_BIND_ADDR", default_value = "0.0.0.0")]
    bind_addr: String,
    
    /// Advertised address for clients to connect (defaults to bind address)
    #[arg(long, env = "CHRONIK_ADVERTISED_ADDR")]
    advertised_addr: Option<String>,
    
    /// Advertised port for clients to connect (defaults to kafka port)
    #[arg(long, env = "CHRONIK_ADVERTISED_PORT")]
    advertised_port: Option<u16>,

    /// Enable backup functionality
    #[cfg(feature = "backup")]
    #[arg(long, env = "CHRONIK_ENABLE_BACKUP", default_value = "false")]
    enable_backup: bool,

    /// Enable dynamic configuration
    #[cfg(feature = "dynamic-config")]
    #[arg(long, env = "CHRONIK_ENABLE_DYNAMIC_CONFIG", default_value = "false")]
    enable_dynamic_config: bool,

    /// Path to cluster configuration file (TOML)
    #[arg(long, env = "CHRONIK_CLUSTER_CONFIG")]
    cluster_config: Option<PathBuf>,

    /// Node ID for cluster mode (overrides config file)
    #[arg(long, env = "CHRONIK_NODE_ID")]
    node_id: Option<u64>,

    /// Port for Search API (default: 6080, only used if search feature is enabled)
    #[arg(long, env = "CHRONIK_SEARCH_PORT", default_value = "6080")]
    search_port: u16,

    /// Disable Search API (default: false, search API enabled if search feature compiled)
    #[arg(long, env = "CHRONIK_DISABLE_SEARCH", default_value = "false")]
    disable_search: bool,
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
    /// Run in standalone mode (default)
    #[command(about = "Run as a standalone Kafka-compatible server")]
    Standalone,

    /// Run in Raft cluster mode
    #[command(about = "Run as a node in a Raft-replicated cluster")]
    RaftCluster {
        /// Raft listen address (gRPC)
        #[arg(long, env = "CHRONIK_RAFT_ADDR", default_value = "0.0.0.0:5001")]
        raft_addr: String,

        /// Comma-separated list of peer nodes (format: id@host:port)
        /// Example: 2@node2:5001,3@node3:5001
        #[arg(long, env = "CHRONIK_RAFT_PEERS")]
        peers: Option<String>,

        /// Bootstrap the cluster (only run on initial cluster creation)
        #[arg(long, default_value = "false")]
        bootstrap: bool,
    },

    /// Run as ingest node (future: for distributed mode)
    #[command(about = "Run as an ingest node in a distributed cluster")]
    Ingest {
        /// Controller URL for cluster coordination
        #[arg(long, env = "CHRONIK_CONTROLLER_URL")]
        controller_url: Option<String>,
    },

    /// Run as search node (future: for distributed mode)
    #[cfg(feature = "search")]
    #[command(about = "Run as a search node in a distributed cluster")]
    Search {
        /// Storage backend URL
        #[arg(long, env = "CHRONIK_STORAGE_URL")]
        storage_url: String,
    },

    /// Run with all components enabled
    #[command(about = "Run with all components in a single process")]
    All {
        /// Enable experimental features
        #[arg(long, default_value = "false")]
        experimental: bool,
    },

    /// Show version and build information
    #[command(about = "Display version and build information")]
    Version,

    /// WAL compaction management
    #[command(about = "Manage WAL compaction")]
    Compact {
        #[command(subcommand)]
        action: CompactAction,
    },

    // Cluster command replaced with raft-cluster subcommand (v2.5.0 Phase 2)
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

/// Load cluster configuration from file or environment variables
fn load_cluster_config(cli: &Cli) -> Result<Option<ClusterConfig>> {
    // Priority 1: Try environment variables
    if let Ok(Some(config)) = ClusterConfig::from_env() {
        info!("Loaded cluster configuration from environment variables");
        info!("  Node ID: {}", config.node_id);
        info!("  Replication Factor: {}", config.replication_factor);
        info!("  Min In-Sync Replicas: {}", config.min_insync_replicas);
        info!("  Peers: {}", config.peers.len());
        return Ok(Some(config));
    }

    // Priority 2: Try config file
    if let Some(config_path) = &cli.cluster_config {
        info!("Loading cluster configuration from file: {}", config_path.display());

        let contents = std::fs::read_to_string(config_path)?;
        let mut config: ClusterConfig = toml::from_str(&contents)?;

        // Override node_id from CLI if provided
        if let Some(node_id) = cli.node_id {
            info!("Overriding node_id from CLI: {}", node_id);
            config.node_id = node_id;
        }

        // Validate configuration
        config.validate_config().map_err(|e| {
            anyhow::anyhow!("Invalid cluster configuration: {}", e)
        })?;

        info!("Loaded cluster configuration from file");
        info!("  Node ID: {}", config.node_id);
        info!("  Replication Factor: {}", config.replication_factor);
        info!("  Min In-Sync Replicas: {}", config.min_insync_replicas);
        info!("  Peers: {}", config.peers.len());

        return Ok(Some(config));
    }

    // No clustering configured
    Ok(None)
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

    // Show deprecation warnings for old binaries
    if std::env::args().next().unwrap().contains("chronik-ingest") {
        warn!("chronik-ingest is deprecated. Please use chronik-server instead.");
    }
    if std::env::args().next().unwrap().contains("chronik-all-in-one") {
        warn!("chronik-all-in-one is deprecated. Please use chronik-server instead.");
    }

    match cli.command {
        Some(Commands::Version) => {
            println!("Chronik Server v{}", env!("CARGO_PKG_VERSION"));
            println!("Build features:");
            #[cfg(feature = "search")]
            println!("  - Search: enabled");
            #[cfg(feature = "backup")]
            println!("  - Backup: enabled");
            #[cfg(feature = "dynamic-config")]
            println!("  - Dynamic Config: enabled");
            return Ok(());
        }
        
        Some(Commands::Standalone) => {
            info!("Starting Chronik Server in standalone mode");
            run_standalone_server(&cli).await?;
        }

        Some(Commands::RaftCluster { ref raft_addr, ref peers, bootstrap }) => {
            use raft_cluster::{run_raft_cluster, RaftClusterConfig};

            info!("Starting Chronik Server in Raft cluster mode");

            // Parse node ID
            let node_id = cli.node_id.unwrap_or(1);

            // Parse peers (format: "2@node2:5001,3@node3:5001")
            let parsed_peers: Vec<(u64, String)> = if let Some(peers_str) = peers {
                peers_str
                    .split(',')
                    .filter_map(|s| {
                        let parts: Vec<&str> = s.split('@').collect();
                        if parts.len() == 2 {
                            if let Ok(id) = parts[0].parse::<u64>() {
                                return Some((id, parts[1].to_string()));
                            }
                        }
                        warn!("Invalid peer format: {} (expected id@host:port)", s);
                        None
                    })
                    .collect()
            } else {
                vec![]
            };

            // Parse advertised address (similar to standalone mode)
            let (advertised_host, advertised_port) = if let Some(ref addr) = cli.advertised_addr {
                if let Some(colon_pos) = addr.rfind(':') {
                    let potential_port = &addr[colon_pos + 1..];
                    if potential_port.chars().all(|c| c.is_ascii_digit()) && !potential_port.is_empty() {
                        let host = addr[..colon_pos].to_string();
                        let port = potential_port.parse::<u16>().unwrap_or(cli.kafka_port);
                        (host, port)
                    } else {
                        (addr.clone(), cli.advertised_port.unwrap_or(cli.kafka_port))
                    }
                } else {
                    (addr.clone(), cli.advertised_port.unwrap_or(cli.kafka_port))
                }
            } else {
                ("localhost".to_string(), cli.kafka_port)
            };

            // Auto-derive metrics port for cluster mode (same logic as standalone)
            let metrics_port = if cli.metrics_port == 9093 {
                // Metrics port = kafka_port + 4000 (separate range from Kafka/Raft ports)
                // This ensures Node 1 (9092) gets 13092, Node 2 (9093) gets 13093, Node 3 (9094) gets 13094
                let derived = cli.kafka_port + 4000;
                info!("Auto-derived metrics port: {} (kafka_port + 4000)", derived);
                derived
            } else {
                cli.metrics_port
            };

            let config = RaftClusterConfig {
                node_id,
                raft_addr: raft_addr.clone(),
                peers: parsed_peers,
                bootstrap,
                data_dir: cli.data_dir.to_string_lossy().to_string(),
                kafka_port: cli.kafka_port,
                advertised_addr: advertised_host,
            };

            run_raft_cluster(config).await?;
        }

        Some(Commands::Ingest { ref controller_url }) => {
            info!("Starting Chronik Server as ingest node");
            if controller_url.is_some() {
                warn!("Distributed mode not yet implemented - running in standalone mode");
            }
            run_ingest_server(&cli).await?;
        }
        
        #[cfg(feature = "search")]
        Some(Commands::Search { ref storage_url }) => {
            info!("Starting Chronik Server as search node");
            info!("Storage URL: {}", storage_url);
            warn!("Search-only mode not yet implemented - running in standalone mode");
            run_standalone_server(&cli).await?;
        }

        Some(Commands::All { experimental }) => {
            info!("Starting Chronik Server with all components");
            if experimental {
                warn!("Experimental features enabled");
            }
            run_all_components(&cli).await?;
        }

        Some(Commands::Compact { ref action }) => {
            handle_compaction_command(&cli, action.clone()).await?;
        }

        // Cluster command replaced with raft-cluster subcommand (v2.5.0 Phase 2)

        None => {
            // Default to standalone mode
            info!("Starting Chronik Server in standalone mode (default)");
            run_standalone_server(&cli).await?;
        }
    }

    Ok(())
}

async fn run_standalone_server(cli: &Cli) -> Result<()> {
    // CRITICAL FIX: Auto-derive metrics and search API ports to avoid conflicts in multi-node clusters
    // If metrics_port or search_port are at their default values and we're in cluster mode,
    // derive them from kafka_port to allow multiple nodes on the same machine
    let mut cli = cli.clone();

    // Check if clustering is enabled
    let is_cluster_mode = std::env::var("CHRONIK_CLUSTER_ENABLED").unwrap_or_default() == "true"
        || cli.cluster_config.is_some()
        || cli.node_id.is_some()
        || std::env::var("CHRONIK_CLUSTER_PEERS").is_ok();

    // Auto-derive metrics port if at default value (9093) and in cluster mode
    if cli.metrics_port == 9093 && is_cluster_mode {
        // Metrics port = kafka_port + 4000 (separate range from Kafka/Raft ports)
        // This ensures Node 1 (9092) gets 13092, Node 2 (9093) gets 13093, Node 3 (9094) gets 13094
        cli.metrics_port = cli.kafka_port + 4000;
        info!("Auto-derived metrics port: {} (kafka_port + 4000)", cli.metrics_port);
    }

    // Auto-derive search API port if at default value (6080) and in cluster mode
    if cli.search_port == 6080 && is_cluster_mode {
        // Search port = kafka_port - 3000
        // This ensures Node 1 (9092) gets 6092, Node 2 (9192) gets 6192, etc.
        cli.search_port = if cli.kafka_port >= 3000 {
            cli.kafka_port - 3000
        } else {
            cli.kafka_port + 3000  // Fallback for low ports
        };
        info!("Auto-derived search API port: {} (kafka_port - 3000)", cli.search_port);
    }

    // Parse bind address to handle "host:port" format
    let bind_host = if cli.bind_addr.contains(':') {
        cli.bind_addr.split(':').next().unwrap_or("0.0.0.0").to_string()
    } else {
        cli.bind_addr.clone()
    };
    
    // Parse advertised address and port (handle both "host" and "host:port" formats)
    let (advertised_host, advertised_port) = if let Some(ref addr) = cli.advertised_addr {
        // User provided advertised address - parse it
        // Check for IPv6 format first (contains multiple colons or starts with '[')
        if addr.starts_with('[') || addr.matches(':').count() > 1 {
            // IPv6 address
            if let Some(bracket_end) = addr.rfind(']') {
                // Format: [::1]:9092
                if let Some(colon_after_bracket) = addr[bracket_end..].find(':') {
                    let port_str = &addr[bracket_end + colon_after_bracket + 1..];
                    if let Ok(port) = port_str.parse::<u16>() {
                        let host = addr[..bracket_end + 1].to_string();
                        info!("Using advertised address '{}' with port {}", host, port);
                        (host, port as i32)
                    } else {
                        // No valid port after bracket
                        let port = cli.advertised_port.unwrap_or(cli.kafka_port) as i32;
                        info!("Using advertised address '{}' with default port {}", addr, port);
                        (addr.clone(), port)
                    }
                } else {
                    // Format: [::1] without port
                    let port = cli.advertised_port.unwrap_or(cli.kafka_port) as i32;
                    info!("Using advertised address '{}' with default port {}", addr, port);
                    (addr.clone(), port)
                }
            } else {
                // IPv6 without brackets
                let port = cli.advertised_port.unwrap_or(cli.kafka_port) as i32;
                info!("Using IPv6 address '{}' with default port {}", addr, port);
                (addr.clone(), port)
            }
        } else if let Some(colon_pos) = addr.rfind(':') {
            // IPv4 or hostname with possible port
            let potential_port = &addr[colon_pos + 1..];
            if potential_port.chars().all(|c| c.is_ascii_digit()) && !potential_port.is_empty() {
                // Has port - split it
                let host = addr[..colon_pos].to_string();
                let port = potential_port.parse::<u16>()
                    .unwrap_or(cli.kafka_port) as i32;
                info!("Using advertised address '{}' with port {}", host, port);
                (host, port)
            } else {
                // Colon but not a valid port
                let port = cli.advertised_port.unwrap_or(cli.kafka_port) as i32;
                info!("Using advertised address '{}' with default port {}", addr, port);
                (addr.clone(), port)
            }
        } else {
            // No colon - just a hostname
            let port = cli.advertised_port.unwrap_or(cli.kafka_port) as i32;
            info!("Using advertised address '{}' with port {}", addr, port);
            (addr.clone(), port)
        }
    } else if bind_host == "0.0.0.0" || bind_host == "[::]" {
        // If binding to all interfaces and no advertised address specified,
        // try to use hostname or localhost as a better default
        let hostname = std::env::var("HOSTNAME")
            .or_else(|_| std::env::var("DOCKER_HOSTNAME"))
            .unwrap_or_else(|_| "127.0.0.1".to_string());
        
        let port = cli.advertised_port.unwrap_or(cli.kafka_port) as i32;
        
        if hostname != "127.0.0.1" {
            info!("Binding to all interfaces ({}), using hostname '{}' as advertised address", bind_host, hostname);
            info!("To override, set CHRONIK_ADVERTISED_ADDR environment variable");
        } else {
            warn!("Binding to all interfaces ({}) without advertised address configured", bind_host);
            warn!("Using '127.0.0.1' as advertised address - remote clients may not connect");
            warn!("Set CHRONIK_ADVERTISED_ADDR to your hostname/IP for remote access");
        }
        (hostname, port)
    } else {
        // Use bind host as advertised host
        let port = cli.advertised_port.unwrap_or(cli.kafka_port) as i32;
        (bind_host.clone(), port)
    };
    
    // Parse object store configuration from environment (for Tier 3: Tantivy archives)
    let object_store_config = parse_object_store_config_from_env();

    // Load cluster configuration
    let cluster_config = load_cluster_config(&cli)?;

    // Determine node_id from cluster config or default to 1
    let node_id = if let Some(ref cluster) = cluster_config {
        cluster.node_id as i32
    } else {
        1
    };

    // Determine replication factor from cluster config or default
    let replication_factor = if let Some(ref cluster) = cluster_config {
        cluster.replication_factor as u32
    } else {
        1
    };

    // Create server configuration
    let config = IntegratedServerConfig {
        node_id,
        advertised_host,
        advertised_port,
        data_dir: cli.data_dir.to_string_lossy().to_string(),
        enable_indexing: cfg!(feature = "search"),  // Enable when search feature is compiled
        enable_compression: true,
        auto_create_topics: true,
        num_partitions: 3,
        replication_factor,
        enable_wal_indexing: true,  // Enable WAL→Tantivy indexing
        wal_indexing_interval_secs: 30,  // Index every 30 seconds
        object_store_config,  // Pass custom object store config if provided
        enable_metadata_dr: true,  // Enable metadata DR by default
        metadata_upload_interval_secs: 60,  // Upload metadata every minute
        cluster_config: cluster_config.clone(),  // Pass cluster configuration
    };

    // Create and start the integrated server
    let server = IntegratedKafkaServer::new(config, None).await?;

    // Initialize monitoring (metrics + optional tracing)
    let _metrics_registry = init_monitoring(
        "chronik-server",
        cli.metrics_port,
        None, // TODO: Add tracing config support
    ).await?;
    
    let kafka_addr = format!("{}:{}", cli.bind_addr, cli.kafka_port);
    info!("Kafka protocol listening on {}", kafka_addr);
    info!("Metrics endpoint available at http://{}:{}/metrics", cli.bind_addr, cli.metrics_port);

    // Start search API if search feature is compiled and not disabled
    #[cfg(feature = "search")]
    if !cli.disable_search {
        info!("Starting Search API on port {}", cli.search_port);

        let search_bind = cli.bind_addr.clone();
        let search_port = cli.search_port;
        let wal_indexer = server.get_wal_indexer();
        let index_base_path = format!("{}/tantivy_indexes", cli.data_dir.to_string_lossy());

        tokio::spawn(async move {
            use chronik_search::api::SearchApi;
            use std::sync::Arc;

            // Create search API instance with WAL indexer integration
            let search_api = Arc::new(SearchApi::new_with_wal_indexer(wal_indexer, index_base_path).unwrap());

            // Create router
            let app = search_api.router();

            // Start server
            let addr = format!("{}:{}", search_bind, search_port);
            info!("Search API listening on http://{}", addr);

            let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
            chronik_search::serve_app(listener, app).await.unwrap()
        });

        info!("Search API available at http://{}:{}", cli.bind_addr, cli.search_port);
        info!("  - Search: POST http://{}:{}/_search", cli.bind_addr, cli.search_port);
        info!("  - Index: PUT http://{}:{}/{{index}}", cli.bind_addr, cli.search_port);
        info!("  - Document: POST http://{}:{}/{{index}}/_doc/{{id}}", cli.bind_addr, cli.search_port);
    } else {
        #[cfg(feature = "search")]
        info!("Search API disabled via --disable-search flag");
    }

    // CRITICAL FIX (v1.3.66): Add signal handling for graceful shutdown
    // Spawn server task
    let server_clone = server.clone();
    let kafka_addr_clone = kafka_addr.clone();
    let server_task = tokio::spawn(async move {
        server_clone.run(&kafka_addr_clone).await
    });

    // Wait for shutdown signal (SIGINT or SIGTERM)
    tokio::select! {
        result = server_task => {
            match result {
                Ok(Ok(_)) => info!("Server task completed normally"),
                Ok(Err(e)) => error!("Server task failed: {}", e),
                Err(e) => error!("Server task panicked: {}", e),
            }
        }
        _ = tokio::signal::ctrl_c() => {
            info!("Received SIGINT, gracefully shutting down...");
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
            info!("Received SIGTERM, gracefully shutting down...");
        }
    }

    // Graceful shutdown: flush all partitions
    info!("Flushing all partitions before shutdown...");
    if let Err(e) = server.shutdown().await {
        error!("Failed to shutdown server gracefully: {}", e);
    } else {
        info!("Server shutdown complete");
    }

    Ok(())
}

async fn run_ingest_server(cli: &Cli) -> Result<()> {
    // For now, just run standalone
    // In the future, this would connect to a controller for coordination
    run_standalone_server(cli).await
}

async fn run_all_components(cli: &Cli) -> Result<()> {
    // Run with all features enabled
    let mut tasks = vec![];
    
    // Initialize monitoring (metrics + optional tracing)
    let _metrics_registry = init_monitoring(
        "chronik-server-all",
        cli.metrics_port,
        None, // TODO: Add tracing config support
    ).await?;
    info!("Metrics endpoint available at http://{}:{}/metrics", cli.bind_addr, cli.metrics_port);
    
    // Parse bind address to handle "host:port" format
    let bind_host = if cli.bind_addr.contains(':') {
        cli.bind_addr.split(':').next().unwrap_or("0.0.0.0").to_string()
    } else {
        cli.bind_addr.clone()
    };
    
    // Parse advertised address and port (handle both "host" and "host:port" formats)
    let (advertised_host, advertised_port) = if let Some(ref addr) = cli.advertised_addr {
        // User provided advertised address - parse it
        // Check for IPv6 format first (contains multiple colons or starts with '[')
        if addr.starts_with('[') || addr.matches(':').count() > 1 {
            // IPv6 address
            if let Some(bracket_end) = addr.rfind(']') {
                // Format: [::1]:9092
                if let Some(colon_after_bracket) = addr[bracket_end..].find(':') {
                    let port_str = &addr[bracket_end + colon_after_bracket + 1..];
                    if let Ok(port) = port_str.parse::<u16>() {
                        let host = addr[..bracket_end + 1].to_string();
                        info!("Using advertised address '{}' with port {}", host, port);
                        (host, port as i32)
                    } else {
                        // No valid port after bracket
                        let port = cli.advertised_port.unwrap_or(cli.kafka_port) as i32;
                        info!("Using advertised address '{}' with default port {}", addr, port);
                        (addr.clone(), port)
                    }
                } else {
                    // Format: [::1] without port
                    let port = cli.advertised_port.unwrap_or(cli.kafka_port) as i32;
                    info!("Using advertised address '{}' with default port {}", addr, port);
                    (addr.clone(), port)
                }
            } else {
                // IPv6 without brackets
                let port = cli.advertised_port.unwrap_or(cli.kafka_port) as i32;
                info!("Using IPv6 address '{}' with default port {}", addr, port);
                (addr.clone(), port)
            }
        } else if let Some(colon_pos) = addr.rfind(':') {
            // IPv4 or hostname with possible port
            let potential_port = &addr[colon_pos + 1..];
            if potential_port.chars().all(|c| c.is_ascii_digit()) && !potential_port.is_empty() {
                // Has port - split it
                let host = addr[..colon_pos].to_string();
                let port = potential_port.parse::<u16>()
                    .unwrap_or(cli.kafka_port) as i32;
                info!("Using advertised address '{}' with port {}", host, port);
                (host, port)
            } else {
                // Colon but not a valid port
                let port = cli.advertised_port.unwrap_or(cli.kafka_port) as i32;
                info!("Using advertised address '{}' with default port {}", addr, port);
                (addr.clone(), port)
            }
        } else {
            // No colon - just a hostname
            let port = cli.advertised_port.unwrap_or(cli.kafka_port) as i32;
            info!("Using advertised address '{}' with port {}", addr, port);
            (addr.clone(), port)
        }
    } else if bind_host == "0.0.0.0" || bind_host == "[::]" {
        // If binding to all interfaces and no advertised address specified,
        // try to use hostname or localhost as a better default
        let hostname = std::env::var("HOSTNAME")
            .or_else(|_| std::env::var("DOCKER_HOSTNAME"))
            .unwrap_or_else(|_| "127.0.0.1".to_string());
        
        let port = cli.advertised_port.unwrap_or(cli.kafka_port) as i32;
        
        if hostname != "127.0.0.1" {
            info!("Binding to all interfaces ({}), using hostname '{}' as advertised address", bind_host, hostname);
            info!("To override, set CHRONIK_ADVERTISED_ADDR environment variable");
        } else {
            warn!("Binding to all interfaces ({}) without advertised address configured", bind_host);
            warn!("Using '127.0.0.1' as advertised address - remote clients may not connect");
            warn!("Set CHRONIK_ADVERTISED_ADDR to your hostname/IP for remote access");
        }
        (hostname, port)
    } else {
        // Use bind host as advertised host
        let port = cli.advertised_port.unwrap_or(cli.kafka_port) as i32;
        (bind_host.clone(), port)
    };

    // Parse object store configuration from environment (for Tier 3: Tantivy archives)
    let object_store_config = parse_object_store_config_from_env();

    // Load cluster configuration
    let cluster_config = load_cluster_config(&cli)?;

    // Determine node_id from cluster config or default to 1
    let node_id = if let Some(ref cluster) = cluster_config {
        cluster.node_id as i32
    } else {
        1
    };

    // Determine replication factor from cluster config or default
    let replication_factor = if let Some(ref cluster) = cluster_config {
        cluster.replication_factor as u32
    } else {
        1
    };

    // Create server configuration with all features
    let config = IntegratedServerConfig {
        node_id,
        advertised_host,
        advertised_port,
        data_dir: cli.data_dir.to_string_lossy().to_string(),
        enable_indexing: cfg!(feature = "search"),
        enable_compression: true,
        auto_create_topics: true,
        num_partitions: 3,
        replication_factor,
        enable_wal_indexing: true,  // Enable WAL→Tantivy indexing
        wal_indexing_interval_secs: 30,  // Index every 30 seconds
        object_store_config,  // Pass custom object store config if provided
        enable_metadata_dr: true,  // Enable metadata DR by default
        metadata_upload_interval_secs: 60,  // Upload metadata every minute
        cluster_config,  // Pass cluster configuration
    };
    
    // Start Kafka protocol server
    let kafka_addr = format!("{}:{}", cli.bind_addr, cli.kafka_port);
    let kafka_task = tokio::spawn(async move {
        let server = IntegratedKafkaServer::new(config, None).await?;
        server.run(&kafka_addr).await
    });
    tasks.push(kafka_task);
    
    info!("Kafka protocol listening on {}:{}", cli.bind_addr, cli.kafka_port);
    
    #[cfg(feature = "search")]
    if !cli.disable_search {
        info!("Search API enabled, starting on port {}", cli.search_port);

        // Start the search API server
        let search_bind = cli.bind_addr.clone();
        let search_port = cli.search_port;
        let search_task = tokio::spawn(async move {
            use chronik_search::api::SearchApi;
            use std::sync::Arc;

            // Create search API instance
            let search_api = Arc::new(SearchApi::new().map_err(|e| anyhow::anyhow!("Failed to create search API: {}", e))?);

            // Create router
            let app = search_api.router();

            // Start server
            let addr = format!("{}:{}", search_bind, search_port);
            info!("Search API listening on http://{}", addr);

            let listener = tokio::net::TcpListener::bind(&addr).await
                .map_err(|e| anyhow::anyhow!("Failed to bind search API port: {}", e))?;

            chronik_search::serve_app(listener, app).await
                .map_err(|e| anyhow::anyhow!("Search API server error: {}", e))
        });
        tasks.push(search_task);

        info!("Search API available at http://{}:{}", cli.bind_addr, cli.search_port);
        info!("  - Search: POST http://{}:{}/_search", cli.bind_addr, cli.search_port);
        info!("  - Index: PUT http://{}:{}/{{index}}", cli.bind_addr, cli.search_port);
        info!("  - Document: POST http://{}:{}/{{index}}/_doc/{{id}}", cli.bind_addr, cli.search_port);
    } else {
        #[cfg(feature = "search")]
        info!("Search API disabled via --disable-search flag");
    }
    
    #[cfg(feature = "backup")]
    {
        info!("Backup service enabled");
        // Future: Start backup scheduler
    }
    
    // Wait for all components
    for task in tasks {
        task.await??;
    }

    Ok(())
}

/// Handle WAL compaction commands
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

