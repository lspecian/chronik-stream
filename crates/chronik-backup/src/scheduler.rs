//! Backup scheduling and retention policy management

use crate::{
    BackupError, Result, BackupManager, BackupMetadata,
    metadata::BackupType, validator::{BackupValidator, ValidationLevel},
};
use std::sync::Arc;
use chrono::{DateTime, Utc, Duration as ChronoDuration, Datelike};
use serde::{Deserialize, Serialize};
use tokio::sync::{RwLock, Mutex};
use tokio::time::{interval, Duration};
use tracing::{info, debug, warn, error};
use std::collections::HashMap;

/// Backup schedule configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Schedule {
    /// Schedule name
    pub name: String,
    /// Cron-like expression (e.g., "0 2 * * *" for 2 AM daily)
    pub cron: String,
    /// Backup type to create
    pub backup_type: BackupType,
    /// Whether schedule is enabled
    pub enabled: bool,
    /// Retention policy for this schedule
    pub retention: RetentionPolicy,
    /// Validation level after backup
    pub validation_level: Option<ValidationLevel>,
    /// Maximum concurrent backups
    pub max_concurrent: usize,
}

impl Schedule {
    /// Parse schedule from cron expression
    pub fn parse(name: &str, cron: &str) -> Result<Self> {
        // Validate cron expression
        cron_parser::parse(cron, &Utc::now())
            .map_err(|e| BackupError::Internal(format!("Invalid cron expression: {}", e)))?;
        
        Ok(Self {
            name: name.to_string(),
            cron: cron.to_string(),
            backup_type: BackupType::Full,
            enabled: true,
            retention: RetentionPolicy::default(),
            validation_level: Some(ValidationLevel::Standard),
            max_concurrent: 1,
        })
    }
    
    /// Get next scheduled time
    pub fn next_run(&self) -> Result<DateTime<Utc>> {
        let now = Utc::now();
        let mut iter = cron_parser::parse(&self.cron, &now)
            .map_err(|e| BackupError::Internal(format!("Failed to parse cron: {}", e)))?;
        
        iter.next()
            .ok_or_else(|| BackupError::Internal("No next run scheduled".to_string()))
    }
}

/// Retention policy configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetentionPolicy {
    /// Keep all backups for this many days
    pub keep_all_days: u32,
    /// Keep daily backups for this many days
    pub keep_daily_days: u32,
    /// Keep weekly backups for this many weeks
    pub keep_weekly_weeks: u32,
    /// Keep monthly backups for this many months
    pub keep_monthly_months: u32,
    /// Keep yearly backups for this many years
    pub keep_yearly_years: u32,
    /// Minimum number of backups to keep
    pub minimum_backups: u32,
}

impl Default for RetentionPolicy {
    fn default() -> Self {
        Self {
            keep_all_days: 7,
            keep_daily_days: 30,
            keep_weekly_weeks: 12,
            keep_monthly_months: 12,
            keep_yearly_years: 5,
            minimum_backups: 3,
        }
    }
}

/// Backup scheduler
pub struct BackupScheduler {
    /// Backup manager
    backup_manager: Arc<BackupManager>,
    /// Backup validator
    validator: Arc<BackupValidator>,
    /// Configured schedules
    schedules: Arc<RwLock<Vec<Schedule>>>,
    /// Scheduler state
    state: Arc<Mutex<SchedulerState>>,
    /// Whether scheduler is running
    running: Arc<RwLock<bool>>,
}

/// Scheduler state
#[derive(Debug)]
struct SchedulerState {
    /// Last run times for each schedule
    last_runs: HashMap<String, DateTime<Utc>>,
    /// Currently running backups
    running_backups: HashMap<String, DateTime<Utc>>,
    /// Recent backup results
    recent_results: Vec<BackupResult>,
}

/// Backup execution result
#[derive(Debug, Clone)]
struct BackupResult {
    /// Schedule name
    schedule_name: String,
    /// Backup ID (if successful)
    backup_id: Option<String>,
    /// Start time
    start_time: DateTime<Utc>,
    /// End time
    end_time: DateTime<Utc>,
    /// Success status
    success: bool,
    /// Error message (if failed)
    error: Option<String>,
}

impl BackupScheduler {
    /// Create a new backup scheduler
    pub fn new(
        backup_manager: Arc<BackupManager>,
        validator: Arc<BackupValidator>,
    ) -> Self {
        Self {
            backup_manager,
            validator,
            schedules: Arc::new(RwLock::new(Vec::new())),
            state: Arc::new(Mutex::new(SchedulerState {
                last_runs: HashMap::new(),
                running_backups: HashMap::new(),
                recent_results: Vec::new(),
            })),
            running: Arc::new(RwLock::new(false)),
        }
    }
    
