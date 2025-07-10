//! Benchmarks comparing OptimizedTansuSegment vs ChronikSegment performance

use criterion::{black_box, criterion_group, criterion_main, Criterion, BenchmarkId};
use chronik_storage::{
    Record, RecordBatch,
    ChronikSegment, ChronikSegmentBuilder,
    OptimizedTansuSegment,
    CompressionType
};
use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};
use tempfile::NamedTempFile;
use std::io::Write;

/// Generate test records with realistic data
fn generate_test_records(count: usize, key_size: usize, value_size: usize) -> Vec<Record> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    
    (0..count).map(|i| {
        let key = format!("key-{:08}-{}", i, "x".repeat(key_size.saturating_sub(13)));
        let value = format!("value-{:08}-{}", i, "x".repeat(value_size.saturating_sub(15)));
        
        Record {
            offset: i as i64,
            timestamp: now + (i as i64 * 100), // 100ms between records
            key: Some(key.into_bytes()),
            value: value.into_bytes(),
            headers: if i % 10 == 0 {
                vec![
                    ("content-type".to_string(), b"application/json".to_vec()),
                    ("correlation-id".to_string(), format!("{}", i).into_bytes()),
                ].into_iter().collect()
            } else {
                HashMap::new()
            },
        }
    }).collect()
}

/// Generate a RecordBatch from records
fn create_record_batch(records: Vec<Record>) -> RecordBatch {
    RecordBatch {
        records,
    }
}

/// Benchmark building segments
fn bench_segment_build(c: &mut Criterion) {
    let mut group = c.benchmark_group("segment_build");
    
    for record_count in [1000, 10000, 50000].iter() {
        let records = generate_test_records(*record_count, 50, 200);
        let batch = create_record_batch(records.clone());
        
        group.bench_with_input(
            BenchmarkId::new("ChronikSegment", record_count),
            record_count,
            |b, _| {
                b.iter(|| {
                    let segment = ChronikSegmentBuilder::new("test-topic".to_string(), 0)
                        .add_batch(batch.clone())
                        .with_index(false)  // Disable index for fair comparison
                        .with_bloom_filter(true)
                        .build()
                        .unwrap();
                    black_box(segment)
                });
            },
        );
        
        group.bench_with_input(
            BenchmarkId::new("OptimizedTansuSegment", record_count),
            record_count,
            |b, _| {
                b.iter(|| {
                    let result = OptimizedTansuSegment::build(records.clone()).unwrap();
                    black_box(result)
                });
            },
        );
    }
    
    group.finish();
}

/// Benchmark time range queries (only for OptimizedTansuSegment since ChronikSegment doesn't support it)
fn bench_time_range_query(c: &mut Criterion) {
    let mut group = c.benchmark_group("time_range_query");
    
    for record_count in [10000, 50000, 100000].iter() {
        let records = generate_test_records(*record_count, 50, 200);
        
        // Build and write OptimizedTansuSegment
        let (_opt_header, opt_data) = OptimizedTansuSegment::build(records.clone()).unwrap();
        let opt_file = NamedTempFile::new().unwrap();
        opt_file.as_file().write_all(&opt_data).unwrap();
        
        // Query middle 10% of records by time
        let first_timestamp = records.first().map(|r| r.timestamp).unwrap_or(0);
        let last_timestamp = records.last().map(|r| r.timestamp).unwrap_or(0);
        let start_time = first_timestamp + ((last_timestamp - first_timestamp) * 45 / 100);
        let end_time = first_timestamp + ((last_timestamp - first_timestamp) * 55 / 100);
        
        group.bench_with_input(
            BenchmarkId::new("OptimizedTansuSegment", record_count),
            record_count,
            |b, _| {
                let runtime = tokio::runtime::Runtime::new().unwrap();
                b.iter(|| {
                    runtime.block_on(async {
                        let segment = OptimizedTansuSegment::open(opt_file.path()).unwrap();
                        let results = segment.read_time_range(start_time, end_time).await.unwrap();
                        black_box(results)
                    })
                });
            },
        );
    }
    
    group.finish();
}

