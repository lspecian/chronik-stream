//! Backup integrity validation

use crate::{
    BackupError, Result, BackupMetadata, compression,
    metadata::{SegmentBackupRef, TopicBackupInfo},
};
use chronik_storage::object_store::ObjectStore;
use std::sync::Arc;
use chrono::{DateTime, Utc};
use tokio::sync::{Semaphore, Mutex};
use futures::stream::StreamExt;
use tracing::{info, debug};
use std::collections::{HashMap, HashSet};
use sha2::{Sha256, Digest};

/// Validation level
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValidationLevel {
    /// Quick validation (metadata only)
    Quick,
    /// Standard validation (metadata + checksums)
    Standard,
    /// Deep validation (full data verification)
    Deep,
    /// Paranoid validation (deep + cross-references)
    Paranoid,
}

/// Validation report
#[derive(Debug, Clone)]
pub struct ValidationReport {
    /// Backup ID that was validated
    pub backup_id: String,
    /// Validation level used
    pub level: ValidationLevel,
    /// Whether backup is valid
    pub is_valid: bool,
    /// Validation timestamp
    pub timestamp: DateTime<Utc>,
    /// Duration of validation
    pub duration: std::time::Duration,
    /// Total items checked
    pub items_checked: u64,
    /// Items with errors
    pub items_failed: u64,
    /// Validation errors
    pub errors: Vec<ValidationError>,
    /// Validation warnings
    pub warnings: Vec<ValidationWarning>,
    /// Detailed statistics
    pub stats: ValidationStats,
}

/// Validation error
#[derive(Debug, Clone)]
pub struct ValidationError {
    /// Error type
    pub error_type: ValidationErrorType,
    /// Item that failed (e.g., segment path)
    pub item: String,
    /// Error message
    pub message: String,
}

/// Validation error types
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValidationErrorType {
    /// File not found
    NotFound,
    /// Checksum mismatch
    ChecksumMismatch,
    /// Size mismatch
    SizeMismatch,
    /// Corruption detected
    Corruption,
    /// Format error
    FormatError,
    /// Reference error (missing dependency)
    ReferenceError,
}

/// Validation warning
#[derive(Debug, Clone)]
pub struct ValidationWarning {
    /// Warning type
    pub warning_type: ValidationWarningType,
    /// Item with warning
    pub item: String,
    /// Warning message
    pub message: String,
}

/// Validation warning types
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValidationWarningType {
    /// Deprecated format
    DeprecatedFormat,
    /// Performance issue
    PerformanceIssue,
    /// Best practice violation
    BestPracticeViolation,
    /// Potential issue
    PotentialIssue,
}

/// Validation statistics
#[derive(Debug, Clone)]
pub struct ValidationStats {
    /// Number of topics validated
    pub topics_validated: u32,
    /// Number of segments validated
    pub segments_validated: u64,
    /// Total bytes validated
    pub bytes_validated: u64,
    /// Checksum verifications performed
    pub checksums_verified: u64,
    /// Compression ratio
    pub avg_compression_ratio: f64,
    /// Oldest segment timestamp
    pub oldest_segment: Option<DateTime<Utc>>,
    /// Newest segment timestamp
    pub newest_segment: Option<DateTime<Utc>>,
}

/// Backup validator
pub struct BackupValidator {
    /// Backup object store
    backup_store: Arc<dyn ObjectStore>,
    /// Semaphore for parallel operations
    semaphore: Arc<Semaphore>,
    /// Validation state
    state: Arc<Mutex<ValidationState>>,
}

/// Internal validation state
struct ValidationState {
    /// Items checked
    items_checked: u64,
    /// Items failed
    items_failed: u64,
    /// Errors found
    errors: Vec<ValidationError>,
    /// Warnings found
    warnings: Vec<ValidationWarning>,
    /// Statistics
    stats: ValidationStats,
}

