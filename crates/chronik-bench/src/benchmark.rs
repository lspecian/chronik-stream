/// Core benchmark runner implementation

use anyhow::{Context, Result};
use hdrhistogram::Histogram;
use indicatif::{ProgressBar, ProgressStyle};
use rdkafka::{
    admin::{AdminClient, AdminOptions, NewTopic, TopicReplication},
    client::DefaultClientContext,
    config::ClientConfig,
    consumer::{Consumer, StreamConsumer},
    producer::{FutureProducer, FutureRecord, Producer},
    util::Timeout,
    Message,
};
use std::{
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use tokio::{
    sync::Mutex,
    time::{interval, sleep},
};
use tracing::{error, info, warn};

use crate::{
    cli::{Args, BenchmarkMode, KeyPattern, PayloadPattern},
    reporter::BenchmarkResults,
};

/// Benchmark runner that orchestrates the load test
pub struct BenchmarkRunner {
    args: Args,
    producer: Option<FutureProducer>,
    consumer: Option<StreamConsumer>,
    admin: Option<AdminClient<DefaultClientContext>>,
}

impl BenchmarkRunner {
    /// Create a new benchmark runner
    pub fn new(args: Args) -> Result<Self> {
        let mut config = ClientConfig::new();
        config
            .set("bootstrap.servers", &args.bootstrap_servers)
            .set("api.version.request", "true")
            .set("api.version.fallback.ms", "0")
            .set("socket.keepalive.enable", "true");

        // Create producer for produce/round-trip modes
        let producer = if matches!(
            args.mode,
            BenchmarkMode::Produce | BenchmarkMode::RoundTrip
        ) {
            let mut producer_config = config.clone();
            producer_config
                .set("compression.type", args.compression.to_rdkafka_str())
                .set("acks", args.acks.to_string())
                .set("linger.ms", args.linger_ms.to_string())
                .set("batch.size", args.batch_size.to_string())
                .set("request.timeout.ms", args.request_timeout_ms.to_string())
                .set("message.timeout.ms", args.message_timeout_ms.to_string())
                .set("queue.buffering.max.messages", "10000000")
                .set("queue.buffering.max.kbytes", "1048576");

            Some(
                producer_config
                    .create()
                    .context("Failed to create Kafka producer")?,
            )
        } else {
            None
        };

        // Create consumer for consume/round-trip modes
        let consumer = if matches!(
            args.mode,
            BenchmarkMode::Consume | BenchmarkMode::RoundTrip
        ) {
            let mut consumer_config = config.clone();
            // Generate a unique client ID for this consumer instance
            // This ensures proper consumer group rebalancing when multiple consumers join
            // Using thread_rng() for guaranteed fresh randomness on each process invocation
            use rand::Rng;
            let random_suffix: u64 = rand::thread_rng().gen();
            let unique_client_id = format!("chronik-bench-{}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_micros(),
                random_suffix
            );
            info!("Generated unique client_id: {}", unique_client_id);  // Debug log
            let auto_commit = if args.disable_auto_commit { "false" } else { "true" };
            consumer_config
                .set("group.id", &args.consumer_group)
                .set("client.id", &unique_client_id)  // Unique client ID for proper group membership
                .set("enable.auto.commit", auto_commit)
                .set("auto.offset.reset", "earliest")
                .set("session.timeout.ms", "30000")
                .set("fetch.min.bytes", "1")
                .set("fetch.wait.max.ms", "100");

            Some(
                consumer_config
                    .create()
                    .context("Failed to create Kafka consumer")?,
            )
        } else {
            None
        };

        // Create admin client for topic management
        let admin = if args.create_topic || args.delete_topic_after {
            Some(
                config
                    .create()
                    .context("Failed to create Kafka admin client")?,
            )
        } else {
            None
        };

        Ok(Self {
            args,
            producer,
            consumer,
            admin,
        })
    }

    /// Run the benchmark
    pub async fn run(&mut self) -> Result<BenchmarkResults> {
        // Create topic if requested
        if self.args.create_topic {
            self.create_topic().await?;
        }

        // Run warmup phase
        if self.args.warmup_duration.as_secs() > 0 {
            info!(
                "Running warmup for {:?}...",
                self.args.warmup_duration
            );
            self.warmup().await?;
        }

        // Run main benchmark based on mode
        let results = match self.args.mode {
            BenchmarkMode::Produce => self.run_produce_benchmark().await?,
            BenchmarkMode::Consume => self.run_consume_benchmark().await?,
            BenchmarkMode::RoundTrip => self.run_roundtrip_benchmark().await?,
            BenchmarkMode::Metadata => self.run_metadata_benchmark().await?,
        };

        // Delete topic if requested
        if self.args.delete_topic_after {
            self.delete_topic().await?;
        }

        Ok(results)
    }

    /// Create topic
    async fn create_topic(&self) -> Result<()> {
        let admin = self
            .admin
            .as_ref()
            .context("Admin client not initialized")?;

        info!("Creating topic '{}'...", self.args.topic);

        let new_topic = NewTopic::new(
            &self.args.topic,
            self.args.partitions,
            TopicReplication::Fixed(self.args.replication_factor as i32),
        );

        // Wrap topic creation with explicit 60-second timeout to prevent indefinite hangs
        use tokio::time::{timeout, Duration};
        let results = match timeout(Duration::from_secs(60), admin.create_topics(&[new_topic], &AdminOptions::new())).await {
            Ok(Ok(results)) => results,
            Ok(Err(e)) => anyhow::bail!("Failed to create topic: {}", e),
            Err(_) => anyhow::bail!("Topic creation timed out after 60 seconds - check if Chronik server is running and accessible"),
        };

        for result in results {
            match result {
                Ok(topic) => info!("Topic '{}' created successfully", topic),
                Err((topic, err)) => {
                    if err.to_string().contains("already exists") {
                        warn!("Topic '{}' already exists", topic);
                    } else {
                        anyhow::bail!("Failed to create topic '{}': {}", topic, err);
                    }
                }
            }
        }

        Ok(())
    }

    /// Delete topic
    async fn delete_topic(&self) -> Result<()> {
        let admin = self
            .admin
            .as_ref()
            .context("Admin client not initialized")?;

        info!("Deleting topic '{}'...", self.args.topic);

        let results = admin
            .delete_topics(&[&self.args.topic], &AdminOptions::new())
            .await
            .context("Failed to delete topic")?;

        for result in results {
            match result {
                Ok(topic) => info!("Topic '{}' deleted successfully", topic),
                Err((topic, err)) => warn!("Failed to delete topic '{}': {}", topic, err),
            }
        }

        Ok(())
    }

    /// Run warmup phase
    async fn warmup(&mut self) -> Result<()> {
        if let Some(producer) = &self.producer {
            let payload = generate_payload(
                self.args.message_size,
                self.args.payload_pattern,
                0,
            );
            let warmup_messages = 1000;

            for i in 0..warmup_messages {
                let key = generate_key(self.args.key_pattern, i);
                let record = FutureRecord::to(&self.args.topic)
                    .payload(&payload)
                    .key(&key);

                let _ = producer.send(record, Timeout::Never).await;
            }

            // Flush all pending messages
            let _ = producer.flush(Timeout::After(self.args.warmup_duration));
        }

        // Brief pause after warmup
        sleep(Duration::from_secs(1)).await;

        Ok(())
    }

    /// Run produce benchmark
    async fn run_produce_benchmark(&self) -> Result<BenchmarkResults> {
        let producer = self
            .producer
            .as_ref()
            .context("Producer not initialized")?;

        info!("Starting produce benchmark...");

        // Shared state
        let messages_sent = Arc::new(AtomicU64::new(0));
        let messages_failed = Arc::new(AtomicU64::new(0));
        let bytes_sent = Arc::new(AtomicU64::new(0));
        let latency_histogram = Arc::new(Mutex::new(
            Histogram::<u64>::new_with_bounds(1, 60_000_000, 3)
                .context("Failed to create histogram")?,
        ));
        let running = Arc::new(AtomicBool::new(true));

        // Progress bar
        let progress = ProgressBar::new_spinner();
        progress.set_style(
            ProgressStyle::default_spinner()
                .template("{spinner:.green} {msg}")
                .unwrap(),
        );

        // Start periodic reporter
        let reporter_handle = {
            let messages_sent = Arc::clone(&messages_sent);
            let bytes_sent = Arc::clone(&bytes_sent);
            let running = Arc::clone(&running);
            let interval_secs = self.args.report_interval_secs;
            let latency_histogram = Arc::clone(&latency_histogram);

            tokio::spawn(async move {
                let mut ticker = interval(Duration::from_secs(interval_secs));
                let mut last_messages = 0u64;
                let mut last_bytes = 0u64;

                while running.load(Ordering::Relaxed) {
                    ticker.tick().await;

                    let current_messages = messages_sent.load(Ordering::Relaxed);
                    let current_bytes = bytes_sent.load(Ordering::Relaxed);

                    let delta_messages = current_messages - last_messages;
                    let delta_bytes = current_bytes - last_bytes;

                    let msg_rate = delta_messages / interval_secs;
                    let byte_rate = delta_bytes / interval_secs;
                    let mb_rate = byte_rate as f64 / (1024.0 * 1024.0);

                    // Get latency stats
                    let histogram = latency_histogram.lock().await;
                    let p99 = histogram.value_at_quantile(0.99) as f64 / 1000.0; // Convert to ms

                    info!(
                        "Rate: {:>8} msg/s | {:>6.1} MB/s | Total: {:>10} msgs | p99: {:>6.1} ms",
                        msg_rate, mb_rate, current_messages, p99
                    );

                    last_messages = current_messages;
                    last_bytes = current_bytes;
                }
            })
        };

        // Spawn producer tasks
        let start_time = Instant::now();
        let mut handles = vec![];

        for task_id in 0..self.args.concurrency {
            let producer = producer.clone();
            let topic = self.args.topic.clone();
            let message_size = self.args.message_size;
            let key_pattern = self.args.key_pattern;
            let payload_pattern = self.args.payload_pattern;
            let messages_sent = Arc::clone(&messages_sent);
            let messages_failed = Arc::clone(&messages_failed);
            let bytes_sent = Arc::clone(&bytes_sent);
            let latency_histogram = Arc::clone(&latency_histogram);
            let running = Arc::clone(&running);
            let duration = self.args.duration;
            let message_count = self.args.message_count;
            let rate_limit = self.args.rate_limit;

            let handle = tokio::spawn(async move {
                let mut local_count = 0u64;
                let task_start = Instant::now();

                loop {
                    // Check duration
                    if duration.as_secs() > 0 && task_start.elapsed() >= duration {
                        break;
                    }

                    // Check message count
                    if message_count > 0 {
                        let total_sent = messages_sent.load(Ordering::Relaxed);
                        if total_sent >= message_count {
                            break;
                        }
                    }

                    // Check if still running
                    if !running.load(Ordering::Relaxed) {
                        break;
                    }

                    // Rate limiting
                    if rate_limit > 0 {
                        let target_interval =
                            Duration::from_secs_f64(1.0 / (rate_limit as f64));
                        sleep(target_interval).await;
                    }

                    // Generate message
                    let key = generate_key(key_pattern, task_id as u64 + local_count);
                    let payload = generate_payload(message_size, payload_pattern, local_count);

                    // Send message
                    let send_start = Instant::now();
                    let record = FutureRecord::to(&topic).payload(&payload).key(&key);

                    match producer.send(record, Timeout::Never).await {
                        Ok(_) => {
                            let latency_us = send_start.elapsed().as_micros() as u64;
                            messages_sent.fetch_add(1, Ordering::Relaxed);
                            bytes_sent.fetch_add(payload.len() as u64, Ordering::Relaxed);

                            // Record latency
                            if let Ok(mut histogram) = latency_histogram.try_lock() {
                                let _ = histogram.record(latency_us);
                            }
                        }
                        Err((err, _)) => {
                            error!("Failed to send message: {}", err);
                            messages_failed.fetch_add(1, Ordering::Relaxed);
                        }
                    }

                    local_count += 1;
                }
            });

            handles.push(handle);
        }

        // Wait for all tasks to complete
        for handle in handles {
            handle.await?;
        }

        // Signal reporter to stop
        running.store(false, Ordering::Relaxed);
        reporter_handle.await?;

        // Flush producer
        info!("Flushing producer...");
        let _ = producer.flush(Timeout::After(Duration::from_secs(30)));

        let elapsed = start_time.elapsed();
        progress.finish_and_clear();

        // Build results
        let total_messages = messages_sent.load(Ordering::Relaxed);
        let failed_messages = messages_failed.load(Ordering::Relaxed);
        let total_bytes = bytes_sent.load(Ordering::Relaxed);

        let histogram = latency_histogram.lock().await;

        Ok(BenchmarkResults {
            mode: self.args.mode,
            duration: elapsed,
            total_messages,
            failed_messages,
            total_bytes,
            latency_p50_us: histogram.value_at_quantile(0.50),
            latency_p90_us: histogram.value_at_quantile(0.90),
            latency_p95_us: histogram.value_at_quantile(0.95),
            latency_p99_us: histogram.value_at_quantile(0.99),
            latency_p999_us: histogram.value_at_quantile(0.999),
            latency_max_us: histogram.max(),
            message_size: self.args.message_size,
            concurrency: self.args.concurrency,
            compression: self.args.compression,
        })
    }

    /// Run consume benchmark
    async fn run_consume_benchmark(&self) -> Result<BenchmarkResults> {
        let consumer = self
            .consumer
            .as_ref()
            .context("Consumer not initialized")?;

        info!("Starting consume benchmark...");

        // Subscribe to topic
        consumer
            .subscribe(&[&self.args.topic])
            .context("Failed to subscribe to topic")?;

        // Shared state
        let messages_received = Arc::new(AtomicU64::new(0));
        let bytes_received = Arc::new(AtomicU64::new(0));
        let latency_histogram = Arc::new(Mutex::new(
            Histogram::<u64>::new_with_bounds(1, 60_000_000, 3)
                .context("Failed to create histogram")?,
        ));

        // Start periodic reporter
        let reporter_handle = {
            let messages_received = Arc::clone(&messages_received);
            let bytes_received = Arc::clone(&bytes_received);
            let interval_secs = self.args.report_interval_secs;
            let latency_histogram = Arc::clone(&latency_histogram);

            tokio::spawn(async move {
                let mut ticker = interval(Duration::from_secs(interval_secs));
                let mut last_messages = 0u64;
                let mut last_bytes = 0u64;

                loop {
                    ticker.tick().await;

                    let current_messages = messages_received.load(Ordering::Relaxed);
                    let current_bytes = bytes_received.load(Ordering::Relaxed);

                    let delta_messages = current_messages - last_messages;
                    let delta_bytes = current_bytes - last_bytes;

                    let msg_rate = delta_messages / interval_secs;
                    let byte_rate = delta_bytes / interval_secs;
                    let mb_rate = byte_rate as f64 / (1024.0 * 1024.0);

                    // Get latency stats
                    let histogram = latency_histogram.lock().await;
                    let p99 = histogram.value_at_quantile(0.99) as f64 / 1000.0; // Convert to ms

                    info!(
                        "Rate: {:>8} msg/s | {:>6.1} MB/s | Total: {:>10} msgs | p99: {:>6.1} ms",
                        msg_rate, mb_rate, current_messages, p99
                    );

                    last_messages = current_messages;
                    last_bytes = current_bytes;
                }
            })
        };

        // Consume messages
        let start_time = Instant::now();
        let duration = self.args.duration;
        let message_count = self.args.message_count;

        loop {
            // Check duration limit
            if duration.as_secs() > 0 && start_time.elapsed() >= duration {
                info!("Duration limit reached, stopping consumer");
                break;
            }

            // Check message count limit
            if message_count > 0 {
                let received = messages_received.load(Ordering::Relaxed);
                if received >= message_count {
                    info!("Message count limit reached ({}/{}), stopping consumer", received, message_count);
                    break;
                }
            }

            match consumer.recv().await {
                Ok(message) => {
                    messages_received.fetch_add(1, Ordering::Relaxed);
                    if let Some(payload) = message.payload() {
                        bytes_received.fetch_add(payload.len() as u64, Ordering::Relaxed);
                    }
                    // Note: Consume latency would require timestamp in message
                }
                Err(err) => {
                    error!("Error receiving message: {}", err);
                }
            }
        }

        // Stop reporter
        drop(reporter_handle);

        let elapsed = start_time.elapsed();

        // Build results
        let total_messages = messages_received.load(Ordering::Relaxed);
        let total_bytes = bytes_received.load(Ordering::Relaxed);

        let histogram = latency_histogram.lock().await;

        Ok(BenchmarkResults {
            mode: self.args.mode,
            duration: elapsed,
            total_messages,
            failed_messages: 0,
            total_bytes,
            latency_p50_us: histogram.value_at_quantile(0.50),
            latency_p90_us: histogram.value_at_quantile(0.90),
            latency_p95_us: histogram.value_at_quantile(0.95),
            latency_p99_us: histogram.value_at_quantile(0.99),
            latency_p999_us: histogram.value_at_quantile(0.999),
            latency_max_us: histogram.max(),
            message_size: 0, // Variable in consume mode
            concurrency: self.args.concurrency,
            compression: self.args.compression,
        })
    }

    /// Run round-trip benchmark (produce + consume)
    async fn run_roundtrip_benchmark(&self) -> Result<BenchmarkResults> {
        // TODO: Implement round-trip benchmark
        // This would involve:
        // 1. Embedding timestamps in messages during produce
        // 2. Consuming messages and calculating end-to-end latency
        // 3. Correlating produce and consume operations
        anyhow::bail!("Round-trip benchmark not yet implemented");
    }

    /// Run metadata benchmark
    async fn run_metadata_benchmark(&self) -> Result<BenchmarkResults> {
        // TODO: Implement metadata benchmark
        // This would involve:
        // 1. Topic creation/deletion operations
        // 2. Metadata queries
        // 3. Consumer group operations
        anyhow::bail!("Metadata benchmark not yet implemented");
    }
}

/// Generate message key based on pattern
fn generate_key(pattern: KeyPattern, index: u64) -> String {
    match pattern {
        KeyPattern::Random => format!("key-{}", rand::random::<u64>()),
        KeyPattern::Sequential => format!("key-{:016x}", index),
        KeyPattern::Fixed => "fixed-key".to_string(),
    }
}

/// Generate message payload based on pattern
fn generate_payload(size: usize, pattern: PayloadPattern, index: u64) -> Vec<u8> {
    match pattern {
        PayloadPattern::Random => {
            let mut payload = vec![0u8; size];
            for byte in &mut payload {
                *byte = rand::random();
            }
            payload
        }
        PayloadPattern::Zeros => vec![0u8; size],
        PayloadPattern::Text => {
            let text = format!("Message-{:016x}-", index);
            let mut payload = text.as_bytes().to_vec();
            payload.resize(size, b'x');
            payload
        }
    }
}
