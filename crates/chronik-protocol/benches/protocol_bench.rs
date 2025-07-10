//! Performance benchmarks for Kafka protocol serialization/deserialization.

use bytes::{BufMut, BytesMut};
use chronik_protocol::handler::ProtocolHandler;
use chronik_protocol::parser::{Encoder, supported_api_versions};
use criterion::{black_box, criterion_group, criterion_main, Criterion, Throughput};

/// Benchmark API versions response encoding
fn bench_api_versions_encoding(c: &mut Criterion) {
    let mut group = c.benchmark_group("api_versions_encoding");
    
    // Test different versions
    let versions = vec![
        ("v0", 0),
        ("v1", 1),
        ("v3_flexible", 3),
    ];
    
    for (name, version) in versions {
        group.throughput(Throughput::Elements(1));
        group.bench_function(name, |b| {
            b.iter(|| {
                let mut buf = BytesMut::with_capacity(512);
                let mut encoder = Encoder::new(&mut buf);
                
                // Correlation ID
                encoder.write_i32(12345);
                
                match version {
                    0 => {
                        // v0: array before error code
                        let api_versions = supported_api_versions();
                        encoder.write_i32(api_versions.len() as i32);
                        
                        for (api_key, version_range) in api_versions {
                            encoder.write_i16(api_key as i16);
                            encoder.write_i16(version_range.min);
                            encoder.write_i16(version_range.max);
                        }
                        
                        encoder.write_i16(0); // error code
                    }
                    1 => {
                        // v1: error code first, then array, then throttle time
                        encoder.write_i16(0); // error code
                        
                        let api_versions = supported_api_versions();
                        encoder.write_i32(api_versions.len() as i32);
                        
                        for (api_key, version_range) in api_versions {
                            encoder.write_i16(api_key as i16);
                            encoder.write_i16(version_range.min);
                            encoder.write_i16(version_range.max);
                        }
                        
                        encoder.write_i32(0); // throttle time
                    }
                    3 => {
                        // v3: flexible version with varints
                        encoder.write_i16(0); // error code
                        
                        let api_versions = supported_api_versions();
                        encoder.write_unsigned_varint((api_versions.len() + 1) as u32);
                        
                        for (api_key, version_range) in api_versions {
                            encoder.write_i16(api_key as i16);
                            encoder.write_i16(version_range.min);
                            encoder.write_i16(version_range.max);
                            encoder.write_unsigned_varint(0); // tagged fields
                        }
                        
                        encoder.write_i32(0); // throttle time
                        encoder.write_unsigned_varint(0); // tagged fields
                    }
                    _ => unreachable!(),
                }
                
                black_box(buf);
            });
        });
    }
    
    group.finish();
}

/// Benchmark string encoding performance
fn bench_string_encoding(c: &mut Criterion) {
    let mut group = c.benchmark_group("string_encoding");
    
    let test_strings = vec![
        ("short", "test"),
        ("medium", "this-is-a-medium-length-client-id-string"),
        ("long", "this-is-a-very-long-client-id-string-that-simulates-real-world-usage-patterns-in-production-environments"),
    ];
    
    for (name, string) in test_strings {
        group.throughput(Throughput::Bytes(string.len() as u64));
        
        // Legacy string encoding
        group.bench_function(format!("{}_legacy", name), |b| {
            b.iter(|| {
                let mut buf = BytesMut::with_capacity(512);
                let mut encoder = Encoder::new(&mut buf);
                encoder.write_string(Some(black_box(string)));
                black_box(buf);
            });
        });
        
        // Compact string encoding
        group.bench_function(format!("{}_compact", name), |b| {
            b.iter(|| {
                let mut buf = BytesMut::with_capacity(512);
                let mut encoder = Encoder::new(&mut buf);
                encoder.write_compact_string(Some(black_box(string)));
                black_box(buf);
            });
        });
    }
    
    group.finish();
}

/// Benchmark varint encoding performance
fn bench_varint_encoding(c: &mut Criterion) {
    let mut group = c.benchmark_group("varint_encoding");
    
    let test_values = vec![
        ("small", 127u32),
        ("medium", 16383u32),
        ("large", 2097151u32),
        ("max", u32::MAX),
    ];
    
    for (name, value) in test_values {
        group.bench_function(name, |b| {
            b.iter(|| {
                let mut buf = BytesMut::with_capacity(16);
                let mut encoder = Encoder::new(&mut buf);
                encoder.write_unsigned_varint(black_box(value));
                black_box(buf);
            });
        });
    }
    
    group.finish();
}

