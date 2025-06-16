//! Performance and benchmark tests

use anyhow::Result;
use rdkafka::config::ClientConfig;
use rdkafka::producer::{FutureProducer, FutureRecord};
use rdkafka::consumer::{StreamConsumer, Consumer};
use futures::StreamExt;
use std::time::{Duration, Instant};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::Semaphore;
use serde_json::json;

use crate::testcontainers_setup::{TestEnvironment, ChronikCluster};

#[tokio::test]
async fn test_throughput_single_producer() -> Result<()> {
    let test_env = TestEnvironment::new().await?;
    test_env.create_bucket("chronik-test").await?;
    
    let mut cluster = ChronikCluster::new(3, &test_env).await?;
    cluster.start(&test_env).await?;
    
    let bootstrap_servers = cluster.bootstrap_servers();
    let topic = "throughput-test";
    
    let producer: FutureProducer = ClientConfig::new()
        .set("bootstrap.servers", &bootstrap_servers)
        .set("batch.size", "16384")
        .set("linger.ms", "10")
        .set("compression.type", "lz4")
        .create()?;
    
    let num_messages = 100_000;
    let message_size = 1024; // 1KB messages
    let payload = vec![b'x'; message_size];
    
    let start = Instant::now();
    
    for i in 0..num_messages {
        let record = FutureRecord::to(topic)
            .key(&format!("key-{}", i))
            .payload(&payload[..]);
        
        producer.send(record, Duration::from_secs(0)).await?;
    }
    
    let duration = start.elapsed();
    let throughput_msgs = num_messages as f64 / duration.as_secs_f64();
    let throughput_mb = (num_messages * message_size) as f64 / (1024.0 * 1024.0) / duration.as_secs_f64();
    
    println!("Single Producer Throughput:");
    println!("  Messages: {}/sec", throughput_msgs as u64);
    println!("  Data: {:.2} MB/sec", throughput_mb);
    println!("  Total time: {:?}", duration);
    
    // Assert minimum performance thresholds
    assert!(throughput_msgs > 10_000.0, "Should achieve at least 10K msgs/sec");
    assert!(throughput_mb > 10.0, "Should achieve at least 10 MB/sec");
    
    cluster.stop().await?;
    Ok(())
}

#[tokio::test]
async fn test_throughput_multiple_producers() -> Result<()> {
    let test_env = TestEnvironment::new().await?;
    test_env.create_bucket("chronik-test").await?;
    
    let mut cluster = ChronikCluster::new(3, &test_env).await?;
    cluster.start(&test_env).await?;
    
    let bootstrap_servers = cluster.bootstrap_servers();
    let topic = "multi-producer-test";
    
    let num_producers = 4;
    let messages_per_producer = 25_000;
    let message_size = 1024;
    let payload = vec![b'x'; message_size];
    
    let start = Instant::now();
    let mut handles = Vec::new();
    
    for producer_id in 0..num_producers {
        let bootstrap_servers = bootstrap_servers.clone();
        let payload = payload.clone();
        
        let handle = tokio::spawn(async move {
            let producer: FutureProducer = ClientConfig::new()
                .set("bootstrap.servers", &bootstrap_servers)
                .set("client.id", &format!("producer-{}", producer_id))
                .set("batch.size", "16384")
                .set("linger.ms", "10")
                .set("compression.type", "lz4")
                .create()?;
            
            for i in 0..messages_per_producer {
                let record = FutureRecord::to(topic)
                    .key(&format!("p{}-key-{}", producer_id, i))
                    .payload(&payload[..]);
                
                producer.send(record, Duration::from_secs(0)).await?;
            }
            
            Ok::<(), anyhow::Error>(())
        });
        
        handles.push(handle);
    }
    
    // Wait for all producers
    for handle in handles {
        handle.await??;
    }
    
    let duration = start.elapsed();
    let total_messages = num_producers * messages_per_producer;
    let throughput_msgs = total_messages as f64 / duration.as_secs_f64();
    let throughput_mb = (total_messages * message_size) as f64 / (1024.0 * 1024.0) / duration.as_secs_f64();
    
    println!("Multiple Producers Throughput:");
    println!("  Producers: {}", num_producers);
    println!("  Messages: {}/sec", throughput_msgs as u64);
    println!("  Data: {:.2} MB/sec", throughput_mb);
    println!("  Total time: {:?}", duration);
    
    // Should achieve better throughput with multiple producers
    assert!(throughput_msgs > 20_000.0, "Should achieve at least 20K msgs/sec with multiple producers");
    
    cluster.stop().await?;
    Ok(())
}

