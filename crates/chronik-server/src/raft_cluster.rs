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
    RaftConfig, RaftMetaLog, MemoryLogStorage,
};
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
        election_timeout_ms: 300,
        heartbeat_interval_ms: 30,
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
    let meta_log_storage = Arc::new(MemoryLogStorage::new());

    // Create shared state for metadata (will be used by both MetadataStateMachine and RaftMetaLog)
    use parking_lot::RwLock;
    use std::sync::atomic::{AtomicU64, Ordering};
    let local_state = Arc::new(RwLock::new(chronik_raft::MetadataState::default()));
    let applied_index = Arc::new(AtomicU64::new(0));

    // Create MetadataStateMachine
    use chronik_raft::MetadataStateMachine;
    let state_machine: Arc<tokio::sync::RwLock<dyn chronik_raft::StateMachine>> =
        Arc::new(tokio::sync::RwLock::new(MetadataStateMachine::new(
            local_state.clone(),
            applied_index.clone(),
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
        object_store_config: None,
        enable_metadata_dr: true,
        metadata_upload_interval_secs: 60,
        cluster_config: Some(cluster_cfg),
    };

    let server = Arc::new(IntegratedKafkaServer::new_with_raft(server_config, Some(raft_manager.clone())).await?);

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
