//! Example demonstrating real-time JSON document indexing with Chronik Stream
//!
//! This example shows how to:
//! - Set up a real-time indexing pipeline
//! - Process JSON documents from Kafka-like streams
//! - Handle various JSON structures and field types
//! - Monitor indexing performance and metrics

use chronik_search::{JsonPipelineBuilder, RealtimeIndexerConfig, FieldIndexingPolicy};
use chronik_storage::{RecordBatch, Record};
use serde_json::json;
use std::collections::HashMap;
use std::time::Duration;
use tempfile::TempDir;
use tokio::time::sleep;
use tracing::{info, Level};
use tracing_subscriber::FmtSubscriber;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Initialize logging
    let subscriber = FmtSubscriber::builder()
        .with_max_level(Level::INFO)
        .finish();
    tracing::subscriber::set_global_default(subscriber)?;

    info!("Starting real-time JSON indexing example");

    // Create temporary directory for index
    let temp_dir = TempDir::new()?;
    info!("Using index directory: {:?}", temp_dir.path());

    // Configure the indexing pipeline
    let mut field_policy = FieldIndexingPolicy::default();
    field_policy.excluded_fields = vec!["password".to_string(), "api_key".to_string()];
    field_policy.max_nesting_depth = 3;

    let indexer_config = RealtimeIndexerConfig {
        index_base_path: temp_dir.path().to_path_buf(),
        batch_size: 50,
        batch_timeout: Duration::from_millis(500),
        num_indexing_threads: 2,
        indexing_memory_budget: 128 * 1024 * 1024, // 128MB
        field_policy,
        ..Default::default()
    };

    // Build the JSON pipeline
    let pipeline = JsonPipelineBuilder::new()
        .channel_buffer_size(1000)
        .batch_size(indexer_config.batch_size)
        .indexing_memory_budget(indexer_config.indexing_memory_budget)
        .num_indexing_threads(indexer_config.num_indexing_threads)
        .parse_keys_as_json(true)
        .build()
        .await?;

    info!("JSON indexing pipeline initialized");

    // Example 1: User activity events
    info!("\n=== Example 1: User Activity Events ===");
    let user_events = create_user_activity_batch();
    pipeline.process_batch("user-events", 0, &user_events).await?;
    
    sleep(Duration::from_millis(100)).await;
    let stats = pipeline.stats().await;
    info!("Processed {} user events", stats.messages_processed);

    // Example 2: E-commerce product catalog
    info!("\n=== Example 2: Product Catalog ===");
    let products = create_product_catalog_batch();
    pipeline.process_batch("products", 0, &products).await?;
    
    sleep(Duration::from_millis(100)).await;
    let stats = pipeline.stats().await;
    info!("Total messages processed: {}", stats.messages_processed);

    // Example 3: IoT sensor data
    info!("\n=== Example 3: IoT Sensor Data ===");
    let sensor_data = create_iot_sensor_batch();
    pipeline.process_batch("iot-sensors", 0, &sensor_data).await?;
    
    sleep(Duration::from_millis(100)).await;

    // Example 4: Log aggregation
    info!("\n=== Example 4: Application Logs ===");
    let logs = create_application_logs_batch();
    pipeline.process_batch("app-logs", 0, &logs).await?;
    
    sleep(Duration::from_millis(100)).await;

    // Final metrics
    info!("\n=== Final Indexing Metrics ===");
    let final_stats = pipeline.stats().await;
    let indexer_metrics = pipeline.indexer_metrics();
    
    info!("Pipeline Statistics:");
    info!("  Messages processed: {}", final_stats.messages_processed);
    info!("  Parse errors: {}", final_stats.parse_errors);
    info!("  Indexing errors: {}", final_stats.indexing_errors);
    info!("  Bytes processed: {}", format_bytes(final_stats.bytes_processed));
    
    info!("\nIndexer Metrics:");
    info!("  Documents indexed: {}", indexer_metrics.documents_indexed);
    info!("  Documents failed: {}", indexer_metrics.documents_failed);
    info!("  Bytes indexed: {}", format_bytes(indexer_metrics.bytes_indexed));
    info!("  Segments created: {}", indexer_metrics.segments_created);
    info!("  Schema evolution count: {}", indexer_metrics.schema_evolution_count);
    info!("  Memory usage: {}", format_bytes(indexer_metrics.memory_usage_bytes));
    
    let avg_ms = if indexer_metrics.documents_indexed > 0 {
        indexer_metrics.indexing_duration_ms as f64 / indexer_metrics.documents_indexed as f64
    } else {
        0.0
    };
    info!("  Average indexing time: {:.2}ms per document", avg_ms);

    // Shutdown
    info!("\nShutting down pipeline...");
    pipeline.shutdown().await?;
    
    info!("Example completed successfully!");
    Ok(())
}