#[tokio::test]
async fn test_consumer_throughput() -> Result<()> {
    let test_env = TestEnvironment::new().await?;
    test_env.create_bucket("chronik-test").await?;
    
    let mut cluster = ChronikCluster::new(3, &test_env).await?;
    cluster.start(&test_env).await?;
    
    let bootstrap_servers = cluster.bootstrap_servers();
    let topic = "consumer-throughput-test";
    
    // Pre-produce messages
    let producer: FutureProducer = ClientConfig::new()
        .set("bootstrap.servers", &bootstrap_servers)
        .set("batch.size", "16384")
        .set("linger.ms", "10")
        .create()?;
    
    let num_messages = 100_000;
    let message_size = 1024;
    let payload = vec![b'x'; message_size];
    
    for i in 0..num_messages {
        let record = FutureRecord::to(topic)
            .key(&format!("key-{}", i))
            .payload(&payload[..]);
        
        producer.send(record, Duration::from_secs(0)).await?;
    }
    
    // Wait for messages to be committed
    tokio::time::sleep(Duration::from_secs(2)).await;
    
    // Start consumer
    let consumer: StreamConsumer = ClientConfig::new()
        .set("bootstrap.servers", &bootstrap_servers)
        .set("group.id", "perf-consumer")
        .set("auto.offset.reset", "earliest")
        .set("fetch.min.bytes", "100000")
        .set("fetch.max.wait.ms", "100")
        .create()?;
    
    consumer.subscribe(&[topic])?;
    
    let start = Instant::now();
    let mut consumed = 0;
    let mut total_bytes = 0;
    let mut stream = consumer.stream();
    
    while consumed < num_messages {
        if let Some(Ok(msg)) = stream.next().await {
            consumed += 1;
            total_bytes += msg.payload().unwrap().len();
        }
    }
    
    let duration = start.elapsed();
    let throughput_msgs = consumed as f64 / duration.as_secs_f64();
    let throughput_mb = total_bytes as f64 / (1024.0 * 1024.0) / duration.as_secs_f64();
    
    println!("Consumer Throughput:");
    println!("  Messages: {}/sec", throughput_msgs as u64);
    println!("  Data: {:.2} MB/sec", throughput_mb);
    println!("  Total time: {:?}", duration);
    
    assert!(throughput_msgs > 10_000.0, "Should achieve at least 10K msgs/sec consumption");
    
    cluster.stop().await?;
    Ok(())
}

#[tokio::test]
async fn test_end_to_end_latency() -> Result<()> {
    let test_env = TestEnvironment::new().await?;
    test_env.create_bucket("chronik-test").await?;
    
    let mut cluster = ChronikCluster::new(3, &test_env).await?;
    cluster.start(&test_env).await?;
    
    let bootstrap_servers = cluster.bootstrap_servers();
    let topic = "latency-test";
    
    let producer: FutureProducer = ClientConfig::new()
        .set("bootstrap.servers", &bootstrap_servers)
        .set("batch.size", "1") // Disable batching for latency test
        .set("linger.ms", "0")
        .create()?;
    
    let consumer: StreamConsumer = ClientConfig::new()
        .set("bootstrap.servers", &bootstrap_servers)
        .set("group.id", "latency-consumer")
        .set("auto.offset.reset", "latest")
        .set("fetch.min.bytes", "1")
        .set("fetch.max.wait.ms", "1")
        .create()?;
    
    consumer.subscribe(&[topic])?;
    
    // Wait for consumer to be ready
    tokio::time::sleep(Duration::from_secs(2)).await;
    
    let mut latencies = Vec::new();
    
    for i in 0..100 {
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_millis() as i64;
        
        let payload = json!({
            "id": i,
            "timestamp": timestamp
        }).to_string();
        
        let record = FutureRecord::to(topic)
            .key(&format!("latency-{}", i))
            .payload(&payload);
        
        let produce_start = Instant::now();
        producer.send(record, Duration::from_secs(0)).await?;
        
        // Consume the message
        let mut stream = consumer.stream();
        let consume_timeout = tokio::time::timeout(Duration::from_secs(1), async {
            while let Some(Ok(msg)) = stream.next().await {
                let msg_payload = String::from_utf8_lossy(msg.payload().unwrap());
                if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&msg_payload) {
                    if parsed["id"].as_i64() == Some(i as i64) {
                        return Some(produce_start.elapsed());
                    }
                }
            }
            None
        }).await;
        
        if let Ok(Some(latency)) = consume_timeout {
            latencies.push(latency);
        }
    }
    
    // Calculate statistics
    latencies.sort();
    let p50 = latencies[latencies.len() / 2];
    let p95 = latencies[latencies.len() * 95 / 100];
    let p99 = latencies[latencies.len() * 99 / 100];
    let avg = latencies.iter().map(|d| d.as_micros()).sum::<u128>() / latencies.len() as u128;
    
    println!("End-to-End Latency:");
    println!("  Average: {} µs", avg);
    println!("  P50: {:?}", p50);
    println!("  P95: {:?}", p95);
    println!("  P99: {:?}", p99);
    
    // Assert reasonable latencies
    assert!(p50 < Duration::from_millis(50), "P50 latency should be under 50ms");
    assert!(p99 < Duration::from_millis(200), "P99 latency should be under 200ms");
    
    cluster.stop().await?;
    Ok(())
}

