//! WAL manager for coordinating segments and partitions

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;
use dashmap::DashMap;
use parking_lot::RwLock;
use tracing::{info, debug, instrument};

use crate::{
    config::WalConfig,
    error::{Result, WalError},
    record::WalRecord,
    segment::{WalSegment, SealedSegment},
    checkpoint::CheckpointManager,
    fsync::FsyncBatcher,
    RecoveryResult,
};

/// Topic-partition identifier
#[derive(Debug, Clone, Hash, Eq, PartialEq)]
pub struct TopicPartition {
    pub topic: String,
    pub partition: i32,
}

/// WAL manager coordinating all partitions
pub struct WalManager {
    config: WalConfig,
    partitions: Arc<DashMap<TopicPartition, PartitionWal>>,
    checkpoint_manager: CheckpointManager,
    fsync_batcher: FsyncBatcher,
}

/// Per-partition WAL state
struct PartitionWal {
    topic: String,
    partition: i32,
    active_segment: Arc<RwLock<WalSegment>>,
    sealed_segments: Vec<SealedSegment>,
    next_segment_id: u64,
    data_dir: PathBuf,
}

impl WalManager {
    /// Create a new WAL manager
    #[instrument(skip(config), fields(data_dir = %config.data_dir.display()))]
    pub async fn new(config: WalConfig) -> Result<Self> {
        // Create data directory if it doesn't exist
        tokio::fs::create_dir_all(&config.data_dir).await?;
        
        let checkpoint_manager = CheckpointManager::new(
            config.data_dir.clone(),
            config.checkpointing.clone(),
        ).await?;
        
        let fsync_batcher = FsyncBatcher::new(config.fsync.clone());
        
        info!("WAL manager initialized successfully");
        
        Ok(Self {
            config,
            partitions: Arc::new(DashMap::new()),
            checkpoint_manager,
            fsync_batcher,
        })
    }
    
    /// Recover WAL from disk
    #[instrument(skip(config), fields(data_dir = %config.data_dir.display()))]
    pub async fn recover(config: &WalConfig) -> Result<Self> {
        info!("Starting WAL recovery from {:?}", config.data_dir);
        
        let manager = Self::new(config.clone()).await?;
        
        // TODO: Scan data directory and recover segments
        // For now, return empty manager
        
        Ok(manager)
    }
    
