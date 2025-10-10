//! WAL manager for coordinating segments and partitions

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;
use dashmap::DashMap;
use tokio::sync::RwLock;
use tracing::{info, debug, warn, instrument};
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

    /// Append a CanonicalRecord batch as a V2 WAL record
    /// Takes pre-serialized canonical data to avoid circular dependencies
    #[instrument(skip(self, canonical_data), fields(
        topic = %topic,
        partition = partition,
        data_size = canonical_data.len()
    ))]
    pub async fn append_canonical(
        &mut self,
        topic: String,
        partition: i32,
        canonical_data: Vec<u8>,
    ) -> Result<()> {
        // Create V2 WAL record
        let wal_record = WalRecord::new_v2(
            topic.clone(),
            partition,
            canonical_data,
        );

        // Append as a single record
        self.append(topic, partition, vec![wal_record]).await
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
        let (active_base_offset, active_path) = {
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

            (active_segment.base_offset, active_segment.path.clone())
        };

        // If buffer was empty but we still need records, check active segment FILE
        if records.is_empty() && offset >= active_base_offset {
            debug!("Active buffer empty, checking active segment file: {:?}", active_path);

            if let Ok(file_data) = tokio::fs::read(&active_path).await {
                if !file_data.is_empty() {
                    debug!("Reading {} bytes from active segment file", file_data.len());

                    // Parse V2 WAL records from file
                    // V2 format: magic(2) + version(1) + flags(1) + length(4) + crc32(4) + topic_len(2) + topic + partition(4) + canonical_data_len(4) + canonical_data
                    let mut cursor = 0;
                    let mut file_records = Vec::new();

                    while cursor < file_data.len() && file_records.len() < max_records {
                        use byteorder::{LittleEndian, ReadBytesExt};
                        use std::io::Cursor as IoCursor;

                        debug!("=== Parsing record at cursor {} (file_data.len={}, file_records.len={}) ===", cursor, file_data.len(), file_records.len());

                        // Need at least 12 bytes for V2 header (magic+version+flags+length+crc32)
                        if cursor + 12 > file_data.len() {
                            debug!("Not enough data for header at cursor {}: need 12 bytes, have {}", cursor, file_data.len() - cursor);
                            break;
                        }

                        let record_start = cursor;
                        let mut rdr = IoCursor::new(&file_data[cursor..]);

                        let magic = rdr.read_u16::<LittleEndian>().unwrap();
                        let version = rdr.read_u8().unwrap();
                        let _flags = rdr.read_u8().unwrap();
                        let length = rdr.read_u32::<LittleEndian>().unwrap() as usize;
                        let _crc32 = rdr.read_u32::<LittleEndian>().unwrap();

                        debug!("Header: magic={:x}, version={}, length={}, crc32={:x}", magic, version, length, _crc32);

                        // Validate magic and version
                        if magic != 0xCA7E || version != 2 {
                            debug!("Invalid WAL record at cursor {}: magic={:x}, version={}", cursor, magic, version);
                            break;
                        }

                        // Parse V2 record body
                        let topic_len = rdr.read_u16::<LittleEndian>().unwrap() as usize;
                        let mut topic_bytes = vec![0u8; topic_len];
                        std::io::Read::read_exact(&mut rdr, &mut topic_bytes).unwrap();
                        let topic = String::from_utf8(topic_bytes).unwrap();

                        let partition = rdr.read_i32::<LittleEndian>().unwrap();

                        let canonical_data_len = rdr.read_u32::<LittleEndian>().unwrap() as usize;
                        let mut canonical_data = vec![0u8; canonical_data_len];
                        std::io::Read::read_exact(&mut rdr, &mut canonical_data).unwrap();

                        // Calculate actual bytes consumed by checking cursor position
                        // Create WalRecord::V2
                        let record = WalRecord::V2 {
                            magic,
                            version,
                            flags: _flags,
                            length: length as u32,
                            crc32: _crc32,
                            topic,
                            partition,
                            canonical_data,
                        };

                        // V2 records don't have offset in WAL record - it's in CanonicalRecord
                        // For recovery, we need ALL V2 records for this partition
                        file_records.push(record);

                        // CRITICAL FIX: Advance cursor by FULL record size
                        // V2 format: 12-byte header + body (where body size is stored in `length` field)
                        // rdr.position() gives us the total bytes read from rdr, which includes BOTH header and body
                        // since rdr was created from &file_data[cursor..] and we read the header from it too
                        let total_bytes_read = rdr.position() as usize; // Total bytes consumed (header + body)
                        let total_record_size = total_bytes_read;

                        debug!("Advancing cursor from {} by {} bytes (total_bytes_read={}, stored length was {})",
                            record_start, total_record_size, total_bytes_read, length);
                        cursor = record_start + total_record_size;
                    }

                    let count = file_records.len();
                    records.extend(file_records);
                    info!("Found {} V2 records in active segment file", count);
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

    /// Get all partitions for recovery
    pub fn get_partitions(&self) -> Vec<TopicPartition> {
        self.partitions.iter()
            .map(|entry| entry.key().clone())
            .collect()
    }

    /// Truncate WAL segments before the given offset
    /// Returns the number of segments truncated
    pub async fn truncate_before(
        &mut self,
        topic: &str,
        partition: i32,
        offset: i64,
    ) -> Result<usize> {
        let tp = TopicPartition {
            topic: topic.to_string(),
            partition,
        };

        let mut partition_wal = self.partitions.get_mut(&tp)
            .ok_or_else(|| WalError::SegmentNotFound(format!("{}-{}", topic, partition)))?;

        let mut truncated_count = 0;

        // Find and remove sealed segments that are completely before the offset
        partition_wal.sealed_segments.retain(|segment| {
            if segment.base_offset + segment.record_count as i64 <= offset {
                // This segment is completely before the truncation point
                if let Err(e) = std::fs::remove_file(&segment.path) {
                    warn!("Failed to delete WAL segment file {:?}: {}", segment.path, e);
                } else {
                    info!("Truncated WAL segment {} (base_offset: {}, records: {})",
                          segment.id, segment.base_offset, segment.record_count);
                    truncated_count += 1;
                }
                false // Remove from list
            } else {
                true // Keep in list
            }
        });

        info!("Truncated {} WAL segments for {}-{} before offset {}",
              truncated_count, topic, partition, offset);

        Ok(truncated_count)
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
            
            // Force flush buffer to disk (APPEND mode, not overwrite)
            let fsync_start = Instant::now();
            use tokio::fs::OpenOptions;
            use tokio::io::AsyncWriteExt;

            let mut file = OpenOptions::new()
                .create(true)
                .append(true)  // CRITICAL: APPEND, not overwrite
                .open(&path)
                .await?;
            file.write_all(&buffer_data).await?;
            file.sync_all().await?;  // Ensure data is on disk

            let fsync_duration = fsync_start.elapsed();
            total_fsync_duration += fsync_duration;

            flushed_count += 1;
            total_bytes += buffer_data.len() as u64;

            // Clear buffer after successful flush (data is now on disk)
            {
                let active = wal.active_segment.read().await;
                let mut buffer = active.buffer.write().await;
                buffer.clear();
            }

            debug!(
                topic = %tp.topic,
                partition = tp.partition,
                segment_id = segment_id,
                bytes_flushed = buffer_data.len(),
                fsync_duration_ms = fsync_duration.as_millis() as u64,
                "WAL segment flushed to disk and buffer cleared"
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
                    // Note: Only V1 records have offset, V2 records are accepted always
                    let record_offset = record.get_offset().unwrap_or(start_offset);
                    if record_offset >= start_offset {
                        debug!(
                            record_offset = record_offset,
                            record_size = record_bytes.len(),
                            "Found record in range"
                        );
                        records.push(record);
                    } else {
                        debug!(
                            record_offset = record_offset,
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

    /// Get list of sealed segment IDs across all partitions
    /// Returns segment identifiers like "topic-0-segment-123"
    pub fn get_sealed_segments(&self) -> Vec<String> {
        let mut sealed = Vec::new();

        for entry in self.partitions.iter() {
            let partition_wal = entry.value();
            for sealed_segment in &partition_wal.sealed_segments {
                let segment_id = format!(
                    "{}-{}-segment-{}",
                    partition_wal.topic,
                    partition_wal.partition,
                    sealed_segment.id
                );
                sealed.push(segment_id);
            }
        }

        debug!(count = sealed.len(), "Found sealed segments");
        sealed
    }

    /// Read all records from a specific sealed segment
    /// segment_id format: "topic-partition-segment-id"
    pub async fn read_segment(&self, segment_id: &str) -> Result<Vec<WalRecord>> {
        // Parse segment_id
        let parts: Vec<&str> = segment_id.split('-').collect();
        if parts.len() < 4 || parts[parts.len() - 2] != "segment" {
            return Err(WalError::InvalidFormat(format!(
                "Invalid segment ID format: {}",
                segment_id
            )));
        }

        let topic = parts[0..parts.len() - 3].join("-");
        let partition: i32 = parts[parts.len() - 3]
            .parse()
            .map_err(|e| WalError::InvalidFormat(format!("Invalid partition number: {}", e)))?;
        let segment_num: u64 = parts[parts.len() - 1]
            .parse()
            .map_err(|e| WalError::InvalidFormat(format!("Invalid segment number: {}", e)))?;

        let tp = TopicPartition {
            topic: topic.clone(),
            partition,
        };

        // Find the sealed segment
        let partition_wal = self
            .partitions
            .get(&tp)
            .ok_or_else(|| WalError::SegmentNotFound(format!("Partition not found: {}-{}", topic, partition)))?;

        let sealed_segment = partition_wal
            .sealed_segments
            .iter()
            .find(|s| s.id == segment_num)
            .ok_or_else(|| WalError::SegmentNotFound(format!("Segment not found: {}", segment_id)))?;

        // Read all records from the segment file
        let segment_data = tokio::fs::read(&sealed_segment.path).await?;

        // Parse records from the segment data
        let mut records = Vec::new();
        let mut cursor = 0;

        while cursor < segment_data.len() {
            let remaining = &segment_data[cursor..];

            // Need at least minimum header size
            if remaining.len() < 28 {
                break;
            }

            // Read record length
            let record_length = {
                use byteorder::{LittleEndian, ReadBytesExt};
                use std::io::Cursor;

                let mut length_cursor = Cursor::new(&remaining[4..8]);
                length_cursor.read_u32::<LittleEndian>()?
            };

            // Check if we have the full record
            if remaining.len() < record_length as usize {
                break;
            }

            // Parse record
            let record_bytes = &remaining[..record_length as usize];
            match WalRecord::from_bytes(record_bytes) {
                Ok(record) => {
                    records.push(record);
                    cursor += record_length as usize;
                }
                Err(e) => {
                    warn!(
                        segment = %segment_id,
                        cursor = cursor,
                        error = %e,
                        "Failed to parse record, stopping"
                    );
                    break;
                }
            }
        }

        info!(
            segment = %segment_id,
            record_count = records.len(),
            "Read records from sealed segment"
        );

        Ok(records)
    }

    /// Delete a sealed segment after it has been indexed
    pub async fn delete_segment(&mut self, segment_id: &str) -> Result<()> {
        // Parse segment_id
        let parts: Vec<&str> = segment_id.split('-').collect();
        if parts.len() < 4 || parts[parts.len() - 2] != "segment" {
            return Err(WalError::InvalidFormat(format!(
                "Invalid segment ID format: {}",
                segment_id
            )));
        }

        let topic = parts[0..parts.len() - 3].join("-");
        let partition: i32 = parts[parts.len() - 3]
            .parse()
            .map_err(|e| WalError::InvalidFormat(format!("Invalid partition number: {}", e)))?;
        let segment_num: u64 = parts[parts.len() - 1]
            .parse()
            .map_err(|e| WalError::InvalidFormat(format!("Invalid segment number: {}", e)))?;

        let tp = TopicPartition {
            topic: topic.clone(),
            partition,
        };

        // Find and remove the sealed segment
        if let Some(mut partition_wal) = self.partitions.get_mut(&tp) {
            if let Some(pos) = partition_wal.sealed_segments.iter().position(|s| s.id == segment_num) {
                let sealed_segment = partition_wal.sealed_segments.remove(pos);

                // Delete the file
                tokio::fs::remove_file(&sealed_segment.path).await?;

                info!(
                    segment = %segment_id,
                    path = %sealed_segment.path.display(),
                    "Deleted sealed segment"
                );

                return Ok(());
            }
        }

        Err(WalError::SegmentNotFound(format!("Segment not found: {}", segment_id)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use crate::config::{
        CompressionType, CheckpointConfig, RecoveryConfig,
        RotationConfig, FsyncConfig, AsyncIoConfig
    };

    async fn setup_test_manager() -> (WalManager, TempDir) {
        let temp_dir = TempDir::new().unwrap();
        let config = WalConfig {
            enabled: true,
            data_dir: temp_dir.path().to_path_buf(),
            segment_size: 1024 * 1024, // 1MB
            flush_interval_ms: 100,
            flush_threshold: 1000,
            compression: CompressionType::None,
            checkpointing: CheckpointConfig::default(),
            recovery: RecoveryConfig::default(),
            rotation: RotationConfig::default(),
            fsync: FsyncConfig::default(),
            async_io: AsyncIoConfig::default(),
        };

        let manager = WalManager::new(config).await.unwrap();
        (manager, temp_dir)
    }

    #[tokio::test]
    async fn test_truncate_before() {
        let (mut manager, _temp_dir) = setup_test_manager().await;

        // Append some records
        let topic = "test-topic";
        let partition = 0;

        // Create records with offsets 0-9
        let mut records = Vec::new();
        for i in 0..10 {
            records.push(WalRecord::new(
                i,
                Some(format!("key-{}", i).into_bytes()),
                format!("value-{}", i).into_bytes(),
                1000 + i,
            ));
        }

        // Append records
        manager.append(topic.to_string(), partition, records.clone()).await.unwrap();

        // Force segment rotation would happen automatically based on size
        // manager.rotate_if_needed(topic, partition).await.unwrap();

        // Append more records (10-19)
        let mut more_records = Vec::new();
        for i in 10..20 {
            more_records.push(WalRecord::new(
                i,
                Some(format!("key-{}", i).into_bytes()),
                format!("value-{}", i).into_bytes(),
                1010 + i,
            ));
        }
        manager.append(topic.to_string(), partition, more_records).await.unwrap();

        // Truncate before offset 10
        let truncated = manager.truncate_before(topic, partition, 10).await.unwrap();

        // Should have truncated at least one segment
        assert!(truncated > 0, "Should have truncated at least one segment");

        // Read remaining records
        let remaining = manager.read_from(topic, partition, 10, 100).await.unwrap();

        // Should only have records with offset >= 10
        for record in &remaining {
            let offset = record.get_offset().unwrap();
            assert!(offset >= 10, "Record offset {} should be >= 10", offset);
        }
    }

    #[tokio::test]
    async fn test_get_partitions() {
        let (mut manager, _temp_dir) = setup_test_manager().await;

        // Initially no partitions
        let partitions = manager.get_partitions();
        assert_eq!(partitions.len(), 0);

        // Add some partitions
        let topics = vec![
            ("topic1", 0),
            ("topic1", 1),
            ("topic2", 0),
        ];

        for (topic, partition) in &topics {
            let record = WalRecord::new(
                0,
                Some(b"key".to_vec()),
                b"value".to_vec(),
                1000,
            );
            manager.append(topic.to_string(), *partition, vec![record]).await.unwrap();
        }

        // Should now have 3 partitions
        let partitions = manager.get_partitions();
        assert_eq!(partitions.len(), 3);

        // Verify partition keys
        for tp in partitions {
            let found = topics.iter().any(|(t, p)| {
                tp.topic == *t && tp.partition == *p
            });
            assert!(found, "Unexpected partition: {}-{}", tp.topic, tp.partition);
        }
    }

    #[tokio::test]
    async fn test_recovery() {
        let temp_dir = TempDir::new().unwrap();
        let config = WalConfig {
            enabled: true,
            data_dir: temp_dir.path().to_path_buf(),
            segment_size: 1024 * 1024,
            flush_interval_ms: 100,
            flush_threshold: 1000,
            compression: CompressionType::None,
            checkpointing: CheckpointConfig::default(),
            recovery: RecoveryConfig::default(),
            rotation: RotationConfig::default(),
            fsync: FsyncConfig::default(),
            async_io: AsyncIoConfig::default(),
        };

        // Create and write some data
        {
            let mut manager = WalManager::new(config.clone()).await.unwrap();

            // Write records to multiple partitions
            for partition in 0..3 {
                let mut records = Vec::new();
                for i in 0..5 {
                    records.push(WalRecord::new(
                        i,
                        Some(format!("key-{}", i).into_bytes()),
                        format!("value-p{}-{}", partition, i).into_bytes(),
                        1000 + i,
                    ));
                }
                manager.append("test-topic".to_string(), partition, records).await.unwrap();
            }

            // Flush to ensure data is persisted
            manager.flush_all().await.unwrap();
        }

        // Recover from disk
        let recovered_manager = WalManager::recover(&config).await.unwrap();

        // Get recovery stats
        let recovery_result = recovered_manager.get_recovery_result();
        assert_eq!(recovery_result.partitions, 3, "Should recover 3 partitions");
        assert_eq!(recovery_result.total_records, 15, "Should recover 15 total records");

        // Verify we can read the recovered data
        for partition in 0..3 {
            let records = recovered_manager.read_from("test-topic", partition, 0, 100).await.unwrap();
            assert_eq!(records.len(), 5, "Should have 5 records in partition {}", partition);

            // Verify record content
            for (i, record) in records.iter().enumerate() {
                assert_eq!(record.get_offset().unwrap(), i as i64);
                let expected_value = format!("value-p{}-{}", partition, i);
                assert_eq!(record.get_value().unwrap(), expected_value.as_bytes());
            }
        }
    }
}