#[tokio::test]
async fn test_search_indexing_performance() -> Result<()> {
    let test_env = TestEnvironment::new().await?;
    test_env.create_bucket("chronik-test").await?;
    
    let mut cluster = ChronikCluster::new(3, &test_env).await?;
    cluster.start(&test_env).await?;
    
    let bootstrap_servers = cluster.bootstrap_servers();
    let topic = "search-perf-test";
    
    let producer: FutureProducer = ClientConfig::new()
        .set("bootstrap.servers", &bootstrap_servers)
        .set("batch.size", "16384")
        .set("linger.ms", "10")
        .create()?;
    
    // Generate diverse log data
    let services = vec!["auth", "api", "database", "cache", "queue"];
    let levels = vec!["INFO", "WARN", "ERROR", "DEBUG"];
    let num_messages = 50_000;
    
    let start = Instant::now();
    
    for i in 0..num_messages {
        let log = json!({
            "timestamp": format!("2024-01-15T10:{:02}:{:02}Z", i / 60 % 60, i % 60),
            "level": levels[i % levels.len()],
            "service": services[i % services.len()],
            "message": format!("Log message number {} with some text", i),
            "request_id": format!("req-{}", i % 1000),
            "user_id": format!("user-{}", i % 100),
            "duration_ms": i % 1000,
            "status_code": if i % 10 == 0 { 500 } else { 200 }
        });
        
        let record = FutureRecord::to(topic)
            .key(&format!("log-{}", i))
            .payload(&log.to_string());
        
        producer.send(record, Duration::from_secs(0)).await?;
    }
    
    let produce_duration = start.elapsed();
    println!("Produced {} messages in {:?}", num_messages, produce_duration);
    
    // Wait for indexing
    tokio::time::sleep(Duration::from_secs(10)).await;
    
    // Run search queries
    let search_port = cluster.nodes[0].admin_port;
    let client = reqwest::Client::new();
    
    // Benchmark different query types
    let queries = vec![
        ("Simple term", "level:ERROR"),
        ("Service filter", "service:database"),
        ("Range query", "duration_ms:[500 TO 1000]"),
        ("Complex query", "level:ERROR AND service:api AND status_code:500"),
        ("Full text", "message"),
    ];
    
    for (query_name, query) in queries {
        let start = Instant::now();
        
        let resp = client
            .get(&format!("http://127.0.0.1:{}/api/v1/search", search_port))
            .query(&[
                ("index", topic),
                ("q", query),
                ("limit", "100")
            ])
            .send()
            .await?;
        
        let duration = start.elapsed();
        
        assert!(resp.status().is_success());
        let results: serde_json::Value = resp.json().await?;
        let hit_count = results["total_hits"].as_u64().unwrap_or(0);
        
        println!("Query '{}': {} hits in {:?}", query_name, hit_count, duration);
        assert!(duration < Duration::from_millis(100), "Search should complete within 100ms");
    }
    
    // Benchmark aggregation
    let start = Instant::now();
    
    let agg_query = json!({
        "aggs": {
            "by_level": {
                "terms": { "field": "level" }
            },
            "avg_duration": {
                "avg": { "field": "duration_ms" }
            }
        }
    });
    
    let resp = client
        .post(&format!("http://127.0.0.1:{}/api/v1/search", search_port))
        .query(&[("index", topic)])
        .json(&agg_query)
        .send()
        .await?;
    
    let duration = start.elapsed();
    assert!(resp.status().is_success());
    
    println!("Aggregation completed in {:?}", duration);
    assert!(duration < Duration::from_millis(200), "Aggregation should complete within 200ms");
    
    cluster.stop().await?;
    Ok(())
}

