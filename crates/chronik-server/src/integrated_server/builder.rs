//! Builder pattern for IntegratedKafkaServer construction
//!
//! This module provides a staged initialization pattern to reduce complexity
//! of the main IntegratedKafkaServer::new() constructor (was 764 → target < 25).
//!
//! ## Design Goals
//! 1. Each initialization stage is a separate function with complexity < 20
//! 2. Clear separation of concerns (directories, Raft, metadata, handlers, replication)
//! 3. Testable stages (each stage can be unit tested independently)
//! 4. Backward compatible (existing code can gradually migrate)

use anyhow::{Result, Context};
use std::path::PathBuf;
use std::sync::Arc;
use tracing::{info, debug, warn, error};

use crate::IntegratedServerConfig;
use crate::IntegratedKafkaServer;
use chronik_common::metadata::traits::MetadataStore;
use chronik_storage::{
    WalIndexer, WalIndexerConfig, ObjectStoreFactory, ObjectStoreTrait, ObjectStoreConfig,
    SegmentReader, SegmentReaderConfig, SegmentWriterConfig,
};
use crate::storage::{StorageService, StorageConfig as IngestStorageConfig};
use chronik_wal::{
    WalConfig, WalManager, CompressionType, CheckpointConfig,
    RecoveryConfig, RotationConfig, FsyncConfig,
    config::AsyncIoConfig,
};
use crate::produce_handler::{ProduceHandler, ProduceHandlerConfig, ProduceFlushProfile};
use crate::wal_integration::WalProduceHandler;
use crate::fetch_handler::FetchHandler;
use crate::kafka_handler::KafkaProtocolHandler;
use std::sync::atomic::Ordering;

/// Builder for IntegratedKafkaServer with staged initialization
///
/// # Example
/// ```ignore
/// let server = IntegratedKafkaServerBuilder::new(config)
///     .with_raft_cluster(cluster)
///     .build()
///     .await?;
/// ```
pub struct IntegratedKafkaServerBuilder {
    // Configuration
    config: IntegratedServerConfig,
    raft_cluster: Option<Arc<crate::raft_cluster::RaftCluster>>,

    // Initialized paths (Stage 1)
    data_dir: Option<PathBuf>,
    segments_dir: Option<PathBuf>,

    // Raft and metadata (Stage 2)
    raft_cluster_for_metadata: Option<Arc<crate::raft_cluster::RaftCluster>>,
    metadata_wal: Option<Arc<crate::metadata_wal::MetadataWal>>,
    metadata_event_bus: Option<Arc<crate::metadata_events::MetadataEventBus>>,
    wal_metadata_store: Option<Arc<chronik_common::metadata::WalMetadataStore>>,
    metadata_store: Option<Arc<dyn MetadataStore>>,

    // Storage and handlers (Stage 3)
    object_store: Option<Arc<dyn chronik_storage::ObjectStoreTrait>>,
    storage_config: Option<IngestStorageConfig>,
    storage_service: Option<Arc<StorageService>>,
    segment_reader: Option<Arc<SegmentReader>>,
    wal_manager: Option<Arc<WalManager>>,
    produce_handler_base: Option<Arc<ProduceHandler>>,
    wal_produce_handler: Option<Arc<WalProduceHandler>>,
    fetch_handler: Option<Arc<FetchHandler>>,
    isr_ack_tracker: Option<Arc<crate::isr_ack_tracker::IsrAckTracker>>,
    response_pipeline: Option<Arc<crate::response_pipeline::ResponsePipeline>>,
    kafka_handler: Option<Arc<KafkaProtocolHandler>>,
    wal_indexer: Option<Arc<WalIndexer>>,

    // Replication and leader election (Stage 4)
    wal_replication_manager: Option<Arc<crate::wal_replication::WalReplicationManager>>,
    leader_elector: Option<Arc<crate::leader_election::LeaderElector>>,

    // Metadata DR (Stage 5)
    metadata_uploader: Option<Arc<chronik_common::metadata::MetadataUploader>>,
}

impl IntegratedKafkaServerBuilder {
    /// Create a new builder with configuration
    pub fn new(config: IntegratedServerConfig) -> Self {
        Self {
            config,
            raft_cluster: None,
            data_dir: None,
            segments_dir: None,
            raft_cluster_for_metadata: None,
            metadata_wal: None,
            metadata_event_bus: None,
            wal_metadata_store: None,
            metadata_store: None,
            object_store: None,
            storage_config: None,
            storage_service: None,
            segment_reader: None,
            wal_manager: None,
            produce_handler_base: None,
            wal_produce_handler: None,
            fetch_handler: None,
            isr_ack_tracker: None,
            response_pipeline: None,
            kafka_handler: None,
            wal_indexer: None,
            wal_replication_manager: None,
            leader_elector: None,
            metadata_uploader: None,
        }
    }

    /// Set optional Raft cluster for multi-node mode
    pub fn with_raft_cluster(mut self, cluster: Arc<crate::raft_cluster::RaftCluster>) -> Self {
        self.raft_cluster = Some(cluster);
        self
    }

    /// Stage 1: Initialize directories
    /// Complexity: < 10 (simple directory creation)
    async fn init_directories(&mut self) -> Result<()> {
        info!("Stage 1: Initializing directories");

        // Create main data directory
        let data_dir = PathBuf::from(&self.config.data_dir);
        std::fs::create_dir_all(&data_dir)
            .context("Failed to create data directory")?;
        debug!("Created data directory: {}", data_dir.display());

        // Create segments directory
        let segments_dir = data_dir.join("segments");
        std::fs::create_dir_all(&segments_dir)
            .context("Failed to create segments directory")?;
        debug!("Created segments directory: {}", segments_dir.display());

        self.data_dir = Some(data_dir);
        self.segments_dir = Some(segments_dir);

        info!("✅ Directories initialized");
        Ok(())
    }

    /// Stage 2: Initialize Raft cluster for metadata coordination
    /// Complexity: < 15 (conditional Raft setup)
    async fn init_raft_cluster(&mut self) -> Result<()> {
        info!("Stage 2: Initializing Raft cluster for metadata coordination");

        let data_dir = self.data_dir.as_ref()
            .context("data_dir not initialized - call init_directories() first")?;

        let raft_cluster_for_metadata = if let Some(ref cluster) = self.raft_cluster {
            // Multi-node mode: use existing RaftCluster
            info!("Multi-node mode: using existing RaftCluster with {} peers", cluster.peer_count());
            cluster.clone()
        } else {
            // Single-node mode: create RaftCluster with empty peers (zero overhead)
            info!("Single-node mode: creating RaftCluster with zero-overhead synchronous apply");
            Arc::new(
                crate::raft_cluster::RaftCluster::bootstrap(
                    self.config.node_id as u64,
                    Vec::new(), // Empty peers = single-node mode
                    data_dir.clone(),
                )
                .await
                .context("Failed to bootstrap single-node Raft cluster")?
            )
        };

        self.raft_cluster_for_metadata = Some(raft_cluster_for_metadata);

        info!("✅ Raft cluster initialized");
        Ok(())
    }

