use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use chronik_common::{Result, Error};
use chronik_common::metadata::traits::MetadataStore;
use tracing::{info, debug, warn};

/// Transaction state
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransactionState {
    Empty,              // Initial state
    Ongoing,           // Transaction is ongoing
    PrepareCommit,     // Preparing to commit
    PrepareAbort,      // Preparing to abort
    CompleteCommit,    // Transaction committed
    CompleteAbort,     // Transaction aborted
    Dead,              // Transaction expired/fenced
}

/// Transaction metadata
#[derive(Debug, Clone)]
pub struct TransactionMetadata {
    pub transactional_id: String,
    pub producer_id: i64,
    pub producer_epoch: i16,
    pub state: TransactionState,
    pub timeout_ms: i32,
    pub started_at: Instant,
    pub last_update: Instant,
    pub partitions: HashSet<(String, i32)>, // (topic, partition)
    pub consumer_group_id: Option<String>,
    pub pending_offsets: HashMap<String, Vec<(i32, i64, Option<String>)>>, // group_id -> [(partition, offset, metadata)]
    pub last_sequence_number: HashMap<(String, i32), i32>, // (topic, partition) -> last_seq_num for idempotence
}

/// Transaction coordinator manages distributed transactions
pub struct TransactionCoordinator {
    /// Active transactions by transactional ID
    transactions: Arc<RwLock<HashMap<String, TransactionMetadata>>>,
    /// Producer ID to transactional ID mapping
    producer_to_txn: Arc<RwLock<HashMap<i64, String>>>,
    /// Next producer ID
    next_producer_id: AtomicI64,
    /// Next producer epoch
    next_producer_epoch: AtomicI64,
    /// Default transaction timeout
    default_timeout: Duration,
    /// Metadata store for persistence
    metadata_store: Option<Arc<dyn MetadataStore>>,
}

impl TransactionCoordinator {
    pub fn new(metadata_store: Option<Arc<dyn MetadataStore>>) -> Self {
        Self {
            transactions: Arc::new(RwLock::new(HashMap::new())),
            producer_to_txn: Arc::new(RwLock::new(HashMap::new())),
            next_producer_id: AtomicI64::new(1000),
            next_producer_epoch: AtomicI64::new(0),
            default_timeout: Duration::from_secs(60),
            metadata_store,
        }
    }

    /// Initialize a new producer ID
    pub async fn init_producer_id(
        &self,
        transactional_id: Option<String>,
        transaction_timeout_ms: i32,
    ) -> Result<(i64, i16)> {
        let producer_id = self.next_producer_id.fetch_add(1, Ordering::SeqCst);
        let producer_epoch = if transactional_id.is_some() {
            self.next_producer_epoch.fetch_add(1, Ordering::SeqCst) as i16
        } else {
            0
        };

        if let Some(txn_id) = transactional_id {
            let metadata = TransactionMetadata {
                transactional_id: txn_id.clone(),
                producer_id,
                producer_epoch,
                state: TransactionState::Empty,
                timeout_ms: transaction_timeout_ms,
                started_at: Instant::now(),
                last_update: Instant::now(),
                partitions: HashSet::new(),
                consumer_group_id: None,
                pending_offsets: HashMap::new(),
                last_sequence_number: HashMap::new(),
            };

            let mut transactions = self.transactions.write().await;
            let mut producer_map = self.producer_to_txn.write().await;

            // Check for existing transaction
            if let Some(existing) = transactions.get(&txn_id) {
                // Fence out old producer
                if existing.producer_id != producer_id {
                    info!(
                        "Fencing out old producer {} for transaction {}",
                        existing.producer_id, txn_id
                    );
                    producer_map.remove(&existing.producer_id);
                }
            }

            let timeout_ms = metadata.timeout_ms;
            transactions.insert(txn_id.clone(), metadata);
            producer_map.insert(producer_id, txn_id.clone());

            // Persist to metadata store
            if let Some(ref metadata_store) = self.metadata_store {
                if let Err(e) = metadata_store.begin_transaction(
                    txn_id.clone(),
                    producer_id,
                    producer_epoch,
                    timeout_ms,
                ).await {
                    tracing::warn!("Failed to persist transaction to metadata store: {}", e);
                }
            }
        }

        Ok((producer_id, producer_epoch))
    }

