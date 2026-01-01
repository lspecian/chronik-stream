//! Benchmarks for SQL query latency via DataFusion
//!
//! Measures:
//! - Query latency by data size (1K, 10K, 100K rows)
//! - Different query types (SELECT *, COUNT, WHERE filter, aggregation)
//! - With and without predicate pushdown

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use std::time::{SystemTime, UNIX_EPOCH};
use tempfile::tempdir;

use chronik_columnar::{
    ColumnarConfig, CompressionCodec, ColumnarQueryEngine,
};
use chronik_columnar::converter::{KafkaRecord, RecordBatchConverter};
use chronik_columnar::schema::kafka_message_schema;
use chronik_columnar::writer::ParquetSegmentWriter;

/// Generate test KafkaRecords with realistic JSON data
fn generate_kafka_records(count: usize) -> Vec<KafkaRecord> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;

    (0..count)
        .map(|i| {
            let value = format!(
                r#"{{"id": {}, "user_id": "user-{}", "amount": {}, "category": "{}", "status": "{}"}}"#,
                i,
                i % 100,
                (i % 1000) as f64 + 0.99,
                ["electronics", "clothing", "food", "books"][i % 4],
                ["pending", "completed", "cancelled"][i % 3]
            );

            KafkaRecord {
                topic: "orders".to_string(),
                partition: (i % 6) as i32,
                offset: i as i64,
                timestamp_ms: now + (i as i64 * 1000), // 1 second apart
                timestamp_type: 0,
                key: Some(format!("order-{:08}", i).into_bytes()),
                value: value.into_bytes(),
                headers: vec![],
                embedding: None,
            }
        })
        .collect()
}

/// Create a Parquet file with test data and return the query engine
fn setup_query_engine(record_count: usize) -> (ColumnarQueryEngine, tempfile::TempDir) {
    let dir = tempdir().unwrap();
    let parquet_path = dir.path().join("test.parquet");

    // Generate and convert records
    let records = generate_kafka_records(record_count);
    let converter = RecordBatchConverter::new();
    let batch = converter.convert(&records).unwrap();

    // Write to Parquet
    let mut config = ColumnarConfig::default();
    config.enabled = true;
    config.compression = CompressionCodec::Zstd;
    config.row_group_size = 10000;

    let schema = kafka_message_schema();
    let writer = ParquetSegmentWriter::new(schema, config);
    writer.write_to_file(&[batch], &parquet_path).unwrap();

    // Create query engine
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let engine = runtime.block_on(async {
        let engine = ColumnarQueryEngine::new();
        engine.register_file("orders", &parquet_path).await.unwrap();
        engine
    });

    (engine, dir)
}

/// Benchmark SELECT * queries
fn bench_select_all(c: &mut Criterion) {
    let mut group = c.benchmark_group("sql_select_all");

    for record_count in [1000, 10000, 50000].iter() {
        let (engine, _dir) = setup_query_engine(*record_count);

        group.throughput(Throughput::Elements(*record_count as u64));
        group.bench_with_input(
            BenchmarkId::new("rows", record_count),
            record_count,
            |b, _| {
                let runtime = tokio::runtime::Runtime::new().unwrap();
                b.iter(|| {
                    runtime.block_on(async {
                        let results = engine
                            .execute_sql(black_box("SELECT * FROM orders LIMIT 1000"))
                            .await
                            .unwrap();
                        black_box(results)
                    })
                });
            },
        );
    }

    group.finish();
}

/// Benchmark COUNT queries
fn bench_count(c: &mut Criterion) {
    let mut group = c.benchmark_group("sql_count");

    for record_count in [1000, 10000, 100000].iter() {
        let (engine, _dir) = setup_query_engine(*record_count);

        group.bench_with_input(
            BenchmarkId::new("rows", record_count),
            record_count,
            |b, _| {
                let runtime = tokio::runtime::Runtime::new().unwrap();
                b.iter(|| {
                    runtime.block_on(async {
                        let results = engine
                            .execute_sql(black_box("SELECT COUNT(*) FROM orders"))
                            .await
                            .unwrap();
                        black_box(results)
                    })
                });
            },
        );
    }

    group.finish();
}

