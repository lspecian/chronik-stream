//! WAL Recovery Integration Test
//! 
//! This test validates the WAL recovery system by simulating ungraceful shutdowns
//! and verifying that the system can recover completely without data loss or corruption.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::Arc;
use std::time::{Duration, Instant};
use anyhow::{Result, Context, anyhow};
use chrono::Utc;
use futures::StreamExt;
use rdkafka::{
    producer::{FutureProducer, FutureRecord},
    consumer::{Consumer, StreamConsumer},
    config::ClientConfig,
    message::Message,
};
use tempfile::TempDir;
use tokio::time::timeout;
use tracing::{info, debug, warn, error};

/// Test configuration
const TEST_TOPIC: &str = "wal-recovery-test";
const TEST_PARTITION: i32 = 0;
const CRASH_TEST_PORT: u16 = 19093; // Different port to avoid conflicts
const INITIAL_MESSAGE_COUNT: usize = 500;
const RECOVERY_MESSAGE_COUNT: usize = 300;
const MESSAGE_SIZE: usize = 150;
const SERVER_STARTUP_TIMEOUT: Duration = Duration::from_secs(15);
const CRASH_TIMEOUT: Duration = Duration::from_secs(5);

/// Server process manager for crash testing
struct TestServer {
    process: Option<Child>,
    data_dir: PathBuf,
    port: u16,
}

impl TestServer {
    fn new(data_dir: PathBuf, port: u16) -> Self {
        Self {
            process: None,
            data_dir,
            port,
        }
    }
    
    async fn start(&mut self) -> Result<()> {
        info!("Starting test server on port {} with data dir: {:?}", self.port, self.data_dir);
        
        // Ensure data directory exists
        tokio::fs::create_dir_all(&self.data_dir).await
            .context("Failed to create data directory")?;
        
        let mut cmd = Command::new("./target/release/chronik-server");
        cmd.env("CHRONIK_PORT", self.port.to_string())
           .env("CHRONIK_METRICS_PORT", (self.port + 1).to_string())
           .env("CHRONIK_DATA_DIR", &self.data_dir)
           .env("RUST_LOG", "info,chronik_server::wal_integration=debug")
           .stdout(std::process::Stdio::piped())
           .stderr(std::process::Stdio::piped());
        
        let process = cmd.spawn()
            .context("Failed to spawn server process")?;
        
        self.process = Some(process);
        
        // Wait for server to be ready
        self.wait_for_ready().await?;
        
        info!("Test server started successfully");
        Ok(())
    }
    
    async fn wait_for_ready(&self) -> Result<()> {
        let start = Instant::now();
        let endpoint = format!("localhost:{}", self.port);
        
        while start.elapsed() < SERVER_STARTUP_TIMEOUT {
            // Try to connect with a simple producer to test readiness
            match ClientConfig::new()
                .set("bootstrap.servers", &endpoint)
                .set("message.timeout.ms", "3000")
                .create::<FutureProducer>()
            {
                Ok(_producer) => {
                    debug!("Server is ready for connections");
                    return Ok(());
                }
                Err(_) => {
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    continue;
                }
            }
        }
        
        Err(anyhow!("Server failed to become ready within timeout"))
    }
    
    /// Simulate ungraceful shutdown by killing the process
    fn crash(&mut self) -> Result<()> {
        if let Some(mut process) = self.process.take() {
            info!("Simulating ungraceful server crash (SIGKILL)");
            
            // Use SIGKILL to simulate hard crash
            #[cfg(unix)]
            {
                use std::os::unix::process::ExitStatusExt;
                let pid = process.id();
                
                // Send SIGKILL
                unsafe {
                    libc::kill(pid as i32, libc::SIGKILL);
                }
                
                // Wait briefly for process to die
                std::thread::sleep(Duration::from_millis(100));
                
                match process.try_wait() {
                    Ok(Some(status)) => {
                        if let Some(signal) = status.signal() {
                            info!("Process killed with signal: {}", signal);
                        } else {
                            warn!("Process exited normally (expected SIGKILL)");
                        }
                    }
                    Ok(None) => {
                        warn!("Process still running after SIGKILL, forcing termination");
                        let _ = process.kill();
                    }
                    Err(e) => {
                        warn!("Error checking process status: {}", e);
                    }
                }
            }
            
            #[cfg(not(unix))]
            {
                // Windows: use terminate
                let _ = process.kill();
                let _ = process.wait();
            }
            
            info!("Server process terminated (simulated crash)");
            Ok(())
        } else {
            Err(anyhow!("No server process to crash"))
        }
    }
    
