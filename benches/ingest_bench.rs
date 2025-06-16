//! Benchmarks for message ingestion.

use chronik_ingest::indexer::Indexer;
use chronik_protocol::Record;
use criterion::{black_box, criterion_group, criterion_main, Criterion, Throughput};
use tempfile::TempDir;

fn create_test_records(count: usize) -> Vec<Record> {
    (0..count)
        .map(|i| Record {
            offset: i as i64,
            timestamp: 1234567890 + i as i64,
            key: Some(format!("key-{}", i).into_bytes()),
            value: Some(format!("Message {} with some test data", i).into_bytes()),
            headers: vec![],
        })
        .collect()
}

fn benchmark_indexing(c: &mut Criterion) {
    let mut group = c.benchmark_group("indexing");
    
    for batch_size in [10, 100, 1000] {
        group.throughput(Throughput::Elements(batch_size as u64));
        group.bench_with_input(
            format!("{}_messages", batch_size),
            &batch_size,
            |b, &size| {
                let runtime = tokio::runtime::Runtime::new().unwrap();
                let temp_dir = TempDir::new().unwrap();
                
                runtime.block_on(async {
                    let indexer = Indexer::new(temp_dir.path()).await.unwrap();
                    let records = create_test_records(size);
                    
                    b.iter(|| {
                        let topic = format!("bench-topic-{}", uuid::Uuid::new_v4());
                        
                        runtime.block_on(async {
                            for record in &records {
                                indexer.index_message(&topic, 0, record).await.unwrap();
                            }
                        });
                        
                        black_box(&topic);
                    });
                });
            },
        );
    }
    
    group.finish();
}

fn benchmark_batch_processing(c: &mut Criterion) {
    let mut group = c.benchmark_group("batch_processing");
    
    group.bench_function("1000_messages_parallel", |b| {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let temp_dir = TempDir::new().unwrap();
        
        runtime.block_on(async {
            let indexer = Indexer::new(temp_dir.path()).await.unwrap();
            let records = create_test_records(1000);
            
            b.iter(|| {
                runtime.block_on(async {
                    let mut handles = vec![];
                    
                    // Process in parallel batches
                    for chunk in records.chunks(100) {
                        let indexer_clone = indexer.clone();
                        let chunk = chunk.to_vec();
                        let topic = "parallel-bench".to_string();
                        
                        let handle = tokio::spawn(async move {
                            for record in chunk {
                                indexer_clone.index_message(&topic, 0, &record).await.unwrap();
                            }
                        });
                        
                        handles.push(handle);
                    }
                    
                    // Wait for all batches
                    for handle in handles {
                        handle.await.unwrap();
                    }
                });
            });
        });
    });
    
    group.finish();
}

fn benchmark_compression(c: &mut Criterion) {
    let mut group = c.benchmark_group("compression");
    
    for compression in ["none", "snappy", "lz4", "zstd"] {
        group.bench_with_input(
            compression,
            &compression,
            |b, &compression_type| {
                let records = create_test_records(100);
                let mut data = Vec::new();
                
                // Serialize records
                for record in &records {
                    if let Some(value) = &record.value {
                        data.extend_from_slice(value);
                    }
                }
                
                b.iter(|| {
                    let compressed = match compression_type {
                        "snappy" => {
                            // Simulate snappy compression
                            let mut encoder = snap::raw::Encoder::new();
                            encoder.compress_vec(&data).unwrap()
                        }
                        "lz4" => {
                            // Simulate lz4 compression
                            lz4::block::compress(&data, None, true).unwrap()
                        }
                        "zstd" => {
                            // Simulate zstd compression
                            zstd::bulk::compress(&data, 3).unwrap()
                        }
                        _ => data.clone(),
                    };
                    
                    black_box(compressed);
                });
            },
        );
    }
    
    group.finish();
}

criterion_group!(
    ingest_benches,
    benchmark_indexing,
    benchmark_batch_processing,
    benchmark_compression
);
criterion_main!(ingest_benches);