impl BackupValidator {
    /// Create a new backup validator
    pub fn new(backup_store: Arc<dyn ObjectStore>, parallel_workers: usize) -> Self {
        let semaphore = Arc::new(Semaphore::new(parallel_workers));
        let state = Arc::new(Mutex::new(ValidationState {
            items_checked: 0,
            items_failed: 0,
            errors: Vec::new(),
            warnings: Vec::new(),
            stats: ValidationStats {
                topics_validated: 0,
                segments_validated: 0,
                bytes_validated: 0,
                checksums_verified: 0,
                avg_compression_ratio: 0.0,
                oldest_segment: None,
                newest_segment: None,
            },
        }));
        
        Self {
            backup_store,
            semaphore,
            state,
        }
    }
    
    /// Validate a backup
    pub async fn validate(
        &self,
        backup_id: &str,
        level: ValidationLevel,
    ) -> Result<ValidationReport> {
        let start_time = std::time::Instant::now();
        
        info!("Starting backup validation: {} (level: {:?})", backup_id, level);
        
        // Load backup metadata
        let metadata = self.load_backup_metadata(backup_id).await?;
        
        // Validate based on level
        match level {
            ValidationLevel::Quick => {
                self.validate_quick(&metadata).await?;
            }
            ValidationLevel::Standard => {
                self.validate_standard(&metadata).await?;
            }
            ValidationLevel::Deep => {
                self.validate_deep(&metadata).await?;
            }
            ValidationLevel::Paranoid => {
                self.validate_paranoid(&metadata).await?;
            }
        }
        
        // Generate report
        let state = self.state.lock().await;
        let is_valid = state.errors.is_empty();
        
        let report = ValidationReport {
            backup_id: backup_id.to_string(),
            level,
            is_valid,
            timestamp: Utc::now(),
            duration: start_time.elapsed(),
            items_checked: state.items_checked,
            items_failed: state.items_failed,
            errors: state.errors.clone(),
            warnings: state.warnings.clone(),
            stats: state.stats.clone(),
        };
        
        info!("Validation completed in {:?}: {} items checked, {} errors, {} warnings",
              report.duration, report.items_checked, report.errors.len(), report.warnings.len());
        
        Ok(report)
    }
    
    /// Load backup metadata
    async fn load_backup_metadata(&self, backup_id: &str) -> Result<BackupMetadata> {
        let key = format!("backups/{}/backup.json", backup_id);
        
        let data = self.backup_store.get(&key).await
            .map_err(|e| BackupError::NotFound(format!("Backup {} not found: {}", backup_id, e)))?;
        
        let metadata: BackupMetadata = serde_json::from_slice(&data)?;
        
        Ok(metadata)
    }
    
    /// Quick validation (metadata only)
    async fn validate_quick(&self, metadata: &BackupMetadata) -> Result<()> {
        debug!("Performing quick validation");
        
        // Validate metadata structure
        self.validate_metadata_structure(metadata).await?;
        
        // Check backup manifest exists
        self.check_backup_manifest(metadata).await?;
        
        // Update stats
        let mut state = self.state.lock().await;
        state.stats.topics_validated = metadata.topics.len() as u32;
        state.items_checked += 1;
        
        Ok(())
    }
    
    /// Standard validation (metadata + checksums)
    async fn validate_standard(&self, metadata: &BackupMetadata) -> Result<()> {
        debug!("Performing standard validation");
        
        // Do quick validation first
        self.validate_quick(metadata).await?;
        
        // Validate backup checksum
        self.validate_backup_checksum(metadata).await?;
        
        // Validate topic metadata
        for topic in &metadata.topics {
            self.validate_topic_metadata(topic, metadata).await?;
        }
        
        Ok(())
    }
    
    /// Deep validation (full data verification)
    async fn validate_deep(&self, metadata: &BackupMetadata) -> Result<()> {
        debug!("Performing deep validation");
        
        // Do standard validation first
        self.validate_standard(metadata).await?;
        
        // Validate all segments
        for topic in &metadata.topics {
            self.validate_topic_segments(topic, metadata).await?;
        }
        
        // Validate metadata backup
        self.validate_metadata_backup(metadata).await?;
        
        Ok(())
    }
    
