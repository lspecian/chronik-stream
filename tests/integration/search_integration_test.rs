//! Integration tests for search functionality

use anyhow::Result;
use rdkafka::config::ClientConfig;
use rdkafka::producer::{FutureProducer, FutureRecord};
use serde_json::json;
use std::time::Duration;
use tokio::time::sleep;

use crate::testcontainers_setup::{TestEnvironment, ChronikCluster};

#[tokio::test]
async fn test_search_indexing() -> Result<()> {
    let test_env = TestEnvironment::new().await?;
    test_env.create_bucket("chronik-test").await?;
    
    let mut cluster = ChronikCluster::new(1, &test_env).await?;
    cluster.start(&test_env).await?;
    
    let bootstrap_servers = cluster.bootstrap_servers();
    let topic = "logs";
    
    // Produce log messages
    let producer: FutureProducer = ClientConfig::new()
        .set("bootstrap.servers", &bootstrap_servers)
        .create()?;
    
    let log_messages = vec![
        json!({
            "timestamp": "2024-01-15T10:00:00Z",
            "level": "ERROR",
            "service": "auth-service",
            "message": "Failed to authenticate user",
            "user_id": "user123",
            "error_code": "AUTH_FAILED"
        }),
        json!({
            "timestamp": "2024-01-15T10:00:01Z",
            "level": "INFO",
            "service": "auth-service",
            "message": "User successfully authenticated",
            "user_id": "user456"
        }),
        json!({
            "timestamp": "2024-01-15T10:00:02Z",
            "level": "ERROR",
            "service": "payment-service",
            "message": "Payment processing failed",
            "user_id": "user123",
            "amount": 99.99,
            "error_code": "PAYMENT_DECLINED"
        }),
        json!({
            "timestamp": "2024-01-15T10:00:03Z",
            "level": "WARN",
            "service": "inventory-service",
            "message": "Low stock warning",
            "product_id": "prod789",
            "quantity": 5
        }),
        json!({
            "timestamp": "2024-01-15T10:00:04Z",
            "level": "INFO",
            "service": "order-service",
            "message": "Order placed successfully",
            "order_id": "order123",
            "user_id": "user456",
            "total": 299.99
        }),
    ];
    
    for (i, log) in log_messages.iter().enumerate() {
        let record = FutureRecord::to(topic)
            .key(&format!("log-{}", i))
            .payload(&log.to_string());
        
        producer.send(record, Duration::from_secs(0)).await?;
    }
    
    // Wait for indexing
    sleep(Duration::from_secs(5)).await;
    
    // Query the search API
    let search_port = cluster.nodes[0].admin_port;
    let client = reqwest::Client::new();
    
    // Test 1: Search by error level
    let resp = client
        .get(&format!("http://127.0.0.1:{}/api/v1/search", search_port))
        .query(&[
            ("index", topic),
            ("q", "level:ERROR"),
            ("limit", "10")
        ])
        .send()
        .await?;
    
    assert!(resp.status().is_success());
    let results: serde_json::Value = resp.json().await?;
    let hits = results["hits"].as_array().unwrap();
    assert_eq!(hits.len(), 2, "Should find 2 ERROR messages");
    
    // Test 2: Search by service
    let resp = client
        .get(&format!("http://127.0.0.1:{}/api/v1/search", search_port))
        .query(&[
            ("index", topic),
            ("q", "service:auth-service"),
            ("limit", "10")
        ])
        .send()
        .await?;
    
    let results: serde_json::Value = resp.json().await?;
    let hits = results["hits"].as_array().unwrap();
    assert_eq!(hits.len(), 2, "Should find 2 auth-service messages");
    
    // Test 3: Full-text search
    let resp = client
        .get(&format!("http://127.0.0.1:{}/api/v1/search", search_port))
        .query(&[
            ("index", topic),
            ("q", "failed"),
            ("limit", "10")
        ])
        .send()
        .await?;
    
    let results: serde_json::Value = resp.json().await?;
    let hits = results["hits"].as_array().unwrap();
    assert_eq!(hits.len(), 2, "Should find 2 messages with 'failed'");
    
    // Test 4: Complex query
    let resp = client
        .get(&format!("http://127.0.0.1:{}/api/v1/search", search_port))
        .query(&[
            ("index", topic),
            ("q", "level:ERROR AND user_id:user123"),
            ("limit", "10")
        ])
        .send()
        .await?;
    
    let results: serde_json::Value = resp.json().await?;
    let hits = results["hits"].as_array().unwrap();
    assert_eq!(hits.len(), 2, "Should find 2 ERROR messages for user123");
    
    // Test 5: Time range query
    let resp = client
        .get(&format!("http://127.0.0.1:{}/api/v1/search", search_port))
        .query(&[
            ("index", topic),
            ("q", "*"),
            ("start_time", "2024-01-15T10:00:00Z"),
            ("end_time", "2024-01-15T10:00:02Z"),
            ("limit", "10")
        ])
        .send()
        .await?;
    
    let results: serde_json::Value = resp.json().await?;
    let hits = results["hits"].as_array().unwrap();
    assert_eq!(hits.len(), 3, "Should find 3 messages in time range");
    
    cluster.stop().await?;
    Ok(())
}

