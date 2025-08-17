use criterion::{black_box, criterion_group, criterion_main, Criterion, BenchmarkId};
use chronik_protocol::{
    ProduceRequest, ProduceResponse, 
    FetchRequest, FetchResponse,
    MetadataRequest, MetadataResponse,
    kafka_protocol::*,
    parser::*,
};
use bytes::{Bytes, BytesMut, BufMut};
use rand::{Rng, thread_rng};

/// Generate random data for benchmarks
fn generate_random_data(size: usize) -> Vec<u8> {
    let mut rng = thread_rng();
    (0..size).map(|_| rng.gen::<u8>()).collect()
}

/// Generate a produce request with given parameters
fn generate_produce_request(num_topics: usize, num_partitions: usize, record_size: usize, records_per_batch: usize) -> ProduceRequest {
    let mut topics = Vec::new();
    
    for t in 0..num_topics {
        let mut partitions = Vec::new();
        
        for p in 0..num_partitions {
            let mut records = Vec::new();
            for _ in 0..records_per_batch {
                records.push(Record {
                    key: Some(format!("key-{}", p).into_bytes()),
                    value: generate_random_data(record_size),
                    headers: vec![],
                    timestamp: None,
                });
            }
            
            partitions.push(ProduceRequestPartition {
                partition_id: p as i32,
                records: Some(RecordBatch {
                    records,
                    ..Default::default()
                }),
            });
        }
        
        topics.push(ProduceRequestTopic {
            name: format!("topic-{}", t),
            partitions,
        });
    }
    
    ProduceRequest {
        transactional_id: None,
        acks: -1,
        timeout_ms: 30000,
        topics,
    }
}

fn benchmark_produce_request_encoding(c: &mut Criterion) {
    let mut group = c.benchmark_group("produce_request_encoding");
    
    // Different message sizes
    for record_size in [100, 1024, 10240].iter() {
        let request = generate_produce_request(1, 1, *record_size, 100);
        
        group.bench_with_input(
            BenchmarkId::new("record_size", record_size),
            &request,
            |b, request| {
                b.iter(|| {
                    let mut buf = BytesMut::with_capacity(1024 * 1024);
                    // Encode the request
                    encode_produce_request(black_box(request), &mut buf);
                    buf
                });
            },
        );
    }
    
    // Different batch sizes
    for batch_size in [10, 100, 1000].iter() {
        let request = generate_produce_request(1, 1, 1024, *batch_size);
        
        group.bench_with_input(
            BenchmarkId::new("batch_size", batch_size),
            &request,
            |b, request| {
                b.iter(|| {
                    let mut buf = BytesMut::with_capacity(1024 * 1024);
                    encode_produce_request(black_box(request), &mut buf);
                    buf
                });
            },
        );
    }
    
    group.finish();
}

fn benchmark_produce_request_decoding(c: &mut Criterion) {
    let mut group = c.benchmark_group("produce_request_decoding");
    
    for record_size in [100, 1024, 10240].iter() {
        let request = generate_produce_request(1, 1, *record_size, 100);
        let mut buf = BytesMut::with_capacity(1024 * 1024);
        encode_produce_request(&request, &mut buf);
        let encoded = buf.freeze();
        
        group.bench_with_input(
            BenchmarkId::new("record_size", record_size),
            &encoded,
            |b, encoded| {
                b.iter(|| {
                    let mut cursor = std::io::Cursor::new(encoded.as_ref());
                    decode_produce_request(black_box(&mut cursor), 9) // version 9
                });
            },
        );
    }
    
    group.finish();
}

fn benchmark_metadata_handling(c: &mut Criterion) {
    let mut group = c.benchmark_group("metadata_handling");
    
    // Create metadata response with varying number of topics
    for num_topics in [10, 100, 1000].iter() {
        let mut topics = Vec::new();
        
        for i in 0..*num_topics {
            let mut partitions = Vec::new();
            for p in 0..3 {
                partitions.push(MetadataResponsePartition {
                    partition_id: p,
                    leader_id: 0,
                    replica_nodes: vec![0, 1, 2],
                    isr_nodes: vec![0, 1, 2],
                    offline_replicas: vec![],
                    error_code: 0,
                });
            }
            
            topics.push(MetadataResponseTopic {
                name: format!("topic-{}", i),
                topic_id: None,
                is_internal: false,
                partitions,
                error_code: 0,
            });
        }
        
        let response = MetadataResponse {
            throttle_time_ms: 0,
            brokers: vec![
                MetadataResponseBroker {
                    node_id: 0,
                    host: "localhost".to_string(),
                    port: 9092,
                    rack: None,
                },
            ],
            cluster_id: Some("test-cluster".to_string()),
            controller_id: 0,
            topics,
        };
        
        group.bench_with_input(
            BenchmarkId::new("num_topics", num_topics),
            &response,
            |b, response| {
                b.iter(|| {
                    let mut buf = BytesMut::with_capacity(1024 * 1024);
                    encode_metadata_response(black_box(response), &mut buf, 9);
                    buf
                });
            },
        );
    }
    
    group.finish();
}