    /// Stage 3: Initialize metadata WAL
    /// Complexity: < 10 (simple WAL creation)
    async fn init_metadata_wal(&mut self) -> Result<()> {
        info!("Stage 3: Initializing Metadata WAL");

        let data_dir = self.data_dir.as_ref()
            .context("data_dir not initialized")?;

        let metadata_wal = Arc::new(
            crate::metadata_wal::MetadataWal::new(data_dir.clone())
                .await
                .context("Failed to create metadata WAL")?
        );

        info!("✅ Metadata WAL created (topic='__chronik_metadata', partition=0)");

        self.metadata_wal = Some(metadata_wal);
        Ok(())
    }

    /// Stage 4: Initialize event bus
    /// Complexity: < 5 (trivial creation)
    async fn init_event_bus(&mut self) -> Result<()> {
        info!("Stage 4: Initializing event bus");

        let metadata_event_bus = Arc::new(crate::metadata_events::MetadataEventBus::default());

        info!("✅ Metadata event bus created (buffer_size=1000)");

        self.metadata_event_bus = Some(metadata_event_bus);
        Ok(())
    }

    /// Stage 5: Initialize WalMetadataStore with callbacks and recovery
    /// Complexity: < 20 (callback creation + event bus wiring + recovery)
    async fn init_metadata_store(&mut self) -> Result<()> {
        info!("Stage 5: Initializing WalMetadataStore");

        let metadata_wal = self.metadata_wal.as_ref()
            .context("metadata_wal not initialized")?;
        let metadata_event_bus = self.metadata_event_bus.as_ref()
            .context("metadata_event_bus not initialized")?;

        // Create WalAppendFn callback for metadata WAL writes
        let metadata_wal_for_callback = metadata_wal.clone();
        let wal_append_fn: chronik_common::metadata::WalAppendFn = Arc::new(move |bytes: Vec<u8>| {
            let wal = metadata_wal_for_callback.clone();
            Box::pin(async move {
                let offset = wal.append_bytes(bytes).await
                    .map_err(|e| format!("WAL append failed: {}", e))?;
                Ok(offset)
            })
        });

        // Create WalMetadataStore
        let mut wal_metadata_store = chronik_common::metadata::WalMetadataStore::new(
            self.config.node_id as u64,
            wal_append_fn,
        );

        // Wire up event bus for metadata replication
        let event_bus_for_store = metadata_event_bus.clone();
        let event_bus_publish_fn: chronik_common::metadata::EventBusPublishFn =
            Arc::new(move |common_event| {
                Self::convert_and_publish_metadata_event(&event_bus_for_store, common_event)
            });

        wal_metadata_store.set_event_bus(event_bus_publish_fn);

        info!("✅ WalMetadataStore created with event bus wiring");

        // Recover metadata state from WAL before wrapping in Arc
        info!("Recovering metadata state from WAL...");
        let recovered_events = metadata_wal.recover().await
            .context("Failed to recover metadata WAL")?;

        if !recovered_events.is_empty() {
            info!("Replaying {} metadata events", recovered_events.len());
            wal_metadata_store.replay_events(recovered_events).await
                .context("Failed to replay metadata events")?;
            info!("✅ Successfully recovered metadata state from WAL");
        } else {
            info!("No metadata events to recover - starting with fresh state");
        }

        // Wrap in Arc now that recovery is complete
        let wal_metadata_store = Arc::new(wal_metadata_store);
        self.wal_metadata_store = Some(wal_metadata_store.clone());
        self.metadata_store = Some(wal_metadata_store);
        Ok(())
    }

    /// Helper: Convert and publish metadata event to event bus
    /// Complexity: < 15 (event type conversion)
    fn convert_and_publish_metadata_event(
        event_bus: &Arc<crate::metadata_events::MetadataEventBus>,
        common_event: chronik_common::metadata::MetadataEvent,
    ) -> usize {
        use chronik_common::metadata::MetadataEventPayload;
        use crate::metadata_events::MetadataEvent as ServerEvent;

        // Convert common MetadataEvent to server MetadataEvent
        let server_event = match &common_event.payload {
            MetadataEventPayload::TopicCreated { name, config } => {
                Some(ServerEvent::TopicCreated {
                    topic: name.clone(),
                    num_partitions: config.partition_count as i32,
                })
            }
            MetadataEventPayload::TopicDeleted { name } => {
                Some(ServerEvent::TopicDeleted {
                    topic: name.clone(),
                })
            }
            MetadataEventPayload::PartitionAssigned { assignment } => {
                Some(ServerEvent::PartitionAssigned {
                    topic: assignment.topic.clone(),
                    partition: assignment.partition as i32,
                    replicas: assignment.replicas.clone(),
                    leader: assignment.leader_id,
                })
            }
            MetadataEventPayload::HighWatermarkUpdated { topic, partition, new_watermark } => {
                Some(ServerEvent::HighWatermarkUpdated {
                    topic: topic.clone(),
                    partition: *partition,
                    offset: *new_watermark,
                })
            }
            // v2.2.15 CRITICAL FIX: Replicate BrokerRegistered events to followers
            // Without this, followers don't know about brokers and return fallback metadata
            MetadataEventPayload::BrokerRegistered { metadata } => {
                Some(ServerEvent::BrokerRegistered {
                    broker_id: metadata.broker_id,
                    host: metadata.host.clone(),
                    port: metadata.port,
                    rack: metadata.rack.clone(),
                })
            }
            _ => None, // Skip other event types (e.g., OffsetUpdated)
        };

        // Publish to event bus if relevant
        match server_event {
            Some(event) => event_bus.publish(event),
            None => {
                debug!("Skipping non-replication event: {:?}", common_event.payload);
                0
            }
        }
    }

