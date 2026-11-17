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
    /// The cluster catalog (topics, assignments, brokers, ACLs, SCRAM).
    cluster_catalog: Option<Arc<crate::cluster_catalog::ClusterCatalog>>,
    catalog_store: Option<Arc<crate::catalog_metadata_store::CatalogMetadataStore>>,

    // Initialized paths (Stage 1)
    data_dir: Option<PathBuf>,
    segments_dir: Option<PathBuf>,

    // Raft and metadata (Stage 2)
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

    /// Shared by the produce path and the fetch long poll, so an append wakes a
    /// parked follower instead of it noticing on a timer (RP-9). Created here
    /// because the two handlers are built in different stages and neither can be
    /// mutated once it is inside an `Arc`.
    append_notify: Arc<dashmap::DashMap<(String, i32), Arc<tokio::sync::Notify>>>,
    wal_produce_handler: Option<Arc<WalProduceHandler>>,
    fetch_handler: Option<Arc<FetchHandler>>,
    isr_ack_tracker: Option<Arc<crate::isr_ack_tracker::IsrAckTracker>>,
    isr_tracker: Option<Arc<crate::isr_tracker::IsrTracker>>,
    response_pipeline: Option<Arc<crate::response_pipeline::ResponsePipeline>>,
    kafka_handler: Option<Arc<KafkaProtocolHandler>>,
    wal_indexer: Option<Arc<WalIndexer>>,
    // HP-1.2: In-memory hot text index (NRT search). None = feature disabled.
    hot_text_index: Option<Arc<chronik_search::hot_text_index::HotTextIndex>>,
    // HP-2.x: In-memory hot vector index + shared batcher-handle slot.
    hot_vector_index: Option<Arc<chronik_columnar::hot_vector_index::HotVectorIndex>>,
    hot_vector_batcher_slot:
        Option<Arc<std::sync::OnceLock<chronik_columnar::hot_vector_batcher::MicroBatcherHandle>>>,


    // Metadata DR (Stage 5)
    metadata_uploader: Option<Arc<chronik_common::metadata::MetadataUploader>>,

    // Follower-pull replication (Stage 16, RP-2.4). Held so the fetcher lives
    // as long as the server rather than being dropped at the end of build().
    replica_fetcher: Option<Arc<crate::replication::replica_fetcher::ReplicaFetcher>>,

    // Partition leader failover (Stage 17, RP-5). Held for the same reason.
    partition_failover: Option<Arc<crate::partition_failover::PartitionFailoverController>>,

    // Publishes the in-sync set that Stage 17 elects from (Stage 18).
    isr_publisher: Option<Arc<crate::isr_publisher::IsrPublisher>>,
    assignment_repair: Option<Arc<crate::assignment_repair::AssignmentRepair>>,

}

impl IntegratedKafkaServerBuilder {
    /// Create a new builder with configuration
    pub fn new(config: IntegratedServerConfig) -> Self {
        Self {
            config,
            cluster_catalog: None,
            catalog_store: None,
            data_dir: None,
            segments_dir: None,
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
            append_notify: Arc::new(dashmap::DashMap::new()),
            wal_produce_handler: None,
            fetch_handler: None,
            isr_ack_tracker: None,
            isr_tracker: None,
            response_pipeline: None,
            kafka_handler: None,
            wal_indexer: None,
            hot_text_index: None,
            hot_vector_index: None,
            hot_vector_batcher_slot: None,
            metadata_uploader: None,
            replica_fetcher: None,
            partition_failover: None,
            isr_publisher: None,
            assignment_repair: None,
        }
    }

