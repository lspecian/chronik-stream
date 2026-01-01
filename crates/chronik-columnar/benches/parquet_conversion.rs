//! Benchmarks for Parquet/Arrow conversion throughput
//!
//! Measures:
//! - KafkaRecord → RecordBatch conversion
//! - RecordBatch → Parquet file writing
//! - End-to-end throughput (records/second)

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use std::time::{SystemTime, UNIX_EPOCH};

use chronik_columnar::{
    ColumnarConfig, CompressionCodec,
};

// Import internal types from the crate
use chronik_columnar::writer::ParquetSegmentWriter;
use chronik_columnar::converter::{KafkaRecord, RecordBatchConverter};
use chronik_columnar::schema::kafka_message_schema;

/// Generate test KafkaRecords with realistic data
fn generate_kafka_records(count: usize, value_size: usize) -> Vec<KafkaRecord> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;

    (0..count)
        .map(|i| {
            let key = format!("key-{:08}", i);
            let value = format!(
                r#"{{"id": {}, "timestamp": "{}", "data": "{}", "user_id": "user-{}", "action": "test-action"}}"#,
                i,
                now + (i as i64 * 100),
                "x".repeat(value_size.saturating_sub(80)),
                i % 1000
            );

            let headers = if i % 10 == 0 {
                vec![
                    ("content-type".to_string(), Some(b"application/json".to_vec())),
                    ("correlation-id".to_string(), Some(format!("{}", i).into_bytes())),
                ]
            } else {
                vec![]
            };

            KafkaRecord {
                topic: "benchmark-topic".to_string(),
                partition: (i % 6) as i32,
                offset: i as i64,
                timestamp_ms: now + (i as i64 * 100),
                timestamp_type: 0,
                key: Some(key.into_bytes()),
                value: value.into_bytes(),
                headers,
                embedding: None,
            }
        })
        .collect()
}

/// Benchmark KafkaRecord → RecordBatch conversion
fn bench_record_conversion(c: &mut Criterion) {
    let mut group = c.benchmark_group("record_conversion");

    for record_count in [1000, 10000, 50000].iter() {
        let records = generate_kafka_records(*record_count, 200);

        group.throughput(Throughput::Elements(*record_count as u64));
        group.bench_with_input(
            BenchmarkId::new("convert", record_count),
            &records,
            |b, records| {
                b.iter(|| {
                    let converter = RecordBatchConverter::new();
                    let batch = converter.convert(black_box(records)).unwrap();
                    black_box(batch)
                });
            },
        );
    }

    group.finish();
}

/// Benchmark RecordBatch → Parquet file writing with different compression codecs
fn bench_parquet_write(c: &mut Criterion) {
    let mut group = c.benchmark_group("parquet_write");

    let record_count = 10000;
    let records = generate_kafka_records(record_count, 200);

    // Pre-convert to RecordBatch
    let converter = RecordBatchConverter::new();
    let batch = converter.convert(&records).unwrap();

    group.throughput(Throughput::Elements(record_count as u64));

    for compression in [
        CompressionCodec::None,
        CompressionCodec::Snappy,
        CompressionCodec::Gzip,
        CompressionCodec::Zstd,
        CompressionCodec::Lz4,
    ] {
        let codec_name = format!("{:?}", compression);

        group.bench_with_input(
            BenchmarkId::new(&codec_name, record_count),
            &batch,
            |b, batch| {
                b.iter(|| {
                    let mut config = ColumnarConfig::default();
                    config.enabled = true;
                    config.compression = compression;
                    config.row_group_size = 10000;
                    config.bloom_filter_enabled = false;

                    let schema = kafka_message_schema();
                    let writer = ParquetSegmentWriter::new(schema, config);
                    let result = writer.write_to_bytes(&[batch.clone()]).unwrap();
                    black_box(result)
                });
            },
        );
    }

    group.finish();
}