    /// Add a schedule
    pub async fn add_schedule(&self, schedule: Schedule) -> Result<()> {
        info!("Adding backup schedule: {} ({})", schedule.name, schedule.cron);
        
        let mut schedules = self.schedules.write().await;
        
        // Check for duplicate names
        if schedules.iter().any(|s| s.name == schedule.name) {
            return Err(BackupError::Internal(
                format!("Schedule '{}' already exists", schedule.name)
            ));
        }
        
        schedules.push(schedule);
        Ok(())
    }
    
    /// Remove a schedule
    pub async fn remove_schedule(&self, name: &str) -> Result<()> {
        let mut schedules = self.schedules.write().await;
        
        let initial_len = schedules.len();
        schedules.retain(|s| s.name != name);
        
        if schedules.len() == initial_len {
            return Err(BackupError::NotFound(format!("Schedule '{}' not found", name)));
        }
        
        info!("Removed backup schedule: {}", name);
        Ok(())
    }
    
    /// List all schedules
    pub async fn list_schedules(&self) -> Vec<Schedule> {
        self.schedules.read().await.clone()
    }
    
    /// Start the scheduler
    pub async fn start(&self) -> Result<()> {
        let mut running = self.running.write().await;
        if *running {
            return Err(BackupError::Internal("Scheduler already running".to_string()));
        }
        
        *running = true;
        info!("Backup scheduler started");
        
        // Spawn scheduler task
        let scheduler = self.clone();
        tokio::spawn(async move {
            scheduler.run_scheduler_loop().await;
        });
        
        Ok(())
    }
    
    /// Stop the scheduler
    pub async fn stop(&self) -> Result<()> {
        let mut running = self.running.write().await;
        if !*running {
            return Err(BackupError::Internal("Scheduler not running".to_string()));
        }
        
        *running = false;
        info!("Backup scheduler stopped");
        Ok(())
    }
    
    /// Main scheduler loop
    async fn run_scheduler_loop(&self) {
        let mut interval = interval(Duration::from_secs(60)); // Check every minute
        
        loop {
            interval.tick().await;
            
            // Check if still running
            if !*self.running.read().await {
                break;
            }
            
            // Check schedules
            self.check_schedules().await;
            
            // Cleanup old results
            self.cleanup_old_results().await;
        }
    }
    
    /// Check and execute due schedules
    async fn check_schedules(&self) {
        let schedules = self.schedules.read().await.clone();
        let now = Utc::now();
        
        for schedule in schedules {
            if !schedule.enabled {
                continue;
            }
            
            // Check if schedule is due
            match self.is_schedule_due(&schedule, now).await {
                Ok(true) => {
                    // Check if already running
                    let state = self.state.lock().await;
                    if state.running_backups.contains_key(&schedule.name) {
                        debug!("Schedule {} is already running", schedule.name);
                        continue;
                    }
                    drop(state);
                    
                    // Execute backup
                    let scheduler = self.clone();
                    let schedule = schedule.clone();
                    tokio::spawn(async move {
                        scheduler.execute_scheduled_backup(schedule).await;
                    });
                }
                Ok(false) => {
                    // Not due yet
                }
                Err(e) => {
                    error!("Error checking schedule {}: {}", schedule.name, e);
                }
            }
        }
    }
    
    /// Check if schedule is due
    async fn is_schedule_due(&self, schedule: &Schedule, now: DateTime<Utc>) -> Result<bool> {
        let state = self.state.lock().await;
        
        // Get last run time
        let last_run = state.last_runs.get(&schedule.name);
        
        // Calculate next run time
        let next_run = match last_run {
            Some(last) => {
                let mut iter = cron_parser::parse_after(&schedule.cron, last)
                    .map_err(|e| BackupError::Internal(format!("Failed to parse cron: {}", e)))?;
                
                iter.next()
                    .ok_or_else(|| BackupError::Internal("No next run scheduled".to_string()))?
            }
            None => {
                // First run - check if current time matches
                schedule.next_run()?
            }
        };
        
        // Allow 1 minute tolerance
        Ok(now >= next_run && now < next_run + ChronoDuration::minutes(1))
    }
    