fn create_user_activity_batch() -> RecordBatch {
    let records = vec![
        // Login event
        Record {
            offset: 1000,
            timestamp: chrono::Utc::now().timestamp_millis(),
            key: Some(r#"{"user_id": "user-123"}"#.as_bytes().to_vec()),
            value: json!({
                "event_type": "login",
                "timestamp": chrono::Utc::now().to_rfc3339(),
                "user": {
                    "id": "user-123",
                    "email": "alice@example.com",
                    "name": "Alice Smith"
                },
                "session": {
                    "id": "sess-456",
                    "ip": "192.168.1.100",
                    "user_agent": "Mozilla/5.0 (Windows NT 10.0; Win64; x64)",
                    "location": {
                        "country": "US",
                        "city": "San Francisco",
                        "lat": 37.7749,
                        "lon": -122.4194
                    }
                },
                "device": {
                    "type": "desktop",
                    "os": "Windows",
                    "browser": "Chrome"
                }
            }).to_string().into_bytes(),
            headers: [
                ("event_source".to_string(), b"web".to_vec()),
                ("version".to_string(), b"1.0".to_vec()),
            ].into_iter().collect(),
        },
        // Page view event
        Record {
            offset: 1001,
            timestamp: chrono::Utc::now().timestamp_millis() + 1000,
            key: Some(r#"{"user_id": "user-123"}"#.as_bytes().to_vec()),
            value: json!({
                "event_type": "page_view",
                "timestamp": chrono::Utc::now().to_rfc3339(),
                "user_id": "user-123",
                "session_id": "sess-456",
                "page": {
                    "url": "https://example.com/products/laptop",
                    "title": "Premium Laptop - Example Store",
                    "referrer": "https://google.com",
                    "load_time_ms": 342
                },
                "utm": {
                    "source": "google",
                    "medium": "cpc",
                    "campaign": "summer-sale"
                }
            }).to_string().into_bytes(),
            headers: HashMap::new(),
        },
        // Purchase event
        Record {
            offset: 1002,
            timestamp: chrono::Utc::now().timestamp_millis() + 2000,
            key: Some(r#"{"user_id": "user-123"}"#.as_bytes().to_vec()),
            value: json!({
                "event_type": "purchase",
                "timestamp": chrono::Utc::now().to_rfc3339(),
                "user_id": "user-123",
                "order": {
                    "id": "order-789",
                    "total": 1299.99,
                    "currency": "USD",
                    "items": [
                        {
                            "product_id": "prod-001",
                            "name": "Premium Laptop",
                            "quantity": 1,
                            "price": 1299.99,
                            "category": "Electronics"
                        }
                    ],
                    "shipping": {
                        "method": "express",
                        "cost": 0.0,
                        "estimated_delivery": "2024-01-20"
                    }
                },
                "payment": {
                    "method": "credit_card",
                    "last_four": "1234"
                }
            }).to_string().into_bytes(),
            headers: HashMap::new(),
        },
    ];
    
    RecordBatch { records }
}

fn create_product_catalog_batch() -> RecordBatch {
    let products = vec![
        json!({
            "product_id": "LAPTOP-001",
            "name": "UltraBook Pro 15",
            "description": "High-performance laptop with 15-inch display",
            "category": ["Electronics", "Computers", "Laptops"],
            "price": {
                "amount": 1299.99,
                "currency": "USD",
                "discount": {
                    "percentage": 10,
                    "valid_until": "2024-02-01"
                }
            },
            "specifications": {
                "display": {
                    "size": "15.6 inches",
                    "resolution": "3840x2160",
                    "type": "OLED"
                },
                "processor": {
                    "brand": "Intel",
                    "model": "Core i7-13700H",
                    "cores": 14,
                    "threads": 20
                },
                "memory": {
                    "ram": "32GB DDR5",
                    "storage": "1TB NVMe SSD"
                },
                "battery": {
                    "capacity": "86Wh",
                    "life": "Up to 12 hours"
                }
            },
            "inventory": {
                "in_stock": true,
                "quantity": 45,
                "warehouse": ["US-WEST", "US-EAST"]
            },
            "ratings": {
                "average": 4.7,
                "count": 234,
                "distribution": {
                    "5": 180,
                    "4": 40,
                    "3": 10,
                    "2": 3,
                    "1": 1
                }
            }
        }),
        json!({
            "product_id": "PHONE-002",
            "name": "SmartPhone X Pro",
            "description": "Latest flagship smartphone with advanced camera system",
            "category": ["Electronics", "Mobile", "Smartphones"],
            "price": {
                "amount": 999.99,
                "currency": "USD"
            },
            "specifications": {
                "display": {
                    "size": "6.7 inches",
                    "resolution": "2796x1290",
                    "type": "Super Retina XDR"
                },
                "camera": {
                    "main": "48MP",
                    "ultra_wide": "12MP",
                    "telephoto": "12MP",
                    "features": ["Night mode", "ProRAW", "4K video"]
                },
                "storage_options": ["128GB", "256GB", "512GB", "1TB"]
            },
            "inventory": {
                "in_stock": true,
                "quantity": 120
            }
        }),
    ];
    
    let records = products.into_iter().enumerate().map(|(i, product)| {
        Record {
            offset: 2000 + i as i64,
            timestamp: chrono::Utc::now().timestamp_millis() + i as i64,
            key: Some(product["product_id"].as_str().unwrap().as_bytes().to_vec()),
            value: product.to_string().into_bytes(),
            headers: HashMap::new(),
        }
    }).collect();
    
    RecordBatch { records }
}

fn create_iot_sensor_batch() -> RecordBatch {
    let records = (0..10).map(|i| {
        let sensor_data = json!({
            "sensor_id": format!("sensor-{:03}", i),
            "timestamp": chrono::Utc::now().timestamp_millis() + i * 100,
            "type": if i % 2 == 0 { "temperature" } else { "humidity" },
            "location": {
                "building": "Building A",
                "floor": i / 3 + 1,
                "room": format!("Room {}", 100 + i),
                "coordinates": {
                    "lat": 37.7749 + (i as f64 * 0.001),
                    "lon": -122.4194 + (i as f64 * 0.001)
                }
            },
            "readings": {
                "value": if i % 2 == 0 { 22.5 + i as f64 * 0.3 } else { 45.0 + i as f64 * 2.0 },
                "unit": if i % 2 == 0 { "celsius" } else { "percent" },
                "quality": "good",
                "calibrated": true
            },
            "metadata": {
                "firmware_version": "2.1.0",
                "battery_level": 85 - i,
                "last_maintenance": "2024-01-01",
                "alerts": if i > 7 { vec!["high_reading"] } else { vec![] }
            }
        });
        
        Record {
            offset: 3000 + i as i64,
            timestamp: chrono::Utc::now().timestamp_millis() + i as i64 * 100,
            key: Some(format!("sensor-{:03}", i).into_bytes()),
            value: sensor_data.to_string().into_bytes(),
            headers: [
                ("sensor_type".to_string(), if i % 2 == 0 { b"temp".to_vec() } else { b"humidity".to_vec() }),
            ].into_iter().collect(),
        }
    }).collect();
    
    RecordBatch { records }
}

fn create_application_logs_batch() -> RecordBatch {
    let log_levels = vec!["DEBUG", "INFO", "WARN", "ERROR"];
    let services = vec!["api-gateway", "user-service", "order-service", "payment-service"];
    
    let records = (0..20).map(|i| {
        let level = log_levels[i % log_levels.len()];
        let service = services[i % services.len()];
        
        let log_entry = match level {
            "ERROR" => json!({
                "timestamp": chrono::Utc::now().to_rfc3339(),
                "level": level,
                "service": service,
                "message": "Failed to process payment",
                "error": {
                    "type": "PaymentGatewayError",
                    "message": "Connection timeout to payment provider",
                    "code": "PG_TIMEOUT_001",
                    "stack_trace": [
                        "at PaymentService.processPayment (payment-service.js:123)",
                        "at OrderService.completeOrder (order-service.js:456)",
                        "at async handleCheckout (api-gateway.js:789)"
                    ]
                },
                "context": {
                    "user_id": "user-456",
                    "order_id": "order-789",
                    "amount": 299.99,
                    "retry_count": 2
                },
                "metadata": {
                    "pod_name": format!("{}-pod-{}", service, i % 3),
                    "node": "k8s-node-02",
                    "region": "us-west-2"
                }
            }),
            "WARN" => json!({
                "timestamp": chrono::Utc::now().to_rfc3339(),
                "level": level,
                "service": service,
                "message": "High memory usage detected",
                "warning": {
                    "type": "ResourceWarning",
                    "current_usage": "85%",
                    "threshold": "80%",
                    "duration_minutes": 5
                },
                "metadata": {
                    "pod_name": format!("{}-pod-{}", service, i % 3),
                    "container": "main"
                }
            }),
            _ => json!({
                "timestamp": chrono::Utc::now().to_rfc3339(),
                "level": level,
                "service": service,
                "message": format!("{} request processed successfully", if level == "DEBUG" { "Debug" } else { "API" }),
                "request": {
                    "method": "GET",
                    "path": "/api/v1/users/123",
                    "duration_ms": 45 + i * 5,
                    "status_code": 200
                }
            })
        };
        
        Record {
            offset: 4000 + i as i64,
            timestamp: chrono::Utc::now().timestamp_millis() + i as i64 * 50,
            key: Some(format!("{}-{}", service, level).into_bytes()),
            value: log_entry.to_string().into_bytes(),
            headers: [
                ("log_level".to_string(), level.as_bytes().to_vec()),
                ("service".to_string(), service.as_bytes().to_vec()),
            ].into_iter().collect(),
        }
    }).collect();
    
    RecordBatch { records }
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB"];
    let mut size = bytes as f64;
    let mut unit_index = 0;
    
    while size >= 1024.0 && unit_index < UNITS.len() - 1 {
        size /= 1024.0;
        unit_index += 1;
    }
    
    format!("{:.2} {}", size, UNITS[unit_index])
}