    /// Stage 6: Initialize metadata replication
    /// Complexity: < 20 (replicator creation + event listener)
    async fn init_metadata_replication(&mut self) -> Result<()> {
        info!("Stage 6: Initializing metadata replication");

        let metadata_wal = self.metadata_wal.as_ref()
            .context("metadata_wal not initialized")?;
        let metadata_event_bus = self.metadata_event_bus.as_ref()
            .context("metadata_event_bus not initialized")?;
        let wal_metadata_store = self.wal_metadata_store.as_ref()
            .context("wal_metadata_store not initialized")?;
        let metadata_store = self.metadata_store.as_ref()
            .context("metadata_store not initialized")?;
        let raft_cluster = self.raft_cluster_for_metadata.as_ref()
            .context("raft_cluster_for_metadata not initialized")?;

        // Create WAL replication manager for metadata
        let wal_replication_manager = crate::wal_replication::WalReplicationManager::new_with_dependencies(
            Vec::new(),  // Empty for auto-discovery
            Some(raft_cluster.clone()),
            None,  // No ISR tracker for metadata
            None,  // No ACK tracker for metadata
            self.config.cluster_config.clone().map(Arc::new),
            Some(metadata_store.clone()),
        );

        // Create metadata WAL replicator
        let metadata_wal_replicator = Arc::new(
            crate::metadata_wal_replication::MetadataWalReplicator::new(
                metadata_wal.clone(),
                wal_replication_manager,
                metadata_event_bus.clone(),
                wal_metadata_store.clone(),
            )
        );

        // Start event listener
        metadata_wal_replicator.clone().start_event_listener();

        info!("✅ Metadata replication initialized with event listener");

        // Note: WAL replication manager is stored in the replicator, not separately here
        // The builder doesn't need to store it as a separate field for metadata replication

        Ok(())
    }

    /// Stage 7: Initialize storage layer (ObjectStore, StorageService, SegmentReader)
    /// Complexity: < 20 (configuration and component creation)
    async fn init_storage(&mut self) -> Result<()> {
        info!("Stage 7: Initializing storage layer");

        let segments_dir = self.segments_dir.as_ref()
            .context("segments_dir not initialized")?
            .to_string_lossy()
            .to_string();

        // Create object store configuration (use custom config if provided, otherwise default to local)
        let object_store_config = if let Some(custom_config) = self.config.object_store_config.clone() {
            info!("Using custom object store configuration from environment/config");
            custom_config
        } else {
            info!("Using default local filesystem object store at {}", segments_dir);
            ObjectStoreConfig {
                backend: chronik_storage::object_store::StorageBackend::Local {
                    path: segments_dir.clone(),
                },
                bucket: "chronik".to_string(),
                prefix: None,
                connection: Default::default(),
                performance: Default::default(),
                retry: Default::default(),
                auth: chronik_storage::object_store::AuthConfig::None,
                default_metadata: None,
                encryption: None,
            }
        };

        // Create object store
        let object_store = ObjectStoreFactory::create(object_store_config.clone()).await?;
        let object_store_arc: Arc<dyn ObjectStoreTrait> = Arc::from(object_store);

        // Create storage service with proper configuration
        let storage_config = IngestStorageConfig {
            object_store_config: object_store_config.clone(),
            segment_writer_config: SegmentWriterConfig {
                data_dir: PathBuf::from(&segments_dir),
                compression_codec: if self.config.enable_compression {
                    "snappy".to_string()
                } else {
                    "none".to_string()
                },
                max_segment_size: 256 * 1024 * 1024, // 256MB
                max_segment_age_secs: 30, // 30 seconds - flush on produce for immediate availability
                retention_period_secs: 7 * 24 * 3600, // 7 days
                enable_cleanup: true,
            },
            segment_reader_config: SegmentReaderConfig::default(),
        };

        let storage_service = Arc::new(StorageService::new(storage_config.clone()).await?);

        // Initialize segment reader with object store
        let segment_reader = Arc::new(SegmentReader::new(
            SegmentReaderConfig::default(),
            storage_service.object_store(),
        ));

        self.object_store = Some(object_store_arc);
        self.storage_config = Some(storage_config);
        self.storage_service = Some(storage_service);
        self.segment_reader = Some(segment_reader);

        info!("✅ Storage layer initialized (ObjectStore + StorageService + SegmentReader)");
        Ok(())
    }

    /// Stage 8: Initialize WAL Manager with recovery
    /// Complexity: < 15 (configuration and recovery)
    async fn init_wal_manager(&mut self) -> Result<()> {
        info!("Stage 8: Initializing WAL Manager with recovery");

        let data_dir = self.data_dir.as_ref()
            .context("data_dir not initialized")?;

        // Create WAL configuration with default settings
        let wal_config = WalConfig {
            enabled: true,  // WAL is always enabled now as the default durability mechanism
            data_dir: data_dir.join("wal"),
            segment_size: 128 * 1024 * 1024, // 128MB segments
            flush_interval_ms: 100, // Sync to disk every 100ms for durability
            flush_threshold: 1024 * 1024, // 1MB buffer threshold
            compression: CompressionType::None,
            checkpointing: CheckpointConfig::default(),
            recovery: RecoveryConfig::default(),
            rotation: RotationConfig {
                max_segment_age_ms: 30 * 60 * 1000, // 30 minutes
                max_segment_size: 128 * 1024 * 1024, // 128MB
                coordinate_with_storage: true,
            },
            fsync: FsyncConfig {
                enabled: true,
                batch_size: 8,     // Batch up to 8 writes for efficiency
                batch_timeout_ms: 50, // Max 50ms latency for fsync batching
            },
            async_io: AsyncIoConfig::default(),
        };

        info!(
            "Skipping segment clearing - segments contain valid flushed data (v1.3.47+)"
        );

        // Initialize WAL manager with recovery (v2.2.7: Always use direct WAL recovery)
        let wal_manager = Arc::new(
            WalManager::recover(&wal_config)
                .await
                .map_err(|e| anyhow::anyhow!("Failed to recover WAL: {}", e))?
        );

        info!("WAL recovery complete - {} partitions loaded", wal_manager.get_partitions().len());

        self.wal_manager = Some(wal_manager);

        info!("✅ WAL Manager initialized with recovery");
        Ok(())
    }