#[tokio::test]
async fn test_concurrent_operations() -> Result<()> {
    let test_env = TestEnvironment::new().await?;
    test_env.create_bucket("chronik-test").await?;
    
    let mut cluster = ChronikCluster::new(3, &test_env).await?;
    cluster.start(&test_env).await?;
    
    let bootstrap_servers = cluster.bootstrap_servers();
    
    // Metrics tracking
    let produced_count = Arc::new(AtomicU64::new(0));
    let consumed_count = Arc::new(AtomicU64::new(0));
    let search_count = Arc::new(AtomicU64::new(0));
    
    let duration = Duration::from_secs(30);
    let start = Instant::now();
    
    // Limit concurrent operations
    let producer_semaphore = Arc::new(Semaphore::new(10));
    let consumer_semaphore = Arc::new(Semaphore::new(5));
    
    let mut handles = Vec::new();
    
    // Concurrent producers
    for producer_id in 0..4 {
        let bootstrap_servers = bootstrap_servers.clone();
        let produced_count = produced_count.clone();
        let semaphore = producer_semaphore.clone();
        
        let handle = tokio::spawn(async move {
            let producer: FutureProducer = ClientConfig::new()
                .set("bootstrap.servers", &bootstrap_servers)
                .set("client.id", &format!("concurrent-producer-{}", producer_id))
                .create()?;
            
            let mut count = 0;
            while start.elapsed() < duration {
                let _permit = semaphore.acquire().await?;
                
                let topic = format!("concurrent-topic-{}", count % 3);
                let record = FutureRecord::to(&topic)
                    .key(&format!("p{}-key-{}", producer_id, count))
                    .payload(&format!("value-{}", count));
                
                producer.send(record, Duration::from_secs(0)).await?;
                count += 1;
                produced_count.fetch_add(1, Ordering::Relaxed);
            }
            
            Ok::<(), anyhow::Error>(())
        });
        
        handles.push(handle);
    }
    
    // Concurrent consumers
    for consumer_id in 0..3 {
        let bootstrap_servers = bootstrap_servers.clone();
        let consumed_count = consumed_count.clone();
        let semaphore = consumer_semaphore.clone();
        
        let handle = tokio::spawn(async move {
            let consumer: StreamConsumer = ClientConfig::new()
                .set("bootstrap.servers", &bootstrap_servers)
                .set("group.id", &format!("concurrent-group-{}", consumer_id))
                .set("client.id", &format!("concurrent-consumer-{}", consumer_id))
                .set("auto.offset.reset", "earliest")
                .create()?;
            
            let topic = format!("concurrent-topic-{}", consumer_id);
            consumer.subscribe(&[&topic])?;
            
            let mut stream = consumer.stream();
            
            while start.elapsed() < duration {
                let _permit = semaphore.acquire().await?;
                
                if let Ok(Some(Ok(_))) = tokio::time::timeout(
                    Duration::from_millis(100),
                    stream.next()
                ).await {
                    consumed_count.fetch_add(1, Ordering::Relaxed);
                }
            }
            
            Ok::<(), anyhow::Error>(())
        });
        
        handles.push(handle);
    }
    
    // Concurrent search queries
    let search_port = cluster.nodes[0].admin_port;
    let search_count_clone = search_count.clone();
    
    let search_handle = tokio::spawn(async move {
        let client = reqwest::Client::new();
        
        while start.elapsed() < duration {
            let topic = format!("concurrent-topic-{}", search_count_clone.load(Ordering::Relaxed) % 3);
            
            let resp = client
                .get(&format!("http://127.0.0.1:{}/api/v1/search", search_port))
                .query(&[
                    ("index", &topic),
                    ("q", "*"),
                    ("limit", "10")
                ])
                .send()
                .await;
            
            if resp.is_ok() {
                search_count_clone.fetch_add(1, Ordering::Relaxed);
            }
            
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        
        Ok::<(), anyhow::Error>(())
    });
    
    handles.push(search_handle);
    
    // Wait for all operations
    for handle in handles {
        let _ = handle.await?;
    }
    
    let total_duration = start.elapsed();
    let produced = produced_count.load(Ordering::Relaxed);
    let consumed = consumed_count.load(Ordering::Relaxed);
    let searches = search_count.load(Ordering::Relaxed);
    
    println!("Concurrent Operations Results:");
    println!("  Duration: {:?}", total_duration);
    println!("  Produced: {} msgs ({}/sec)", produced, produced / 30);
    println!("  Consumed: {} msgs ({}/sec)", consumed, consumed / 30);
    println!("  Searches: {} queries ({}/sec)", searches, searches / 30);
    
    // Verify reasonable performance under concurrent load
    assert!(produced > 10_000, "Should produce at least 10K messages under concurrent load");
    assert!(consumed > 5_000, "Should consume at least 5K messages under concurrent load");
    assert!(searches > 50, "Should handle at least 50 search queries");
    
    cluster.stop().await?;
    Ok(())
}