/// Benchmark bloom filter key lookups
fn bench_bloom_filter(c: &mut Criterion) {
    let mut group = c.benchmark_group("bloom_filter");
    
    let record_count = 50000;
    let records = generate_test_records(record_count, 50, 200);
    let batch = create_record_batch(records.clone());
    
    // Build segments
    let (_opt_header, opt_data) = OptimizedTansuSegment::build(records.clone()).unwrap();
    let opt_file = NamedTempFile::new().unwrap();
    opt_file.as_file().write_all(&opt_data).unwrap();
    
    let mut chronik_segment = ChronikSegmentBuilder::new("test-topic".to_string(), 0)
        .add_batch(batch.clone())
        .with_index(false)
        .with_bloom_filter(true)
        .build()
        .unwrap();
    
    let chronik_file = NamedTempFile::new().unwrap();
    chronik_segment.write_to(&mut chronik_file.as_file()).unwrap();
    
    // Test keys
    let existing_key = format!("key-{:08}-{}", 25000, "x".repeat(37));
    let _non_existing_key = b"non-existing-key-12345";
    
    group.bench_function("ChronikSegment_bloom_existing", |b| {
        b.iter(|| {
            let mut file = std::fs::File::open(chronik_file.path()).unwrap();
            let segment = ChronikSegment::read_from(&mut file).unwrap();
            let result = segment.might_contain_key(existing_key.as_bytes());
            black_box(result)
        });
    });
    
    group.bench_function("OptimizedTansuSegment_bloom_existing", |b| {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        b.iter(|| {
            runtime.block_on(async {
                let segment = OptimizedTansuSegment::open(opt_file.path()).unwrap();
                let result = segment.might_contain_key(existing_key.as_bytes()).await.unwrap();
                black_box(result)
            })
        });
    });
    
    group.finish();
}

/// Measure storage efficiency
fn bench_storage_efficiency(_c: &mut Criterion) {
    println!("\n=== Storage Efficiency Comparison ===\n");
    
    for record_count in [1000, 10000, 50000].iter() {
        let records = generate_test_records(*record_count, 50, 200);
        let batch = create_record_batch(records.clone());
        
        // Measure raw data size
        let raw_size: usize = records.iter()
            .map(|r| {
                8 + 8 + // offset + timestamp
                r.key.as_ref().map(|k| k.len() + 4).unwrap_or(4) + // key + length
                r.value.len() + 4 + // value + length
                r.headers.iter().map(|(k, v)| k.len() + v.len() + 8).sum::<usize>() // headers
            })
            .sum();
        
        // Build and measure ChronikSegment
        let mut chronik_segment = ChronikSegmentBuilder::new("test-topic".to_string(), 0)
            .add_batch(batch.clone())
            .with_index(false)
            .with_bloom_filter(true)
            .compression(CompressionType::Gzip)
            .build()
            .unwrap();
        
        let chronik_file = NamedTempFile::new().unwrap();
        chronik_segment.write_to(&mut chronik_file.as_file()).unwrap();
        let chronik_size = chronik_file.as_file().metadata().unwrap().len() as usize;
        
        // Build and measure OptimizedTansuSegment
        let (_opt_header, opt_data) = OptimizedTansuSegment::build(records.clone()).unwrap();
        let opt_size = opt_data.len();
        
        println!("Record count: {}", record_count);
        println!("  Raw data size: {} bytes", raw_size);
        println!("  ChronikSegment size: {} bytes ({:.1}% of raw)", 
                 chronik_size, (chronik_size as f64 / raw_size as f64) * 100.0);
        println!("  OptimizedTansuSegment size: {} bytes ({:.1}% of raw)", 
                 opt_size, (opt_size as f64 / raw_size as f64) * 100.0);
        println!("  OptimizedTansuSegment vs ChronikSegment: {:.1}% smaller", 
                 (1.0 - opt_size as f64 / chronik_size as f64) * 100.0);
        println!();
    }
}

criterion_group!(
    benches,
    bench_segment_build,
    bench_time_range_query,
    bench_bloom_filter,
    bench_storage_efficiency
);
criterion_main!(benches);