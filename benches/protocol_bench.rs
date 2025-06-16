//! Benchmarks for protocol encoding/decoding.

use chronik_protocol::{
    ProduceRequest, FetchRequest, Record, TopicProduceData, PartitionProduceData,
    FetchTopic, FetchPartition, RequestHeader, ApiKey,
};
use criterion::{black_box, criterion_group, criterion_main, Criterion, Throughput};
use std::io::Cursor;

fn create_produce_request(message_count: usize) -> ProduceRequest {
    let mut records = Vec::with_capacity(message_count);
    for i in 0..message_count {
        records.push(Record {
            offset: i as i64,
            timestamp: 1234567890,
            key: Some(format!("key-{}", i).into_bytes()),
            value: Some(format!("value-{}", i).into_bytes()),
            headers: vec![],
        });
    }
    
    ProduceRequest {
        acks: 1,
        timeout_ms: 1000,
        topic_data: vec![
            TopicProduceData {
                name: "benchmark-topic".to_string(),
                partition_data: vec![
                    PartitionProduceData {
                        index: 0,
                        records: Some(records),
                    },
                ],
            },
        ],
    }
}

fn benchmark_produce_encoding(c: &mut Criterion) {
    let mut group = c.benchmark_group("produce_encoding");
    
    for message_count in [10, 100, 1000] {
        group.throughput(Throughput::Elements(message_count as u64));
        group.bench_with_input(
            format!("{}_messages", message_count),
            &message_count,
            |b, &count| {
                let request = create_produce_request(count);
                b.iter(|| {
                    let mut buffer = Vec::new();
                    request.encode(&mut buffer).unwrap();
                    black_box(buffer);
                });
            },
        );
    }
    
    group.finish();
}

fn benchmark_produce_decoding(c: &mut Criterion) {
    let mut group = c.benchmark_group("produce_decoding");
    
    for message_count in [10, 100, 1000] {
        group.throughput(Throughput::Elements(message_count as u64));
        
        let request = create_produce_request(message_count);
        let mut encoded = Vec::new();
        request.encode(&mut encoded).unwrap();
        
        group.bench_with_input(
            format!("{}_messages", message_count),
            &encoded,
            |b, encoded| {
                b.iter(|| {
                    let mut cursor = Cursor::new(encoded);
                    let decoded = ProduceRequest::decode(&mut cursor).unwrap();
                    black_box(decoded);
                });
            },
        );
    }
    
    group.finish();
}

fn benchmark_fetch_request(c: &mut Criterion) {
    let mut group = c.benchmark_group("fetch_request");
    
    for partition_count in [1, 10, 100] {
        group.bench_with_input(
            format!("{}_partitions", partition_count),
            &partition_count,
            |b, &count| {
                let mut partitions = Vec::with_capacity(count);
                for i in 0..count {
                    partitions.push(FetchPartition {
                        partition: i as i32,
                        fetch_offset: 0,
                        partition_max_bytes: 1024 * 1024,
                    });
                }
                
                let request = FetchRequest {
                    replica_id: -1,
                    max_wait_ms: 100,
                    min_bytes: 1,
                    max_bytes: Some(1024 * 1024),
                    isolation_level: Some(0),
                    topics: vec![
                        FetchTopic {
                            name: "benchmark-topic".to_string(),
                            partitions,
                        },
                    ],
                };
                
                b.iter(|| {
                    let mut buffer = Vec::new();
                    request.encode(&mut buffer).unwrap();
                    black_box(buffer);
                });
            },
        );
    }
    
    group.finish();
}

fn benchmark_header_encoding(c: &mut Criterion) {
    c.bench_function("request_header_encode", |b| {
        let header = RequestHeader {
            api_key: ApiKey::Produce as i16,
            api_version: 7,
            correlation_id: 12345,
            client_id: Some("benchmark-client".to_string()),
        };
        
        b.iter(|| {
            let mut buffer = Vec::new();
            header.encode(&mut buffer).unwrap();
            black_box(buffer);
        });
    });
}

criterion_group!(
    protocol_benches,
    benchmark_produce_encoding,
    benchmark_produce_decoding,
    benchmark_fetch_request,
    benchmark_header_encoding
);
criterion_main!(protocol_benches);