    /// Paranoid validation (deep + cross-references)
    async fn validate_paranoid(&self, metadata: &BackupMetadata) -> Result<()> {
        debug!("Performing paranoid validation");
        
        // Do deep validation first
        self.validate_deep(metadata).await?;
        
        // Cross-reference validation
        self.validate_cross_references(metadata).await?;
        
        // Validate incremental chain if applicable
        if let Some(ref parent_id) = metadata.parent_backup_id {
            self.validate_incremental_chain(metadata, parent_id).await?;
        }
        
        // Validate segment ordering
        self.validate_segment_ordering(metadata).await?;
        
        Ok(())
    }
    
    /// Validate metadata structure
    async fn validate_metadata_structure(&self, metadata: &BackupMetadata) -> Result<()> {
        let mut state = self.state.lock().await;
        
        // Check required fields
        if metadata.backup_id.is_empty() {
            state.errors.push(ValidationError {
                error_type: ValidationErrorType::FormatError,
                item: "metadata".to_string(),
                message: "Backup ID is empty".to_string(),
            });
        }
        
        if metadata.version.is_empty() {
            state.errors.push(ValidationError {
                error_type: ValidationErrorType::FormatError,
                item: "metadata".to_string(),
                message: "Version is empty".to_string(),
            });
        }
        
        // Check consistency
        let calculated_size: u64 = metadata.topics.iter()
            .map(|t| t.total_size)
            .sum();
        
        if calculated_size != metadata.total_size {
            state.warnings.push(ValidationWarning {
                warning_type: ValidationWarningType::PotentialIssue,
                item: "metadata".to_string(),
                message: format!("Total size mismatch: metadata says {}, calculated {}",
                               metadata.total_size, calculated_size),
            });
        }
        
        state.items_checked += 1;
        
        Ok(())
    }
    
    /// Check backup manifest exists
    async fn check_backup_manifest(&self, metadata: &BackupMetadata) -> Result<()> {
        let key = format!("backups/{}/backup.json", metadata.backup_id);
        
        match self.backup_store.head(&key).await {
            Ok(_) => {
                let mut state = self.state.lock().await;
                state.items_checked += 1;
                Ok(())
            }
            Err(e) => {
                let mut state = self.state.lock().await;
                state.errors.push(ValidationError {
                    error_type: ValidationErrorType::NotFound,
                    item: key,
                    message: e.to_string(),
                });
                state.items_failed += 1;
                Ok(())
            }
        }
    }
    
    /// Validate backup checksum
    async fn validate_backup_checksum(&self, metadata: &BackupMetadata) -> Result<()> {
        // Recalculate checksum
        let data = serde_json::to_vec(metadata)
            .map_err(|e| BackupError::Serialization(e.to_string()))?;
        
        let mut hasher = Sha256::new();
        hasher.update(&data);
        let result = hasher.finalize();
        let calculated = hex::encode(result);
        
        let mut state = self.state.lock().await;
        
        if calculated != metadata.checksum {
            state.errors.push(ValidationError {
                error_type: ValidationErrorType::ChecksumMismatch,
                item: "backup.json".to_string(),
                message: format!("Expected {}, got {}", metadata.checksum, calculated),
            });
            state.items_failed += 1;
        }
        
        state.items_checked += 1;
        state.stats.checksums_verified += 1;
        
        Ok(())
    }
    
    /// Validate topic metadata
    async fn validate_topic_metadata(
        &self,
        topic: &TopicBackupInfo,
        metadata: &BackupMetadata,
    ) -> Result<()> {
        let mut state = self.state.lock().await;
        
        // Check segment count matches
        if topic.segments.len() != topic.segment_count as usize {
            state.errors.push(ValidationError {
                error_type: ValidationErrorType::FormatError,
                item: format!("topic/{}", topic.name),
                message: format!("Segment count mismatch: metadata says {}, found {}",
                               topic.segment_count, topic.segments.len()),
            });
            state.items_failed += 1;
        }
        
        // Check total size
        let calculated_size: u64 = topic.segments.iter()
            .map(|s| s.size)
            .sum();
        
        if calculated_size != topic.total_size {
            state.warnings.push(ValidationWarning {
                warning_type: ValidationWarningType::PotentialIssue,
                item: format!("topic/{}", topic.name),
                message: format!("Size mismatch: metadata says {}, calculated {}",
                               topic.total_size, calculated_size),
            });
        }
        
        state.items_checked += 1;
        
        Ok(())
    }
    
