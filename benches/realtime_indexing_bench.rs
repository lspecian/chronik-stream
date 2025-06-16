//! Benchmarks for real-time indexing performance

use chronik_search::{RealtimeIndexer, RealtimeIndexerConfig, JsonDocument};
use criterion::{black_box, criterion_group, criterion_main, Criterion, BatchSize};
use serde_json::json;
use std::time::Duration;
use tempfile::TempDir;
use tokio::sync::mpsc;

async fn setup_indexer() -> (RealtimeIndexer, mpsc::Sender<JsonDocument>, TempDir) {
    let temp_dir = TempDir::new().unwrap();
    
    let config = RealtimeIndexerConfig {
        index_base_path: temp_dir.path().to_path_buf(),
        batch_size: 100,
        batch_timeout: Duration::from_millis(100),
        num_indexing_threads: 4,
        indexing_memory_budget: 256 * 1024 * 1024,
        ..Default::default()
    };
    
    let indexer = RealtimeIndexer::new(config).unwrap();
    let (sender, receiver) = mpsc::channel(10000);
    
    let _ = indexer.start(receiver).await.unwrap();
    
    (indexer, sender, temp_dir)
}

fn create_test_document(id: u64) -> JsonDocument {
    JsonDocument {
        id: format!("doc-{}", id),
        topic: "bench-topic".to_string(),
        partition: (id % 4) as i32,
        offset: id as i64,
        timestamp: chrono::Utc::now().timestamp_millis(),
        content: json!({
            "id": id,
            "type": "benchmark",
            "title": format!("Document {}", id),
            "content": "This is a test document for benchmarking real-time indexing performance",
            "score": id as f64 * 1.5,
            "tags": vec![
                format!("tag-{}", id % 10),
                format!("category-{}", id % 5),
            ],
            "metadata": {
                "author": format!("user-{}", id % 100),
                "created_at": chrono::Utc::now().to_rfc3339(),
                "version": 1,
                "properties": {
                    "field1": "value1",
                    "field2": id * 2,
                    "field3": id % 2 == 0,
                }
            },
            "nested": {
                "level1": {
                    "level2": {
                        "data": format!("nested-{}", id)
                    }
                }
            }
        }),
        metadata: None,
    }
}

fn bench_single_document_indexing(c: &mut Criterion) {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    
    c.bench_function("index_single_document", |b| {
        b.iter_batched(
            || {
                runtime.block_on(async {
                    setup_indexer().await
                })
            },
            |(indexer, sender, _temp_dir)| {
                runtime.block_on(async {
                    let doc = create_test_document(1);
                    sender.send(doc).await.unwrap();
                    tokio::time::sleep(Duration::from_millis(10)).await;
                    
                    let metrics = indexer.metrics();
                    assert_eq!(metrics.documents_indexed, 1);
                })
            },
            BatchSize::SmallInput,
        )
    });
}

fn bench_batch_indexing(c: &mut Criterion) {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    
    let mut group = c.benchmark_group("batch_indexing");
    
    for batch_size in &[10, 100, 1000] {
        group.bench_function(format!("batch_{}", batch_size), |b| {
            b.iter_batched(
                || {
                    runtime.block_on(async {
                        setup_indexer().await
                    })
                },
                |(indexer, sender, _temp_dir)| {
                    runtime.block_on(async {
                        for i in 0..*batch_size {
                            let doc = create_test_document(i as u64);
                            sender.send(doc).await.unwrap();
                        }
                        
                        // Wait for indexing to complete
                        tokio::time::sleep(Duration::from_millis(200)).await;
                        
                        let metrics = indexer.metrics();
                        assert_eq!(metrics.documents_indexed, *batch_size as u64);
                    })
                },
                BatchSize::SmallInput,
            )
        });
    }
    
    group.finish();
}

fn bench_throughput(c: &mut Criterion) {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    
    let mut group = c.benchmark_group("throughput");
    group.measurement_time(Duration::from_secs(10));
    
    group.bench_function("sustained_load", |b| {
        b.iter_batched(
            || {
                runtime.block_on(async {
                    setup_indexer().await
                })
            },
            |(indexer, sender, _temp_dir)| {
                runtime.block_on(async {
                    let start = std::time::Instant::now();
                    let target_duration = Duration::from_secs(1);
                    let mut count = 0u64;
                    
                    while start.elapsed() < target_duration {
                        let doc = create_test_document(count);
                        if sender.send(doc).await.is_err() {
                            break;
                        }
                        count += 1;
                    }
                    
                    // Wait for all documents to be indexed
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    
                    let metrics = indexer.metrics();
                    black_box(metrics.documents_indexed);
                })
            },
            BatchSize::SmallInput,
        )
    });
    
    group.finish();
}

fn bench_json_complexity(c: &mut Criterion) {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    
    let mut group = c.benchmark_group("json_complexity");
    
    // Simple flat JSON
    group.bench_function("simple_json", |b| {
        b.iter_batched(
            || runtime.block_on(setup_indexer()),
            |(indexer, sender, _temp_dir)| {
                runtime.block_on(async {
                    for i in 0..100 {
                        let doc = JsonDocument {
                            id: format!("doc-{}", i),
                            topic: "bench".to_string(),
                            partition: 0,
                            offset: i,
                            timestamp: chrono::Utc::now().timestamp_millis(),
                            content: json!({
                                "id": i,
                                "name": format!("Item {}", i),
                                "value": i * 10,
                            }),
                            metadata: None,
                        };
                        sender.send(doc).await.unwrap();
                    }
                    
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    
                    let metrics = indexer.metrics();
                    assert_eq!(metrics.documents_indexed, 100);
                })
            },
            BatchSize::SmallInput,
        )
    });
    
    // Complex nested JSON
    group.bench_function("complex_json", |b| {
        b.iter_batched(
            || runtime.block_on(setup_indexer()),
            |(indexer, sender, _temp_dir)| {
                runtime.block_on(async {
                    for i in 0..100 {
                        let doc = create_test_document(i as u64);
                        sender.send(doc).await.unwrap();
                    }
                    
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    
                    let metrics = indexer.metrics();
                    assert_eq!(metrics.documents_indexed, 100);
                })
            },
            BatchSize::SmallInput,
        )
    });
    
    // Large JSON documents
    group.bench_function("large_json", |b| {
        b.iter_batched(
            || runtime.block_on(setup_indexer()),
            |(indexer, sender, _temp_dir)| {
                runtime.block_on(async {
                    for i in 0..10 {
                        let mut large_array = vec![];
                        for j in 0..100 {
                            large_array.push(json!({
                                "index": j,
                                "data": format!("Item {} in document {}", j, i),
                                "value": j * i,
                            }));
                        }
                        
                        let doc = JsonDocument {
                            id: format!("doc-{}", i),
                            topic: "bench".to_string(),
                            partition: 0,
                            offset: i,
                            timestamp: chrono::Utc::now().timestamp_millis(),
                            content: json!({
                                "id": i,
                                "large_array": large_array,
                                "metadata": {
                                    "size": "large",
                                    "items": 100,
                                }
                            }),
                            metadata: None,
                        };
                        sender.send(doc).await.unwrap();
                    }
                    
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    
                    let metrics = indexer.metrics();
                    assert_eq!(metrics.documents_indexed, 10);
                })
            },
            BatchSize::SmallInput,
        )
    });
    
    group.finish();
}

criterion_group!(
    benches,
    bench_single_document_indexing,
    bench_batch_indexing,
    bench_throughput,
    bench_json_complexity
);
criterion_main!(benches);