    /// Stage 9: Initialize ProduceHandler with all dependencies and wiring
    /// Complexity: < 25 (configuration, creation, and multiple wiring steps)
    async fn init_produce_handler(&mut self) -> Result<()> {
        info!("Stage 9: Initializing ProduceHandler with dependencies");

        let object_store = self.object_store.as_ref()
            .context("object_store not initialized")?;
        let metadata_store = self.metadata_store.as_ref()
            .context("metadata_store not initialized")?;
        let wal_manager = self.wal_manager.as_ref()
            .context("wal_manager not initialized")?;
        let metadata_event_bus = self.metadata_event_bus.as_ref()
            .context("metadata_event_bus not initialized")?;
        let storage_config = self.storage_config.as_ref()
            .context("storage_config not initialized")?;

        // Configure produce handler with proper indexer config
        let indexer_config = chronik_search::realtime_indexer::RealtimeIndexerConfig {
            index_base_path: PathBuf::from(format!("{}/index", self.config.data_dir)),
            ..Default::default()
        };

        let flush_profile = ProduceFlushProfile::auto_select();
        let produce_config = ProduceHandlerConfig {
            node_id: self.config.node_id,
            storage_config: storage_config.clone(),
            indexer_config,
            enable_indexing: self.config.enable_indexing,
            enable_idempotence: true,
            enable_transactions: false, // Start without transactions
            max_in_flight_requests: 5,
            batch_size: 16384,
            linger_ms: flush_profile.linger_ms(),
            compression_type: if self.config.enable_compression {
                chronik_storage::kafka_records::CompressionType::Snappy
            } else {
                chronik_storage::kafka_records::CompressionType::None
            },
            request_timeout_ms: 120000,  // 120 seconds
            buffer_memory: flush_profile.buffer_memory(),
            auto_create_topics_enable: self.config.auto_create_topics,
            num_partitions: self.config.num_partitions,
            default_replication_factor: self.config.replication_factor,
            flush_profile,
        };

        // Initialize produce handler with inline WAL support
        let mut produce_handler_inner = ProduceHandler::new_with_wal(
            produce_config,
            object_store.clone(),
            metadata_store.clone(),
            wal_manager.clone(),
        ).await?;

        // Wire RaftCluster to ProduceHandler for partition metadata
        if let Some(ref cluster) = self.raft_cluster_for_metadata {
            info!("Setting RaftCluster for ProduceHandler");
            produce_handler_inner.set_raft_cluster(Arc::clone(cluster));
        }

        // Wire MetadataEventBus to ProduceHandler for < 10ms watermark replication
        info!("Setting MetadataEventBus for ProduceHandler");
        produce_handler_inner.set_event_bus(metadata_event_bus.clone());

        // Create ISR ACK tracker for acks=-1 quorum support
        // CRITICAL: Must be created on ALL nodes (leader + followers)
        let isr_ack_tracker = crate::isr_ack_tracker::IsrAckTracker::new();
        info!("Created IsrAckTracker for acks=-1 quorum tracking");
        produce_handler_inner.set_isr_ack_tracker(isr_ack_tracker.clone());

        // Create leader elector if Raft clustering is enabled
        let leader_elector_for_produce = if let Some(ref raft) = self.raft_cluster_for_metadata {
            info!("Creating LeaderElector for event-driven elections");
            let elector = Arc::new(
                crate::leader_election::LeaderElector::new(raft.clone(), metadata_store.clone())
            );
            info!("✓ LeaderElector ready (event-driven mode)");
            Some(elector)
        } else {
            None
        };

        // Wire leader elector to ProduceHandler for heartbeat recording
        if let Some(ref elector) = leader_elector_for_produce {
            produce_handler_inner.set_leader_elector(elector.clone());
        }

        // CRITICAL INTEGRATION FIX (v2.2.14): Create and wire WalReplicationManager for DATA messages
        // This enables the replica cache (6.2x speedup) and ISR-aware replication
        if let Some(ref raft) = self.raft_cluster_for_metadata {
            info!("Creating WalReplicationManager for data message replication");
            let data_wal_repl_manager = crate::wal_replication::WalReplicationManager::new_with_dependencies(
                Vec::new(),  // Empty for auto-discovery from cluster config
                Some(raft.clone()),
                None,  // No separate ISR tracker (use IsrAckTracker instead)
                Some(isr_ack_tracker.clone()),  // ISR+ACK tracker for data replication
                self.config.cluster_config.clone().map(Arc::new),
                Some(metadata_store.clone()),  // CRITICAL: Enables replica cache with metadata lookups
            );

            // Wire to ProduceHandler - THIS WAS MISSING and caused 2.65x regression!
            produce_handler_inner.set_wal_replication_manager(data_wal_repl_manager);
            info!("✅ Data WAL replication manager wired to ProduceHandler (replica cache enabled)");
        } else {
            info!("⚠️  Raft clustering disabled - WAL replication manager not created (single-node mode)");
        }

        // Wire ResponsePipeline for async acks=1 responses (v2.2.10 CRITICAL FIX #7)
        info!("Setting up ResponsePipeline for async acks=1 responses");
        let response_pipeline = Arc::new(crate::response_pipeline::ResponsePipeline::new());

        // Create callback closure that notifies ResponsePipeline when batches commit
        let response_pipeline_clone = response_pipeline.clone();
        let partition_states_for_callback = produce_handler_inner.partition_states.clone();
        let commit_callback: chronik_wal::group_commit::CommitCallback = Arc::new(
            move |topic: &str, partition: i32, min_offset: i64, max_offset: i64| {
                // CRITICAL BUG FIX #8 (v2.2.11): Update in-memory high watermark after WAL batch commit
                let new_watermark = max_offset + 1; // High watermark is the next offset to be written
                if let Some(state) = partition_states_for_callback.get(&(topic.to_string(), partition)) {
                    state.high_watermark.store(new_watermark as u64, Ordering::Release);
                    debug!("✅ WAL_CALLBACK: Updated high watermark {}-{} = {}", topic, partition, new_watermark);
                }

                // Notify ResponsePipeline for async response delivery
                response_pipeline_clone.notify_batch_committed(topic, partition, min_offset, max_offset);
            }
        );

        // Wire callback to GroupCommitWal
        wal_manager.group_commit_wal().set_commit_callback(commit_callback);
        info!("✅ ResponsePipeline callback wired to GroupCommitWal");

        // Wire ResponsePipeline to ProduceHandler
        produce_handler_inner.set_response_pipeline(response_pipeline.clone());
        info!("✅ ResponsePipeline wired to ProduceHandler");

        // Wrap in Arc and start background tasks
        let produce_handler_base = Arc::new(produce_handler_inner);
        produce_handler_base.clone().start_metadata_event_listener();
        info!("✅ ProduceHandler metadata event listener started");

        produce_handler_base.start_background_tasks().await;
        info!("✅ ProduceHandler background tasks started");

        // Wrap with WalProduceHandler for backward compatibility
        let wal_handler = Arc::new(WalProduceHandler::new_passthrough(
            wal_manager.clone(),
            produce_handler_base.clone()
        ));

        self.produce_handler_base = Some(produce_handler_base);
        self.wal_produce_handler = Some(wal_handler);
        self.isr_ack_tracker = Some(isr_ack_tracker);
        self.response_pipeline = Some(response_pipeline);
        self.leader_elector = leader_elector_for_produce;

        info!("✅ ProduceHandler initialized with all dependencies and wiring");
        Ok(())
    }

