//! Raft cluster mode runner
//!
//! Implements the cluster mode for Chronik Server, enabling distributed
//! replication via Raft consensus with automatic health-check-based bootstrap.

use crate::raft_integration::{RaftReplicaManager, RaftManagerConfig};
use anyhow::{anyhow, Result};
use chronik_common::metadata::MetadataStore;
use chronik_config::GossipConfig;
use chronik_raft::{
    BootstrapDecision, BootstrapEvent, HealthCheckBootstrap, HealthCheckConfig, NodeMetadata,
    RaftConfig, RaftMetaLog,
    SnapshotManager, SnapshotConfig, SnapshotCompression,
};
use chronik_wal::RaftWalStorage;  // WAL-backed Raft log storage
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::signal;
use tracing::{error, info, warn};

pub struct RaftClusterConfig {
    pub node_id: u64,
    pub raft_addr: String,
    pub peers: Vec<(u64, String)>,
    pub bootstrap: bool,
    pub data_dir: PathBuf,
    pub file_metadata: bool,
    pub kafka_port: u16,
    pub metrics_port: u16,
    pub bind_addr: String,
    pub advertised_host: String,
    pub advertised_port: u16,
    pub gossip_config: Option<GossipConfig>,
}

/// Run in Raft cluster mode
pub async fn run_raft_cluster(config: RaftClusterConfig) -> Result<()> {
    info!("Initializing Raft cluster mode");
    info!("Node ID: {}", config.node_id);
    info!("Raft address: {}", config.raft_addr);
    info!("Parsed {} peers", config.peers.len());
    for (id, addr) in &config.peers {
        info!("  Peer {}: {}", id, addr);
    }

    // Create Raft configuration
    let raft_config = RaftConfig {
        node_id: config.node_id,
        listen_addr: config.raft_addr.clone(),
        // CRITICAL (v2.0.0): Production-safe timeouts to prevent election churn
        // With heartbeat_interval_ms = 150ms, election_timeout_ms = 3000ms gives us:
        // election_tick = 3000/150 = 20 ticks (meets tikv/raft recommendation of 10-20)
        // This provides ample margin for:
        // - Network delays: 100-200ms
        // - State machine blocking: 50-200ms
        // - gRPC queuing: 50-100ms
        // - Safety margin: 2500ms
        election_timeout_ms: 3000,
        // OPTIMIZATION (v2.0.0): 150ms heartbeat = 6-7 heartbeats per election timeout
        // Provides redundancy while keeping heartbeat traffic reasonable
        heartbeat_interval_ms: 150,
        max_entries_per_batch: 100,
        snapshot_threshold: 10_000,
    };

    // Extract peer IDs for Raft initialization (INCLUDING self)
    // CRITICAL: Raft requires all nodes in the cluster (including self) for correct quorum
    let mut peer_ids: Vec<u64> = config.peers.iter().map(|(id, _)| *id).collect();
    peer_ids.push(config.node_id);  // Add self to peer list
    let peer_count = peer_ids.len();

    let manager_config = RaftManagerConfig {
        raft_config: raft_config.clone(),
        enabled: true,
        tick_interval_ms: 10,
        initial_peers: peer_ids.clone(),  // Pre-populate peers from cluster config
    };

    // Create WAL manager first (needed by both metadata store and raft manager)
    use chronik_wal::{config::WalConfig, WalManager};
    let wal_config = WalConfig {
        enabled: true,
        data_dir: config.data_dir.join("wal"),
        ..Default::default()
    };
    info!("Starting WAL recovery...");
    let wal_manager = Arc::new(WalManager::recover(&wal_config).await?);
    info!("WAL recovery complete - {} partitions loaded", wal_manager.get_partitions().len());

    // Create object store for snapshots and data storage
    // v1.3.66+: Read configuration from environment variables (S3/GCS/Azure/Local)
    use chronik_storage::object_store::{ObjectStoreConfig, StorageBackend, AuthConfig};
    use chronik_storage::ObjectStoreFactory;

    let object_store_config = crate::parse_object_store_config_from_env().unwrap_or_else(|| {
        info!("No OBJECT_STORE_BACKEND environment variable found, using default local storage");
        ObjectStoreConfig {
            backend: StorageBackend::Local {
                path: config.data_dir.join("object_store").to_string_lossy().to_string(),
            },
            bucket: "chronik-snapshots".to_string(),
            prefix: None,
            connection: Default::default(),
            performance: Default::default(),
            retry: Default::default(),
            auth: AuthConfig::None,
            default_metadata: None,
            encryption: None,
        }
    });

    let object_store = ObjectStoreFactory::create(object_store_config.clone()).await?;
    let _object_store_arc: Arc<Box<dyn chronik_storage::object_store::ObjectStore>> = Arc::new(object_store); // Reserved for snapshot integration (Task 3.1)

    // Log the configured backend
    match &object_store_config.backend {
        StorageBackend::S3 { endpoint, .. } => {
            info!("Object store initialized: S3 (bucket: {}, endpoint: {:?})",
                  object_store_config.bucket, endpoint);
        }
        StorageBackend::Gcs { .. } => {
            info!("Object store initialized: Google Cloud Storage (bucket: {})",
                  object_store_config.bucket);
        }
        StorageBackend::Azure { .. } => {
            info!("Object store initialized: Azure Blob Storage (container: {})",
                  object_store_config.bucket);
        }
        StorageBackend::Local { path } => {
            info!("Object store initialized: Local filesystem (path: {})", path);
        }
    }

    // Create a temporary metadata store for bootstrapping the Raft manager
    // We'll replace this with RaftMetaLog after creating the __meta replica
    use chronik_common::metadata::FileMetadataStore;
    let temp_metadata_path = config.data_dir.join("metadata_temp");
    std::fs::create_dir_all(&temp_metadata_path)?;
    let temp_metadata = Arc::new(FileMetadataStore::new(temp_metadata_path).await?)
        as Arc<dyn chronik_common::metadata::MetadataStore>;

    // Create Raft replica manager with temporary metadata
    let raft_manager = Arc::new(RaftReplicaManager::new(
        manager_config,
        temp_metadata.clone(),
        wal_manager.clone(),
    ));

    // Spawn event handler to process Raft state changes (leader elections, etc.)
    info!("Spawning Raft event handler loop");
    tokio::spawn(raft_manager.clone().run_event_handler());

    // Bootstrap __meta partition - either via health-check or static configuration
    let bootstrap_peer_ids = if let Some(gossip_cfg) = &config.gossip_config {
        // Health-check-based automatic bootstrap
        info!("Starting health-check-based automatic bootstrap");

        // Build peer metadata map from config
        let mut peer_map = HashMap::new();
        for (peer_id, peer_addr) in &config.peers {
            peer_map.insert(
                *peer_id,
                NodeMetadata {
                    node_id: *peer_id,
                    kafka_addr: format!("{}:{}", config.advertised_host, config.advertised_port),
                    raft_addr: peer_addr.clone(),
                    is_server: true,
                    startup_time: chrono::Utc::now().timestamp(),
                },
            );
        }

        // Add self to peer map
        peer_map.insert(
            config.node_id,
            NodeMetadata {
                node_id: config.node_id,
                kafka_addr: format!("{}:{}", config.advertised_host, config.advertised_port),
                raft_addr: config.raft_addr.clone(),
                is_server: true,
                startup_time: chrono::Utc::now().timestamp(),
            },
        );

        // Create health-check bootstrap config
        let hc_config = HealthCheckConfig {
            node_metadata: peer_map[&config.node_id].clone(),
            peers: peer_map.clone(),
            bootstrap_expect: gossip_cfg.bootstrap_expect,
            data_dir: config.data_dir.clone(),
            check_interval: Duration::from_secs(5),
            check_timeout: Duration::from_secs(2),
            quorum_relax_timeout: Duration::from_secs(300), // 5 minutes
        };

        let health_check = Arc::new(HealthCheckBootstrap::new(hc_config));

        // Start monitoring in background
        let (bootstrap_tx, mut bootstrap_rx) = tokio::sync::mpsc::channel(1);
        let _monitor_task = health_check.clone().start_monitor(bootstrap_tx);

        // Wait for bootstrap decision
        info!("Waiting for cluster bootstrap decision...");
        let bootstrap_event: BootstrapEvent = bootstrap_rx.recv().await
            .ok_or_else(|| anyhow!("Health-check bootstrap channel closed"))?;

        info!(
            "Health-check quorum reached! Bootstrapping with {} peers: {:?}",
            bootstrap_event.peer_ids.len(),
            bootstrap_event.peer_ids
        );

        // Add discovered peers to raft client (async, non-blocking)
        let raft_manager_for_hc = raft_manager.clone();
        let peer_metadata = bootstrap_event.peer_metadata.clone();
        let node_id_clone = config.node_id;
        tokio::spawn(async move {
            // CRITICAL FIX: Initial delay to allow peer gRPC servers to start
            // Without this, all nodes try to connect immediately before servers are ready
            tokio::time::sleep(Duration::from_secs(2)).await;
            info!("Starting peer connections after initial startup delay");

            for (peer_id, meta) in peer_metadata {
                if peer_id == node_id_clone {
                    continue; // Skip self
                }

                let peer_url = if meta.raft_addr.starts_with("http://") || meta.raft_addr.starts_with("https://") {
                    meta.raft_addr
                } else {
                    format!("http://{}", meta.raft_addr)
                };

                // Retry with exponential backoff (starts at 1 second, not 2)
                let mut retry_count = 0;
                let max_retries = 10;
                loop {
                    match raft_manager_for_hc.add_peer(peer_id, peer_url.clone()).await {
                        Ok(_) => {
                            info!("Added health-check-discovered peer {} to Raft client", peer_id);
                            break;
                        }
                        Err(e) => {
                            retry_count += 1;
                            if retry_count >= max_retries {
                                error!("Failed to add peer {} after {} retries: {:?}", peer_id, max_retries, e);
                                break;
                            }
                            // Start with 1 second delay, double each retry (1s, 2s, 4s, 8s, ...)
                            let delay = Duration::from_secs(1_u64 << retry_count.min(5));
                            warn!("Failed to add peer {}: {:?}, retrying in {:?}...", peer_id, e, delay);
                            tokio::time::sleep(delay).await;
                        }
                    }
                }
            }
        });

        bootstrap_event.peer_ids
    } else {
        // Static peer list bootstrap (original behavior)
        info!("Using static peer configuration for bootstrap");
        peer_ids.clone()
    };

    // CRITICAL FIX: Create and set RaftService BEFORE creating __meta replica
    // This ensures the replica can be registered with the gRPC service immediately
    info!("Creating Raft gRPC service on {}", config.raft_addr);
    use chronik_raft::rpc::{RaftServiceImpl, start_raft_server};
    let raft_service = RaftServiceImpl::new();

    // Store reference to RaftService in RaftReplicaManager FIRST
    // (RaftReplicaManager will register replicas with the service when they're created)
    raft_manager.set_raft_service(Arc::new(raft_service.clone())).await;
    info!("Raft gRPC service registered with RaftReplicaManager");

    // CRITICAL FIX: Register all peer addresses with RaftClient BEFORE creating __meta replica
    // This prevents "No address for peer X" errors when Raft messages are sent
    // The background loop starts immediately after replica creation, so peers must be ready
    // We register addresses without connecting - connections happen lazily on first message send
    if config.gossip_config.is_none() {
        info!("Registering {} static peer addresses with RaftClient before creating replica", config.peers.len());

        for (peer_id, peer_addr) in &config.peers {
            let peer_url = if peer_addr.starts_with("http://") || peer_addr.starts_with("https://") {
                peer_addr.clone()
            } else {
                format!("http://{}", peer_addr)
            };

            // Register the address without connecting (connection happens lazily)
            raft_manager.raft_client().register_peer_address(*peer_id, peer_url).await;
        }

        info!("All {} peer addresses registered with RaftClient", config.peers.len());
    } else {
        info!("Using gossip-based peer discovery - peer addresses will be added dynamically");
    }

    // CRITICAL: Create MetadataStateMachine for __meta partition
    // This will apply committed metadata operations to RaftMetaLog's local state
    info!("Creating __meta partition replica with {} peers: {:?}", bootstrap_peer_ids.len(), bootstrap_peer_ids);

    // Create WAL-backed storage for metadata Raft log (enables persistence + S3 upload)
    use chronik_wal::{GroupCommitWal, GroupCommitConfig};
    let meta_wal_config = GroupCommitConfig::default();
    let meta_wal_dir = config.data_dir.join("wal/__meta/0");
    tokio::fs::create_dir_all(&meta_wal_dir).await?;
    let meta_wal = Arc::new(GroupCommitWal::new(meta_wal_dir.clone(), meta_wal_config));
    let meta_log_storage = Arc::new(RaftWalStorage::new(meta_wal));

    // Recover any existing Raft log entries from WAL
    meta_log_storage.recover(&config.data_dir).await?;
    info!("Metadata Raft log storage initialized (WAL-backed with persistence)");

    // Create shared state for metadata (will be used by both MetadataStateMachine and RaftMetaLog)
    use parking_lot::RwLock;
    use std::sync::atomic::{AtomicU64, Ordering};
    let local_state = Arc::new(RwLock::new(chronik_raft::MetadataState::default()));
    let applied_index = Arc::new(AtomicU64::new(0));

    // Create callback for automatic Raft replica creation when topics are created
    let raft_mgr_for_callback = raft_manager.clone();
    let peer_ids_for_callback = bootstrap_peer_ids.clone();
    let data_dir_for_callback = config.data_dir.clone();
    let callback = Arc::new(move |topic_name: &str, topic_meta: &chronik_common::metadata::TopicMetadata| {
        info!("MetadataStateMachine callback: Topic '{}' created with {} partitions", topic_name, topic_meta.config.partition_count);

        // Create Raft replicas for each partition
        let raft_mgr_clone = raft_mgr_for_callback.clone();
        let topic_name_clone = topic_name.to_string();
        let partition_count = topic_meta.config.partition_count;
        let peers_clone = peer_ids_for_callback.clone();
        let data_dir_clone = data_dir_for_callback.clone();

        tokio::spawn(async move {
            use chronik_wal::{GroupCommitWal, GroupCommitConfig};

            for partition_id in 0..partition_count {
                // Create WAL-backed storage for this partition replica (enables persistence + S3 upload)
                let partition_wal_config = GroupCommitConfig::default();
                let partition_wal_dir = data_dir_clone.join(format!("wal/{}/{}", topic_name_clone, partition_id));

                if let Err(e) = tokio::fs::create_dir_all(&partition_wal_dir).await {
                    error!("Failed to create WAL directory for {}-{}: {:?}", topic_name_clone, partition_id, e);
                    continue;
                }

                let partition_wal = Arc::new(GroupCommitWal::new(partition_wal_dir.clone(), partition_wal_config));
                let log_storage = Arc::new(RaftWalStorage::new(partition_wal));

                // Recover any existing Raft log entries from WAL
                if let Err(e) = log_storage.recover(&data_dir_clone).await {
                    error!("Failed to recover Raft log for {}-{}: {:?}", topic_name_clone, partition_id, e);
                    continue;
                }

                match raft_mgr_clone.create_replica(
                    topic_name_clone.clone(),
                    partition_id as i32,
                    log_storage,
                    peers_clone.clone(),
                ).await {
                    Ok(_) => {
                        info!("Created Raft replica for {}-{} on all nodes via callback", topic_name_clone, partition_id);
                    }
                    Err(e) => {
                        error!("Failed to create Raft replica for {}-{}: {:?}", topic_name_clone, partition_id, e);
                    }
                }
            }
            info!("Completed Raft replica creation for all {} partitions of '{}'", partition_count, topic_name_clone);
        });
    });

    // Create MetadataStateMachine with callback
    use chronik_raft::MetadataStateMachine;
    let state_machine: Arc<tokio::sync::RwLock<dyn chronik_raft::StateMachine>> =
        Arc::new(tokio::sync::RwLock::new(MetadataStateMachine::with_callback(
            local_state.clone(),
            applied_index.clone(),
            Some(callback),
        )));

    raft_manager.create_meta_replica(
        "__meta".to_string(),
        0,
        meta_log_storage,
        bootstrap_peer_ids.clone(),
        state_machine,
    ).await?;

    info!("__meta partition replica created successfully for node {} with {} bootstrap peers", config.node_id, bootstrap_peer_ids.len());

    // CRITICAL: Register the shared metadata state with RaftReplicaManager
    // This allows integrated_server.rs to retrieve the SAME state used by MetadataStateMachine
    raft_manager.set_meta_partition_state(local_state.clone(), applied_index.clone()).await;
    info!("Registered metadata state with RaftReplicaManager for sharing with RaftMetaLog");

    // Create SnapshotManager for automatic log compaction
    // Parse snapshot configuration from environment variables
    use chronik_raft::{SnapshotManager, SnapshotConfig, SnapshotCompression};

    let snapshot_enabled = std::env::var("CHRONIK_SNAPSHOT_ENABLED")
        .unwrap_or_else(|_| "true".to_string())
        .parse::<bool>()
        .unwrap_or(true);

    let log_threshold = std::env::var("CHRONIK_SNAPSHOT_LOG_THRESHOLD")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(10_000);

    let time_threshold_secs = std::env::var("CHRONIK_SNAPSHOT_TIME_THRESHOLD_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(3600);

    let max_concurrent = std::env::var("CHRONIK_SNAPSHOT_MAX_CONCURRENT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(2);

    let compression_str = std::env::var("CHRONIK_SNAPSHOT_COMPRESSION")
        .unwrap_or_else(|_| "gzip".to_string());

    let compression = match compression_str.to_lowercase().as_str() {
        "none" => SnapshotCompression::None,
        "zstd" => SnapshotCompression::Zstd,
        _ => SnapshotCompression::Gzip,
    };

    let retention_count = std::env::var("CHRONIK_SNAPSHOT_RETENTION_COUNT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(3);

    let snapshot_config = SnapshotConfig {
        enabled: snapshot_enabled,
        log_size_threshold: log_threshold,
        time_threshold: Duration::from_secs(time_threshold_secs),
        max_concurrent_snapshots: max_concurrent,
        compression,
        retention_count,
    };

    info!("Snapshot configuration: {:?}", snapshot_config);

    // Create object store adapter for SnapshotManager
    // The SnapshotManager uses chronik_raft::ObjectStoreTrait which is simpler than chronik_storage::ObjectStore
    use chronik_raft::snapshot::ObjectStoreTrait;

    struct ObjectStoreAdapter {
        inner: Arc<Box<dyn chronik_storage::object_store::ObjectStore>>,
    }

    #[async_trait::async_trait]
    impl ObjectStoreTrait for ObjectStoreAdapter {
        async fn put(&self, key: &str, data: bytes::Bytes) -> anyhow::Result<()> {
            self.inner.put(key, data).await.map_err(|e| anyhow::anyhow!("ObjectStore put error: {}", e))
        }

        async fn get(&self, key: &str) -> anyhow::Result<bytes::Bytes> {
            self.inner.get(key).await.map_err(|e| anyhow::anyhow!("ObjectStore get error: {}", e))
        }

        async fn delete(&self, key: &str) -> anyhow::Result<()> {
            self.inner.delete(key).await.map_err(|e| anyhow::anyhow!("ObjectStore delete error: {}", e))
        }

        async fn list(&self, prefix: &str) -> anyhow::Result<Vec<chronik_raft::snapshot::ObjectMetadata>> {
            let list_result = self.inner.list(prefix).await.map_err(|e| anyhow::anyhow!("ObjectStore list error: {}", e))?;

            Ok(list_result.into_iter().map(|obj| chronik_raft::snapshot::ObjectMetadata {
                key: obj.key,
                size: obj.size,
                last_modified: obj.last_modified as u64,
            }).collect())
        }

        async fn exists(&self, key: &str) -> anyhow::Result<bool> {
            self.inner.exists(key).await.map_err(|e| anyhow::anyhow!("ObjectStore exists error: {}", e))
        }
    }

    let object_store_adapter = Arc::new(ObjectStoreAdapter {
        inner: _object_store_arc.clone(),
    });

    // Create RaftMetaLog as the metadata store for SnapshotManager
    // This is a placeholder - in production we'd use the actual RaftMetaLog
    let snapshot_metadata = temp_metadata.clone();

    // Create SnapshotManager
    let snapshot_manager = Arc::new(SnapshotManager::new(
        config.node_id,
        raft_manager.clone() as Arc<dyn chronik_raft::RaftReplicaProvider>,
        snapshot_metadata,
        snapshot_config,
        object_store_adapter,
    ));

    // Spawn snapshot background loop if enabled
    let _snapshot_loop_handle = if snapshot_enabled {
        info!("Starting snapshot background loop");
        let handle = snapshot_manager.spawn_snapshot_loop();
        Some(handle)
    } else {
        info!("Snapshot creation disabled via CHRONIK_SNAPSHOT_ENABLED=false");
        None
    };

    info!("SnapshotManager initialized successfully");

    // REMOVED: Background peer addition task
    // Peers are now added synchronously before replica creation (see lines 218-246)
    // This eliminates the race condition where messages were sent before peers were configured

    // Save peer list for later use (for cluster config building)
    let peer_list_for_cluster_config = config.peers.clone();

    // TODO: Create Raft replicas for existing topics/partitions
    // This would happen during startup or on-demand when topics are created

    // Start Raft gRPC server (service is already created and __meta replica is registered)
    info!("Starting Raft gRPC server on {}", config.raft_addr);

    // Start gRPC server in background
    let raft_addr_clone = config.raft_addr.clone();
    let grpc_task = tokio::spawn(async move {
        if let Err(e) = start_raft_server(raft_addr_clone, raft_service).await {
            error!("Raft gRPC server failed: {:?}", e);
        }
    });

    // CRITICAL FIX: Wait for gRPC server to be ready before attempting peer connections
    // The server needs time to bind to the socket and start listening
    info!("Waiting for Raft gRPC server to be ready...");

    // Extract host and port for health check
    let raft_addr_parts: Vec<&str> = config.raft_addr.split(':').collect();
    let raft_host = raft_addr_parts.get(0).unwrap_or(&"127.0.0.1");
    let raft_port = raft_addr_parts.get(1).and_then(|s| s.parse::<u16>().ok()).unwrap_or(5001);

    // Wait until the port is listening (max 5 seconds)
    let mut ready = false;
    for attempt in 1..=50 {
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Try to connect to the local gRPC server
        if let Ok(stream) = tokio::net::TcpStream::connect((*raft_host, raft_port)).await {
            drop(stream);
            ready = true;
            info!("Raft gRPC server is ready after {} ms", attempt * 100);
            break;
        }
    }

    if !ready {
        warn!("Raft gRPC server readiness check timed out, but continuing anyway");
    }

    info!("Raft gRPC server started successfully");

    // Start the main Kafka server (with Raft integration)
    info!("Starting Kafka server with Raft integration");

    // Create IntegratedKafkaServer with Raft manager
    use crate::integrated_server::{IntegratedKafkaServer, IntegratedServerConfig};

    // Convert Raft cluster config to chronik-config ClusterConfig for broker verification
    use chronik_config::cluster::{ClusterConfig as ChronikClusterConfig, NodeConfig as ChronikNodeConfig};

    // Build peer list from Raft cluster config
    // config.peers is Vec<(u64, String)> where String is the Raft address (host:port)
    // Use the saved peer list (config.peers was moved earlier)
    let mut peers: Vec<ChronikNodeConfig> = peer_list_for_cluster_config.iter().map(|(peer_id, peer_addr)| {
        // Extract hostname and Raft port from the peer address
        let parts: Vec<&str> = peer_addr.split(':').collect();
        let hostname = parts.get(0).map(|s| s.to_string()).unwrap_or_else(|| "localhost".to_string());
        let raft_port = parts.get(1).and_then(|s| s.parse::<u16>().ok()).unwrap_or(5000);

        ChronikNodeConfig {
            id: *peer_id,
            addr: format!("{}:{}", hostname, config.kafka_port), // Use actual Kafka port
            raft_port,
        }
    }).collect();

    // Add this node to the peers list
    peers.push(ChronikNodeConfig {
        id: config.node_id,
        addr: format!("{}:{}", config.advertised_host, config.advertised_port),
        raft_port: config.raft_addr.split(':').nth(1).and_then(|s| s.parse().ok()).unwrap_or(5001),
    });

    let cluster_cfg = ChronikClusterConfig {
        enabled: true,
        node_id: config.node_id,
        replication_factor: 3,
        min_insync_replicas: 2,
        peers,
        gossip: None,
    };

    let server_config = IntegratedServerConfig {
        node_id: config.node_id as i32,
        advertised_host: config.advertised_host.clone(),
        advertised_port: config.advertised_port as i32,
        data_dir: config.data_dir.to_string_lossy().to_string(),
        enable_indexing: false,
        enable_compression: true,
        auto_create_topics: true,
        num_partitions: 3,
        replication_factor: 3, // For Raft cluster
        use_wal_metadata: !config.file_metadata,
        enable_wal_indexing: true,
        wal_indexing_interval_secs: 30,
        object_store_config: Some(object_store_config.clone()), // v1.3.66+: Use S3/GCS/Azure from env vars
        enable_metadata_dr: true,
        metadata_upload_interval_secs: 60,
        cluster_config: Some(cluster_cfg),
    };

    let server = Arc::new(IntegratedKafkaServer::new_with_raft(server_config, Some(raft_manager.clone())).await?);

    // CRITICAL FIX: Replace RaftReplicaManager's temporary FileMetadataStore with the RaftMetaLog
    // The server created a RaftMetaLog internally, but the event handler still uses the temp metadata
    // We need to update the manager's metadata reference so events update the Raft-replicated state
    let raft_metadata = server.metadata_store();
    raft_manager.set_metadata_store(raft_metadata).await;
    info!("RaftReplicaManager metadata store updated with RaftMetaLog");

    // Initialize monitoring (metrics + optional tracing)
    use chronik_monitoring::init_monitoring;
    let _metrics_registry = init_monitoring(
        "chronik-server",
        config.metrics_port,
        None, // TODO: Add tracing config support
    ).await?;

    info!("Metrics endpoint available at http://{}:{}/metrics", config.bind_addr, config.metrics_port);

    // Start serving in background
    info!("Kafka+Raft server initialized, starting to accept connections");
    let bind_addr = format!("{}:{}", config.bind_addr, config.kafka_port);
    let server_clone = server.clone();
    let bind_addr_owned = bind_addr.to_string();

    let server_task = tokio::spawn(async move {
        server_clone.run(&bind_addr_owned).await
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
        _ = signal::ctrl_c() => {
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

    // Graceful shutdown: Call proper shutdown sequence
    // This seals WAL segments, runs WalIndexer, and ensures data durability
    info!("Executing graceful shutdown sequence...");
    if let Err(e) = server.shutdown().await {
        error!("Failed to shutdown server: {}", e);
    } else {
        info!("Server shutdown complete - all data flushed and WAL segments sealed");
    }

    Ok(())
}