#[tokio::test]
async fn test_aggregations() -> Result<()> {
    let test_env = TestEnvironment::new().await?;
    test_env.create_bucket("chronik-test").await?;
    
    let mut cluster = ChronikCluster::new(1, &test_env).await?;
    cluster.start(&test_env).await?;
    
    let bootstrap_servers = cluster.bootstrap_servers();
    let topic = "metrics";
    
    // Produce metric messages
    let producer: FutureProducer = ClientConfig::new()
        .set("bootstrap.servers", &bootstrap_servers)
        .create()?;
    
    // Generate metrics data
    for hour in 0..24 {
        for i in 0..10 {
            let metric = json!({
                "timestamp": format!("2024-01-15T{:02}:00:00Z", hour),
                "metric_name": if i % 2 == 0 { "cpu_usage" } else { "memory_usage" },
                "value": 50.0 + (i as f64) * 5.0 + (hour as f64),
                "host": format!("server-{}", i % 3),
                "datacenter": if i < 5 { "dc1" } else { "dc2" }
            });
            
            let record = FutureRecord::to(topic)
                .key(&format!("metric-{}-{}", hour, i))
                .payload(&metric.to_string());
            
            producer.send(record, Duration::from_secs(0)).await?;
        }
    }
    
    // Wait for indexing
    sleep(Duration::from_secs(5)).await;
    
    let search_port = cluster.nodes[0].admin_port;
    let client = reqwest::Client::new();
    
    // Test 1: Terms aggregation
    let agg_query = json!({
        "aggs": {
            "by_metric": {
                "terms": {
                    "field": "metric_name"
                }
            }
        }
    });
    
    let resp = client
        .post(&format!("http://127.0.0.1:{}/api/v1/search", search_port))
        .query(&[("index", topic)])
        .json(&agg_query)
        .send()
        .await?;
    
    assert!(resp.status().is_success());
    let results: serde_json::Value = resp.json().await?;
    let buckets = results["aggregations"]["by_metric"]["buckets"].as_array().unwrap();
    assert_eq!(buckets.len(), 2, "Should have 2 metric types");
    
    // Test 2: Stats aggregation
    let agg_query = json!({
        "query": {
            "term": { "metric_name": "cpu_usage" }
        },
        "aggs": {
            "cpu_stats": {
                "stats": {
                    "field": "value"
                }
            }
        }
    });
    
    let resp = client
        .post(&format!("http://127.0.0.1:{}/api/v1/search", search_port))
        .query(&[("index", topic)])
        .json(&agg_query)
        .send()
        .await?;
    
    let results: serde_json::Value = resp.json().await?;
    let stats = &results["aggregations"]["cpu_stats"];
    assert!(stats["count"].as_u64().unwrap() > 0);
    assert!(stats["avg"].as_f64().is_some());
    assert!(stats["min"].as_f64().is_some());
    assert!(stats["max"].as_f64().is_some());
    
    // Test 3: Date histogram
    let agg_query = json!({
        "aggs": {
            "over_time": {
                "date_histogram": {
                    "field": "timestamp",
                    "interval": "1h",
                    "aggs": {
                        "avg_value": {
                            "avg": {
                                "field": "value"
                            }
                        }
                    }
                }
            }
        }
    });
    
    let resp = client
        .post(&format!("http://127.0.0.1:{}/api/v1/search", search_port))
        .query(&[("index", topic)])
        .json(&agg_query)
        .send()
        .await?;
    
    let results: serde_json::Value = resp.json().await?;
    let buckets = results["aggregations"]["over_time"]["buckets"].as_array().unwrap();
    assert_eq!(buckets.len(), 24, "Should have 24 hourly buckets");
    
    cluster.stop().await?;
    Ok(())
}