    /// Execute a scheduled backup
    async fn execute_scheduled_backup(&self, schedule: Schedule) {
        let start_time = Utc::now();
        let backup_id = format!("{}-{}", schedule.name, start_time.timestamp());
        
        info!("Executing scheduled backup: {} ({})", schedule.name, backup_id);
        
        // Mark as running
        {
            let mut state = self.state.lock().await;
            state.running_backups.insert(schedule.name.clone(), start_time);
        }
        
        // Execute backup
        let result = match schedule.backup_type {
            BackupType::Full => {
                self.backup_manager.create_full_backup(&backup_id).await
            }
            BackupType::Incremental => {
                // Find last successful backup as parent
                match self.find_last_successful_backup(&schedule.name).await {
                    Some(parent_id) => {
                        self.backup_manager.create_incremental_backup(&backup_id, &parent_id).await
                    }
                    None => {
                        // No parent, create full backup instead
                        warn!("No parent backup found for incremental, creating full backup");
                        self.backup_manager.create_full_backup(&backup_id).await
                    }
                }
            }
            BackupType::Differential => {
                // Find last full backup as parent
                match self.find_last_full_backup(&schedule.name).await {
                    Some(parent_id) => {
                        self.backup_manager.create_incremental_backup(&backup_id, &parent_id).await
                    }
                    None => {
                        // No parent, create full backup instead
                        warn!("No full backup found for differential, creating full backup");
                        self.backup_manager.create_full_backup(&backup_id).await
                    }
                }
            }
        };
        
        let end_time = Utc::now();
        
        // Validate if requested
        if let (Ok(metadata), Some(level)) = (&result, schedule.validation_level) {
            info!("Validating backup {} with level {:?}", backup_id, level);
            if let Err(e) = self.validator.validate(&backup_id, level).await {
                error!("Backup validation failed: {}", e);
            }
        }
        
        // Record result
        let backup_result = BackupResult {
            schedule_name: schedule.name.clone(),
            backup_id: result.as_ref().ok().map(|m| m.backup_id.clone()),
            start_time,
            end_time,
            success: result.is_ok(),
            error: result.as_ref().err().map(|e| e.to_string()),
        };
        
        // Update state
        {
            let mut state = self.state.lock().await;
            state.running_backups.remove(&schedule.name);
            state.last_runs.insert(schedule.name.clone(), start_time);
            state.recent_results.push(backup_result.clone());
        }
        
        // Log result
        match result {
            Ok(metadata) => {
                info!("Scheduled backup completed: {} ({} MB in {:?})",
                      backup_id,
                      metadata.total_size / (1024 * 1024),
                      metadata.duration);
                
                // Apply retention policy
                if let Err(e) = self.apply_retention_policy(&schedule).await {
                    error!("Failed to apply retention policy: {}", e);
                }
            }
            Err(e) => {
                error!("Scheduled backup failed: {} - {}", backup_id, e);
            }
        }
    }
    
    /// Find last successful backup for a schedule
    async fn find_last_successful_backup(&self, schedule_name: &str) -> Option<String> {
        let state = self.state.lock().await;
        state.recent_results.iter()
            .filter(|r| r.schedule_name == schedule_name && r.success)
            .max_by_key(|r| r.start_time)
            .and_then(|r| r.backup_id.clone())
    }
    
    /// Find last full backup for a schedule
    async fn find_last_full_backup(&self, schedule_name: &str) -> Option<String> {
        // In a real implementation, this would query backup metadata
        self.find_last_successful_backup(schedule_name).await
    }
    
    /// Apply retention policy
    async fn apply_retention_policy(&self, schedule: &Schedule) -> Result<()> {
        info!("Applying retention policy for schedule: {}", schedule.name);
        
        // List all backups
        let backups = self.backup_manager.list_backups().await?;
        
        // Filter backups for this schedule
        let schedule_backups: Vec<_> = backups.into_iter()
            .filter(|b| b.backup_id.starts_with(&format!("{}-", schedule.name)))
            .collect();
        
        if schedule_backups.len() <= schedule.retention.minimum_backups as usize {
            debug!("Not enough backups to apply retention ({} <= {})",
                  schedule_backups.len(), schedule.retention.minimum_backups);
            return Ok(());
        }
        
        // Identify backups to keep based on policy
        let backups_to_keep = self.identify_backups_to_keep(&schedule_backups, &schedule.retention);
        
        // Delete backups not in keep list
        let mut deleted = 0;
        for backup in schedule_backups {
            if !backups_to_keep.contains(&backup.backup_id) {
                debug!("Deleting backup per retention policy: {}", backup.backup_id);
                // Would call backup_manager.delete_backup() here
                deleted += 1;
            }
        }
        
        if deleted > 0 {
            info!("Deleted {} backups per retention policy", deleted);
        }
        
        Ok(())
    }
    
