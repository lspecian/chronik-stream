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
    isr_tracker: Option<Arc<crate::isr_tracker::IsrTracker>>,
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
            isr_tracker: None,
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
    /// Complexity: < 25 (orchestrates initialization using extracted helpers)
    async fn init_produce_handler(&mut self) -> Result<()> {
        info!("Stage 9: Initializing ProduceHandler with dependencies");

        let object_store = self.object_store.as_ref().context("object_store not initialized")?;
        let metadata_store = self.metadata_store.as_ref().context("metadata_store not initialized")?;
        let wal_manager = self.wal_manager.as_ref().context("wal_manager not initialized")?;
        let metadata_event_bus = self.metadata_event_bus.as_ref().context("metadata_event_bus not initialized")?;
        let storage_config = self.storage_config.as_ref().context("storage_config not initialized")?;

        // Step 1: Build configuration
        let produce_config = self.create_produce_handler_config(storage_config);

        // Step 2: Create ProduceHandler with WAL support
        let mut produce_handler_inner = ProduceHandler::new_with_wal(
            produce_config,
            object_store.clone(),
            metadata_store.clone(),
            wal_manager.clone(),
        ).await?;

        // Step 3: Wire event bus and ISR trackers
        produce_handler_inner.set_event_bus(metadata_event_bus.clone());
        let isr_ack_tracker = crate::isr_ack_tracker::IsrAckTracker::new();
        let isr_tracker = Arc::new(crate::isr_tracker::IsrTracker::default());
        produce_handler_inner.set_isr_ack_tracker(isr_ack_tracker.clone());
        info!("✅ EventBus, IsrAckTracker, and IsrTracker wired");

        // Step 4: Wire Raft dependencies (cluster mode only)
        let leader_elector = self.wire_raft_dependencies(&mut produce_handler_inner, &isr_ack_tracker, &isr_tracker, metadata_store);

        // Step 5: Setup response pipeline with WAL callback
        let response_pipeline = self.setup_response_pipeline(&mut produce_handler_inner, wal_manager);

        // Step 6: Finalize and start background tasks
        let (produce_handler_base, wal_handler) = self.finalize_produce_handler(produce_handler_inner, wal_manager).await;

        // Store all components
        self.produce_handler_base = Some(produce_handler_base);
        self.wal_produce_handler = Some(wal_handler);
        self.isr_ack_tracker = Some(isr_ack_tracker);
        self.isr_tracker = Some(isr_tracker);
        self.response_pipeline = Some(response_pipeline);
        self.leader_elector = leader_elector;

        info!("✅ ProduceHandler initialized with all dependencies and wiring");
        Ok(())
    }

    /// Helper: Build ProduceHandler configuration
    fn create_produce_handler_config(&self, storage_config: &IngestStorageConfig) -> ProduceHandlerConfig {
        let indexer_config = chronik_search::realtime_indexer::RealtimeIndexerConfig {
            index_base_path: PathBuf::from(format!("{}/index", self.config.data_dir)),
            ..Default::default()
        };

        let flush_profile = ProduceFlushProfile::auto_select();
        let compression_type = if self.config.enable_compression {
            chronik_storage::kafka_records::CompressionType::Snappy
        } else {
            chronik_storage::kafka_records::CompressionType::None
        };

        ProduceHandlerConfig {
            node_id: self.config.node_id,
            storage_config: storage_config.clone(),
            indexer_config,
            enable_indexing: self.config.enable_indexing,
            enable_idempotence: true,
            enable_transactions: false,
            max_in_flight_requests: 5,
            batch_size: 16384,
            linger_ms: flush_profile.linger_ms(),
            compression_type,
            request_timeout_ms: 120000,
            buffer_memory: flush_profile.buffer_memory(),
            auto_create_topics_enable: self.config.auto_create_topics,
            num_partitions: self.config.num_partitions,
            default_replication_factor: self.config.replication_factor,
            flush_profile,
        }
    }

    /// Helper: Wire Raft-related dependencies to ProduceHandler
    fn wire_raft_dependencies(
        &self,
        produce_handler: &mut crate::produce_handler::ProduceHandler,
        isr_ack_tracker: &Arc<crate::isr_ack_tracker::IsrAckTracker>,
        isr_tracker: &Arc<crate::isr_tracker::IsrTracker>,
        metadata_store: &Arc<dyn MetadataStore>,
    ) -> Option<Arc<crate::leader_election::LeaderElector>> {
        let raft_cluster = match self.raft_cluster_for_metadata.as_ref() {
            Some(c) => c,
            None => {
                info!("⚠️  Raft clustering disabled - single-node mode");
                return None;
            }
        };

        // Wire RaftCluster
        produce_handler.set_raft_cluster(Arc::clone(raft_cluster));

        // Create and wire LeaderElector
        let elector = Arc::new(crate::leader_election::LeaderElector::new(
            raft_cluster.clone(),
            metadata_store.clone(),
        ));
        produce_handler.set_leader_elector(elector.clone());
        info!("✓ LeaderElector ready (event-driven mode)");

        // Create and wire WalReplicationManager for data messages
        let data_wal_repl_manager = crate::wal_replication::WalReplicationManager::new_with_dependencies(
            Vec::new(),
            Some(raft_cluster.clone()),
            Some(isr_tracker.clone()),
            Some(isr_ack_tracker.clone()),
            self.config.cluster_config.clone().map(Arc::new),
            Some(metadata_store.clone()),
        );
        produce_handler.set_wal_replication_manager(data_wal_repl_manager);
        info!("✅ Data WAL replication manager wired (replica cache enabled)");

        Some(elector)
    }

    /// Helper: Setup ResponsePipeline with WAL commit callback
    fn setup_response_pipeline(
        &self,
        produce_handler: &mut crate::produce_handler::ProduceHandler,
        wal_manager: &Arc<WalManager>,
    ) -> Arc<crate::response_pipeline::ResponsePipeline> {
        let response_pipeline = Arc::new(crate::response_pipeline::ResponsePipeline::new());

        // Create commit callback for WAL batches
        let response_pipeline_clone = response_pipeline.clone();
        let partition_states = produce_handler.partition_states.clone();
        let commit_callback: chronik_wal::group_commit::CommitCallback = Arc::new(
            move |topic: &str, partition: i32, min_offset: i64, max_offset: i64| {
                // Update in-memory high watermark
                let new_watermark = max_offset + 1;
                if let Some(state) = partition_states.get(&(topic.to_string(), partition)) {
                    state.high_watermark.store(new_watermark as u64, Ordering::Release);
                    debug!("✅ WAL_CALLBACK: Updated high watermark {}-{} = {}", topic, partition, new_watermark);
                }
                // Notify ResponsePipeline
                response_pipeline_clone.notify_batch_committed(topic, partition, min_offset, max_offset);
            }
        );

        wal_manager.group_commit_wal().set_commit_callback(commit_callback);
        produce_handler.set_response_pipeline(response_pipeline.clone());
        info!("✅ ResponsePipeline wired to GroupCommitWal and ProduceHandler");

        response_pipeline
    }

    /// Helper: Finalize ProduceHandler - wrap in Arc and start background tasks
    async fn finalize_produce_handler(
        &self,
        produce_handler_inner: crate::produce_handler::ProduceHandler,
        wal_manager: &Arc<WalManager>,
    ) -> (Arc<crate::produce_handler::ProduceHandler>, Arc<WalProduceHandler>) {
        let produce_handler_base = Arc::new(produce_handler_inner);

        // Start background tasks
        produce_handler_base.clone().start_metadata_event_listener();
        produce_handler_base.start_background_tasks().await;
        info!("✅ ProduceHandler background tasks started");

        // Wrap with WalProduceHandler for backward compatibility
        let wal_handler = Arc::new(WalProduceHandler::new_passthrough(
            wal_manager.clone(),
            produce_handler_base.clone(),
        ));

        (produce_handler_base, wal_handler)
    }

    /// Stage 10: WAL recovery and watermark restoration
    /// Complexity: < 25 (orchestrates recovery using extracted helpers)
    async fn recover_wal_watermarks(&mut self) -> Result<()> {
        info!("Stage 10: Recovering WAL watermarks");

        let produce_handler_base = self.produce_handler_base.as_ref()
            .context("produce_handler_base not initialized")?;
        let wal_manager = self.wal_manager.as_ref()
            .context("wal_manager not initialized")?;
        let metadata_store = self.metadata_store.as_ref()
            .context("metadata_store not initialized")?;

        // CRITICAL FIX (v1.3.52): Clear all partition buffers before WAL recovery
        info!("Clearing all partition buffers before WAL recovery...");
        if let Err(e) = produce_handler_base.clear_all_buffers().await {
            warn!("Failed to clear partition buffers: {}. Recovery may have duplicates.", e);
        }

        // Replay WAL to restore high watermarks after crash
        let recovered_partitions = wal_manager.get_partitions();
        if !recovered_partitions.is_empty() {
            Self::recover_partitions_from_wal(produce_handler_base, wal_manager, &recovered_partitions).await;
        } else {
            // WAL is empty - restore from metadata store instead
            Self::recover_partitions_from_metadata(produce_handler_base, metadata_store).await;
        }

        info!("✅ WAL watermark recovery complete");
        Ok(())
    }

    /// Helper: Find maximum offset from WAL records (handles V1 and V2 formats)
    fn find_max_offset_in_records(records: &[chronik_wal::record::WalRecord]) -> i64 {
        use chronik_wal::record::WalRecord;
        use chronik_storage::CanonicalRecord;

        let mut max_offset: i64 = -1;

        for record in records {
            let offset = match record {
                WalRecord::V1 { offset, .. } => *offset,
                WalRecord::V2 { canonical_data, .. } => {
                    bincode::deserialize::<CanonicalRecord>(canonical_data)
                        .map(|c| c.last_offset())
                        .unwrap_or(-1)
                }
            };
            if offset > max_offset {
                max_offset = offset;
            }
        }
        max_offset
    }

    /// Helper: Recover partitions from WAL records
    async fn recover_partitions_from_wal(
        produce_handler: &Arc<crate::produce_handler::ProduceHandler>,
        wal_manager: &Arc<WalManager>,
        partitions: &[chronik_wal::manager::TopicPartition],
    ) {
        info!("Replaying WAL to restore high watermarks for {} partitions...", partitions.len());

        for tp in partitions {
            match wal_manager.read_from(&tp.topic, tp.partition, 0, usize::MAX).await {
                Ok(records) if !records.is_empty() => {
                    let max_offset = Self::find_max_offset_in_records(&records);
                    if max_offset >= 0 {
                        let high_watermark = (max_offset + 1) as u64;
                        match produce_handler.restore_partition_state(&tp.topic, tp.partition, high_watermark).await {
                            Ok(_) => info!("Restored {}-{}: {} records, high watermark = {}",
                                          tp.topic, tp.partition, records.len(), high_watermark),
                            Err(e) => warn!("Failed to restore partition state for {}-{}: {}", tp.topic, tp.partition, e),
                        }
                    }
                }
                Ok(_) => {} // No records, nothing to restore
                Err(e) => warn!("Failed to read WAL for {}-{}: {}", tp.topic, tp.partition, e),
            }
        }
        info!("WAL replay complete");
    }

    /// Helper: Recover partitions from metadata store (fallback when WAL is empty)
    async fn recover_partitions_from_metadata(
        produce_handler: &Arc<crate::produce_handler::ProduceHandler>,
        metadata_store: &Arc<dyn MetadataStore>,
    ) {
        info!("WAL is empty, attempting to restore partition states from metadata store...");

        let topics = match metadata_store.list_topics().await {
            Ok(t) => t,
            Err(e) => {
                warn!("Failed to list topics from metadata store: {}", e);
                return;
            }
        };

        for topic_meta in topics {
            let partition_count = topic_meta.config.partition_count as i32;
            for partition_id in 0..partition_count {
                Self::restore_partition_from_metadata(produce_handler, metadata_store, &topic_meta.name, partition_id).await;
            }
        }
        info!("Metadata store recovery complete");
    }

    /// Helper: Restore single partition from metadata store
    async fn restore_partition_from_metadata(
        produce_handler: &Arc<crate::produce_handler::ProduceHandler>,
        metadata_store: &Arc<dyn MetadataStore>,
        topic: &str,
        partition_id: i32,
    ) {
        match metadata_store.get_partition_offset(topic, partition_id as u32).await {
            Ok(Some((high_watermark, _log_start_offset))) if high_watermark > 0 => {
                match produce_handler.restore_partition_state(topic, partition_id, high_watermark as u64).await {
                    Ok(_) => info!("Restored {}-{} from metadata store: high watermark = {}", topic, partition_id, high_watermark),
                    Err(e) => warn!("Failed to restore partition state from metadata for {}-{}: {}", topic, partition_id, e),
                }
            }
            Ok(_) => {} // No offset data or hwm=0, nothing to restore
            Err(e) => warn!("Failed to get partition offset from metadata for {}-{}: {}", topic, partition_id, e),
        }
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

        // v2.2.23: Read S3 upload configuration from environment variables
        // These follow the local-first design: S3 upload is opt-in
        let columnar_use_object_store = std::env::var("CHRONIK_COLUMNAR_USE_OBJECT_STORE")
            .map(|v| v.to_lowercase() == "true")
            .unwrap_or(false);
        let columnar_s3_prefix = std::env::var("CHRONIK_COLUMNAR_S3_PREFIX")
            .unwrap_or_else(|_| "columnar".to_string());
        let columnar_keep_local = std::env::var("CHRONIK_COLUMNAR_KEEP_LOCAL")
            .map(|v| v.to_lowercase() != "false")
            .unwrap_or(true); // Default: keep local copies

        // If object storage is enabled for columnar, use the same config as Tantivy indexes
        // but with a different prefix. Users can override with separate bucket via
        // CHRONIK_COLUMNAR_S3_BUCKET environment variable in the future if needed.
        let columnar_object_store = if columnar_use_object_store {
            Some(storage_config.object_store_config.clone())
        } else {
            None
        };

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
            stale_segment_seal_secs: 30, // v2.2.16: Seal stale segments after 30s idle
            columnar_base_path: format!("{}/columnar", self.config.data_dir), // v2.2.21: Parquet files
            columnar_use_object_store, // v2.2.23: Enable S3 upload for Parquet files
            columnar_object_store, // v2.2.23: Object store config for Parquet files
            columnar_s3_prefix, // v2.2.23: Prefix for Parquet files in object storage
            columnar_keep_local, // v2.2.23: Keep local copy of Parquet files
            vector_base_path: format!("{}/vectors", self.config.data_dir), // v2.2.22: Vector embeddings
            vector_snapshot_interval_secs: 300, // v2.2.22: Snapshot vector indexes every 5 minutes
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
        let isr_tracker = self.isr_tracker;

        let server = IntegratedKafkaServer::new_from_components(
            self.config,
            kafka_handler,
            metadata_store,
            wal_indexer,
            metadata_uploader,
            leader_elector,
            isr_tracker,
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