#[tokio::test]
async fn test_search_with_kafka_offset_tracking() -> Result<()> {
    let test_env = TestEnvironment::new().await?;
    test_env.create_bucket("chronik-test").await?;
    
    let mut cluster = ChronikCluster::new(1, &test_env).await?;
    cluster.start(&test_env).await?;
    
    let bootstrap_servers = cluster.bootstrap_servers();
    let topic = "events";
    
    let producer: FutureProducer = ClientConfig::new()
        .set("bootstrap.servers", &bootstrap_servers)
        .create()?;
    
    // Produce initial batch
    for i in 0..50 {
        let event = json!({
            "timestamp": format!("2024-01-15T10:00:{:02}Z", i),
            "event_type": "user_action",
            "user_id": format!("user{}", i % 10),
            "action": if i % 2 == 0 { "click" } else { "view" }
        });
        
        let record = FutureRecord::to(topic)
            .key(&format!("event-{}", i))
            .payload(&event.to_string());
        
        producer.send(record, Duration::from_secs(0)).await?;
    }
    
    sleep(Duration::from_secs(3)).await;
    
    let search_port = cluster.nodes[0].admin_port;
    let client = reqwest::Client::new();
    
    // Get current index stats
    let resp = client
        .get(&format!("http://127.0.0.1:{}/api/v1/indices/{}/stats", search_port, topic))
        .send()
        .await?;
    
    let stats: serde_json::Value = resp.json().await?;
    let initial_doc_count = stats["doc_count"].as_u64().unwrap();
    assert_eq!(initial_doc_count, 50);
    
    // Produce more messages
    for i in 50..100 {
        let event = json!({
            "timestamp": format!("2024-01-15T10:01:{:02}Z", i - 50),
            "event_type": "user_action",
            "user_id": format!("user{}", i % 10),
            "action": "purchase"
        });
        
        let record = FutureRecord::to(topic)
            .key(&format!("event-{}", i))
            .payload(&event.to_string());
        
        producer.send(record, Duration::from_secs(0)).await?;
    }
    
    sleep(Duration::from_secs(3)).await;
    
    // Check updated stats
    let resp = client
        .get(&format!("http://127.0.0.1:{}/api/v1/indices/{}/stats", search_port, topic))
        .send()
        .await?;
    
    let stats: serde_json::Value = resp.json().await?;
    let updated_doc_count = stats["doc_count"].as_u64().unwrap();
    assert_eq!(updated_doc_count, 100);
    
    // Search for new events
    let resp = client
        .get(&format!("http://127.0.0.1:{}/api/v1/search", search_port))
        .query(&[
            ("index", topic),
            ("q", "action:purchase"),
            ("limit", "100")
        ])
        .send()
        .await?;
    
    let results: serde_json::Value = resp.json().await?;
    let hits = results["hits"].as_array().unwrap();
    assert_eq!(hits.len(), 50, "Should find all purchase events");
    
    // Verify Kafka offset is tracked in search results
    for hit in hits {
        assert!(hit["_kafka_offset"].as_u64().is_some());
        assert!(hit["_kafka_partition"].as_u64().is_some());
    }
    
    cluster.stop().await?;
    Ok(())
}