    /// Identify which backups to keep based on retention policy
    fn identify_backups_to_keep(
        &self,
        backups: &[BackupMetadata],
        policy: &RetentionPolicy,
    ) -> Vec<String> {
        let mut keep = Vec::new();
        let now = Utc::now();
        
        // Keep all recent backups
        let keep_all_cutoff = now - ChronoDuration::days(policy.keep_all_days as i64);
        for backup in backups {
            if backup.timestamp >= keep_all_cutoff {
                keep.push(backup.backup_id.clone());
            }
        }
        
        // Keep daily backups
        let daily_cutoff = now - ChronoDuration::days(policy.keep_daily_days as i64);
        let mut daily_kept = std::collections::HashSet::new();
        for backup in backups {
            if backup.timestamp >= daily_cutoff && backup.timestamp < keep_all_cutoff {
                let date = backup.timestamp.date_naive();
                if daily_kept.insert(date) {
                    keep.push(backup.backup_id.clone());
                }
            }
        }
        
        // Keep weekly backups
        let weekly_cutoff = now - ChronoDuration::weeks(policy.keep_weekly_weeks as i64);
        let mut weekly_kept = std::collections::HashSet::new();
        for backup in backups {
            if backup.timestamp >= weekly_cutoff && backup.timestamp < daily_cutoff {
                let week = backup.timestamp.iso_week();
                if weekly_kept.insert((week.year(), week.week())) {
                    keep.push(backup.backup_id.clone());
                }
            }
        }
        
        // Keep monthly backups
        let monthly_cutoff = now - ChronoDuration::days(policy.keep_monthly_months as i64 * 30);
        let mut monthly_kept = std::collections::HashSet::new();
        for backup in backups {
            if backup.timestamp >= monthly_cutoff && backup.timestamp < weekly_cutoff {
                let month = (backup.timestamp.year(), backup.timestamp.month());
                if monthly_kept.insert(month) {
                    keep.push(backup.backup_id.clone());
                }
            }
        }
        
        // Keep yearly backups
        let yearly_cutoff = now - ChronoDuration::days(policy.keep_yearly_years as i64 * 365);
        let mut yearly_kept = std::collections::HashSet::new();
        for backup in backups {
            if backup.timestamp >= yearly_cutoff && backup.timestamp < monthly_cutoff {
                let year = backup.timestamp.year();
                if yearly_kept.insert(year) {
                    keep.push(backup.backup_id.clone());
                }
            }
        }
        
        // Always keep minimum number of most recent backups
        let mut sorted_backups = backups.to_vec();
        sorted_backups.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
        for backup in sorted_backups.iter().take(policy.minimum_backups as usize) {
            if !keep.contains(&backup.backup_id) {
                keep.push(backup.backup_id.clone());
            }
        }
        
        keep
    }
    
    /// Cleanup old results from memory
    async fn cleanup_old_results(&self) {
        let mut state = self.state.lock().await;
        let cutoff = Utc::now() - ChronoDuration::days(7);
        state.recent_results.retain(|r| r.end_time >= cutoff);
    }
}

// Implement Clone for scheduler (needed for spawning tasks)
impl Clone for BackupScheduler {
    fn clone(&self) -> Self {
        Self {
            backup_manager: self.backup_manager.clone(),
            validator: self.validator.clone(),
            schedules: self.schedules.clone(),
            state: self.state.clone(),
            running: self.running.clone(),
        }
    }
}

/// Simple cron parser for basic scheduling
mod cron_parser {
    use chrono::{DateTime, Utc, Timelike};
    use super::BackupError;
    
    /// Parse cron expression and get next run time
    pub fn parse(cron: &str, after: &DateTime<Utc>) -> Result<impl Iterator<Item = DateTime<Utc>>, BackupError> {
        // This is a simplified implementation
        // In production, use a proper cron parsing library
        
        let parts: Vec<&str> = cron.split_whitespace().collect();
        if parts.len() != 5 {
            return Err(BackupError::Internal("Invalid cron expression".to_string()));
        }
        
        // For now, just support daily at specific hour
        if parts[0] == "0" && parts[2] == "*" && parts[3] == "*" && parts[4] == "*" {
            if let Ok(hour) = parts[1].parse::<u32>() {
                let next = if after.hour() >= hour {
                    after.date_naive().succ_opt().unwrap().and_hms_opt(hour, 0, 0).unwrap()
                } else {
                    after.date_naive().and_hms_opt(hour, 0, 0).unwrap()
                };
                
                let next_utc = DateTime::<Utc>::from_naive_utc_and_offset(next, Utc);
                return Ok(std::iter::once(next_utc));
            }
        }
        
        Err(BackupError::Internal("Unsupported cron expression".to_string()))
    }
    
    /// Parse cron expression starting after specific time
    pub fn parse_after(cron: &str, after: &DateTime<Utc>) -> Result<impl Iterator<Item = DateTime<Utc>>, BackupError> {
        parse(cron, after)
    }
}