/// Benchmark WHERE filter queries (predicate pushdown)
fn bench_where_filter(c: &mut Criterion) {
    let mut group = c.benchmark_group("sql_where_filter");

    for record_count in [10000, 50000, 100000].iter() {
        let (engine, _dir) = setup_query_engine(*record_count);

        // Filter by partition (should use predicate pushdown)
        group.bench_with_input(
            BenchmarkId::new("partition_filter", record_count),
            record_count,
            |b, _| {
                let runtime = tokio::runtime::Runtime::new().unwrap();
                b.iter(|| {
                    runtime.block_on(async {
                        let results = engine
                            .execute_sql(black_box(
                                "SELECT * FROM orders WHERE _partition = 0 LIMIT 100",
                            ))
                            .await
                            .unwrap();
                        black_box(results)
                    })
                });
            },
        );

        // Filter by offset range (should use predicate pushdown)
        group.bench_with_input(
            BenchmarkId::new("offset_range", record_count),
            record_count,
            |b, _| {
                let runtime = tokio::runtime::Runtime::new().unwrap();
                b.iter(|| {
                    runtime.block_on(async {
                        let results = engine
                            .execute_sql(black_box(
                                "SELECT * FROM orders WHERE _offset BETWEEN 1000 AND 2000",
                            ))
                            .await
                            .unwrap();
                        black_box(results)
                    })
                });
            },
        );
    }

    group.finish();
}

/// Benchmark aggregation queries
fn bench_aggregation(c: &mut Criterion) {
    let mut group = c.benchmark_group("sql_aggregation");

    for record_count in [10000, 50000, 100000].iter() {
        let (engine, _dir) = setup_query_engine(*record_count);

        // Group by partition
        group.bench_with_input(
            BenchmarkId::new("group_by_partition", record_count),
            record_count,
            |b, _| {
                let runtime = tokio::runtime::Runtime::new().unwrap();
                b.iter(|| {
                    runtime.block_on(async {
                        let results = engine
                            .execute_sql(black_box(
                                "SELECT _partition, COUNT(*) as cnt FROM orders GROUP BY _partition",
                            ))
                            .await
                            .unwrap();
                        black_box(results)
                    })
                });
            },
        );

        // Min/Max aggregation
        group.bench_with_input(
            BenchmarkId::new("min_max", record_count),
            record_count,
            |b, _| {
                let runtime = tokio::runtime::Runtime::new().unwrap();
                b.iter(|| {
                    runtime.block_on(async {
                        let results = engine
                            .execute_sql(black_box(
                                "SELECT MIN(_offset), MAX(_offset), MIN(_timestamp), MAX(_timestamp) FROM orders",
                            ))
                            .await
                            .unwrap();
                        black_box(results)
                    })
                });
            },
        );
    }

    group.finish();
}

/// Benchmark EXPLAIN query (plan generation)
fn bench_explain(c: &mut Criterion) {
    let mut group = c.benchmark_group("sql_explain");
    group.sample_size(50);

    let (engine, _dir) = setup_query_engine(10000);

    group.bench_function("simple_select", |b| {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        b.iter(|| {
            runtime.block_on(async {
                let plan = engine
                    .explain(black_box("SELECT * FROM orders WHERE _offset > 5000"))
                    .await
                    .unwrap();
                black_box(plan)
            })
        });
    });

    group.bench_function("complex_query", |b| {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        b.iter(|| {
            runtime.block_on(async {
                let plan = engine
                    .explain(black_box(
                        "SELECT _partition, COUNT(*) FROM orders WHERE _offset BETWEEN 1000 AND 9000 GROUP BY _partition ORDER BY _partition",
                    ))
                    .await
                    .unwrap();
                black_box(plan)
            })
        });
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_select_all,
    bench_count,
    bench_where_filter,
    bench_aggregation,
    bench_explain
);
criterion_main!(benches);