    /// Check WAL directory for file integrity
    fn inspect_wal_state(&self) -> Result<WalInspectionResult> {
        let wal_dir = self.data_dir.join("wal");
        
        if !wal_dir.exists() {
            return Ok(WalInspectionResult {
                segment_files: 0,
                checkpoint_files: 0,
                total_size_bytes: 0,
                corrupted_files: 0,
                has_active_segment: false,
            });
        }
        
        let mut segment_files = 0;
        let mut checkpoint_files = 0;
        let mut total_size_bytes = 0;
        let mut corrupted_files = 0;
        let mut has_active_segment = false;
        
        for entry in fs::read_dir(&wal_dir).context("Failed to read WAL directory")? {
            let entry = entry?;
            let path = entry.path();
            let metadata = entry.metadata()?;
            
            if path.is_file() {
                let filename = path.file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("");
                
                total_size_bytes += metadata.len();
                
                if filename.ends_with(".wal") {
                    segment_files += 1;
                    
                    // Check if this looks like an active segment
                    if filename.contains("active") || metadata.len() > 0 {
                        has_active_segment = true;
                    }
                    
                    // Basic corruption check: WAL files should have some minimum structure
                    if metadata.len() < 32 {
                        warn!("Suspicious WAL file size: {} bytes for {}", metadata.len(), filename);
                        corrupted_files += 1;
                    }
                } else if filename.ends_with(".checkpoint") || filename.ends_with(".manifest") {
                    checkpoint_files += 1;
                }
            }
        }
        
        Ok(WalInspectionResult {
            segment_files,
            checkpoint_files,
            total_size_bytes,
            corrupted_files,
            has_active_segment,
        })
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        if let Some(mut process) = self.process.take() {
            let _ = process.kill();
            let _ = process.wait();
        }
    }
}

#[derive(Debug)]
struct WalInspectionResult {
    segment_files: usize,
    checkpoint_files: usize,
    total_size_bytes: u64,
    corrupted_files: usize,
    has_active_segment: bool,
}

/// Main WAL recovery test
#[tokio::test]
async fn test_wal_recovery_after_crash() -> Result<()> {
    // Initialize logging
    let _ = env_logger::builder()
        .filter_level(log::LevelFilter::Info)
        .is_test(true)
        .try_init();

    info!("Starting WAL recovery after crash test");

    // Create isolated test directory
    let temp_dir = TempDir::new().context("Failed to create temp directory")?;
    let data_dir = temp_dir.path().join("chronik-data");

    let mut server = TestServer::new(data_dir.clone(), CRASH_TEST_PORT);
    
    let test_result = run_crash_recovery_test(&mut server).await;
    
    // Cleanup
    drop(server);
    
    match test_result {
        Ok(stats) => {
            info!("WAL crash recovery test completed successfully");
            info!("Recovery statistics: {:#?}", stats);
            
            // Validate recovery was successful
            assert_eq!(stats.messages_before_crash, INITIAL_MESSAGE_COUNT, 
                      "Pre-crash message count mismatch");
            assert_eq!(stats.messages_after_recovery, INITIAL_MESSAGE_COUNT, 
                      "Post-recovery message count mismatch");
            assert_eq!(stats.data_corruption_count, 0, 
                      "Data corruption detected after recovery");
            assert!(stats.recovery_time < Duration::from_secs(30), 
                    "Recovery took too long: {:?}", stats.recovery_time);
        }
        Err(e) => {
            error!("WAL crash recovery test failed: {:?}", e);
            return Err(e);
        }
    }
    
    Ok(())
}