    /// Stage 10: WAL recovery and watermark restoration
    /// Complexity: < 25 (WAL replay and metadata restoration logic)
    async fn recover_wal_watermarks(&mut self) -> Result<()> {
        info!("Stage 10: Recovering WAL watermarks");

        let produce_handler_base = self.produce_handler_base.as_ref()
            .context("produce_handler_base not initialized")?;
        let wal_manager = self.wal_manager.as_ref()
            .context("wal_manager not initialized")?;
        let metadata_store = self.metadata_store.as_ref()
            .context("metadata_store not initialized")?;

        // CRITICAL FIX (v1.3.52): Clear all partition buffers before WAL recovery
        // This prevents duplicate messages by ensuring we start with clean in-memory state.
        info!("Clearing all partition buffers before WAL recovery...");
        if let Err(e) = produce_handler_base.clear_all_buffers().await {
            warn!("Failed to clear partition buffers: {}. Recovery may have duplicates.", e);
        }

        // Replay WAL to restore high watermarks after crash
        let recovered_partitions = wal_manager.get_partitions();
        if !recovered_partitions.is_empty() {
            info!("Replaying WAL to restore high watermarks for {} partitions...", recovered_partitions.len());

            for tp in recovered_partitions {
                // Read all WAL records to find highest offset
                match wal_manager.read_from(&tp.topic, tp.partition, 0, usize::MAX).await {
                    Ok(records) if !records.is_empty() => {
                        use chronik_wal::record::WalRecord;
                        use chronik_storage::CanonicalRecord;

                        let mut max_offset: i64 = -1;

                        // Find max offset across all records (handle both V1 and V2 formats)
                        for record in &records {
                            match record {
                                WalRecord::V1 { offset, .. } => {
                                    if *offset > max_offset {
                                        max_offset = *offset;
                                    }
                                },
                                WalRecord::V2 { canonical_data, .. } => {
                                    // Deserialize CanonicalRecord to get offsets
                                    if let Ok(canonical) = bincode::deserialize::<CanonicalRecord>(canonical_data) {
                                        let last_offset = canonical.last_offset();
                                        if last_offset > max_offset {
                                            max_offset = last_offset;
                                        }
                                    }
                                }
                            }
                        }

                        if max_offset >= 0 {
                            // High watermark is max_offset + 1 (next offset to be written)
                            let high_watermark = (max_offset + 1) as u64;

                            // Restore partition state
                            if let Err(e) = produce_handler_base.restore_partition_state(&tp.topic, tp.partition, high_watermark).await {
                                warn!("Failed to restore partition state for {}-{}: {}", tp.topic, tp.partition, e);
                            } else {
                                info!("Restored {}-{}: {} records, high watermark = {}",
                                      tp.topic, tp.partition, records.len(), high_watermark);
                            }
                        }
                    },
                    Ok(_) => {
                        // No records, nothing to restore
                    },
                    Err(e) => {
                        warn!("Failed to read WAL for {}-{}: {}", tp.topic, tp.partition, e);
                    }
                }
            }

            info!("WAL replay complete");
        } else {
            // WAL is empty (e.g., after deletion or fresh start with persisted segments)
            // Restore partition states from metadata store instead
            info!("WAL is empty, attempting to restore partition states from metadata store...");

            match metadata_store.list_topics().await {
                Ok(topics) => {
                    for topic_meta in topics {
                        let partition_count = topic_meta.config.partition_count as i32;
                        for partition_id in 0..partition_count {
                            // Get high watermark from metadata store
                            match metadata_store.get_partition_offset(&topic_meta.name, partition_id as u32).await {
                                Ok(Some((high_watermark, _log_start_offset))) if high_watermark > 0 => {
                                    // Restore partition state
                                    if let Err(e) = produce_handler_base.restore_partition_state(&topic_meta.name, partition_id, high_watermark as u64).await {
                                        warn!("Failed to restore partition state from metadata for {}-{}: {}", topic_meta.name, partition_id, e);
                                    } else {
                                        info!("Restored {}-{} from metadata store: high watermark = {}", topic_meta.name, partition_id, high_watermark);
                                    }
                                }
                                Ok(_) => {
                                    // No offset data or hwm=0, nothing to restore
                                }
                                Err(e) => {
                                    warn!("Failed to get partition offset from metadata for {}-{}: {}", topic_meta.name, partition_id, e);
                                }
                            }
                        }
                    }
                    info!("Metadata store recovery complete");
                }
                Err(e) => {
                    warn!("Failed to list topics from metadata store: {}", e);
                }
            }
        }

        info!("✅ WAL watermark recovery complete");
        Ok(())
    }

    /// Stage 11: Initialize FetchHandler
    /// Complexity: < 10 (simple creation with dependencies)
    async fn init_fetch_handler(&mut self) -> Result<()> {
        info!("Stage 11: Initializing FetchHandler");

        let segment_reader = self.segment_reader.as_ref()
            .context("segment_reader not initialized")?;
        let metadata_store = self.metadata_store.as_ref()
            .context("metadata_store not initialized")?;
        let object_store = self.object_store.as_ref()
            .context("object_store not initialized")?;
        let wal_manager = self.wal_manager.as_ref()
            .context("wal_manager not initialized")?;
        let produce_handler_base = self.produce_handler_base.as_ref()
            .context("produce_handler_base not initialized")?;

        // Create FetchHandler with WAL and ProduceHandler integration
        // FetchHandler needs ProduceHandler to get the real-time high watermark
        let fetch_handler = Arc::new(FetchHandler::new_with_wal(
            segment_reader.clone(),
            metadata_store.clone(),
            object_store.clone(),
            wal_manager.clone(),
            produce_handler_base.clone(),
        ));

        self.fetch_handler = Some(fetch_handler);

        info!("✅ FetchHandler initialized");
        Ok(())
    }

    /// Stage 12: Initialize KafkaProtocolHandler
    /// Complexity: < 15 (final assembly of all handlers)
    async fn init_kafka_handler(&mut self) -> Result<()> {
        info!("Stage 12: Initializing KafkaProtocolHandler");

        let produce_handler_base = self.produce_handler_base.as_ref()
            .context("produce_handler_base not initialized")?;
        let segment_reader = self.segment_reader.as_ref()
            .context("segment_reader not initialized")?;
        let metadata_store = self.metadata_store.as_ref()
            .context("metadata_store not initialized")?;
        let object_store = self.object_store.as_ref()
            .context("object_store not initialized")?;
        let fetch_handler = self.fetch_handler.as_ref()
            .context("fetch_handler not initialized")?;
        let wal_handler = self.wal_produce_handler.as_ref()
            .context("wal_produce_handler not initialized")?;

        // Initialize Kafka protocol handler with all components
        // WAL is MANDATORY - no longer optional
        let kafka_handler = Arc::new(KafkaProtocolHandler::new(
            produce_handler_base.clone(),
            segment_reader.clone(),
            metadata_store.clone(),
            object_store.clone(),
            fetch_handler.clone(),
            wal_handler.clone(),  // WAL is mandatory, not optional
            self.config.node_id,
            self.config.advertised_host.clone(),
            self.config.advertised_port,
            self.config.num_partitions,  // Pass default partition count from config
        ).await?);

        self.kafka_handler = Some(kafka_handler);

        info!("✅ KafkaProtocolHandler initialized");
        Ok(())
    }