/// Benchmark end-to-end conversion (Records → Parquet bytes)
fn bench_end_to_end(c: &mut Criterion) {
    let mut group = c.benchmark_group("end_to_end");

    for record_count in [1000, 10000, 50000].iter() {
        let records = generate_kafka_records(*record_count, 200);

        group.throughput(Throughput::Elements(*record_count as u64));
        group.bench_with_input(
            BenchmarkId::new("zstd", record_count),
            &records,
            |b, records| {
                b.iter(|| {
                    // Convert records to batch
                    let converter = RecordBatchConverter::new();
                    let batch = converter.convert(black_box(records)).unwrap();

                    // Write to Parquet
                    let mut config = ColumnarConfig::default();
                    config.enabled = true;
                    config.compression = CompressionCodec::Zstd;
                    config.row_group_size = 10000;
                    config.bloom_filter_enabled = true;

                    let schema = kafka_message_schema();
                    let writer = ParquetSegmentWriter::new(schema, config);
                    let result = writer.write_to_bytes(&[batch]).unwrap();
                    black_box(result)
                });
            },
        );
    }

    group.finish();
}

/// Benchmark with embeddings (vector-enabled topics)
fn bench_with_embeddings(c: &mut Criterion) {
    let mut group = c.benchmark_group("with_embeddings");

    let record_count = 10000;
    let dimensions = 1536; // OpenAI text-embedding-3-small

    // Generate records with embeddings
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;

    let records: Vec<KafkaRecord> = (0..record_count)
        .map(|i| {
            let embedding: Vec<f32> = (0..dimensions).map(|j| ((i * 7 + j) % 256) as f32 / 256.0).collect();

            KafkaRecord {
                topic: "vector-topic".to_string(),
                partition: (i % 6) as i32,
                offset: i as i64,
                timestamp_ms: now + (i as i64 * 100),
                timestamp_type: 0,
                key: Some(format!("key-{:08}", i).into_bytes()),
                value: format!(r#"{{"id": {}, "text": "sample text for embedding"}}"#, i).into_bytes(),
                headers: vec![],
                embedding: Some(embedding),
            }
        })
        .collect();

    group.throughput(Throughput::Elements(record_count as u64));
    group.bench_with_input(
        BenchmarkId::new("zstd_with_vectors", record_count),
        &records,
        |b, records| {
            b.iter(|| {
                let converter = RecordBatchConverter::new();
                let batch = converter.convert(black_box(records)).unwrap();

                let mut config = ColumnarConfig::default();
                config.enabled = true;
                config.compression = CompressionCodec::Zstd;
                config.row_group_size = 10000;
                config.bloom_filter_enabled = false;

                let schema = kafka_message_schema();
                let writer = ParquetSegmentWriter::new(schema, config);
                let result = writer.write_to_bytes(&[batch]).unwrap();
                black_box(result)
            });
        },
    );

    group.finish();
}

/// Measure and report storage efficiency
fn bench_storage_efficiency(c: &mut Criterion) {
    let mut group = c.benchmark_group("storage_efficiency");
    group.sample_size(10);

    println!("\n=== Parquet Storage Efficiency ===\n");

    for record_count in [1000, 10000, 50000].iter() {
        let records = generate_kafka_records(*record_count, 200);

        // Calculate raw data size
        let raw_size: usize = records
            .iter()
            .map(|r| {
                8 + 4 + 8 + // offset + partition + timestamp
                r.key.as_ref().map(|k| k.len()).unwrap_or(0) +
                r.value.len() +
                r.headers.iter().map(|(k, v)| k.len() + v.as_ref().map(|x| x.len()).unwrap_or(0)).sum::<usize>()
            })
            .sum();

        // Convert
        let converter = RecordBatchConverter::new();
        let batch = converter.convert(&records).unwrap();
        let schema = kafka_message_schema();

        for compression in [
            CompressionCodec::None,
            CompressionCodec::Snappy,
            CompressionCodec::Zstd,
        ] {
            let mut config = ColumnarConfig::default();
            config.enabled = true;
            config.compression = compression;
            config.row_group_size = 10000;
            config.bloom_filter_enabled = false;

            let writer = ParquetSegmentWriter::new(schema.clone(), config);
            let (bytes, _stats) = writer.write_to_bytes(&[batch.clone()]).unwrap();
            let compressed_size = bytes.len();

            println!(
                "Records: {:6} | {:8?} | Raw: {:>10} | Parquet: {:>10} | Ratio: {:.1}%",
                record_count,
                compression,
                raw_size,
                compressed_size,
                (compressed_size as f64 / raw_size as f64) * 100.0
            );
        }
    }

    println!();
    group.finish();
}

criterion_group!(
    benches,
    bench_record_conversion,
    bench_parquet_write,
    bench_end_to_end,
    bench_with_embeddings,
    bench_storage_efficiency
);
criterion_main!(benches);