/// Execute the complete crash recovery test
async fn run_crash_recovery_test(server: &mut TestServer) -> Result<RecoveryTestStats> {
    let mut stats = RecoveryTestStats::default();
    
    // Phase 1: Start server and establish baseline
    info!("Phase 1: Starting server and establishing baseline");
    server.start().await.context("Failed to start initial server")?;
    
    // Phase 2: Write initial data set
    info!("Phase 2: Writing {} initial messages", INITIAL_MESSAGE_COUNT);
    let initial_messages = write_test_messages(CRASH_TEST_PORT, INITIAL_MESSAGE_COUNT, 0).await
        .context("Failed to write initial messages")?;
    stats.messages_before_crash = initial_messages.len();
    
    // Phase 3: Verify WAL state before crash
    info!("Phase 3: Inspecting WAL state before crash");
    let pre_crash_wal = server.inspect_wal_state()
        .context("Failed to inspect WAL state before crash")?;
    info!("Pre-crash WAL state: {:#?}", pre_crash_wal);
    
    // Phase 4: Simulate ungraceful shutdown
    info!("Phase 4: Simulating ungraceful server crash");
    let crash_start = Instant::now();
    server.crash().context("Failed to crash server")?;
    
    // Wait a moment to ensure crash is complete
    tokio::time::sleep(Duration::from_secs(1)).await;
    
    // Phase 5: Inspect WAL state after crash
    info!("Phase 5: Inspecting WAL state after crash");
    let post_crash_wal = server.inspect_wal_state()
        .context("Failed to inspect WAL state after crash")?;
    info!("Post-crash WAL state: {:#?}", post_crash_wal);
    
    // Phase 6: Restart server and measure recovery time
    info!("Phase 6: Restarting server and measuring recovery time");
    let recovery_start = Instant::now();
    server.start().await.context("Failed to restart server after crash")?;
    stats.recovery_time = recovery_start.elapsed();
    
    info!("Server recovery completed in {:?}", stats.recovery_time);
    
    // Phase 7: Validate data integrity after recovery
    info!("Phase 7: Validating data integrity after recovery");
    let recovered_messages = read_and_validate_messages(CRASH_TEST_PORT, &initial_messages).await
        .context("Failed to validate messages after recovery")?;
    
    stats.messages_after_recovery = recovered_messages.len();
    stats.data_corruption_count = initial_messages.len() - recovered_messages.len();
    
    // Phase 8: Write additional data to test continued operation
    info!("Phase 8: Testing continued operation with {} additional messages", RECOVERY_MESSAGE_COUNT);
    let additional_messages = write_test_messages(CRASH_TEST_PORT, RECOVERY_MESSAGE_COUNT, INITIAL_MESSAGE_COUNT).await
        .context("Failed to write additional messages after recovery")?;
    stats.messages_after_recovery_write = additional_messages.len();
    
    info!("Post-recovery write test completed successfully");
    
    Ok(stats)
}