    /// Add partitions to a transaction
    pub async fn add_partitions_to_txn(
        &self,
        transactional_id: String,
        producer_id: i64,
        producer_epoch: i16,
        partitions: Vec<(String, i32)>,
    ) -> Result<()> {
        let mut transactions = self.transactions.write().await;

        let metadata = transactions.get_mut(&transactional_id)
            .ok_or_else(|| Error::Internal(format!("Transaction {} not found", transactional_id)))?;

        // Verify producer ID and epoch
        if metadata.producer_id != producer_id {
            return Err(Error::Internal("Invalid producer ID".to_string()));
        }
        if metadata.producer_epoch != producer_epoch {
            return Err(Error::Internal("Producer fenced".to_string()));
        }

        // Update state if needed
        if metadata.state == TransactionState::Empty {
            metadata.state = TransactionState::Ongoing;
        }

        // Add partitions
        for partition in &partitions {
            metadata.partitions.insert(partition.clone());
        }
        metadata.last_update = Instant::now();

        // Mark partitions as part of the transaction in the log
        tracing::info!("Added {} partitions to transaction {}", partitions.len(), transactional_id);

        // Persist to metadata store
        if let Some(ref metadata_store) = self.metadata_store {
            let partitions_u32: Vec<(String, u32)> = partitions.iter()
                .map(|(topic, partition)| (topic.clone(), *partition as u32))
                .collect();

            if let Err(e) = metadata_store.add_partitions_to_transaction(
                transactional_id.clone(),
                producer_id,
                producer_epoch,
                partitions_u32,
            ).await {
                tracing::warn!("Failed to persist partition addition to metadata store: {}", e);
            }
        }

        Ok(())
    }

    /// Add consumer group offsets to transaction
    pub async fn add_offsets_to_txn(
        &self,
        transactional_id: String,
        producer_id: i64,
        producer_epoch: i16,
        consumer_group_id: String,
    ) -> Result<()> {
        let mut transactions = self.transactions.write().await;

        let metadata = transactions.get_mut(&transactional_id)
            .ok_or_else(|| Error::Internal(format!("Transaction {} not found", transactional_id)))?;

        // Verify producer ID and epoch
        if metadata.producer_id != producer_id {
            return Err(Error::Internal("Invalid producer ID".to_string()));
        }
        if metadata.producer_epoch != producer_epoch {
            return Err(Error::Internal("Producer fenced".to_string()));
        }

        // Set consumer group
        metadata.consumer_group_id = Some(consumer_group_id.clone());
        metadata.last_update = Instant::now();

        // Persist to metadata store
        if let Some(ref metadata_store) = self.metadata_store {
            if let Err(e) = metadata_store.add_offsets_to_transaction(
                transactional_id.clone(),
                producer_id,
                producer_epoch,
                consumer_group_id.clone(),
            ).await {
                tracing::warn!("Failed to persist consumer group addition to metadata store: {}", e);
            }
        }

        Ok(())
    }

    /// End a transaction (commit or abort)
    pub async fn end_txn(
        &self,
        transactional_id: String,
        producer_id: i64,
        producer_epoch: i16,
        committed: bool,
    ) -> Result<()> {
        let mut transactions = self.transactions.write().await;

        let metadata = transactions.get_mut(&transactional_id)
            .ok_or_else(|| Error::Internal(format!("Transaction {} not found", transactional_id)))?;

        // Verify producer ID and epoch
        if metadata.producer_id != producer_id {
            return Err(Error::Internal("Invalid producer ID".to_string()));
        }
        if metadata.producer_epoch != producer_epoch {
            return Err(Error::Internal("Producer fenced".to_string()));
        }

        // Update state
        metadata.state = if committed {
            TransactionState::PrepareCommit
        } else {
            TransactionState::PrepareAbort
        };
        metadata.last_update = Instant::now();

        // TODO: Coordinate with partition leaders to commit/abort
        // For now, immediately complete
        metadata.state = if committed {
            TransactionState::CompleteCommit
        } else {
            TransactionState::CompleteAbort
        };

        // Persist transaction completion to metadata store
        if let Some(ref metadata_store) = self.metadata_store {
            let result = if committed {
                metadata_store.commit_transaction(
                    transactional_id.clone(),
                    producer_id,
                    producer_epoch,
                ).await
            } else {
                metadata_store.abort_transaction(
                    transactional_id.clone(),
                    producer_id,
                    producer_epoch,
                ).await
            };

            if let Err(e) = result {
                tracing::warn!("Failed to persist transaction completion to metadata store: {}", e);
            }
        }

        // Send markers to partition leaders and handle pending offsets
        if committed {
            // Commit pending offsets if any
            if let Some(ref group_id) = metadata.consumer_group_id {
                if let Some(pending) = metadata.pending_offsets.get(group_id) {
                    if let Some(ref metadata_store) = self.metadata_store {
                        let offsets: Vec<(String, u32, i64, Option<String>)> = pending.iter()
                            .flat_map(|(partition, offset, meta)| {
                                metadata.partitions.iter()
                                    .filter_map(|(topic, p)| {
                                        if *p == *partition {
                                            Some((topic.clone(), *p as u32, *offset, meta.clone()))
                                        } else {
                                            None
                                        }
                                    })
                            })
                            .collect();

                        if let Err(e) = metadata_store.commit_transactional_offsets(
                            transactional_id.clone(),
                            producer_id,
                            producer_epoch,
                            group_id.clone(),
                            offsets,
                        ).await {
                            tracing::warn!("Failed to commit transactional offsets: {}", e);
                        }
                    }
                }
            }
        }

        // TODO: Send transaction markers to partition leaders

        Ok(())
    }