    /// Stage 13: Initialize WalIndexer background task
    /// Complexity: < 15 (configuration and task startup)
    async fn init_wal_indexer(&mut self) -> Result<()> {
        info!("Stage 13: Initializing WAL Indexer for background indexing");

        let object_store = self.object_store.as_ref()
            .context("object_store not initialized")?;
        let wal_handler = self.wal_produce_handler.as_ref()
            .context("wal_produce_handler not initialized")?;
        let metadata_store = self.metadata_store.as_ref()
            .context("metadata_store not initialized")?;
        let storage_config = self.storage_config.as_ref()
            .context("storage_config not initialized")?;

        let indexer_config = WalIndexerConfig {
            interval_secs: self.config.wal_indexing_interval_secs,
            min_segment_age_secs: 10,
            max_segments_per_run: 100,
            delete_after_index: true,
            object_store: storage_config.object_store_config.clone(),
            index_base_path: format!("{}/tantivy_indexes", self.config.data_dir),
            parallel_indexing: false, // Start with serial processing
            max_concurrent_tasks: 4,
            segment_index_path: Some(PathBuf::from(format!("{}/segment_index.json", self.config.data_dir))),
            segment_index_auto_save: true,
        };

        let wal_manager = wal_handler.wal_manager().clone();

        let wal_indexer = Arc::new(WalIndexer::new(
            indexer_config,
            wal_manager,
            object_store.clone(),
            metadata_store.clone(),
        ));

        // Start the background indexing task
        wal_indexer.start().await
            .map_err(|e| anyhow::anyhow!("Failed to start WAL indexer: {}", e))?;

        info!("WAL Indexer started successfully (interval: {}s)", self.config.wal_indexing_interval_secs);
        info!("  Segment index will track Tantivy segments for future query optimization");

        self.wal_indexer = Some(wal_indexer);

        info!("✅ WAL Indexer initialized");
        Ok(())
    }

    /// Stage 14: Initialize MetadataUploader for disaster recovery
    /// Complexity: < 15 (configuration and conditional startup)
    async fn init_metadata_uploader(&mut self) -> Result<()> {
        info!("Stage 14: Initializing Metadata Uploader (if enabled)");

        let object_store = self.object_store.as_ref()
            .context("object_store not initialized")?;
        let storage_config = self.storage_config.as_ref()
            .context("storage_config not initialized")?;

        // Only enable for remote object stores (S3/GCS/Azure)
        let is_remote_object_store = matches!(
            storage_config.object_store_config.backend,
            chronik_storage::object_store::StorageBackend::S3 { .. } |
            chronik_storage::object_store::StorageBackend::Gcs { .. } |
            chronik_storage::object_store::StorageBackend::Azure { .. }
        );

        let metadata_uploader = if self.config.enable_metadata_dr && is_remote_object_store {
            info!("Initializing Metadata Uploader for disaster recovery (metadata WAL → Object Store)");

            let uploader_config = chronik_common::metadata::MetadataUploaderConfig {
                upload_interval_secs: self.config.metadata_upload_interval_secs,
                wal_base_path: "metadata-wal".to_string(),
                snapshot_base_path: "metadata-snapshots".to_string(),
                delete_after_upload: false, // Keep local WAL for redundancy
                delete_old_snapshots: true,
                keep_local_snapshot_count: 2,
                enabled: true,
            };

            let uploader = Arc::new(crate::metadata_dr::create_metadata_uploader(
                object_store.clone(),
                &self.config.data_dir,
                uploader_config,
            ));

            // Start the background upload task
            uploader.start().await
                .map_err(|e| anyhow::anyhow!("Failed to start metadata uploader: {}", e))?;

            info!("Metadata Uploader started successfully (interval: {}s)", self.config.metadata_upload_interval_secs);
            info!("  Metadata WAL and snapshots will be uploaded to object store for disaster recovery");

            Some(uploader)
        } else {
            if !is_remote_object_store {
                info!("Metadata Uploader disabled (local filesystem backend - no DR benefit)");
                info!("  Local metadata already persists to disk and survives restarts");
                info!("  For true DR, configure S3/GCS/Azure object store");
            } else {
                info!("Metadata Uploader disabled by configuration");
            }
            None
        };

        self.metadata_uploader = metadata_uploader;

        info!("✅ Metadata Uploader initialization complete");
        Ok(())
    }

    /// Stage 15: Initialize and start WalReceiver (cluster mode only)
    /// Complexity: < 20 (conditional cluster setup)
    async fn init_wal_receiver(&mut self) -> Result<()> {
        info!("Stage 15: Initializing WAL Receiver (cluster mode only)");

        // Only start WAL receiver in cluster mode
        if let Some(ref cluster_config) = self.config.cluster_config {
            // Get WAL bind address from cluster config
            let wal_bind_addr = cluster_config.bind.as_ref()
                .and_then(|bind| Some(bind.wal.clone()))
                .context("WAL bind address not configured in cluster config")?;

            info!("Starting WAL receiver on {}", wal_bind_addr);

            // Get required dependencies
            let wal_manager = self.wal_manager.as_ref()
                .context("wal_manager not initialized")?;
            let isr_ack_tracker = self.isr_ack_tracker.as_ref()
                .context("isr_ack_tracker not initialized")?;
            let leader_elector = self.leader_elector.as_ref();
            let raft_cluster = self.raft_cluster_for_metadata.as_ref();
            let produce_handler = self.produce_handler_base.as_ref()
                .context("produce_handler not initialized")?;
            let metadata_store = self.metadata_store.as_ref()
                .context("metadata_store not initialized")?;

            // Create WalReceiver
            let mut wal_receiver = crate::wal_replication::WalReceiver::new_with_isr_tracker(
                wal_bind_addr.clone(),
                wal_manager.clone(),
                isr_ack_tracker.clone(),
                cluster_config.node_id,
            );

            // Wire up leader elector if available
            if let Some(elector) = leader_elector {
                wal_receiver.set_leader_elector(elector.clone());
            }

            // Wire up Raft cluster if available
            if let Some(raft) = raft_cluster {
                wal_receiver.set_raft_cluster(raft.clone());
            }

            // Wire up ProduceHandler for watermark updates
            wal_receiver.set_produce_handler(produce_handler.clone());

            // Wire up MetadataStore for partition offset updates
            wal_receiver.set_metadata_store(metadata_store.clone());

            // Start WalReceiver in background task
            let wal_receiver = Arc::new(wal_receiver);
            let receiver_clone = wal_receiver.clone();
            tokio::spawn(async move {
                if let Err(e) = receiver_clone.run().await {
                    error!("WAL receiver failed: {}", e);
                }
            });

            info!("✅ WAL Receiver started on {}", wal_bind_addr);
        } else {
            info!("Skipping WAL Receiver initialization (standalone mode - no clustering)");
        }

        Ok(())
    }

