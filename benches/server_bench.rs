//! Performance benchmarks for the TCP server

use bytes::{BytesMut};
use chronik_ingest::{IngestServer, ServerConfig, ConnectionPoolConfig};
use chronik_protocol::parser::Encoder;
use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::runtime::Runtime;
use tokio::time::sleep;

/// Create a metadata request
fn create_metadata_request(correlation_id: i32) -> Vec<u8> {
    let mut buf = BytesMut::new();
    let mut encoder = Encoder::new(&mut buf);
    
    // Request header
    encoder.write_i16(3); // API key (Metadata)
    encoder.write_i16(9); // API version
    encoder.write_i32(correlation_id);
    encoder.write_string(Some("bench-client")); // Client ID
    
    // Request body
    encoder.write_i32(-1); // All topics
    encoder.write_bool(false); // Don't auto-create topics
    encoder.write_bool(false); // Include cluster authorized operations
    encoder.write_bool(false); // Include topic authorized operations
    
    buf.to_vec()
}

/// Create a produce request with variable size payload
fn create_produce_request(correlation_id: i32, payload_size: usize) -> Vec<u8> {
    let mut buf = BytesMut::new();
    let mut encoder = Encoder::new(&mut buf);
    
    // Request header
    encoder.write_i16(0); // API key (Produce)
    encoder.write_i16(8); // API version
    encoder.write_i32(correlation_id);
    encoder.write_string(Some("bench-client")); // Client ID
    
    // Request body
    encoder.write_string(None); // Transactional ID
    encoder.write_i16(0); // Acks (0 = no acknowledgment)
    encoder.write_i32(30000); // Timeout ms
    
    // Topics array
    encoder.write_i32(1); // One topic
    encoder.write_string(Some("bench-topic"));
    
    // Partitions array
    encoder.write_i32(1); // One partition
    encoder.write_i32(0); // Partition 0
    
    // Create dummy payload
    let payload = vec![0u8; payload_size];
    encoder.write_bytes(Some(&payload));
    
    buf.to_vec()
}

/// Send request and receive response
async fn send_request(stream: &mut TcpStream, request: Vec<u8>) -> Result<(), Box<dyn std::error::Error>> {
    // Send size header
    let size = request.len() as u32;
    stream.write_all(&size.to_be_bytes()).await?;
    
    // Send request
    stream.write_all(&request).await?;
    stream.flush().await?;
    
    // Read response size
    let mut size_buf = [0u8; 4];
    stream.read_exact(&mut size_buf).await?;
    let response_size = u32::from_be_bytes(size_buf) as usize;
    
    // Read response
    let mut response = vec![0u8; response_size];
    stream.read_exact(&mut response).await?;
    
    Ok(())
}

/// Benchmark metadata requests
fn bench_metadata_requests(c: &mut Criterion) {
    let runtime = Runtime::new().unwrap();
    
    // Setup server
    let addr: SocketAddr = "127.0.0.1:19200".parse().unwrap();
    let config = ServerConfig {
        listen_addr: addr,
        max_connections: 1000,
        request_timeout: Duration::from_secs(30),
        buffer_size: 1024 * 1024,
        idle_timeout: Duration::from_secs(600),
        tls_config: None,
        pool_config: ConnectionPoolConfig::default(),
        backpressure_threshold: 10000,
        shutdown_timeout: Duration::from_secs(30),
        metrics_interval: Duration::from_secs(60),
    };
    
    let data_dir = tempfile::tempdir().unwrap();
    let server = runtime.block_on(async {
        IngestServer::new(config.clone(), data_dir.path().to_path_buf()).await.unwrap()
    });
    
    let _handle = runtime.block_on(server.start()).unwrap();
    runtime.block_on(sleep(Duration::from_millis(500)));
    
    let mut group = c.benchmark_group("metadata_requests");
    group.throughput(Throughput::Elements(1));
    
    group.bench_function("single_request", |b| {
        b.iter(|| {
            runtime.block_on(async {
                let mut stream = TcpStream::connect(addr).await.unwrap();
                let request = create_metadata_request(1);
                send_request(&mut stream, request).await.unwrap();
            });
        });
    });
    
    group.bench_function("pipelined_requests", |b| {
        b.iter(|| {
            runtime.block_on(async {
                let mut stream = TcpStream::connect(addr).await.unwrap();
                for i in 0..10 {
                    let request = create_metadata_request(i);
                    send_request(&mut stream, request).await.unwrap();
                }
            });
        });
    });
    
    group.finish();
}

