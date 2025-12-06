//! Checkpoint management for WAL recovery optimization

use std::path::PathBuf;
use serde::{Deserialize, Serialize};
use tracing::{info, debug, instrument};

use crate::{
    config::CheckpointConfig,
    error::Result,
};

/// Checkpoint for fast recovery
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Checkpoint {
    pub offset: i64,
    pub segment_id: u64,
    pub position: u64,
    pub crc: u32,
    pub timestamp: i64,
}

/// Manages checkpoints for all partitions
pub struct CheckpointManager {
    data_dir: PathBuf,
    config: CheckpointConfig,
    last_checkpoint_records: u64,
    last_checkpoint_bytes: u64,
}

impl CheckpointManager {
    /// Create a new checkpoint manager
    #[instrument(skip(config), fields(
        data_dir = %data_dir.display(),
        enabled = config.enabled,
        interval_records = config.interval_records
    ))]
    pub async fn new(data_dir: PathBuf, config: CheckpointConfig) -> Result<Self> {
        debug!("Creating new checkpoint manager");
        let manager = Self {
            data_dir,
            config,
            last_checkpoint_records: 0,
            last_checkpoint_bytes: 0,
        };
        
        info!("Checkpoint manager created successfully");
        Ok(manager)
    }
    
    /// Check if a checkpoint should be created
    ///
    /// # Arguments
    /// - `topic`: Topic name
    /// - `partition`: Partition ID
    /// - `offset`: Current high watermark offset
    /// - `record_count`: Total records written since startup
    /// - `segment_state`: Optional (segment_id, position) from WAL for checkpoint-based seek
    #[instrument(skip(self), fields(
        topic = topic,
        partition = partition,
        offset = offset,
        record_count = record_count,
        records_since_last = tracing::field::Empty,
        checkpoint_created = tracing::field::Empty
    ))]
    pub async fn maybe_checkpoint(
        &mut self,
        topic: &str,
        partition: i32,
        offset: i64,
        record_count: u64,
        segment_state: Option<(u64, u64)>,
    ) -> Result<()> {
        if !self.config.enabled {
            debug!("Checkpointing disabled, skipping");
            return Ok(());
        }

        let records_since = record_count - self.last_checkpoint_records;

        // Record metrics in span
        tracing::Span::current()
            .record("records_since_last", records_since);

        if records_since >= self.config.interval_records {
            let (segment_id, position) = segment_state.unwrap_or((0, 0));
            self.create_checkpoint(topic, partition, offset, segment_id, position).await?;
            self.last_checkpoint_records = record_count;

            tracing::Span::current()
                .record("checkpoint_created", true);

            debug!(
                records_since = records_since,
                interval = self.config.interval_records,
                segment_id = segment_id,
                position = position,
                "Checkpoint created"
            );
        } else {
            tracing::Span::current()
                .record("checkpoint_created", false);
        }

        Ok(())
    }
    
    /// Create a checkpoint with segment state for fast WAL recovery
    ///
    /// The checkpoint stores the current WAL segment_id and position (byte offset)
    /// so recovery can seek directly to the checkpoint position instead of
    /// scanning from the beginning of the WAL.
    #[instrument(skip(self), fields(
        topic = topic,
        partition = partition,
        offset = offset,
        segment_id = segment_id,
        position = position,
        checkpoint_path = tracing::field::Empty
    ))]
    async fn create_checkpoint(
        &self,
        topic: &str,
        partition: i32,
        offset: i64,
        segment_id: u64,
        position: u64,
    ) -> Result<()> {
        let timestamp = chrono::Utc::now().timestamp_millis();

        // Calculate CRC over all checkpoint fields for integrity verification
        let mut hasher = crc32fast::Hasher::new();
        hasher.update(&offset.to_le_bytes());
        hasher.update(&segment_id.to_le_bytes());
        hasher.update(&position.to_le_bytes());
        hasher.update(&timestamp.to_le_bytes());
        let crc = hasher.finalize();

        let checkpoint = Checkpoint {
            offset,
            segment_id,
            position,
            crc,
            timestamp,
        };
        
        let checkpoint_path = self.data_dir
            .join(topic)
            .join(partition.to_string())
            .join("checkpoint.json");
        
        // Record path in span
        tracing::Span::current()
            .record("checkpoint_path", &tracing::field::display(&checkpoint_path.display()));
        
        let json = serde_json::to_string(&checkpoint)?;
        tokio::fs::write(&checkpoint_path, json).await?;
        
        info!(
            checkpoint_path = %checkpoint_path.display(),
            checkpoint_offset = checkpoint.offset,
            checkpoint_segment_id = checkpoint.segment_id,
            checkpoint_position = checkpoint.position,
            checkpoint_timestamp = checkpoint.timestamp,
            "Checkpoint written to disk"
        );
        
        Ok(())
    }
    
    /// Load checkpoint for a partition
    #[instrument(skip(self), fields(
        topic = topic,
        partition = partition,
        checkpoint_exists = tracing::field::Empty,
        loaded_offset = tracing::field::Empty
    ))]
    pub async fn load_checkpoint(
        &self,
        topic: &str,
        partition: i32,
    ) -> Result<Option<Checkpoint>> {
        let checkpoint_path = self.data_dir
            .join(topic)
            .join(partition.to_string())
            .join("checkpoint.json");
        
        if !checkpoint_path.exists() {
            tracing::Span::current()
                .record("checkpoint_exists", false);
            
            debug!("No checkpoint file found");
            return Ok(None);
        }
        
        tracing::Span::current()
            .record("checkpoint_exists", true);
        
        let json = tokio::fs::read_to_string(&checkpoint_path).await?;
        let checkpoint: Checkpoint = serde_json::from_str(&json)?;
        
        tracing::Span::current()
            .record("loaded_offset", checkpoint.offset);
        
        info!(
            checkpoint_path = %checkpoint_path.display(),
            loaded_offset = checkpoint.offset,
            loaded_segment_id = checkpoint.segment_id,
            loaded_position = checkpoint.position,
            checkpoint_timestamp = checkpoint.timestamp,
            "Checkpoint loaded from disk"
        );
        
        Ok(Some(checkpoint))
    }
}