    /// Validate topic segments
    async fn validate_topic_segments(
        &self,
        topic: &TopicBackupInfo,
        metadata: &BackupMetadata,
    ) -> Result<()> {
        let segment_futures = topic.segments.iter()
            .map(|segment| self.validate_segment(segment, metadata))
            .collect::<Vec<_>>();
        
        futures::future::join_all(segment_futures).await;
        
        let mut state = self.state.lock().await;
        state.stats.topics_validated += 1;
        
        Ok(())
    }
    
    /// Validate individual segment
    async fn validate_segment(
        &self,
        segment: &SegmentBackupRef,
        metadata: &BackupMetadata,
    ) -> Result<()> {
        let _permit = self.semaphore.acquire().await.unwrap();
        
        let key = format!("backups/{}/{}", metadata.backup_id, segment.backup_path);
        
        // Check segment exists
        match self.backup_store.head(&key).await {
            Ok(object_meta) => {
                let mut state = self.state.lock().await;
                
                // Check size
                if object_meta.size != segment.size as usize {
                    state.errors.push(ValidationError {
                        error_type: ValidationErrorType::SizeMismatch,
                        item: key.clone(),
                        message: format!("Expected {} bytes, found {}",
                                       segment.size, object_meta.size),
                    });
                    state.items_failed += 1;
                }
                
                // Deep validation: verify checksum
                if let Ok(data) = self.backup_store.get(&key).await {
                    let checksum = self.calculate_checksum(&data);
                    if checksum != segment.checksum {
                        state.errors.push(ValidationError {
                            error_type: ValidationErrorType::ChecksumMismatch,
                            item: key.clone(),
                            message: format!("Expected {}, got {}", segment.checksum, checksum),
                        });
                        state.items_failed += 1;
                    }
                    state.stats.checksums_verified += 1;
                    state.stats.bytes_validated += data.len() as u64;
                }
                
                state.items_checked += 1;
                state.stats.segments_validated += 1;
                
                // Update timestamp range
                if state.stats.oldest_segment.is_none() || 
                   segment.timestamp < state.stats.oldest_segment.unwrap() {
                    state.stats.oldest_segment = Some(segment.timestamp);
                }
                
                if state.stats.newest_segment.is_none() || 
                   segment.timestamp > state.stats.newest_segment.unwrap() {
                    state.stats.newest_segment = Some(segment.timestamp);
                }
            }
            Err(e) => {
                let mut state = self.state.lock().await;
                state.errors.push(ValidationError {
                    error_type: ValidationErrorType::NotFound,
                    item: key,
                    message: e.to_string(),
                });
                state.items_failed += 1;
            }
        }
        
        Ok(())
    }
    
    /// Validate metadata backup
    async fn validate_metadata_backup(&self, metadata: &BackupMetadata) -> Result<()> {
        let key = format!("backups/{}/metadata.bin", metadata.backup_id);
        
        match self.backup_store.get(&key).await {
            Ok(data) => {
                // Try to decompress and deserialize
                match compression::decompress(&data, metadata.compression) {
                    Ok(decompressed) => {
                        match bincode::deserialize::<crate::MetadataBackup>(&decompressed) {
                            Ok(_) => {
                                let mut state = self.state.lock().await;
                                state.items_checked += 1;
                            }
                            Err(e) => {
                                let mut state = self.state.lock().await;
                                state.errors.push(ValidationError {
                                    error_type: ValidationErrorType::FormatError,
                                    item: key,
                                    message: format!("Failed to deserialize: {}", e),
                                });
                                state.items_failed += 1;
                            }
                        }
                    }
                    Err(e) => {
                        let mut state = self.state.lock().await;
                        state.errors.push(ValidationError {
                            error_type: ValidationErrorType::Corruption,
                            item: key,
                            message: format!("Failed to decompress: {}", e),
                        });
                        state.items_failed += 1;
                    }
                }
            }
            Err(e) => {
                let mut state = self.state.lock().await;
                state.errors.push(ValidationError {
                    error_type: ValidationErrorType::NotFound,
                    item: key,
                    message: e.to_string(),
                });
                state.items_failed += 1;
            }
        }
        
        Ok(())
    }
    