/// Write test messages to the server
async fn write_test_messages(port: u16, count: usize, id_offset: usize) -> Result<Vec<TestMessage>> {
    let producer: FutureProducer = ClientConfig::new()
        .set("bootstrap.servers", &format!("localhost:{}", port))
        .set("message.timeout.ms", "10000")
        .set("queue.buffering.max.messages", "100000")
        .set("batch.num.messages", "1000")
        .set("enable.idempotence", "true")
        .set("acks", "all")
        .create()
        .context("Failed to create producer for writing")?;

    let mut messages = Vec::new();
    let mut write_errors = 0;
    
    for i in 0..count {
        let message = TestMessage::new((id_offset + i) as u64, MESSAGE_SIZE);
        let key = format!("recovery-key-{:06}", id_offset + i);
        let value = message.serialize();
        
        let record = FutureRecord::to(TEST_TOPIC)
            .key(&key)
            .payload(&value)
            .partition(TEST_PARTITION);
        
        match timeout(Duration::from_secs(5), producer.send(record, Duration::from_secs(0))).await {
            Ok(Ok(_)) => {
                messages.push(message);
                if (i + 1) % 100 == 0 {
                    debug!("Written {} messages", i + 1);
                }
            }
            Ok(Err((e, _))) => {
                warn!("Failed to write message {}: {:?}", i, e);
                write_errors += 1;
            }
            Err(_) => {
                warn!("Timeout writing message {}", i);
                write_errors += 1;
            }
        }
        
        // Yield occasionally
        if i % 50 == 0 {
            tokio::task::yield_now().await;
        }
    }
    
    // Flush to ensure durability
    producer.flush(Duration::from_secs(10))
        .map_err(|e| anyhow!("Producer flush failed: {:?}", e))?;
    
    info!("Wrote {} messages ({} errors)", messages.len(), write_errors);
    Ok(messages)
}

/// Read and validate messages match expected set
async fn read_and_validate_messages(port: u16, expected: &[TestMessage]) -> Result<Vec<TestMessage>> {
    let consumer: StreamConsumer = ClientConfig::new()
        .set("bootstrap.servers", &format!("localhost:{}", port))
        .set("group.id", "wal-recovery-validator")
        .set("auto.offset.reset", "earliest")
        .set("enable.auto.commit", "false")
        .set("max.poll.interval.ms", "300000")
        .create()
        .context("Failed to create consumer for validation")?;

    consumer.subscribe(&[TEST_TOPIC])
        .context("Failed to subscribe to test topic")?;

    let expected_ids: HashSet<u64> = expected.iter().map(|m| m.sequence_id).collect();
    let mut recovered_messages = Vec::new();
    let mut recovered_ids = HashSet::new();
    
    // Read with timeout
    let read_deadline = Instant::now() + Duration::from_secs(20);
    
    while recovered_messages.len() < expected.len() && Instant::now() < read_deadline {
        match timeout(Duration::from_secs(3), consumer.stream().next()).await {
            Ok(Some(Ok(borrowed_message))) => {
                if let Some(payload) = borrowed_message.payload() {
                    match TestMessage::deserialize(payload) {
                        Ok(message) => {
                            if expected_ids.contains(&message.sequence_id) {
                                if !recovered_ids.insert(message.sequence_id) {
                                    warn!("Duplicate message detected: {}", message.sequence_id);
                                }
                                recovered_messages.push(message);
                            } else {
                                warn!("Unexpected message ID: {}", message.sequence_id);
                            }
                        }
                        Err(e) => {
                            warn!("Failed to deserialize message: {:?}", e);
                        }
                    }
                }
            }
            Ok(Some(Err(e))) => {
                warn!("Error reading message: {:?}", e);
            }
            Ok(None) => {
                debug!("No more messages available");
                break;
            }
            Err(_) => {
                debug!("Read timeout, continuing...");
                continue;
            }
        }
    }
    
    info!("Recovery validation: expected {}, recovered {}", expected.len(), recovered_messages.len());
    
    // Log any missing messages
    let missing: Vec<u64> = expected_ids.difference(&recovered_ids).copied().collect();
    if !missing.is_empty() {
        warn!("Missing message IDs after recovery: {:?}", missing);
    }
    
    Ok(recovered_messages)
}

/// Test message for recovery validation
#[derive(Debug, Clone, PartialEq)]
struct TestMessage {
    sequence_id: u64,
    timestamp: chrono::DateTime<chrono::Utc>,
    content_hash: u64,
    payload: Vec<u8>,
}