    /// Commit offsets as part of a transaction
    pub async fn txn_offset_commit(
        &self,
        transactional_id: String,
        consumer_group_id: String,
        producer_id: i64,
        producer_epoch: i16,
        offsets: Vec<(String, i32, i64)>, // (topic, partition, offset)
    ) -> Result<()> {
        let transactions = self.transactions.read().await;

        let metadata = transactions.get(&transactional_id)
            .ok_or_else(|| Error::Internal(format!("Transaction {} not found", transactional_id)))?;

        // Verify producer ID and epoch
        if metadata.producer_id != producer_id {
            return Err(Error::Internal("Invalid producer ID".to_string()));
        }
        if metadata.producer_epoch != producer_epoch {
            return Err(Error::Internal("Producer fenced".to_string()));
        }

        // Verify transaction state allows offset commits
        if metadata.state != TransactionState::Ongoing {
            return Err(Error::Internal("Transaction not in ongoing state".to_string()));
        }

        // Store pending offsets that will be committed when transaction completes
        let mut transactions = self.transactions.write().await;

        let metadata = transactions.get_mut(&transactional_id)
            .ok_or_else(|| Error::Internal(format!("Transaction {} not found", transactional_id)))?;

        // Store offsets grouped by partition
        let offset_entries: Vec<(i32, i64, Option<String>)> = offsets.iter()
            .map(|(topic, partition, offset)| (*partition, *offset, None))
            .collect();

        metadata.pending_offsets.insert(consumer_group_id.clone(), offset_entries);

        info!(
            "Transactional offset commit for txn: {}, group: {}, offsets: {:?}",
            transactional_id, consumer_group_id, offsets
        );

        Ok(())
    }

    /// Check for expired transactions
    pub async fn check_timeouts(&self) {
        let mut transactions = self.transactions.write().await;
        let mut producer_map = self.producer_to_txn.write().await;
        let now = Instant::now();
        let mut expired = Vec::new();

        for (txn_id, metadata) in transactions.iter() {
            let timeout = Duration::from_millis(metadata.timeout_ms as u64);
            if now.duration_since(metadata.last_update) > timeout {
                expired.push((txn_id.clone(), metadata.producer_id));
            }
        }

        for (txn_id, producer_id) in expired {
            if let Some(mut metadata) = transactions.remove(&txn_id) {
                info!("Transaction {} timed out, aborting", txn_id);
                metadata.state = TransactionState::CompleteAbort;

                // Clean up producer mapping
                producer_map.remove(&producer_id);

                // Persist abort to metadata store
                if let Some(ref metadata_store) = self.metadata_store {
                    if let Err(e) = metadata_store.abort_transaction(
                        txn_id.clone(),
                        metadata.producer_id,
                        metadata.producer_epoch,
                    ).await {
                        tracing::warn!("Failed to persist transaction abort: {}", e);
                    }
                }

                // TODO: Send abort markers to partitions
            }
        }
    }

    /// Get transaction state
    pub async fn get_transaction_state(&self, transactional_id: &str) -> Option<TransactionState> {
        let transactions = self.transactions.read().await;
        transactions.get(transactional_id).map(|m| m.state.clone())
    }

    /// Start background task for checking timeouts
    pub fn start_timeout_checker(self: Arc<Self>) {
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(5));
            loop {
                interval.tick().await;
                self.check_timeouts().await;
            }
        });
    }

    /// Load transactions from metadata store on recovery
    pub async fn recover_from_metadata_store(&self) -> Result<()> {
        // For now, we don't persist full transaction state to the metadata store
        // In a production system, we would:
        // 1. Load all active transactions from the transaction log
        // 2. Restore their state and partitions
        // 3. Resume or abort incomplete transactions

        info!("Transaction coordinator recovery complete");
        Ok(())
    }

    /// Write transaction markers to partition leaders
    async fn write_transaction_markers(
        &self,
        transactional_id: &str,
        producer_id: i64,
        producer_epoch: i16,
        committed: bool,
        partitions: &HashSet<(String, i32)>,
    ) -> Result<()> {
        // Transaction markers are special records written to each partition
        // involved in the transaction to mark commit/abort

        for (topic, partition) in partitions {
            tracing::debug!(
                "Writing {} marker for transaction {} to {}-{}",
                if committed { "COMMIT" } else { "ABORT" },
                transactional_id,
                topic,
                partition
            );

            // TODO: Actually write markers to the partition logs
            // This requires coordination with the produce handler
        }

        Ok(())
    }

    /// Get all active transactions (for monitoring)
    pub async fn list_active_transactions(&self) -> Vec<(String, TransactionState)> {
        let transactions = self.transactions.read().await;
        transactions.iter()
            .map(|(id, meta)| (id.clone(), meta.state.clone()))
            .collect()
    }
}