    /// Validate cross references
    async fn validate_cross_references(&self, metadata: &BackupMetadata) -> Result<()> {
        // Validate all segment references are unique
        let mut seen_segments = HashSet::new();
        let mut state = self.state.lock().await;
        
        for topic in &metadata.topics {
            for segment in &topic.segments {
                let key = format!("{}/{}/{}", segment.topic, segment.partition, segment.segment_id);
                if !seen_segments.insert(key.clone()) {
                    state.warnings.push(ValidationWarning {
                        warning_type: ValidationWarningType::PotentialIssue,
                        item: key,
                        message: "Duplicate segment reference".to_string(),
                    });
                }
            }
        }
        
        Ok(())
    }
    
    /// Validate incremental backup chain
    async fn validate_incremental_chain(
        &self,
        metadata: &BackupMetadata,
        parent_id: &str,
    ) -> Result<()> {
        // Check parent backup exists
        match self.load_backup_metadata(parent_id).await {
            Ok(parent) => {
                let mut state = self.state.lock().await;
                
                // Verify parent is older
                if parent.timestamp >= metadata.timestamp {
                    state.errors.push(ValidationError {
                        error_type: ValidationErrorType::ReferenceError,
                        item: "incremental_chain".to_string(),
                        message: format!("Parent backup {} is newer than child",
                                       parent_id),
                    });
                }
                
                state.items_checked += 1;
            }
            Err(e) => {
                let mut state = self.state.lock().await;
                state.errors.push(ValidationError {
                    error_type: ValidationErrorType::ReferenceError,
                    item: "parent_backup".to_string(),
                    message: format!("Parent backup {} not found: {}", parent_id, e),
                });
                state.items_failed += 1;
            }
        }
        
        Ok(())
    }
    
    /// Validate segment ordering
    async fn validate_segment_ordering(&self, metadata: &BackupMetadata) -> Result<()> {
        let mut state = self.state.lock().await;
        
        for topic in &metadata.topics {
            // Group segments by partition
            let mut partitions: HashMap<u32, Vec<&SegmentBackupRef>> = HashMap::new();
            for segment in &topic.segments {
                partitions.entry(segment.partition)
                    .or_insert_with(Vec::new)
                    .push(segment);
            }
            
            // Check ordering within each partition
            for (partition, segments) in partitions {
                let mut sorted = segments.clone();
                sorted.sort_by_key(|s| s.start_offset);
                
                for i in 1..sorted.len() {
                    if sorted[i-1].end_offset >= sorted[i].start_offset {
                        state.warnings.push(ValidationWarning {
                            warning_type: ValidationWarningType::PotentialIssue,
                            item: format!("{}/partition-{}", topic.name, partition),
                            message: "Overlapping segment offsets detected".to_string(),
                        });
                    }
                }
            }
        }
        
        Ok(())
    }
    
    /// Calculate checksum
    fn calculate_checksum(&self, data: &[u8]) -> String {
        let mut hasher = Sha256::new();
        hasher.update(data);
        let result = hasher.finalize();
        hex::encode(result)
    }
}

/// Generate validation report summary
impl ValidationReport {
    /// Get a human-readable summary
    pub fn summary(&self) -> String {
        let status = if self.is_valid { "VALID" } else { "INVALID" };
        
        format!(
            "Backup {} is {}\n\
             Validation Level: {:?}\n\
             Duration: {:?}\n\
             Items Checked: {}\n\
             Items Failed: {}\n\
             Errors: {}\n\
             Warnings: {}\n\
             Topics: {}\n\
             Segments: {}\n\
             Data Validated: {} MB\n\
             Checksums Verified: {}",
            self.backup_id,
            status,
            self.level,
            self.duration,
            self.items_checked,
            self.items_failed,
            self.errors.len(),
            self.warnings.len(),
            self.stats.topics_validated,
            self.stats.segments_validated,
            self.stats.bytes_validated / (1024 * 1024),
            self.stats.checksums_verified
        )
    }
}

/// Hex encoding utility
mod hex {
    pub fn encode(data: impl AsRef<[u8]>) -> String {
        data.as_ref()
            .iter()
            .map(|byte| format!("{:02x}", byte))
            .collect()
    }
}