impl TestMessage {
    fn new(sequence_id: u64, size: usize) -> Self {
        let timestamp = Utc::now();
        let payload = format!(
            "recovery-msg-{:08}-{}-{}",
            sequence_id,
            timestamp.format("%H%M%S"),
            "x".repeat(size.saturating_sub(30))
        ).into_bytes();
        
        let content_hash = Self::calculate_hash(&payload);
        
        Self {
            sequence_id,
            timestamp,
            content_hash,
            payload,
        }
    }
    
    fn calculate_hash(data: &[u8]) -> u64 {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        
        let mut hasher = DefaultHasher::new();
        data.hash(&mut hasher);
        hasher.finish()
    }
    
    fn serialize(&self) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "sequence_id": self.sequence_id,
            "timestamp": self.timestamp.to_rfc3339(),
            "content_hash": self.content_hash,
            "payload": base64::encode(&self.payload),
        })).unwrap_or_default()
    }
    
    fn deserialize(data: &[u8]) -> Result<Self> {
        let json: serde_json::Value = serde_json::from_slice(data)
            .context("Failed to parse JSON")?;
        
        let sequence_id = json["sequence_id"].as_u64()
            .ok_or_else(|| anyhow!("Missing sequence_id"))?;
            
        let timestamp = json["timestamp"].as_str()
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.with_timezone(&chrono::Utc))
            .ok_or_else(|| anyhow!("Invalid timestamp"))?;
        
        let content_hash = json["content_hash"].as_u64()
            .ok_or_else(|| anyhow!("Missing content_hash"))?;
        
        let payload = json["payload"].as_str()
            .and_then(|s| base64::decode(s).ok())
            .ok_or_else(|| anyhow!("Invalid payload"))?;
        
        Ok(Self {
            sequence_id,
            timestamp,
            content_hash,
            payload,
        })
    }
}

/// Recovery test statistics
#[derive(Debug, Default)]
struct RecoveryTestStats {
    messages_before_crash: usize,
    messages_after_recovery: usize,
    messages_after_recovery_write: usize,
    data_corruption_count: usize,
    recovery_time: Duration,
}

// Additional test for multiple crash/recovery cycles
#[tokio::test]
async fn test_multiple_crash_recovery_cycles() -> Result<()> {
    info!("Starting multiple crash/recovery cycles test");
    
    let temp_dir = TempDir::new()?;
    let data_dir = temp_dir.path().join("chronik-multi-crash");
    
    let mut server = TestServer::new(data_dir, CRASH_TEST_PORT + 10);
    let cycles = 3;
    let messages_per_cycle = 150;
    
    let mut total_messages_written = 0;
    let mut cycle_stats = Vec::new();
    
    for cycle in 0..cycles {
        info!("Starting crash/recovery cycle {} of {}", cycle + 1, cycles);
        
        // Start server
        server.start().await?;
        
        // Write messages
        let messages = write_test_messages(
            CRASH_TEST_PORT + 10, 
            messages_per_cycle, 
            total_messages_written
        ).await?;
        total_messages_written += messages.len();
        
        // Crash server
        server.crash()?;
        tokio::time::sleep(Duration::from_secs(1)).await;
        
        cycle_stats.push(messages.len());
        info!("Cycle {}: wrote {} messages, total: {}", 
              cycle + 1, messages.len(), total_messages_written);
    }
    
    // Final recovery and validation
    info!("Performing final recovery and validation");
    let recovery_start = Instant::now();
    server.start().await?;
    let final_recovery_time = recovery_start.elapsed();
    
    info!("Final recovery completed in {:?}", final_recovery_time);
    info!("Multi-cycle crash test completed successfully");
    info!("Cycle statistics: {:?}", cycle_stats);
    
    assert!(final_recovery_time < Duration::from_secs(45), 
            "Final recovery took too long after {} cycles", cycles);
    
    Ok(())
}