/// Benchmark produce requests with different payload sizes
fn bench_produce_requests(c: &mut Criterion) {
    let runtime = Runtime::new().unwrap();
    
    // Setup server
    let addr: SocketAddr = "127.0.0.1:19201".parse().unwrap();
    let config = ServerConfig {
        listen_addr: addr,
        max_connections: 1000,
        request_timeout: Duration::from_secs(30),
        buffer_size: 10 * 1024 * 1024, // 10MB
        idle_timeout: Duration::from_secs(600),
        tls_config: None,
        pool_config: ConnectionPoolConfig::default(),
        backpressure_threshold: 10000,
        shutdown_timeout: Duration::from_secs(30),
        metrics_interval: Duration::from_secs(60),
    };
    
    let data_dir = tempfile::tempdir().unwrap();
    let server = runtime.block_on(async {
        IngestServer::new(config.clone(), data_dir.path().to_path_buf()).await.unwrap()
    });
    
    let _handle = runtime.block_on(server.start()).unwrap();
    runtime.block_on(sleep(Duration::from_millis(500)));
    
    let mut group = c.benchmark_group("produce_requests");
    
    for payload_size in &[100, 1024, 10240, 102400] {
        group.throughput(Throughput::Bytes(*payload_size as u64));
        group.bench_function(format!("payload_{}_bytes", payload_size), |b| {
            b.iter(|| {
                runtime.block_on(async {
                    let mut stream = TcpStream::connect(addr).await.unwrap();
                    let request = create_produce_request(1, *payload_size);
                    send_request(&mut stream, request).await.unwrap();
                });
            });
        });
    }
    
    group.finish();
}

/// Benchmark concurrent connections
fn bench_concurrent_connections(c: &mut Criterion) {
    let runtime = Runtime::new().unwrap();
    
    // Setup server
    let addr: SocketAddr = "127.0.0.1:19202".parse().unwrap();
    let config = ServerConfig {
        listen_addr: addr,
        max_connections: 10000,
        request_timeout: Duration::from_secs(30),
        buffer_size: 1024 * 1024,
        idle_timeout: Duration::from_secs(600),
        tls_config: None,
        pool_config: ConnectionPoolConfig {
            max_per_ip: 1000,
            rate_limit: 1000,
            ban_duration: Duration::from_secs(1),
        },
        backpressure_threshold: 10000,
        shutdown_timeout: Duration::from_secs(30),
        metrics_interval: Duration::from_secs(60),
    };
    
    let data_dir = tempfile::tempdir().unwrap();
    let server = runtime.block_on(async {
        IngestServer::new(config.clone(), data_dir.path().to_path_buf()).await.unwrap()
    });
    
    let _handle = runtime.block_on(server.start()).unwrap();
    runtime.block_on(sleep(Duration::from_millis(500)));
    
    let mut group = c.benchmark_group("concurrent_connections");
    
    for num_clients in &[10, 50, 100, 500] {
        group.bench_function(format!("{}_clients", num_clients), |b| {
            b.iter(|| {
                runtime.block_on(async {
                    let counter = Arc::new(AtomicU64::new(0));
                    let mut handles = vec![];
                    
                    for i in 0..*num_clients {
                        let counter = counter.clone();
                        let handle = tokio::spawn(async move {
                            match TcpStream::connect(addr).await {
                                Ok(mut stream) => {
                                    let request = create_metadata_request(i);
                                    if send_request(&mut stream, request).await.is_ok() {
                                        counter.fetch_add(1, Ordering::Relaxed);
                                    }
                                }
                                Err(_) => {}
                            }
                        });
                        handles.push(handle);
                    }
                    
                    // Wait for all clients
                    for handle in handles {
                        let _ = handle.await;
                    }
                    
                    let completed = counter.load(Ordering::Relaxed);
                    assert!(completed > 0);
                });
            });
        });
    }
    
    group.finish();
}

/// Benchmark request throughput
fn bench_throughput(c: &mut Criterion) {
    let runtime = Runtime::new().unwrap();
    
    // Setup server
    let addr: SocketAddr = "127.0.0.1:19203".parse().unwrap();
    let config = ServerConfig {
        listen_addr: addr,
        max_connections: 10000,
        request_timeout: Duration::from_secs(30),
        buffer_size: 1024 * 1024,
        idle_timeout: Duration::from_secs(600),
        tls_config: None,
        pool_config: ConnectionPoolConfig::default(),
        backpressure_threshold: 10000,
        shutdown_timeout: Duration::from_secs(30),
        metrics_interval: Duration::from_secs(60),
    };
    
    let data_dir = tempfile::tempdir().unwrap();
    let server = runtime.block_on(async {
        IngestServer::new(config.clone(), data_dir.path().to_path_buf()).await.unwrap()
    });
    
    let _handle = runtime.block_on(server.start()).unwrap();
    runtime.block_on(sleep(Duration::from_millis(500)));
    
    let mut group = c.benchmark_group("throughput");
    group.measurement_time(Duration::from_secs(10));
    
    group.bench_function("requests_per_second", |b| {
        b.iter(|| {
            runtime.block_on(async {
                let duration = Duration::from_secs(1);
                let start = tokio::time::Instant::now();
                let mut count = 0u64;
                
                let mut stream = TcpStream::connect(addr).await.unwrap();
                
                while start.elapsed() < duration {
                    let request = create_metadata_request(count as i32);
                    if send_request(&mut stream, request).await.is_ok() {
                        count += 1;
                    }
                }
                
                count
            });
        });
    });
    
    group.finish();
}

criterion_group!(
    benches,
    bench_metadata_requests,
    bench_produce_requests,
    bench_concurrent_connections,
    bench_throughput
);
criterion_main!(benches);