    /// Append records to a partition
    #[instrument(skip(self, records), fields(
        topic = %topic,
        partition = partition,
        record_count = records.len()
    ))]
    pub async fn append(
        &mut self,
        topic: String,
        partition: i32,
        records: Vec<WalRecord>,
    ) -> Result<()> {
        let tp = TopicPartition { topic: topic.clone(), partition };
        
        // Get or create partition WAL
        if !self.partitions.contains_key(&tp) {
            debug!("Creating new partition WAL");
            self.create_partition_wal(topic.clone(), partition).await?;
        }
        
        let partition_wal = self.partitions.get(&tp)
            .ok_or_else(|| WalError::SegmentNotFound(format!("{}-{}", topic, partition)))?;
        
        // Append records to active segment
        let mut active = partition_wal.active_segment.write();
        
        for record in records {
            // Check if rotation is needed
            if active.should_rotate(
                self.config.rotation.max_segment_size,
                self.config.rotation.max_segment_age_ms,
            ) {
                debug!(
                    segment_id = active.id,
                    segment_size = active.size,
                    "WAL segment rotation triggered"
                );
                drop(active);
                self.rotate_segment(&tp).await?;
                active = partition_wal.active_segment.write();
            }
            
            // Release lock before async operation
            {
                active.append(record).await?;
            }
        }
        
        // Update checkpoint if needed
        if self.config.checkpointing.enabled {
            self.checkpoint_manager.maybe_checkpoint(
                &topic,
                partition,
                active.last_offset,
                active.record_count,
            ).await?;
        }
        
        debug!(
            last_offset = active.last_offset,
            total_records = active.record_count,
            "WAL append completed successfully"
        );
        
        Ok(())
    }
    
    /// Read records from a specific offset
    #[instrument(skip(self), fields(
        topic = topic,
        partition = partition,
        offset = offset,
        max_records = max_records
    ))]
    pub async fn read_from(
        &self,
        topic: &str,
        partition: i32,
        offset: i64,
        max_records: usize,
    ) -> Result<Vec<WalRecord>> {
        let tp = TopicPartition {
            topic: topic.to_string(),
            partition,
        };
        
        let _partition_wal = self.partitions.get(&tp)
            .ok_or_else(|| WalError::SegmentNotFound(format!("{}-{}", topic, partition)))?;
        
        // TODO: Implement actual reading from segments
        // For now, return empty vec
        Ok(Vec::new())
    }
    
    /// Create a new partition WAL
    #[instrument(skip(self), fields(
        topic = %topic,
        partition = partition,
        segment_id = 0,
        base_offset = 0
    ))]
    async fn create_partition_wal(&self, topic: String, partition: i32) -> Result<()> {
        let partition_dir = self.config.data_dir
            .join(&topic)
            .join(partition.to_string());
        
        tokio::fs::create_dir_all(&partition_dir).await?;
        
        let segment_id = 0;
        let base_offset = 0;
        let segment_path = partition_dir.join(format!("wal_{}_{}.log", segment_id, base_offset));
        
        let active_segment = WalSegment::new(segment_id, base_offset, segment_path);
        
        let wal = PartitionWal {
            topic: topic.clone(),
            partition,
            active_segment: Arc::new(RwLock::new(active_segment)),
            sealed_segments: Vec::new(),
            next_segment_id: 1,
            data_dir: partition_dir.clone(),
        };
        
        self.partitions.insert(
            TopicPartition { topic: topic.clone(), partition },
            wal,
        );
        
        info!(
            partition_dir = %partition_dir.display(),
            segment_id = segment_id,
            base_offset = base_offset,
            "WAL partition created successfully"
        );
        
        Ok(())
    }
    
    /// Rotate the active segment
    #[instrument(skip(self), fields(
        topic = %tp.topic,
        partition = tp.partition,
        rotation_triggered = true
    ))]
    async fn rotate_segment(&self, tp: &TopicPartition) -> Result<()> {
        let rotation_start = Instant::now();
        let mut partition_wal = self.partitions.get_mut(tp)
            .ok_or_else(|| WalError::SegmentNotFound(format!("{}-{}", tp.topic, tp.partition)))?;
        
        // Get needed values from active segment before rotating
        let (next_segment_id, base_offset, data_dir) = {
            let active = partition_wal.active_segment.read();
            (
                partition_wal.next_segment_id,
                active.last_offset + 1,
                partition_wal.data_dir.clone()
            )
        };
        
        // Create new segment
        let segment_path = data_dir.join(format!("wal_{}_{}.log", next_segment_id, base_offset));
        let new_segment = WalSegment::new(next_segment_id, base_offset, segment_path);
        
        // Swap segments
        let old_segment = {
            let mut active = partition_wal.active_segment.write();
            std::mem::replace(&mut *active, new_segment)
        };
        
        // Update next segment ID
        partition_wal.next_segment_id += 1;
        
        // Seal and save the old segment
        let seal_start = Instant::now();
        let sealed = old_segment.seal().await?;
        let seal_duration = seal_start.elapsed();
        let sealed_segment_id = sealed.id;
        let sealed_segment_size = sealed.size;
        let sealed_record_count = sealed.record_count;
        
        partition_wal.sealed_segments.push(sealed);
        
        let rotation_duration = rotation_start.elapsed();
        
        info!(
            old_segment_id = sealed_segment_id,
            old_segment_size = sealed_segment_size,
            old_segment_records = sealed_record_count,
            new_segment_id = next_segment_id,
            new_base_offset = base_offset,
            seal_duration_ms = seal_duration.as_millis() as u64,
            rotation_duration_ms = rotation_duration.as_millis() as u64,
            "WAL segment rotated successfully"
        );
        
        Ok(())
    }
    
    /// Get recovery statistics
    pub fn get_recovery_result(&self) -> RecoveryResult {
        let total_records = 0;
        let mut last_offsets = Vec::new();
        
        for entry in self.partitions.iter() {
            let tp = entry.key();
            let _wal = entry.value();
            
            // This would need async to read active_segment properly
            // For now, use placeholder values
            last_offsets.push((
                tp.topic.clone(),
                tp.partition,
                0, // Would be active_segment.last_offset
            ));
        }
        
        RecoveryResult {
            total_records,
            partitions: self.partitions.len(),
            corrupted_segments: 0,
            last_offsets,
        }
    }
    
    /// Flush all partitions to disk
    #[instrument(skip(self), fields(
        partition_count = self.partitions.len(),
        total_bytes_flushed = tracing::field::Empty,
        fsync_duration_ms = tracing::field::Empty
    ))]
    pub async fn flush_all(&self) -> Result<()> {
        let flush_start = Instant::now();
        let mut flushed_count = 0;
        let mut total_bytes = 0u64;
        let mut total_fsync_duration = std::time::Duration::ZERO;
        
        for entry in self.partitions.iter() {
            let tp = entry.key();
            let wal = entry.value();
            let active = wal.active_segment.read();
            
            // Force flush buffer to disk
            let buffer = active.buffer.read();
            if !buffer.is_empty() {
                let fsync_start = Instant::now();
                tokio::fs::write(&active.path, &buffer[..]).await?;
                let fsync_duration = fsync_start.elapsed();
                total_fsync_duration += fsync_duration;
                
                flushed_count += 1;
                total_bytes += buffer.len() as u64;
                
                debug!(
                    topic = %tp.topic,
                    partition = tp.partition,
                    segment_id = active.id,
                    bytes_flushed = buffer.len(),
                    fsync_duration_ms = fsync_duration.as_millis() as u64,
                    "WAL segment flushed to disk"
                );
            }
        }
        
        let total_flush_duration = flush_start.elapsed();
        
        // Record metrics in span
        tracing::Span::current()
            .record("total_bytes_flushed", total_bytes)
            .record("fsync_duration_ms", total_fsync_duration.as_millis() as u64);
        
        info!(
            partitions_flushed = flushed_count,
            total_bytes_flushed = total_bytes,
            total_flush_duration_ms = total_flush_duration.as_millis() as u64,
            avg_fsync_duration_ms = if flushed_count > 0 { 
                total_fsync_duration.as_millis() as u64 / flushed_count as u64 
            } else { 0 },
            "WAL flush completed"
        );
        
        Ok(())
    }
    
    /// Read next record after the given offset
    #[instrument(skip(self), fields(
        offset = offset,
        found_record = tracing::field::Empty
    ))]
    pub async fn read_next(&self, offset: i64) -> Result<Option<WalRecord>> {
        // TODO: Implement streaming read
        debug!("WAL read_next not yet implemented");
        Ok(None)
    }
}