/// Benchmark full request handling
fn bench_request_handling(c: &mut Criterion) {
    let mut group = c.benchmark_group("request_handling");
    
    // Create test requests
    let requests = vec![
        ("api_versions_v0", {
            let mut req = BytesMut::new();
            req.put_i16(18); // ApiVersions
            req.put_i16(0); // Version
            req.put_i32(12345);
            req.put_i16(4);
            req.put_slice(b"test");
            req.freeze()
        }),
        ("metadata_v9", {
            let mut req = BytesMut::new();
            req.put_i16(3); // Metadata
            req.put_i16(9); // Version
            req.put_i32(12345);
            req.put_i16(4);
            req.put_slice(b"test");
            req.put_i32(-1); // topics: null
            req.put_u8(1); // allow_auto_topic_creation
            req.put_u8(0); // include_cluster_authorized_operations
            req.put_u8(0); // include_topic_authorized_operations
            req.freeze()
        }),
        ("describe_configs_v0", {
            let mut req = BytesMut::new();
            req.put_i16(32); // DescribeConfigs
            req.put_i16(0); // Version
            req.put_i32(12345);
            req.put_i16(4);
            req.put_slice(b"test");
            req.put_i32(0); // resources: empty
            req.freeze()
        }),
    ];
    
    let rt = tokio::runtime::Runtime::new().unwrap();
    
    for (name, request) in requests {
        group.throughput(Throughput::Bytes(request.len() as u64));
        group.bench_function(name, |b| {
            b.iter(|| {
                rt.block_on(async {
                    let handler = ProtocolHandler::new();
                    let result = handler.handle_request(black_box(&request)).await;
                    black_box(result);
                });
            });
        });
    }
    
    group.finish();
}

/// Benchmark array encoding with different sizes
fn bench_array_encoding(c: &mut Criterion) {
    let mut group = c.benchmark_group("array_encoding");
    
    let sizes = vec![
        ("empty", 0),
        ("small", 10),
        ("medium", 100),
        ("large", 1000),
    ];
    
    for (name, size) in sizes {
        group.throughput(Throughput::Elements(size as u64));
        group.bench_function(name, |b| {
            b.iter(|| {
                let mut buf = BytesMut::with_capacity(size * 8 + 16);
                let mut encoder = Encoder::new(&mut buf);
                
                // Write array length
                encoder.write_i32(black_box(size as i32));
                
                // Write array elements (simulating broker IDs)
                for i in 0..size {
                    encoder.write_i32(i as i32);
                }
                
                black_box(buf);
            });
        });
    }
    
    group.finish();
}

/// Benchmark metadata response encoding
fn bench_metadata_response_encoding(c: &mut Criterion) {
    let mut group = c.benchmark_group("metadata_response");
    
    let broker_counts = vec![
        ("single_broker", 1),
        ("small_cluster", 3),
        ("medium_cluster", 9),
        ("large_cluster", 30),
    ];
    
    for (name, broker_count) in broker_counts {
        group.throughput(Throughput::Elements(broker_count as u64));
        group.bench_function(name, |b| {
            b.iter(|| {
                let mut buf = BytesMut::with_capacity(4096);
                let mut encoder = Encoder::new(&mut buf);
                
                // Correlation ID
                encoder.write_i32(12345);
                
                // Throttle time (v3+)
                encoder.write_i32(0);
                
                // Brokers array
                encoder.write_i32(black_box(broker_count));
                for i in 0..broker_count {
                    encoder.write_i32(i); // node_id
                    encoder.write_string(Some(&format!("broker-{}.example.com", i))); // host
                    encoder.write_i32(9092); // port
                    encoder.write_string(None); // rack: null
                }
                
                // Cluster ID
                encoder.write_string(Some("test-cluster"));
                
                // Controller ID
                encoder.write_i32(0);
                
                // Topics array (empty)
                encoder.write_i32(0);
                
                black_box(buf);
            });
        });
    }
    
    group.finish();
}

/// Benchmark message batch compression
fn bench_message_compression(c: &mut Criterion) {
    let mut group = c.benchmark_group("message_compression");
    
    // Create test data
    let message_sizes = vec![
        ("small", vec![0u8; 100]),
        ("medium", vec![0u8; 1024]),
        ("large", vec![0u8; 10240]),
    ];
    
    for (name, message) in message_sizes {
        group.throughput(Throughput::Bytes(message.len() as u64));
        
        // No compression baseline
        group.bench_function(format!("{}_none", name), |b| {
            b.iter(|| {
                let mut buf = BytesMut::with_capacity(message.len() + 100);
                buf.put_slice(black_box(&message));
                black_box(buf);
            });
        });
        
        // GZIP compression
        group.bench_function(format!("{}_gzip", name), |b| {
            use flate2::write::GzEncoder;
            use flate2::Compression;
            use std::io::Write;
            
            b.iter(|| {
                let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
                encoder.write_all(black_box(&message)).unwrap();
                let compressed = encoder.finish().unwrap();
                black_box(compressed);
            });
        });
    }
    
    group.finish();
}

criterion_group!(
    benches,
    bench_api_versions_encoding,
    bench_string_encoding,
    bench_varint_encoding,
    bench_request_handling,
    bench_array_encoding,
    bench_metadata_response_encoding,
    bench_message_compression
);
criterion_main!(benches);