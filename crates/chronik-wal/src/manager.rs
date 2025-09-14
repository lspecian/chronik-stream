//! WAL manager for coordinating segments and partitions

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;
use dashmap::DashMap;
use tokio::sync::RwLock;
use tracing::{info, debug, instrument};
use serde::{Serialize, Deserialize};

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
#[derive(Debug, Clone, Hash, Eq, PartialEq, Serialize, Deserialize)]
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

        // Create a new manager instance
        let mut manager = Self::new(config.clone()).await?;

        // Scan data directory for existing WAL partitions
        if config.data_dir.exists() {
            manager.scan_and_recover_partitions().await?;
        } else {
            info!("Data directory does not exist, starting with empty WAL");
        }

        info!("WAL manager recovered with {} partitions", manager.partitions.len());
        Ok(manager)
    }

    /// Scan data directory and recover existing partitions
    async fn scan_and_recover_partitions(&mut self) -> Result<()> {
        let data_dir = &self.config.data_dir;

        // Read all topic directories
        let mut entries = tokio::fs::read_dir(data_dir).await?;
        let mut recovered_count = 0;

        while let Some(entry) = entries.next_entry().await? {
            let topic_path = entry.path();
            if !topic_path.is_dir() {
                continue;
            }

            let topic_name = match entry.file_name().to_str() {
                Some(name) => name.to_string(),
                None => continue,
            };

            // Read partition directories within topic
            let mut partition_entries = tokio::fs::read_dir(&topic_path).await?;

            while let Some(partition_entry) = partition_entries.next_entry().await? {
                let partition_path = partition_entry.path();
                if !partition_path.is_dir() {
                    continue;
                }

                let partition_name = match partition_entry.file_name().to_str() {
                    Some(name) => name.to_string(),
                    None => continue,
                };

                let partition = match partition_name.parse::<i32>() {
                    Ok(p) => p,
                    Err(_) => continue,
                };

                // Recover this partition
                let tp = TopicPartition {
                    topic: topic_name.clone(),
                    partition,
                };

                if let Ok(segment) = self.recover_partition(&tp, &partition_path).await {
                    info!("Recovered partition {}/{} with segment {}",
                          topic_name, partition, segment.id);
                    recovered_count += 1;
                }
            }
        }

        info!("Recovered {} partitions from disk", recovered_count);
        Ok(())
    }

    /// Recover a single partition from disk
    async fn recover_partition(&mut self, tp: &TopicPartition, path: &Path) -> Result<WalSegment> {
        // Find the latest segment file in the partition directory
        let mut entries = tokio::fs::read_dir(path).await?;
        let mut latest_segment: Option<(u64, i64, PathBuf)> = None;

        while let Some(entry) = entries.next_entry().await? {
            let file_path = entry.path();
            let file_name = match entry.file_name().to_str() {
                Some(name) => name.to_string(),
                None => continue,
            };

            // Parse segment file name: wal_<segment_id>_<base_offset>.log
            if file_name.starts_with("wal_") && file_name.ends_with(".log") {
                let parts: Vec<&str> = file_name
                    .trim_start_matches("wal_")
                    .trim_end_matches(".log")
                    .split('_')
                    .collect();

                if parts.len() == 2 {
                    if let (Ok(segment_id), Ok(base_offset)) =
                        (parts[0].parse::<u64>(), parts[1].parse::<i64>()) {

                        // Keep track of the latest segment
                        if latest_segment.is_none() ||
                           latest_segment.as_ref().unwrap().0 < segment_id {
                            latest_segment = Some((segment_id, base_offset, file_path));
                        }
                    }
                }
            }
        }

        // Recover the latest segment
        if let Some((segment_id, base_offset, segment_path)) = latest_segment {
            // Read the segment file to determine the last offset
            let buffer = tokio::fs::read(&segment_path).await?;
            let last_offset = self.calculate_last_offset(&buffer, base_offset);

            // Create the recovered segment
            let mut segment = WalSegment::new(
                segment_id,
                base_offset,
                segment_path.clone(),
            );

            // Restore the buffer content
            {
                let mut segment_buffer = segment.buffer.write().await;
                segment_buffer.extend_from_slice(&buffer);
            }

            // Update segment's last offset
            segment.last_offset = last_offset;

            // Store in partitions map
            let wal = PartitionWal {
                topic: tp.topic.clone(),
                partition: tp.partition,
                active_segment: Arc::new(tokio::sync::RwLock::new(segment)),
                sealed_segments: Vec::new(),
                next_segment_id: segment_id + 1,
                data_dir: path.to_path_buf(),
            };

            self.partitions.insert(tp.clone(), wal);

            // Return a segment copy for logging
            let wal_ref = self.partitions.get(tp).unwrap();
            let active = wal_ref.active_segment.read().await;
            Ok(WalSegment::new(
                active.id,
                active.base_offset,
                active.path.clone(),
            ))
        } else {
            Err(WalError::SegmentNotFound(format!("{}/{}", tp.topic, tp.partition)))
        }
    }

    /// Calculate the last offset from buffer content
    fn calculate_last_offset(&self, buffer: &[u8], base_offset: i64) -> i64 {
        // Simple calculation: count the number of records
        // In a real implementation, we'd parse the records properly
        let record_count = buffer.len() / 100; // Assume average record size of 100 bytes
        base_offset + record_count as i64 - 1
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
        for record in records {
            // Check if rotation is needed
            let needs_rotation = {
                let active = partition_wal.active_segment.read().await;
                active.should_rotate(
                    self.config.rotation.max_segment_size,
                    self.config.rotation.max_segment_age_ms,
                )
            };
            
            if needs_rotation {
                let segment_info = {
                    let active = partition_wal.active_segment.read().await;
                    (active.id, active.size)
                };
                debug!(
                    segment_id = segment_info.0,
                    segment_size = segment_info.1,
                    "WAL segment rotation triggered"
                );
                self.rotate_segment(&tp).await?;
            }
            
            // Perform append without holding lock
            {
                let mut active = partition_wal.active_segment.write().await;
                active.append(record).await?;
            }
        }
        
        // Update checkpoint if needed and get current segment stats
        let (last_offset, record_count) = {
            let active = partition_wal.active_segment.read().await;
            (active.last_offset, active.record_count)
        };
        
        if self.config.checkpointing.enabled {
            self.checkpoint_manager.maybe_checkpoint(
                &topic,
                partition,
                last_offset,
                record_count,
            ).await?;
        }
        
        debug!(
            last_offset = last_offset,
            total_records = record_count,
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
        
        let partition_wal = self.partitions.get(&tp)
            .ok_or_else(|| WalError::SegmentNotFound(format!("{}-{}", topic, partition)))?;
        
        let mut records = Vec::new();
        
        info!(
            "Reading from WAL: topic={}, partition={}, offset={}, max_records={}", 
            topic, partition, offset, max_records
        );
        
        // First check active segment buffer
        {
            let active_segment = partition_wal.active_segment.read().await;
            let buffer = active_segment.buffer.read().await;
            
            if !buffer.is_empty() && offset >= active_segment.base_offset {
                debug!("Checking active segment buffer for offset {}", offset);
                
                // Parse records from active segment buffer
                if let Ok(parsed_records) = self.parse_buffer_records(&buffer, active_segment.base_offset, offset, max_records) {
                    records.extend(parsed_records);
                    info!("Found {} records in active segment buffer", records.len());
                }
            }
        }
        
        // If we still need more records and haven't hit the limit, check sealed segments
        if records.len() < max_records && !partition_wal.sealed_segments.is_empty() {
            for sealed_segment in &partition_wal.sealed_segments {
                if sealed_segment.contains_offset(offset) {
                    debug!("Reading from sealed segment {}", sealed_segment.id);
                    
                    // Read the sealed segment file
                    if let Ok(segment_data) = tokio::fs::read(&sealed_segment.path).await {
                        if let Ok(sealed_records) = self.parse_buffer_records(&segment_data, sealed_segment.base_offset, offset, max_records - records.len()) {
                            let sealed_count = sealed_records.len();
                            records.extend(sealed_records);
                            info!("Found {} additional records in sealed segment {}", sealed_count, sealed_segment.id);
                        }
                    }
                    
                    // Stop if we have enough records
                    if records.len() >= max_records {
                        break;
                    }
                }
            }
        }
        
        info!("WAL read completed: found {} records starting from offset {}", records.len(), offset);
        Ok(records)
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
            let active = partition_wal.active_segment.read().await;
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
            let mut active = partition_wal.active_segment.write().await;
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
            
            // Extract data needed for flush without holding locks across await
            let (path, buffer_data, segment_id) = {
                let active = wal.active_segment.read().await;
                let buffer = active.buffer.read().await;
                if buffer.is_empty() {
                    continue;
                }
                (active.path.clone(), buffer.to_vec(), active.id)
            };
            
            // Force flush buffer to disk
            let fsync_start = Instant::now();
            tokio::fs::write(&path, &buffer_data).await?;
            let fsync_duration = fsync_start.elapsed();
            total_fsync_duration += fsync_duration;
            
            flushed_count += 1;
            total_bytes += buffer_data.len() as u64;
            
            debug!(
                topic = %tp.topic,
                partition = tp.partition,
                segment_id = segment_id,
                bytes_flushed = buffer_data.len(),
                fsync_duration_ms = fsync_duration.as_millis() as u64,
                "WAL segment flushed to disk"
            );
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

    /// Read records from a topic-partition
    pub async fn read(
        &self,
        tp: TopicPartition,
        offset: i64,
        _max_bytes: usize,
    ) -> Result<Vec<WalRecord>> {
        let _partition_wal = self.partitions.get(&tp)
            .ok_or_else(|| WalError::SegmentNotFound(format!("{}-{}", tp.topic, tp.partition)))?;

        // For now, return empty vec - full implementation would read from segments
        debug!("Reading from WAL: topic={}, partition={}, offset={}", tp.topic, tp.partition, offset);
        Ok(Vec::new())
    }

    /// Get the latest offset for a topic-partition
    pub async fn get_latest_offset(&self, tp: TopicPartition) -> Result<i64> {
        let partition_wal = self.partitions.get(&tp)
            .ok_or_else(|| WalError::SegmentNotFound(format!("{}-{}", tp.topic, tp.partition)))?;

        let active = partition_wal.active_segment.read().await;
        Ok(active.last_offset)
    }
    
    /// Parse buffer records within offset range
    fn parse_buffer_records(
        &self,
        buffer: &[u8],
        base_offset: i64,
        start_offset: i64,
        max_records: usize,
    ) -> Result<Vec<WalRecord>> {
        let mut records = Vec::new();
        let mut cursor = 0;
        
        debug!(
            buffer_len = buffer.len(),
            base_offset = base_offset,
            start_offset = start_offset,
            max_records = max_records,
            "Parsing buffer records"
        );
        
        while cursor < buffer.len() && records.len() < max_records {
            // Try to read a record from current position
            let remaining = &buffer[cursor..];
            
            // Need at least the minimum header size to proceed
            if remaining.len() < 28 { // Fixed header size from record.rs:246
                debug!("Insufficient buffer data for header at cursor {}", cursor);
                break;
            }
            
            // Parse the record length to determine full record size
            let record_length = {
                use byteorder::{LittleEndian, ReadBytesExt};
                use std::io::Cursor;
                
                let mut length_cursor = Cursor::new(&remaining[4..8]); // Length is at offset 4
                length_cursor.read_u32::<LittleEndian>()?
            };
            
            // Check if we have the full record
            if remaining.len() < record_length as usize {
                debug!(
                    "Incomplete record: need {} bytes, have {}",
                    record_length,
                    remaining.len()
                );
                break;
            }
            
            // Try to parse the record
            let record_bytes = &remaining[..record_length as usize];
            match WalRecord::from_bytes(record_bytes) {
                Ok(record) => {
                    // Check if this record is within our offset range
                    if record.offset >= start_offset {
                        debug!(
                            record_offset = record.offset,
                            record_size = record_bytes.len(),
                            "Found record in range"
                        );
                        records.push(record);
                    } else {
                        debug!(
                            record_offset = record.offset,
                            start_offset = start_offset,
                            "Skipping record before start offset"
                        );
                    }
                    
                    // Move cursor to next record
                    cursor += record_length as usize;
                }
                Err(e) => {
                    // If we can't parse a record, it might be corruption or incomplete data
                    debug!("Failed to parse record at cursor {}: {}", cursor, e);
                    break;
                }
            }
        }
        
        debug!(
            "Parsed {} records from buffer (cursor final position: {})",
            records.len(),
            cursor
        );
        
        Ok(records)
    }
}