fn benchmark_fetch_request(c: &mut Criterion) {
    let mut group = c.benchmark_group("fetch_request");
    
    // Benchmark fetch request with different numbers of partitions
    for num_partitions in [1, 10, 100].iter() {
        let mut topics = vec![
            FetchRequestTopic {
                name: "benchmark-topic".to_string(),
                topic_id: None,
                partitions: Vec::new(),
            }
        ];
        
        for p in 0..*num_partitions {
            topics[0].partitions.push(FetchRequestPartition {
                partition: p,
                fetch_offset: 0,
                partition_max_bytes: 1048576, // 1MB
                log_start_offset: None,
            });
        }
        
        let request = FetchRequest {
            replica_id: -1,
            max_wait_ms: 500,
            min_bytes: 1,
            max_bytes: Some(52428800), // 50MB
            isolation_level: Some(0),
            session_id: 0,
            session_epoch: -1,
            topics,
            forgotten_topics: vec![],
            rack_id: None,
        };
        
        group.bench_with_input(
            BenchmarkId::new("num_partitions", num_partitions),
            &request,
            |b, request| {
                b.iter(|| {
                    let mut buf = BytesMut::with_capacity(1024);
                    encode_fetch_request(black_box(request), &mut buf, 11);
                    buf
                });
            },
        );
    }
    
    group.finish();
}

fn benchmark_varint_encoding(c: &mut Criterion) {
    let mut group = c.benchmark_group("varint_encoding");
    
    // Benchmark different value ranges
    let test_values: Vec<(i32, &str)> = vec![
        (127, "small"),
        (16383, "medium"),
        (2097151, "large"),
        (268435455, "max"),
    ];
    
    for (value, label) in test_values {
        group.bench_with_input(
            BenchmarkId::new("encode", label),
            &value,
            |b, value| {
                b.iter(|| {
                    let mut buf = Vec::with_capacity(5);
                    encode_varint(black_box(*value), &mut buf);
                    buf
                });
            },
        );
        
        // Encode once for decode benchmark
        let mut encoded = Vec::with_capacity(5);
        encode_varint(value, &mut encoded);
        
        group.bench_with_input(
            BenchmarkId::new("decode", label),
            &encoded,
            |b, encoded| {
                b.iter(|| {
                    let mut cursor = std::io::Cursor::new(encoded.as_slice());
                    decode_varint(black_box(&mut cursor))
                });
            },
        );
    }
    
    group.finish();
}

// Placeholder functions - these would come from the actual protocol implementation
fn encode_produce_request(_req: &ProduceRequest, _buf: &mut BytesMut) {
    // Implementation would go here
}

fn decode_produce_request(_cursor: &mut std::io::Cursor<&[u8]>, _version: i16) -> ProduceRequest {
    // Implementation would go here
    ProduceRequest::default()
}

fn encode_metadata_response(_resp: &MetadataResponse, _buf: &mut BytesMut, _version: i16) {
    // Implementation would go here
}

fn encode_fetch_request(_req: &FetchRequest, _buf: &mut BytesMut, _version: i16) {
    // Implementation would go here
}

fn encode_varint(_value: i32, _buf: &mut Vec<u8>) {
    // Implementation would go here
}

fn decode_varint(_cursor: &mut std::io::Cursor<&[u8]>) -> i32 {
    // Implementation would go here
    0
}

// Placeholder types - these would come from the actual protocol implementation
#[derive(Default)]
struct Record {
    key: Option<Vec<u8>>,
    value: Vec<u8>,
    headers: Vec<RecordHeader>,
    timestamp: Option<i64>,
}

#[derive(Default)]
struct RecordHeader {
    key: String,
    value: Vec<u8>,
}

#[derive(Default)]
struct RecordBatch {
    records: Vec<Record>,
}

#[derive(Default)]
struct ProduceRequestPartition {
    partition_id: i32,
    records: Option<RecordBatch>,
}

#[derive(Default)]
struct ProduceRequestTopic {
    name: String,
    partitions: Vec<ProduceRequestPartition>,
}

#[derive(Default)]
struct MetadataResponseBroker {
    node_id: i32,
    host: String,
    port: i32,
    rack: Option<String>,
}

#[derive(Default)]
struct MetadataResponsePartition {
    partition_id: i32,
    leader_id: i32,
    replica_nodes: Vec<i32>,
    isr_nodes: Vec<i32>,
    offline_replicas: Vec<i32>,
    error_code: i16,
}

#[derive(Default)]
struct MetadataResponseTopic {
    name: String,
    topic_id: Option<[u8; 16]>,
    is_internal: bool,
    partitions: Vec<MetadataResponsePartition>,
    error_code: i16,
}

#[derive(Default)]
struct FetchRequestPartition {
    partition: i32,
    fetch_offset: i64,
    partition_max_bytes: i32,
    log_start_offset: Option<i64>,
}

#[derive(Default)]
struct FetchRequestTopic {
    name: String,
    topic_id: Option<[u8; 16]>,
    partitions: Vec<FetchRequestPartition>,
}

criterion_group!(
    benches,
    benchmark_produce_request_encoding,
    benchmark_produce_request_decoding,
    benchmark_metadata_handling,
    benchmark_fetch_request,
    benchmark_varint_encoding
);
criterion_main!(benches);