    /// Build the IntegratedKafkaServer
    ///
    /// This orchestrates all initialization stages in order.
    /// Complexity: < 25 (orchestration only, delegates to stage functions)
    pub async fn build(mut self) -> Result<IntegratedKafkaServer> {
        info!("🔧 Starting IntegratedKafkaServer build process (15 stages)...");

        // Stage 1: Directories
        self.init_directories().await
            .context("Stage 1 failed: directory initialization")?;

        // Stage 2: Raft cluster
        self.init_raft_cluster().await
            .context("Stage 2 failed: Raft cluster initialization")?;

        // Stage 3: Metadata WAL
        self.init_metadata_wal().await
            .context("Stage 3 failed: Metadata WAL initialization")?;

        // Stage 4: Event bus
        self.init_event_bus().await
            .context("Stage 4 failed: Event bus initialization")?;

        // Stage 5: Metadata store (includes WAL recovery)
        self.init_metadata_store().await
            .context("Stage 5 failed: Metadata store initialization")?;

        // Stage 6: Metadata replication
        self.init_metadata_replication().await
            .context("Stage 6 failed: Metadata replication initialization")?;

        // Stage 7: Storage layer
        self.init_storage().await
            .context("Stage 7 failed: Storage layer initialization")?;

        // Stage 8: WAL Manager
        self.init_wal_manager().await
            .context("Stage 8 failed: WAL Manager initialization")?;

        // Stage 9: ProduceHandler
        self.init_produce_handler().await
            .context("Stage 9 failed: ProduceHandler initialization")?;

        // Stage 10: WAL recovery and watermark restoration
        self.recover_wal_watermarks().await
            .context("Stage 10 failed: WAL recovery")?;

        // Stage 11: FetchHandler
        self.init_fetch_handler().await
            .context("Stage 11 failed: FetchHandler initialization")?;

        // Stage 12: KafkaProtocolHandler
        self.init_kafka_handler().await
            .context("Stage 12 failed: KafkaProtocolHandler initialization")?;

        // Stage 13: WalIndexer
        self.init_wal_indexer().await
            .context("Stage 13 failed: WalIndexer initialization")?;

        // Stage 14: MetadataUploader
        self.init_metadata_uploader().await
            .context("Stage 14 failed: MetadataUploader initialization")?;

        // Stage 15: WalReceiver (cluster mode only)
        self.init_wal_receiver().await
            .context("Stage 15 failed: WAL Receiver initialization")?;

        info!("✅ All 15 stages complete - assembling IntegratedKafkaServer");

        // Create the final server instance using the new_from_components constructor
        let kafka_handler = self.kafka_handler.unwrap();
        let metadata_store = self.metadata_store.unwrap();
        let wal_indexer = self.wal_indexer.unwrap();
        let metadata_uploader = self.metadata_uploader;
        let leader_elector = self.leader_elector;

        let server = IntegratedKafkaServer::new_from_components(
            self.config,
            kafka_handler,
            metadata_store,
            wal_indexer,
            metadata_uploader,
            leader_elector,
        );

        info!("🎉 IntegratedKafkaServer build complete!");
        Ok(server)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_builder_init_directories() {
        let temp_dir = TempDir::new().unwrap();
        let config = IntegratedServerConfig {
            data_dir: temp_dir.path().to_string_lossy().to_string(),
            ..Default::default()
        };

        let mut builder = IntegratedKafkaServerBuilder::new(config);
        builder.init_directories().await.unwrap();

        // Verify directories were created
        assert!(builder.data_dir.is_some());
        assert!(builder.segments_dir.is_some());
        assert!(builder.data_dir.as_ref().unwrap().exists());
        assert!(builder.segments_dir.as_ref().unwrap().exists());
    }

    #[tokio::test]
    async fn test_builder_init_raft_single_node() {
        let temp_dir = TempDir::new().unwrap();
        let config = IntegratedServerConfig {
            node_id: 1,
            data_dir: temp_dir.path().to_string_lossy().to_string(),
            ..Default::default()
        };

        let mut builder = IntegratedKafkaServerBuilder::new(config);
        builder.init_directories().await.unwrap();
        builder.init_raft_cluster().await.unwrap();

        // Verify Raft cluster created
        assert!(builder.raft_cluster_for_metadata.is_some());
    }

    #[tokio::test]
    async fn test_builder_init_metadata_wal() {
        let temp_dir = TempDir::new().unwrap();
        let config = IntegratedServerConfig {
            data_dir: temp_dir.path().to_string_lossy().to_string(),
            ..Default::default()
        };

        let mut builder = IntegratedKafkaServerBuilder::new(config);
        builder.init_directories().await.unwrap();
        builder.init_metadata_wal().await.unwrap();

        // Verify metadata WAL created
        assert!(builder.metadata_wal.is_some());
    }

    #[tokio::test]
    async fn test_builder_init_event_bus() {
        let temp_dir = TempDir::new().unwrap();
        let config = IntegratedServerConfig {
            data_dir: temp_dir.path().to_string_lossy().to_string(),
            ..Default::default()
        };

        let mut builder = IntegratedKafkaServerBuilder::new(config);
        builder.init_event_bus().await.unwrap();

        // Verify event bus created
        assert!(builder.metadata_event_bus.is_some());
    }

    #[tokio::test]
    async fn test_builder_init_metadata_store() {
        let temp_dir = TempDir::new().unwrap();
        let config = IntegratedServerConfig {
            data_dir: temp_dir.path().to_string_lossy().to_string(),
            ..Default::default()
        };

        let mut builder = IntegratedKafkaServerBuilder::new(config);
        builder.init_directories().await.unwrap();
        builder.init_metadata_wal().await.unwrap();
        builder.init_event_bus().await.unwrap();
        builder.init_metadata_store().await.unwrap();

        // Verify metadata store created
        assert!(builder.wal_metadata_store.is_some());
        assert!(builder.metadata_store.is_some());
    }

    #[tokio::test]
    async fn test_builder_init_metadata_replication() {
        let temp_dir = TempDir::new().unwrap();
        let config = IntegratedServerConfig {
            data_dir: temp_dir.path().to_string_lossy().to_string(),
            ..Default::default()
        };

        let mut builder = IntegratedKafkaServerBuilder::new(config);
        builder.init_directories().await.unwrap();
        builder.init_raft_cluster().await.unwrap();
        builder.init_metadata_wal().await.unwrap();
        builder.init_event_bus().await.unwrap();
        builder.init_metadata_store().await.unwrap();

        // Metadata replication creates background tasks but doesn't store state
        // Just verify it completes without error
        builder.init_metadata_replication().await.unwrap();
    }

    #[tokio::test]
    async fn test_builder_init_storage() {
        let temp_dir = TempDir::new().unwrap();
        let config = IntegratedServerConfig {
            data_dir: temp_dir.path().to_string_lossy().to_string(),
            ..Default::default()
        };

        let mut builder = IntegratedKafkaServerBuilder::new(config);
        builder.init_directories().await.unwrap();
        builder.init_storage().await.unwrap();

        // Verify storage components created
        assert!(builder.object_store.is_some());
        assert!(builder.storage_config.is_some());
        assert!(builder.storage_service.is_some());
        assert!(builder.segment_reader.is_some());
    }

    #[tokio::test]
    async fn test_builder_init_wal_manager() {
        let temp_dir = TempDir::new().unwrap();
        let config = IntegratedServerConfig {
            data_dir: temp_dir.path().to_string_lossy().to_string(),
            ..Default::default()
        };

        let mut builder = IntegratedKafkaServerBuilder::new(config);
        builder.init_directories().await.unwrap();
        builder.init_wal_manager().await.unwrap();

        // Verify WAL manager created
        assert!(builder.wal_manager.is_some());
    }

    #[tokio::test]
    async fn test_builder_init_produce_handler() {
        let temp_dir = TempDir::new().unwrap();
        let config = IntegratedServerConfig {
            data_dir: temp_dir.path().to_string_lossy().to_string(),
            ..Default::default()
        };

        let mut builder = IntegratedKafkaServerBuilder::new(config);
        builder.init_directories().await.unwrap();
        builder.init_raft_cluster().await.unwrap();
        builder.init_metadata_wal().await.unwrap();
        builder.init_event_bus().await.unwrap();
        builder.init_metadata_store().await.unwrap();
        builder.init_storage().await.unwrap();
        builder.init_wal_manager().await.unwrap();
        builder.init_produce_handler().await.unwrap();

        // Verify ProduceHandler components created
        assert!(builder.produce_handler_base.is_some());
        assert!(builder.wal_produce_handler.is_some());
        assert!(builder.isr_ack_tracker.is_some());
        assert!(builder.response_pipeline.is_some());
    }

    #[tokio::test]
    async fn test_builder_init_fetch_handler() {
        let temp_dir = TempDir::new().unwrap();
        let config = IntegratedServerConfig {
            data_dir: temp_dir.path().to_string_lossy().to_string(),
            ..Default::default()
        };

        let mut builder = IntegratedKafkaServerBuilder::new(config);
        builder.init_directories().await.unwrap();
        builder.init_raft_cluster().await.unwrap();
        builder.init_metadata_wal().await.unwrap();
        builder.init_event_bus().await.unwrap();
        builder.init_metadata_store().await.unwrap();
        builder.init_storage().await.unwrap();
        builder.init_wal_manager().await.unwrap();
        builder.init_produce_handler().await.unwrap();
        builder.init_fetch_handler().await.unwrap();

        // Verify FetchHandler created
        assert!(builder.fetch_handler.is_some());
    }

    #[tokio::test]
    async fn test_builder_init_kafka_handler() {
        let temp_dir = TempDir::new().unwrap();
        let config = IntegratedServerConfig {
            data_dir: temp_dir.path().to_string_lossy().to_string(),
            ..Default::default()
        };

        let mut builder = IntegratedKafkaServerBuilder::new(config);
        builder.init_directories().await.unwrap();
        builder.init_raft_cluster().await.unwrap();
        builder.init_metadata_wal().await.unwrap();
        builder.init_event_bus().await.unwrap();
        builder.init_metadata_store().await.unwrap();
        builder.init_storage().await.unwrap();
        builder.init_wal_manager().await.unwrap();
        builder.init_produce_handler().await.unwrap();
        builder.init_fetch_handler().await.unwrap();
        builder.init_kafka_handler().await.unwrap();

        // Verify KafkaProtocolHandler created
        assert!(builder.kafka_handler.is_some());
    }

    #[tokio::test]
    async fn test_builder_init_wal_indexer() {
        let temp_dir = TempDir::new().unwrap();
        let config = IntegratedServerConfig {
            data_dir: temp_dir.path().to_string_lossy().to_string(),
            ..Default::default()
        };

        let mut builder = IntegratedKafkaServerBuilder::new(config);
        builder.init_directories().await.unwrap();
        builder.init_storage().await.unwrap();
        builder.init_wal_indexer().await.unwrap();

        // Verify WalIndexer created
        assert!(builder.wal_indexer.is_some());
    }

    #[tokio::test]
    async fn test_builder_init_metadata_uploader_local() {
        let temp_dir = TempDir::new().unwrap();
        let config = IntegratedServerConfig {
            data_dir: temp_dir.path().to_string_lossy().to_string(),
            ..Default::default()
        };

        let mut builder = IntegratedKafkaServerBuilder::new(config);
        builder.init_directories().await.unwrap();
        builder.init_metadata_wal().await.unwrap();
        builder.init_storage().await.unwrap();
        builder.init_metadata_uploader().await.unwrap();

        // For local backend, metadata_uploader should be None
        assert!(builder.metadata_uploader.is_none());
    }

    #[tokio::test]
    async fn test_builder_full_pipeline() {
        // Test the complete build() pipeline to ensure all stages work together
        let temp_dir = TempDir::new().unwrap();
        let config = IntegratedServerConfig {
            data_dir: temp_dir.path().to_string_lossy().to_string(),
            node_id: 1,
            advertised_host: "localhost".to_string(),
            advertised_port: 9092,
            ..Default::default()
        };

        let server = IntegratedKafkaServerBuilder::new(config)
            .build()
            .await
            .unwrap();

        // Verify server is fully initialized
        let stats = server.get_stats().await.unwrap();
        assert_eq!(stats.node_id, 1);
    }
}