    /// Use a running cluster catalog (cluster mode).
    pub fn with_cluster_catalog(mut self, catalog: Arc<crate::cluster_catalog::ClusterCatalog>) -> Self {
        self.cluster_catalog = Some(catalog);
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

    /// Stage 2: Start (or adopt) the cluster catalog.
    ///
    /// Cluster mode hands in a running catalog; single-node mode starts a
    /// one-voter catalog with no network: its raft group commits locally.
    async fn init_cluster_catalog(&mut self) -> Result<()> {
        info!("Stage 2: Initializing the cluster catalog");
        let data_dir = self.data_dir.as_ref()
            .context("data_dir not initialized - call init_directories() first")?;
        if self.cluster_catalog.is_none() {
            let node_id = self.config.node_id as u64;
            let config = crate::cluster_catalog::ClusterCatalogConfig::new(
                node_id,
                data_dir.join("catalog-raft"),
                vec![(node_id, "127.0.0.1:0".to_string())],
                vec![node_id],
            );
            let catalog = crate::cluster_catalog::ClusterCatalog::start(
                config,
                crate::cluster::catalog_setup::catalog_runtime().handle().clone(),
            )
            .context("Failed to start the single-node catalog")?;
            self.cluster_catalog = Some(Arc::new(catalog));
        }
        info!("✅ Cluster catalog ready");
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
        let catalog = self.cluster_catalog.clone().context("cluster catalog not initialized")?;
        let catalog_store = Arc::new(crate::catalog_metadata_store::CatalogMetadataStore::new(
            catalog,
            wal_metadata_store,
        ));
        self.catalog_store = Some(catalog_store.clone());
        self.metadata_store = Some(catalog_store);
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
            MetadataEventPayload::TopicCreated { name, config, auto_created } => {
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
                    // Every field, because followers rebuild the assignment from
                    // exactly these. Dropping one here sets it to its default on
                    // every other node.
                    leader_epoch: assignment.leader_epoch,
                    isr: assignment.isr.clone(),
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
            // A config change replicates by the same route as a creation: the
            // replicator re-reads the topic and ships its current config, so
            // `TopicCreated` is simply "here is this topic's config now", and
            // the apply side treats a newer one as an update.
            //
            // Without this arm `TopicUpdated` fell into the `_ => None` below
            // and was dropped as a "non-replication event" — it had never
            // replicated at all. So every per-topic switch set on a running
            // cluster (`vector.enabled`, `columnar.enabled`, `searchable`,
            // retention) took effect only on the broker that served the
            // request, with the client told it had succeeded.
            MetadataEventPayload::TopicUpdated { name, config } => {
                Some(ServerEvent::TopicCreated {
                    topic: name.clone(),
                    num_partitions: config.partition_count as i32,
                })
            }
            // Security state must be the same on every broker: a credential or
            // rule revoked through one broker has to stop working on all of
            // them. These used to fall into `_ => None` below, so they never
            // left the broker that served the request.
            MetadataEventPayload::AclCreated { .. }
            | MetadataEventPayload::AclDeleted { .. }
            | MetadataEventPayload::ScramCredentialUpserted { .. }
            | MetadataEventPayload::ScramCredentialDeleted { .. } => {
                Some(ServerEvent::SecurityChanged { event: common_event.clone() })
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

    /// Stage 6: Wire catalog changes into this node.
    ///
    /// Every applied catalog change, on every node: local state for deleted
    /// topics is purged, security caches reload, and the metadata event bus
    /// carries the change to the listeners that re-plan on it (the replica
    /// fetcher). Brokers listed in the config are registered once the catalog
    /// has caught up (cluster mode does this from `main`).
    async fn init_catalog_bridge(&mut self) -> Result<()> {
        info!("Stage 6: Wiring catalog changes into this node");
        let catalog = self.cluster_catalog.clone().context("cluster catalog not initialized")?;
        let store = self.catalog_store.clone().context("catalog store not initialized")?;
        let bus = self.metadata_event_bus.clone().context("metadata_event_bus not initialized")?;
        let mut changes = catalog.subscribe();
        tokio::spawn(async move {
            use chronik_common::metadata::catalog::CatalogChange;
            use crate::metadata_events::MetadataEvent as ServerEvent;
            loop {
                let change = match changes.recv().await {
                    Ok(c) => c,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        // Missed changes: tell listeners to re-read everything.
                        warn!("Catalog change listener lagged by {}; re-announcing the catalog", n);
                        CatalogChange::Reset
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                };
                store.handle_change(&change).await;
                let events: Vec<ServerEvent> = {
                    let state = catalog.read();
                    let assignment_event = |topic: &str, partition: u32| {
                        state.assignment(topic, partition).map(|a| ServerEvent::PartitionAssigned {
                            topic: a.topic.clone(),
                            partition: a.partition as i32,
                            replicas: a.replicas.clone(),
                            leader: a.leader_id,
                            leader_epoch: a.leader_epoch,
                            isr: a.isr.clone(),
                        })
                    };
                    let topic_event = |topic: &str| {
                        state.topic(topic).map(|t| ServerEvent::TopicCreated {
                            topic: t.name.clone(),
                            num_partitions: t.config.partition_count as i32,
                        })
                    };
                    match &change {
                        CatalogChange::TopicCreated(t) | CatalogChange::TopicConfigChanged(t) => {
                            let mut v: Vec<ServerEvent> = topic_event(t).into_iter().collect();
                            v.extend(state.assignments_for(t).iter().filter_map(|a| assignment_event(t, a.partition)));
                            v
                        }
                        CatalogChange::PartitionsAdded { topic, from, to } => {
                            let mut v: Vec<ServerEvent> = topic_event(topic).into_iter().collect();
                            v.extend((*from..*to).filter_map(|p| assignment_event(topic, p)));
                            v
                        }
                        CatalogChange::TopicDeleted(t) => vec![ServerEvent::TopicDeleted { topic: t.clone() }],
                        CatalogChange::AssignmentChanged { topic, partition } => {
                            assignment_event(topic, *partition).into_iter().collect()
                        }
                        CatalogChange::BrokersChanged => state
                            .brokers()
                            .map(|b| ServerEvent::BrokerRegistered {
                                broker_id: b.broker_id,
                                host: b.host.clone(),
                                port: b.port,
                                rack: b.rack.clone(),
                            })
                            .collect(),
                        CatalogChange::Reset => state.topics().filter_map(|t| topic_event(&t.name)).collect(),
                        CatalogChange::AclsChanged
                        | CatalogChange::ScramChanged
                        | CatalogChange::NodeAddressChanged(_) => Vec::new(),
                    }
                };
                for e in events {
                    bus.publish(e);
                }
            }
        });
        // First boot after upgrading from the pre-catalog metadata: import what
        // this node's metadata WAL holds, once.
        {
            let catalog = self.cluster_catalog.clone().context("cluster catalog not initialized")?;
            let local = self.wal_metadata_store.clone().context("wal_metadata_store not initialized")?;
            let dir = self.data_dir.as_ref().context("data_dir not initialized")?.join("catalog-raft");
            crate::catalog_import::spawn(catalog, local, dir);
        }

        // Single-node mode has no config listing brokers: register this one, so
        // the catalog can place partitions (it refuses to with no brokers).
        if self.config.cluster_config.is_none() {
            let store = self.catalog_store.clone().context("catalog store not initialized")?;
            let catalog = self.cluster_catalog.clone().context("cluster catalog not initialized")?;
            let broker = chronik_common::metadata::BrokerMetadata {
                broker_id: self.config.node_id,
                host: self.config.advertised_host.clone(),
                port: self.config.advertised_port,
                rack: None,
                status: chronik_common::metadata::BrokerStatus::Online,
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
            };
            if !catalog.wait_raft_caught_up(std::time::Duration::from_secs(30)).await {
                anyhow::bail!("single-node catalog did not elect itself within 30s");
            }
            store.register_broker(broker).await.context("registering this broker in the catalog")?;
        }
        info!("✅ Catalog changes wired");
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
        produce_handler_inner.set_append_notify(Arc::clone(&self.append_notify));
        let isr_ack_tracker = crate::isr_ack_tracker::IsrAckTracker::new();
        let isr_tracker = Arc::new(crate::isr_tracker::IsrTracker::from_env());
        produce_handler_inner.set_isr_ack_tracker(isr_ack_tracker.clone());
        // Reap `acks=all` waits that never reach quorum. The producer's own
        // timeout releases the caller but leaves the registration behind, so
        // without this the map grows for the life of the process — which it did,
        // once per acks=all produce, for as long as replication was silently not
        // running.
        isr_ack_tracker.start_cleanup_task();
        info!("✅ EventBus, IsrAckTracker, and IsrTracker wired");

        // Step 4: Wire Raft dependencies (cluster mode only)
        self.wire_raft_dependencies(&mut produce_handler_inner, &isr_ack_tracker, &isr_tracker, metadata_store);

        // Step 5: Setup response pipeline with WAL callback
        let response_pipeline = self.setup_response_pipeline(&mut produce_handler_inner, wal_manager);

        // Step 5b (HP-1.2): Attach hot text index if enabled
        if let Some(hot_idx) = self.init_hot_text_index() {
            produce_handler_inner.set_hot_text_index(hot_idx.clone());
            self.hot_text_index = Some(hot_idx);
        }

        // Step 5c (HP-2.x): Create the hot vector index + shared batcher-handle
        // slot. The slot is empty for now — main.rs populates it once the
        // embedder has been constructed from env vars.
        if let Some((hot_vec_idx, slot)) = self.init_hot_vector_slot() {
            produce_handler_inner.set_hot_vector_batcher_slot(slot.clone());
            self.hot_vector_index = Some(hot_vec_idx);
            self.hot_vector_batcher_slot = Some(slot);
        }

        // Step 6: Finalize and start background tasks
        let (produce_handler_base, wal_handler) = self.finalize_produce_handler(produce_handler_inner, wal_manager).await;

        // Store all components
        self.produce_handler_base = Some(produce_handler_base);
        self.wal_produce_handler = Some(wal_handler);
        self.isr_ack_tracker = Some(isr_ack_tracker);
        self.isr_tracker = Some(isr_tracker);
        self.response_pipeline = Some(response_pipeline);

        info!("✅ ProduceHandler initialized with all dependencies and wiring");
        Ok(())
    }

    /// HP-1.2: Initialize the hot text index if enabled by env.
    ///
    /// Controlled by `CHRONIK_HOT_TEXT_ENABLED` (default: true). When enabled,
    /// spawns a background commit timer that flips in-memory buffer visibility
    /// on `CHRONIK_HOT_TEXT_COMMIT_INTERVAL_MS` (default: 100ms).
    ///
    /// No disk I/O — Tantivy RAMDirectory only.
    fn init_hot_text_index(
        &self,
    ) -> Option<Arc<chronik_search::hot_text_index::HotTextIndex>> {
        use chronik_search::hot_text_index::{HotTextConfig, HotTextIndex};

        let enabled = std::env::var("CHRONIK_HOT_TEXT_ENABLED")
            .map(|v| v != "false" && v != "0")
            .unwrap_or(true);
        if !enabled {
            info!("HotTextIndex disabled via CHRONIK_HOT_TEXT_ENABLED=false");
            return None;
        }

        let commit_interval_ms = std::env::var("CHRONIK_HOT_TEXT_COMMIT_INTERVAL_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(100);
        let max_docs = std::env::var("CHRONIK_HOT_TEXT_MAX_DOCS")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(100_000);
        let heap_bytes = std::env::var("CHRONIK_HOT_TEXT_WRITER_HEAP_BYTES")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(15_000_000);

        let idx = Arc::new(HotTextIndex::new(HotTextConfig {
            writer_heap_bytes: heap_bytes,
            max_docs_per_partition: max_docs,
        }));

        // Background commit timer — in-memory visibility flip, no disk writes.
        let _handle = Arc::clone(&idx)
            .start_commit_timer(std::time::Duration::from_millis(commit_interval_ms));

        info!(
            "✅ HotTextIndex enabled — commit_interval={}ms, max_docs/partition={}, heap={}B",
            commit_interval_ms, max_docs, heap_bytes
        );
        Some(idx)
    }

    /// HP-2.x: Initialize the hot vector index and shared batcher-handle slot.
    ///
    /// The index itself is created here so it can be wired into the
    /// `ProduceHandler` (for later enqueue via the `OnceLock` slot),
    /// `WalIndexer` (for eviction via the `ColdFlushListener` adapter),
    /// and the unified API (for query-time hot+cold merge). The actual
    /// batcher — which needs an `EmbeddingProvider` — is constructed in
    /// `main.rs` after the embedder is built from env vars; it sets the
    /// `OnceLock` once it has the handle.
    ///
    /// Controlled by `CHRONIK_HOT_VECTOR_ENABLED` (default: true).
    fn init_hot_vector_slot(
        &self,
    ) -> Option<(
        Arc<chronik_columnar::hot_vector_index::HotVectorIndex>,
        Arc<std::sync::OnceLock<chronik_columnar::hot_vector_batcher::MicroBatcherHandle>>,
    )> {
        use chronik_columnar::hot_vector_index::{
            HotDistanceMetric, HotVectorConfig, HotVectorIndex,
        };

        let enabled = std::env::var("CHRONIK_HOT_VECTOR_ENABLED")
            .map(|v| v != "false" && v != "0")
            .unwrap_or(true);
        if !enabled {
            info!("HotVectorIndex disabled via CHRONIK_HOT_VECTOR_ENABLED=false");
            return None;
        }

        let max_vectors = std::env::var("CHRONIK_HOT_VECTOR_MAX_VECTORS")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(50_000);
        let dims = std::env::var("CHRONIK_EMBEDDING_DIMENSIONS")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(1536);
        // Metric is fixed to Cosine for Phase 2 first pass — matches OpenAI
        // text-embedding-3-* and the default VectorIndexConfig.
        let config = HotVectorConfig {
            max_vectors_per_partition: max_vectors,
            dimensions: dims,
            metric: HotDistanceMetric::Cosine,
        };
        let idx = Arc::new(HotVectorIndex::new(config));

        // Background trim timer — keeps RAM bounded independent of eviction.
        let _ = Arc::clone(&idx)
            .start_trim_timer(std::time::Duration::from_millis(250));

        let slot = Arc::new(std::sync::OnceLock::new());
        info!(
            "✅ HotVectorIndex enabled — max_vectors/partition={}, dims={}",
            max_vectors, dims
        );
        Some((idx, slot))
    }

    /// HP-1.5: Populate the hot text index from the WAL tail so it is not
    /// blind for the ~30s between server start and the first cold-path
    /// Tantivy commit.
    ///
    /// Reads up to `CHRONIK_HOT_TEXT_MAX_DOCS` recent WAL records per
    /// (searchable topic, partition) and inserts their entries. No disk I/O
    /// writes — only WAL reads. Errors are logged but non-fatal; startup
    /// continues even if warm-up fails.
    /// RP-3: rebuild each partition's leader-epoch history from the WAL.
    ///
    /// The history is derived from the log, so it lives only in memory and is
    /// empty after a restart — which would make this node answer every
    /// `OffsetForLeaderEpoch` with UNDEFINED and leave a returning replica no
    /// way to find its divergence point. Rebuilding it is therefore part of
    /// coming back up, not an optimisation.
    ///
    /// This scans each partition's WAL. It is bounded (indexed segments are
    /// reclaimed) but not free; Kafka keeps a `leader-epoch-checkpoint` file to
    /// avoid the scan entirely, which is the eventual answer if startup time
    /// becomes a problem.
    async fn warm_up_leader_epochs(&self) -> Result<()> {
        let Some(produce_handler) = self.produce_handler_base.as_ref() else {
            return Ok(());
        };
        let Some(wal_manager) = self.wal_manager.as_ref() else {
            return Ok(());
        };
        let Some(metadata_store) = self.metadata_store.as_ref() else {
            return Ok(());
        };

        use chronik_storage::CanonicalRecord;
        use chronik_wal::record::WalRecord;

        let epochs = produce_handler.leader_epochs();
        let start = std::time::Instant::now();

        // Enumerate from the WAL directory, not from metadata.
        //
        // This used to iterate `list_topics()`, which races metadata recovery:
        // if a topic was not in the catalog yet at this moment, its partition
        // was skipped, the epoch history stayed empty, and the follower then
        // skipped the RP-3.3 handshake entirely — silently, because the
        // completion line only printed when it warmed at least one partition.
        // Observed on a cluster: a returning node warmed nothing and never sent
        // an epoch query. The log on disk is what we are rebuilding from, so it
        // is also the right thing to enumerate.
        let _ = metadata_store; // kept for signature stability; no longer read here
        let mut partitions_warmed = 0usize;
        let mut transitions = 0usize;

        {
            for tp in wal_manager.get_partitions() {
                let (topic_name, partition) = (tp.topic.clone(), tp.partition);
                if topic_name.starts_with("__") {
                    continue; // Raft's own topics carry no partition epochs
                }
                let records = match wal_manager
                    .read_from(&topic_name, partition, 0, usize::MAX)
                    .await
                {
                    Ok(r) if !r.is_empty() => r,
                    _ => continue,
                };
                let before = epochs.snapshot(&topic_name, partition).len();
                let mut unstamped = 0usize;
                for wal_record in &records {
                    if let WalRecord::V2 { canonical_data, base_offset, .. } = wal_record {
                        let canonical: CanonicalRecord = match bincode::deserialize(canonical_data) {
                            Ok(c) => c,
                            Err(_) => continue,
                        };
                        // Batches written before RP-3 carry -1; observe_append
                        // ignores those rather than inventing an epoch 0 at their
                        // offset, so an upgraded cluster simply has no history
                        // until its first stamped append.
                        if canonical.partition_leader_epoch < 0 {
                            unstamped += 1;
                        }
                        epochs.observe_append(
                            &topic_name,
                            partition,
                            canonical.partition_leader_epoch,
                            *base_offset,
                        );
                    }
                }

                let after = epochs.snapshot(&topic_name, partition).len();
                if after > 0 {
                    partitions_warmed += 1;
                    transitions += after - before;
                } else if unstamped > 0 {
                    // Worth naming: a partition whose log carries no epochs
                    // cannot take part in truncation, so it will silently skip
                    // the RP-3.3 handshake for as long as that remains true.
                    debug!(
                        "leader-epoch warm-up: {}-{} has {} unstamped batch(es) and no epoch history",
                        topic_name, partition, unstamped
                    );
                }
            }
        }

        // Logged unconditionally. The previous version printed only when it
        // warmed something, so "warmed nothing" and "never ran" were the same
        // observation — which is how a returning replica silently skipped
        // truncation on a real cluster.
        info!(
            "Leader-epoch history rebuilt for {} partition(s), {} leadership transition(s), in {:?}",
            partitions_warmed, transitions, start.elapsed()
        );
        Ok(())
    }

    async fn warm_up_hot_text_index(&self) -> Result<()> {
        let Some(hot_idx) = self.hot_text_index.as_ref() else {
            return Ok(());
        };
        let wal_manager = self
            .wal_manager
            .as_ref()
            .context("wal_manager not initialized before warm-up")?;
        let metadata_store = self
            .metadata_store
            .as_ref()
            .context("metadata_store not initialized before warm-up")?;

        use chronik_search::hot_text_index::HotDoc;
        use chronik_storage::CanonicalRecord;
        use chronik_wal::record::WalRecord;

        let max_docs = std::env::var("CHRONIK_HOT_TEXT_MAX_DOCS")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(100_000);

        let start = std::time::Instant::now();
        let topics = match metadata_store.list_topics().await {
            Ok(t) => t,
            Err(e) => {
                warn!("warm-up: failed to list topics: {}", e);
                return Ok(());
            }
        };

        let mut total_docs = 0usize;
        let mut partitions_warmed = 0usize;

        for topic_meta in topics {
            if !topic_meta.config.is_searchable() {
                continue;
            }
            let partition_count = topic_meta.config.partition_count as i32;
            for partition in 0..partition_count {
                let max_offset = match wal_manager
                    .get_max_offset_for_partition(&topic_meta.name, partition)
                    .await
                {
                    Ok(Some(m)) if m >= 0 => m,
                    Ok(_) => continue,
                    Err(e) => {
                        debug!(
                            topic = %topic_meta.name, partition,
                            "warm-up: get_max_offset failed: {}", e
                        );
                        continue;
                    }
                };

                // Approximate: read the trailing `max_docs` offset range.
                let start_offset = (max_offset + 1).saturating_sub(max_docs as i64).max(0);
                let records = match wal_manager
                    .read_from(&topic_meta.name, partition, start_offset, max_docs)
                    .await
                {
                    Ok(r) => r,
                    Err(e) => {
                        debug!(
                            topic = %topic_meta.name, partition,
                            "warm-up: read_from failed: {}", e
                        );
                        continue;
                    }
                };

                let mut docs: Vec<HotDoc> = Vec::new();
                for wal_record in &records {
                    match wal_record {
                        WalRecord::V2 { canonical_data, .. } => {
                            let canonical: CanonicalRecord = match bincode::deserialize(canonical_data) {
                                Ok(c) => c,
                                Err(_) => continue,
                            };
                            for entry in &canonical.records {
                                let value_str = entry
                                    .value
                                    .as_deref()
                                    .map(|v| String::from_utf8_lossy(v).into_owned())
                                    .unwrap_or_default();
                                let key_str = entry
                                    .key
                                    .as_deref()
                                    .and_then(|k| String::from_utf8(k.to_vec()).ok());
                                docs.push(HotDoc {
                                    offset: entry.offset,
                                    timestamp: entry.timestamp,
                                    key: key_str,
                                    value: value_str,
                                    headers: None,
                                });
                            }
                        }
                        WalRecord::V1 {
                            offset,
                            timestamp,
                            key,
                            value,
                            ..
                        } => {
                            let key_str = key
                                .as_deref()
                                .and_then(|k| String::from_utf8(k.to_vec()).ok());
                            docs.push(HotDoc {
                                offset: *offset,
                                timestamp: *timestamp,
                                key: key_str,
                                value: String::from_utf8_lossy(value).into_owned(),
                                headers: None,
                            });
                        }
                    }
                }

                // Trim to the last `max_docs` messages — docs are offset-ordered
                // because read_from returns batches in offset order.
                if docs.len() > max_docs {
                    let excess = docs.len() - max_docs;
                    docs.drain(0..excess);
                }

                if docs.is_empty() {
                    continue;
                }
                let doc_count = docs.len();
                if let Err(e) = hot_idx
                    .add_batch(&topic_meta.name, partition, &docs)
                    .await
                {
                    warn!(
                        topic = %topic_meta.name, partition,
                        "warm-up: hot add_batch failed: {}", e
                    );
                    continue;
                }
                if let Err(e) = hot_idx.commit(&topic_meta.name, partition).await {
                    warn!(
                        topic = %topic_meta.name, partition,
                        "warm-up: hot commit failed: {}", e
                    );
                    continue;
                }

                total_docs += doc_count;
                partitions_warmed += 1;
            }
        }

        info!(
            docs = total_docs,
            partitions = partitions_warmed,
            duration_ms = start.elapsed().as_millis() as u64,
            "✅ HotTextIndex warm-up complete (HP-1.5)"
        );
        Ok(())
    }

    /// Helper: Build ProduceHandler configuration
    fn create_produce_handler_config(&self, storage_config: &IngestStorageConfig) -> ProduceHandlerConfig {
        let flush_profile = ProduceFlushProfile::auto_select();
        let compression_type = if self.config.enable_compression {
            chronik_storage::kafka_records::CompressionType::Snappy
        } else {
            chronik_storage::kafka_records::CompressionType::None
        };

        ProduceHandlerConfig {
            node_id: self.config.node_id,
            storage_config: storage_config.clone(),
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
            // RP-1.3: how many ACKs `acks=all` waits for, the leader's included.
            // Single-node has no cluster config and no followers, so 1 is both
            // correct and unchanged behaviour there.
            min_insync_replicas: self
                .config
                .cluster_config
                .as_ref()
                .map(|c| c.min_insync_replicas)
                .unwrap_or(1),
            flush_profile,
        }
    }

    /// Helper: Wire Raft-related dependencies to ProduceHandler
    ///
    /// RP-4: there is no data-push manager to wire any more. Followers fetch
    /// (RP-2.4); the leader does not push. Metadata keeps its own manager from
    /// stage 6 and is unaffected.
    fn wire_raft_dependencies(
        &self,
        produce_handler: &mut crate::produce_handler::ProduceHandler,
        _isr_ack_tracker: &Arc<crate::isr_ack_tracker::IsrAckTracker>,
        _isr_tracker: &Arc<crate::isr_tracker::IsrTracker>,
        _metadata_store: &Arc<dyn MetadataStore>,
    ) {
        if let Some(catalog) = self.cluster_catalog.as_ref() {
            produce_handler.set_raft_cluster(Arc::clone(catalog));
        }
        info!("✅ Pull replication: followers fetch from the leader; nothing is pushed");
    }

    /// Helper: Setup ResponsePipeline with WAL commit callback
    fn setup_response_pipeline(
        &self,
        produce_handler: &mut crate::produce_handler::ProduceHandler,
        wal_manager: &Arc<WalManager>,
    ) -> Arc<crate::response_pipeline::ResponsePipeline> {
        let response_pipeline = Arc::new(crate::response_pipeline::ResponsePipeline::new());

        // Create commit callback for WAL batches.
        // The ResponsePipeline notification is spawned async to avoid blocking
        // the GroupCommitWal worker thread (which caused acks=1 timeouts at scale).
        let response_pipeline_clone = response_pipeline.clone();
        let partition_states = produce_handler.partition_states.clone();
        let callback_handle = tokio::runtime::Handle::current();
        let commit_notify = Arc::clone(&self.append_notify);
        let commit_callback: chronik_wal::group_commit::CommitCallback = Arc::new(
            move |topic: &str, partition: i32, min_offset: i64, max_offset: i64| {
                // A parked follower fetch is waiting for exactly this moment.
                //
                // The produce path signals on append, which is when the record
                // gets its offset — but a follower is served from the durable
                // end (`readable_end_offset`), so waking it before the commit
                // only has it look, find nothing new, and park again. This is
                // the wake that has something behind it.
                if let Some(notify) = commit_notify.get(&(topic.to_string(), partition)) {
                    notify.notify_waiters();
                }

                // Update in-memory high watermark (fast, O(1), stays synchronous)
                let new_watermark = max_offset + 1;
                if let Some(state) = partition_states.get(&(topic.to_string(), partition)) {
                    // Raises the log end with it. On a follower this callback is
                    // the *only* thing that moved the watermark, and it ran
                    // ahead of the replication path that moves the log end —
                    // which then found the watermark already high enough and
                    // skipped. See `PartitionState::raise_watermark`.
                    state.raise_watermark(new_watermark.max(0) as u64);
                    debug!("✅ WAL_CALLBACK: Updated high watermark {}-{} = {}", topic, partition, new_watermark);
                }
                // Notify ResponsePipeline asynchronously to avoid blocking the worker
                let pipeline = response_pipeline_clone.clone();
                let topic = topic.to_string();
                callback_handle.spawn(async move {
                    pipeline.notify_batch_committed(&topic, partition, min_offset, max_offset);
                });
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

        // Leader-epoch history first: the WAL warm-up later adds only what the
        // WAL still holds, and records the indexer archived took their epochs
        // with them. The checkpoint is what remembers those.
        let epoch_dir = PathBuf::from(format!("{}/leader-epochs", self.config.data_dir));
        match produce_handler_base.leader_epochs().enable_checkpoints(epoch_dir.clone()) {
            Ok(loaded) => info!("Leader-epoch checkpoints loaded for {} partition(s) from {:?}", loaded, epoch_dir),
            Err(e) => warn!(
                "Leader-epoch checkpoints unavailable at {:?}: {}. Epoch history will be rebuilt from \
                 the WAL only, and lost for archived records on every restart.",
                epoch_dir, e
            ),
        }

        // CRITICAL FIX (v1.3.52): Clear all partition buffers before WAL recovery
        info!("Clearing all partition buffers before WAL recovery...");
        if let Err(e) = produce_handler_base.clear_all_buffers().await {
            warn!("Failed to clear partition buffers: {}. Recovery may have duplicates.", e);
        }

        // Replay WAL to restore high watermarks after crash
        let recovered_partitions = wal_manager.get_partitions();
        if !recovered_partitions.is_empty() {
            Self::recover_partitions_from_wal(produce_handler_base, wal_manager, &recovered_partitions).await;
            // Rebuild the read_committed transaction index from the WAL tail so an
            // open (produced-but-not-committed) transaction that was interrupted by a
            // crash keeps its records below the Last Stable Offset — i.e. invisible to
            // read_committed — instead of leaking as if committed. Bounded + best-effort.
            Self::rebuild_transaction_index_from_wal(produce_handler_base, wal_manager, &recovered_partitions).await;
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
    /// v2.4.1: Uses get_max_offset_for_partition instead of loading all records with usize::MAX.
    /// This reduces memory from ~1.5GB per partition to ~120MB (reads only last segment file,
    /// skips canonical_data). Critical for large datasets (43.9M+ records) where loading
    /// all WAL records caused OOM during startup.
    async fn recover_partitions_from_wal(
        produce_handler: &Arc<crate::produce_handler::ProduceHandler>,
        wal_manager: &Arc<WalManager>,
        partitions: &[chronik_wal::manager::TopicPartition],
    ) {
        info!("Replaying WAL to restore high watermarks for {} partitions...", partitions.len());

        for tp in partitions {
            match wal_manager.get_max_offset_for_partition(&tp.topic, tp.partition).await {
                Ok(Some(max_offset)) => {
                    let high_watermark = (max_offset + 1) as u64;
                    match produce_handler.restore_partition_state(&tp.topic, tp.partition, high_watermark).await {
                        Ok(_) => info!("Restored {}-{}: high watermark = {}",
                                      tp.topic, tp.partition, high_watermark),
                        Err(e) => warn!("Failed to restore partition state for {}-{}: {}", tp.topic, tp.partition, e),
                    }
                }
                Ok(None) => {} // No records, nothing to restore
                Err(e) => warn!("Failed to read WAL for {}-{}: {}", tp.topic, tp.partition, e),
            }
        }
        info!("WAL replay complete");
    }

    /// Rebuild the per-partition `read_committed` transaction index by replaying the
    /// tail of each partition's WAL.
    ///
    /// The transaction index (open transactions → LSO cap, aborted transactions →
    /// aborted list) is in-memory, so a crash would otherwise reset it to "nothing
    /// open" and a transaction that was produced-but-never-committed before the crash
    /// would leak to `read_committed` consumers as if committed. Replaying the WAL
    /// reconstructs the state deterministically from the log: a transactional data
    /// batch opens a transaction (records its first offset), a COMMIT marker closes
    /// it, an ABORT marker closes and records it. What remains open after replay caps
    /// the LSO exactly as Kafka does.
    ///
    /// Bounded: only the trailing `CHRONIK_TXN_INDEX_REBUILD_WINDOW` offsets (default
    /// 1,000,000) per partition are scanned. Realistic open transactions live at the
    /// very tail, so this recovers them while keeping startup cost proportional. A
    /// transaction left open for longer than the window is not reconstructed (a full
    /// coordinator would have timed it out and written an ABORT); this is a
    /// best-effort safety net, not a substitute for transaction-timeout recovery.
    async fn rebuild_transaction_index_from_wal(
        produce_handler: &Arc<crate::produce_handler::ProduceHandler>,
        wal_manager: &Arc<WalManager>,
        partitions: &[chronik_wal::manager::TopicPartition],
    ) {
        use chronik_wal::record::WalRecord;
        use chronik_storage::CanonicalRecord;

        let window: usize = std::env::var("CHRONIK_TXN_INDEX_REBUILD_WINDOW")
            .ok()
            .and_then(|v| v.parse().ok())
            .filter(|&n| n > 0)
            .unwrap_or(1_000_000);

        let txn_index = produce_handler.transaction_index();
        let mut open_total = 0usize;
        let mut aborted_total = 0usize;

        for tp in partitions {
            let max_offset = match wal_manager.get_max_offset_for_partition(&tp.topic, tp.partition).await {
                Ok(Some(m)) if m >= 0 => m,
                _ => continue,
            };
            let start_offset = (max_offset + 1).saturating_sub(window as i64).max(0);
            let records = match wal_manager.read_from(&tp.topic, tp.partition, start_offset, window).await {
                Ok(r) => r,
                Err(e) => {
                    debug!(topic = %tp.topic, partition = tp.partition,
                           "txn-index rebuild: read_from failed: {}", e);
                    continue;
                }
            };

            for wal_record in &records {
                let WalRecord::V2 { canonical_data, .. } = wal_record else { continue };
                let canonical: CanonicalRecord = match bincode::deserialize(canonical_data) {
                    Ok(c) => c,
                    Err(_) => continue,
                };
                if canonical.producer_id < 0 {
                    continue;
                }
                if canonical.is_control {
                    // Control batch: the single control record's key encodes the
                    // marker type (i16 version, i16 type: 0 = ABORT, 1 = COMMIT).
                    let marker_type = canonical.records.first()
                        .and_then(|r| r.key.as_ref())
                        .filter(|k| k.len() >= 4)
                        .map(|k| i16::from_be_bytes([k[2], k[3]]));
                    match marker_type {
                        Some(1) => txn_index.on_commit(&tp.topic, tp.partition, canonical.producer_id),
                        Some(0) => txn_index.on_abort(&tp.topic, tp.partition, canonical.producer_id),
                        _ => {}
                    }
                } else if canonical.is_transactional {
                    txn_index.on_transactional_produce(
                        &tp.topic, tp.partition, canonical.producer_id, canonical.base_offset,
                    );
                }
            }

            let hwm = (max_offset + 1) as i64;
            let lso = txn_index.last_stable_offset(&tp.topic, tp.partition, hwm);
            if lso < hwm {
                open_total += 1;
                info!("txn-index rebuild: {}-{} has open transaction(s), LSO={} (HWM={})",
                      tp.topic, tp.partition, lso, hwm);
            }
            aborted_total += txn_index.aborted_below(&tp.topic, tp.partition, hwm).len();
        }

        if open_total > 0 || aborted_total > 0 {
            info!("Transaction index rebuilt from WAL: {} partition(s) with open transactions, {} aborted transaction(s) recorded",
                  open_total, aborted_total);
        } else {
            debug!("Transaction index rebuild: no open/aborted transactions found in WAL tail");
        }
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
        let mut fetch_handler = FetchHandler::new_with_wal(
            segment_reader.clone(),
            metadata_store.clone(),
            object_store.clone(),
            wal_manager.clone(),
            produce_handler_base.clone(),
        );

        // RP-2.1: follower fetches report replication progress. Cluster mode only
        // — there is no ISR tracker in single-node, and no followers to report.
        if let Some(ref isr_tracker) = self.isr_tracker {
            fetch_handler.set_isr_tracker(isr_tracker.clone());
        }

        // RP-2.3: under pull, a follower's fetch offset is what releases an
        // `acks=all` producer — there is no ACK frame to do it.
        if let Some(ref isr_ack_tracker) = self.isr_ack_tracker {
            fetch_handler.set_isr_ack_tracker(isr_ack_tracker.clone());
        }
        fetch_handler.set_append_notify(Arc::clone(&self.append_notify));

        // RP-2.3: consumers see only what the in-sync set holds. Unconditional
        // since RP-4 — follower positions are their own fetch offsets, which
        // cannot be stale without the follower having stopped, in which case it
        // leaves ISR and stops constraining the watermark.
        fetch_handler.set_hw_from_isr(true);

        self.fetch_handler = Some(Arc::new(fetch_handler));

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
            segment_pause_ms: std::env::var("CHRONIK_WAL_INDEXER_SEGMENT_PAUSE_MS")
                .ok()
                .and_then(|v| v.trim().parse::<u64>().ok())
                .unwrap_or(0),
            // Reclaiming WAL space after indexing. Off by env for tests that
            // need the WAL to stay put — RP-3.3's divergence test cannot observe
            // a truncation of records the indexer has already archived away.
            delete_after_index: std::env::var("CHRONIK_WAL_DELETE_AFTER_INDEX")
                .map(|v| v != "false" && v != "0")
                .unwrap_or(true),
            object_store: storage_config.object_store_config.clone(),
            // A relative key: the data directory used to be prefixed, which put
            // this node's local path into every archive key (in a bucket too,
            // and nested under the local segments root).
            index_base_path: "tantivy_indexes".to_string(),
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

        // Segment records are this node's own state (its local metadata WAL), so
        // every node persists them; nothing is gated on catalog leadership.
        let is_leader_flag: Option<Arc<std::sync::atomic::AtomicBool>> = None;

        let wal_indexer = Arc::new(WalIndexer::new(
            indexer_config,
            wal_manager,
            object_store.clone(),
            metadata_store.clone(),
            is_leader_flag,
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

    /// Stage 16: Start follower-pull replication (RP-2.4).
    ///
    /// Cluster mode only. This is the sole data-replication mechanism as of
    /// RP-4 — there is no push path left to choose between.
    async fn init_replica_fetcher(&mut self) -> Result<()> {
        let Some(ref cluster_config) = self.config.cluster_config else {
            info!("Stage 16: single-node mode, nothing to replicate");
            return Ok(());
        };

        info!("Stage 16: Starting follower-pull replication");

        let metadata_store = self.metadata_store.as_ref()
            .context("metadata_store not initialized")?;
        let wal_manager = self.wal_manager.as_ref()
            .context("wal_manager not initialized")?;
        let produce_handler = self.produce_handler_base.as_ref()
            .context("produce_handler not initialized")?;

        // A follower fetches over the leader's *Kafka* port, not the WAL
        // replication port — the whole point of pull is that replication is an
        // ordinary Fetch.
        //
        // `replication` overrides which address that Fetch goes to, defaulting
        // to `kafka`. The two cannot be the same field: `kafka` is what the
        // cluster publishes to clients in Metadata responses, so using it for a
        // dedicated replication fabric advertises an address clients cannot
        // reach (RP-10).
        let peers: std::collections::HashMap<u64, String> = cluster_config
            .peers
            .iter()
            .filter(|peer| peer.id != cluster_config.node_id)
            .map(|peer| {
                let addr = peer.replication.clone().unwrap_or_else(|| peer.kafka.clone());
                if peer.replication.is_some() {
                    info!(
                        "Replicating from node {} over {} (clients use {})",
                        peer.id, addr, peer.kafka
                    );
                }
                (peer.id, addr)
            })
            .collect();

        if peers.is_empty() {
            warn!("Pull replication requested but no peers are configured — nothing to fetch from");
            return Ok(());
        }

        let fetcher = crate::replication::replica_fetcher::ReplicaFetcher::new(
            cluster_config.node_id,
            crate::replication::replica_fetcher::ReplicaFetcherConfig::from_env(),
            metadata_store.clone(),
            wal_manager.clone(),
            peers,
        )
        .with_produce_handler(produce_handler.clone());

        // A truncation must cut what the indexer archived, not only the WAL.
        let fetcher = match self.object_store.as_ref() {
            Some(store) => fetcher.with_archive(crate::replication::replica_fetcher::LocalArchive {
                store: store.clone(),
                index: self.wal_indexer.as_ref().map(|indexer| indexer.segment_index().clone()),
            }),
            None => {
                warn!(
                    "No object store: a follower truncation will cut only the WAL, and archived \
                     records above the cut stay servable"
                );
                fetcher
            }
        };

        // Learn about new and reassigned partitions when they happen, not on the
        // next 10s tick. A follower that has not yet heard of a partition cannot
        // acknowledge writes to it, and `acks=all` blocks until it does.
        let fetcher = match self.metadata_event_bus.as_ref() {
            Some(bus) => fetcher.with_metadata_events(bus.clone()),
            None => {
                warn!(
                    "No metadata event bus: the replica fetcher will only notice new partitions \
                     on its {:?} refresh tick, and acks=all writes to a new topic will block until then",
                    crate::replication::replica_fetcher::ReplicaFetcherConfig::from_env().refresh_interval
                );
                fetcher
            }
        };

        fetcher.start();
        self.replica_fetcher = Some(fetcher);

        info!("✅ Follower-pull replication started (node {})", cluster_config.node_id);
        Ok(())
    }

    /// Stage 17: partition leader failover (RP-5).
    ///
    /// Independent of the replication mode. A partition whose leader dies must
    /// move to a live replica under push and pull alike — the measured failure
    /// that motivated this reproduced identically under both.
    ///
    /// Every node runs the controller; it acts only while it is the Raft
    /// leader, which is what stops two nodes handing the same partition to
    /// different replicas.
    async fn init_partition_failover(&mut self) -> Result<()> {
        let Some(ref cluster_config) = self.config.cluster_config else {
            debug!("Stage 17: single-node mode, no partition failover");
            return Ok(());
        };

        let Some(raft) = self.cluster_catalog.as_ref() else {
            warn!(
                "Stage 17: cluster mode without a cluster catalog — partition leader failover is \
                 DISABLED. A partition whose leader dies will stay unavailable."
            );
            return Ok(());
        };

        let metadata_store = self.metadata_store.as_ref()
            .context("metadata_store not initialized")?;

        let controller = crate::partition_failover::PartitionFailoverController::new(
            cluster_config.node_id,
            raft.clone(),
            metadata_store.clone(),
        );
        controller.start();
        self.partition_failover = Some(controller);

        info!("✅ Partition leader failover active (node {})", cluster_config.node_id);
        Ok(())
    }

    /// Stage 18: publish the in-sync set (RP-5 fix).
    ///
    /// Failover elects from what this writes. Without it the in-sync set exists
    /// only in the partition leader's memory, so it vanishes at the moment it is
    /// needed — and failover falls back to electing whoever is merely reachable,
    /// which promoted a replica holding none of the partition and destroyed
    /// `acks=all`-acknowledged records.
    async fn init_isr_publisher(&mut self) -> Result<()> {
        let Some(ref cluster_config) = self.config.cluster_config else {
            debug!("Stage 18: single-node mode, nothing to keep in sync");
            return Ok(());
        };

        let metadata_store = self.metadata_store.as_ref()
            .context("metadata_store not initialized")?;
        let produce_handler = self.produce_handler_base.as_ref()
            .context("produce_handler not initialized")?;

        let Some(isr_tracker) = self.isr_tracker.as_ref() else {
            warn!(
                "Stage 18: no ISR tracker — the in-sync set will not be published, so a failover \
                 cannot tell a caught-up replica from a reachable one."
            );
            return Ok(());
        };

        let publisher = crate::isr_publisher::IsrPublisher::new(
            cluster_config.node_id,
            metadata_store.clone(),
            isr_tracker.clone(),
            produce_handler.clone(),
        );
        publisher.start();
        self.isr_publisher = Some(publisher);

        info!("✅ In-sync set publication active (node {})", cluster_config.node_id);
        Ok(())
    }

    /// Purge what this node holds for a topic once its deletion is applied
    /// here — on every node, not only the one that served DeleteTopics.
    ///
    /// The archive matters most: the fetch tier serves it, and the watermark
    /// is read from it, so a topic recreated under the same name would start
    /// at the old topic's end and serve the old topic's records. Before this,
    /// nothing removed it at all; only the WAL was cleaned, and on other nodes
    /// only after several indexer passes.
    fn init_topic_deletion_cleanup(&self) {
        let (Some(metadata), Some(store), Some(wal_manager), Some(produce_handler)) = (
            self.wal_metadata_store.clone(),
            self.object_store.clone(),
            self.wal_manager.clone(),
            self.produce_handler_base.clone(),
        ) else {
            warn!("Topic deletion cleanup not wired: a deleted topic's archive will outlive it");
            return;
        };
        let index = self.wal_indexer.as_ref().map(|indexer| indexer.segment_index().clone());
        let runtime = tokio::runtime::Handle::current();

        metadata.on_topic_deleted(Arc::new(move |topic: String| {
            let (store, index, wal_manager, produce_handler) =
                (store.clone(), index.clone(), wal_manager.clone(), produce_handler.clone());
            runtime.spawn(async move {
                produce_handler.forget_topic(&topic);
                wal_manager.cleanup_topic(&topic).await;

                let partitions = match chronik_storage::archive::archived_partitions(
                    store.as_ref(),
                    index.as_ref(),
                    &topic,
                )
                .await
                {
                    Ok(partitions) => partitions,
                    Err(e) => {
                        warn!("Deleted topic {}: cannot list its archive to remove it: {}", topic, e);
                        return;
                    }
                };
                let mut removed = 0usize;
                for partition in &partitions {
                    // The indexer holds this while it archives; without it a
                    // pass in flight re-archives what this removes.
                    let lock = wal_manager.archive_lock(&topic, *partition);
                    let _archiving = lock.lock().await;
                    match chronik_storage::archive::truncate_archive(
                        store.as_ref(),
                        index.as_ref(),
                        &topic,
                        *partition,
                        0,
                    )
                    .await
                    {
                        Ok(cut) => removed += cut.removed,
                        Err(e) => warn!(
                            "Deleted topic {}: could not remove the archive of partition {}: {}",
                            topic, partition, e
                        ),
                    }
                }
                info!(
                    "Deleted topic {}: removed its WAL, local state and {} archived segment(s) across {} partition(s)",
                    topic, removed, partitions.len()
                );
            });
        }));
        info!("✅ Topic deletion cleanup wired (archive, WAL, offsets, epochs)");
    }

    /// Stage 19: repair partitions the catalog assigns to nobody.
    ///
    /// See `assignment_repair`: a partition without a leader is unreachable
    /// and can never be written, and the node that holds its records must be
    /// the one to lead it.
    async fn init_assignment_repair(&mut self) -> Result<()> {
        let Some(ref cluster_config) = self.config.cluster_config else {
            debug!("Stage 19: single-node mode, every partition is led here");
            return Ok(());
        };
        let Some(raft) = self.cluster_catalog.as_ref() else {
            warn!("Stage 19: cluster mode without a cluster catalog — unassigned partitions will not be repaired");
            return Ok(());
        };
        let metadata_store = self.metadata_store.as_ref()
            .context("metadata_store not initialized")?;
        let produce_handler = self.produce_handler_base.as_ref()
            .context("produce_handler not initialized")?;

        let repair = crate::assignment_repair::AssignmentRepair::new(
            cluster_config.node_id,
            cluster_config.peers.iter().map(|peer| peer.id).collect(),
            cluster_config.replication_factor,
            metadata_store.clone(),
            produce_handler.clone() as Arc<dyn crate::assignment_repair::LocalLog>,
            raft.is_leader_flag(),
        );
        repair.start();
        self.assignment_repair = Some(repair);

        info!("✅ Assignment repair active (node {})", cluster_config.node_id);
        Ok(())
    }

    /// Build the IntegratedKafkaServer
    ///
    /// This orchestrates all initialization stages in order.
    /// Complexity: < 25 (orchestration only, delegates to stage functions)
    pub async fn build(mut self) -> Result<IntegratedKafkaServer> {
        info!("🔧 Starting IntegratedKafkaServer build process (17 stages)...");

        // Stage 1: Directories
        self.init_directories().await
            .context("Stage 1 failed: directory initialization")?;

        // Stage 2: Raft cluster
        self.init_cluster_catalog().await
            .context("Stage 2 failed: cluster catalog")?;

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
        self.init_catalog_bridge().await
            .context("Stage 6 failed: catalog change wiring")?;

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

        // A deleted topic leaves nothing behind on this node
        self.init_topic_deletion_cleanup();

        // Stage 14: MetadataUploader
        self.init_metadata_uploader().await
            .context("Stage 14 failed: MetadataUploader initialization")?;

        // RP-3: the leader-epoch history is derived from the log and therefore
        // empty after a restart. Rebuild it before serving, or this node answers
        // every OffsetForLeaderEpoch with UNDEFINED.
        //
        // ⚠️ This MUST run before the replica fetcher starts (stage 16). It used
        // to run after all stages, and the ordering silently disabled RP-3.3: on
        // a real cluster the fetcher logged "Replicating 1 partition(s)" 553µs
        // *before* the warm-up finished, so its first reconcile pass read an
        // empty epoch store, found nothing to ask about, skipped the handshake
        // and cleared its reconcile flag. The history then arrived too late to
        // matter. A returning replica would never truncate.
        if let Err(e) = self.warm_up_leader_epochs().await {
            warn!("Leader-epoch warm-up failed (continuing): {}", e);
        }

        // Stage 16: ReplicaFetcher (cluster mode, pull replication only)
        self.init_replica_fetcher().await
            .context("Stage 16 failed: ReplicaFetcher initialization")?;

        // Stage 17: partition leader failover (cluster mode)
        self.init_partition_failover().await
            .context("Stage 17 failed: partition failover controller")?;

        // Stage 18: publish the in-sync set that Stage 17 elects from
        self.init_isr_publisher().await
            .context("Stage 18 failed: ISR publisher")?;

        // Stage 19: give every partition a leader
        self.init_assignment_repair().await
            .context("Stage 19 failed: assignment repair")?;

        info!("✅ All 17 stages complete - assembling IntegratedKafkaServer");

        // HP-1.5: warm up the hot text index from WAL tail so queries are
        // not blind for the first ~30s after startup.
        if let Err(e) = self.warm_up_hot_text_index().await {
            warn!("Hot text index warm-up failed (continuing): {}", e);
        }

        // The catalog applies its log while the stages above run, before every
        // listener has subscribed (the ACL cache subscribes in Stage 12), so
        // changes applied during boot are announced once more as a reset when
        // the catalog has caught up.
        if let (Some(store), Some(catalog)) = (self.catalog_store.clone(), self.cluster_catalog.clone()) {
            tokio::spawn(async move {
                while !catalog.wait_caught_up(std::time::Duration::from_secs(30)).await {}
                store.handle_change(&chronik_common::metadata::catalog::CatalogChange::Reset).await;
            });
        }

        // Create the final server instance using the new_from_components constructor
        let kafka_handler = self.kafka_handler.unwrap();
        let metadata_store = self.metadata_store.unwrap();
        let wal_indexer = self.wal_indexer.unwrap();
        let metadata_uploader = self.metadata_uploader;
        let isr_tracker = self.isr_tracker;
        let hot_text_index = self.hot_text_index;
        let hot_vector_index = self.hot_vector_index;
        let hot_vector_batcher_slot = self.hot_vector_batcher_slot;

        let server = IntegratedKafkaServer::new_from_components(
            self.config,
            kafka_handler,
            metadata_store,
            wal_indexer,
            metadata_uploader,
            isr_tracker,
            hot_text_index,
            hot_vector_index,
            hot_vector_batcher_slot,
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
    async fn test_builder_init_catalog_single_node() {
        let temp_dir = TempDir::new().unwrap();
        let config = IntegratedServerConfig {
            node_id: 1,
            data_dir: temp_dir.path().to_string_lossy().to_string(),
            ..Default::default()
        };

        let mut builder = IntegratedKafkaServerBuilder::new(config);
        builder.init_directories().await.unwrap();
        builder.init_cluster_catalog().await.unwrap();

        // Verify Raft cluster created
        assert!(builder.cluster_catalog.is_some());
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
        builder.init_cluster_catalog().await.unwrap();
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
        builder.init_cluster_catalog().await.unwrap();
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
        builder.init_cluster_catalog().await.unwrap();
        builder.init_metadata_wal().await.unwrap();
        builder.init_event_bus().await.unwrap();
        builder.init_metadata_store().await.unwrap();

        // Metadata replication creates background tasks but doesn't store state
        // Just verify it completes without error
        builder.init_catalog_bridge().await.unwrap();
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
        builder.init_cluster_catalog().await.unwrap();
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
        builder.init_cluster_catalog().await.unwrap();
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
        builder.init_cluster_catalog().await.unwrap();
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_builder_init_wal_indexer() {
        let temp_dir = TempDir::new().unwrap();
        let config = IntegratedServerConfig {
            data_dir: temp_dir.path().to_string_lossy().to_string(),
            ..Default::default()
        };

        let mut builder = IntegratedKafkaServerBuilder::new(config);
        builder.init_directories().await.unwrap();
        builder.init_cluster_catalog().await.unwrap();
        builder.init_metadata_wal().await.unwrap();
        builder.init_event_bus().await.unwrap();
        builder.init_metadata_store().await.unwrap();
        builder.init_storage().await.unwrap();
        builder.init_wal_manager().await.unwrap();
        builder.init_produce_handler().await.unwrap();
        builder.init_wal_indexer().await.unwrap();

        // Verify WalIndexer created
        assert!(builder.wal_indexer.is_some());

        // Drop builder in blocking context (WalIndexer has its own tokio runtime)
        tokio::task::spawn_blocking(move || drop(builder)).await.unwrap();
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
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

        // Drop server in blocking context (WalIndexer has its own tokio runtime)
        tokio::task::spawn_blocking(move || drop(server)).await.unwrap();
    }

    /// Security events used to fall into the adapter's `_ => None` and never
    /// reach the replicator. Each must come out the other side, whole.
    #[test]
    fn security_events_are_forwarded_to_the_replicator() {
        use chronik_common::metadata::{
            AclBindingRecord, MetadataEvent as CommonEvent, MetadataEventPayload, ScramCredentialRecord,
        };
        use crate::metadata_events::{MetadataEvent as ServerEvent, MetadataEventBus};

        let binding = AclBindingRecord {
            resource_type: 2,
            resource_name: "orders".to_string(),
            pattern_type: 3,
            principal: "User:alice".to_string(),
            host: "*".to_string(),
            operation: 4,
            permission_type: 3,
        };
        let credential = ScramCredentialRecord {
            username: "alice".to_string(),
            mechanism: 2,
            iterations: 4096,
            salt: vec![1; 16],
            stored_key: vec![2; 64],
            server_key: vec![3; 64],
        };
        let payloads = vec![
            MetadataEventPayload::AclCreated { binding: binding.clone() },
            MetadataEventPayload::AclDeleted { binding },
            MetadataEventPayload::ScramCredentialUpserted { credential },
            MetadataEventPayload::ScramCredentialDeleted { username: "alice".to_string(), mechanism: 2 },
        ];

        let bus = Arc::new(MetadataEventBus::new(16));
        let mut rx = bus.subscribe();
        for payload in payloads {
            let sent = CommonEvent::new_with_node(payload, 1);
            let delivered =
                IntegratedKafkaServerBuilder::convert_and_publish_metadata_event(&bus, sent.clone());
            assert_eq!(delivered, 1, "dropped: {:?}", sent.payload);
            match rx.try_recv().unwrap() {
                ServerEvent::SecurityChanged { event } => {
                    assert_eq!(event.to_bytes().unwrap(), sent.to_bytes().unwrap());
                }
                other => panic!("unexpected event: {:?}", other),
            }
        }
    }
}
