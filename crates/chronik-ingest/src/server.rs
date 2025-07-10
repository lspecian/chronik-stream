//! Production-ready TCP server for handling Kafka protocol requests.
//! 
//! Features:
//! - Connection lifecycle management with proper cleanup
//! - Connection pooling and backpressure handling
//! - TLS termination support for secure connections
//! - Graceful shutdown and error handling
//! - Comprehensive logging and metrics
//! - Integration with Kafka protocol parser and frame codec

use crate::kafka_handler::KafkaProtocolHandler;
use crate::produce_handler::{ProduceHandler, ProduceHandlerConfig};
use crate::storage::{StorageService, StorageConfig};
use bytes::{Buf, Bytes, BytesMut, BufMut};
use chronik_auth::tls::TlsAcceptor;
use chronik_common::{Result, Error};
use chronik_common::metadata::traits::MetadataStore;
use chronik_common::metadata::TiKVMetadataStore;
use chronik_monitoring::ServerMetrics;
use chronik_protocol::frame::KafkaFrameCodec;
use chronik_storage::{SegmentReader, SegmentReaderConfig};
use futures::{SinkExt, StreamExt};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{RwLock, Semaphore, oneshot};
use tokio::task::JoinHandle;
use tokio::time::{timeout, interval};
use tokio_util::codec::Framed;
use tracing::{debug, error, info, warn, instrument};
use uuid::Uuid;

/// TCP server configuration
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// Listen address
    pub listen_addr: SocketAddr,
    /// Max concurrent connections
    pub max_connections: usize,
    /// Request timeout
    pub request_timeout: Duration,
    /// Buffer size for reading
    pub buffer_size: usize,
    /// Connection idle timeout
    pub idle_timeout: Duration,
    /// TLS configuration
    pub tls_config: Option<TlsConfig>,
    /// Connection pool configuration
    pub pool_config: ConnectionPoolConfig,
    /// Backpressure threshold (max pending requests per connection)
    pub backpressure_threshold: usize,
    /// Server shutdown timeout
    pub shutdown_timeout: Duration,
    /// Metrics collection interval
    pub metrics_interval: Duration,
}

/// TLS configuration
#[derive(Debug, Clone)]
pub struct TlsConfig {
    /// Path to certificate file
    pub cert_path: PathBuf,
    /// Path to private key file
    pub key_path: PathBuf,
    /// Optional CA certificate for client authentication
    pub ca_path: Option<PathBuf>,
    /// Require client certificate
    pub require_client_cert: bool,
}

/// Connection pool configuration
#[derive(Debug, Clone)]
pub struct ConnectionPoolConfig {
    /// Max connections per client IP
    pub max_per_ip: usize,
    /// Connection rate limit per IP (connections per second)
    pub rate_limit: usize,
    /// Ban duration for rate limit violations
    pub ban_duration: Duration,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            listen_addr: "0.0.0.0:9092".parse().unwrap(),
            max_connections: 10000,
            request_timeout: Duration::from_secs(30),
            buffer_size: 104857600, // 100MB default
            idle_timeout: Duration::from_secs(600), // 10 minutes
            tls_config: None,
            pool_config: ConnectionPoolConfig::default(),
            backpressure_threshold: 1000,
            shutdown_timeout: Duration::from_secs(30),
            metrics_interval: Duration::from_secs(60),
        }
    }
}

impl Default for ConnectionPoolConfig {
    fn default() -> Self {
        Self {
            max_per_ip: 100,
            rate_limit: 10,
            ban_duration: Duration::from_secs(300),
        }
    }
}

/// Connection metadata
#[derive(Debug)]
struct ConnectionInfo {
    id: Uuid,
    peer_addr: SocketAddr,
    connected_at: Instant,
    last_activity: AtomicU64, // Stores milliseconds since start
    request_count: AtomicUsize,
    bytes_sent: AtomicUsize,
    bytes_received: AtomicUsize,
    is_tls: bool,
    start_time: Instant, // For calculating relative times
}

/// Connection state
struct ConnectionState {
    info: Arc<ConnectionInfo>,
    pending_requests: AtomicUsize,
    shutdown_tx: Option<oneshot::Sender<()>>,
}

/// Ingest server that handles Kafka protocol connections
pub struct IngestServer {
    config: ServerConfig,
    kafka_handler: Arc<KafkaProtocolHandler>,
    metadata_store: Arc<dyn MetadataStore>,
    
    // Server state
    running: Arc<AtomicBool>,
    connection_count: Arc<AtomicUsize>,
    connections: Arc<RwLock<HashMap<Uuid, ConnectionState>>>,
    connection_limiter: Arc<Semaphore>,
    
    // Connection tracking per IP
    ip_connections: Arc<RwLock<HashMap<String, Vec<Uuid>>>>,
    ip_rate_limiter: Arc<RwLock<HashMap<String, RateLimitInfo>>>,
    
    // Metrics
    metrics: Arc<ServerMetrics>,
    
    // TLS acceptor
    tls_acceptor: Option<Arc<TlsAcceptor>>,
}

/// Rate limit tracking per IP
struct RateLimitInfo {
    last_reset: Instant,
    connection_count: usize,
    banned_until: Option<Instant>,
}

impl IngestServer {
    /// Create a new ingest server
    pub async fn new(config: ServerConfig, data_dir: PathBuf) -> Result<Self> {
        // Create metadata store using TiKV
        let pd_endpoints = std::env::var("TIKV_PD_ENDPOINTS")
            .unwrap_or_else(|_| "localhost:2379".to_string());
        let endpoints = pd_endpoints
            .split(',')
            .map(|s| s.to_string())
            .collect::<Vec<String>>();
        let metadata_store: Arc<dyn MetadataStore> = Arc::new(
            TiKVMetadataStore::new(endpoints).await
                .map_err(|e| Error::Internal(format!("Failed to create TiKV metadata store: {}", e)))?
        );
        
        // Create storage service
        let storage_config = StorageConfig {
            object_store_config: chronik_storage::ObjectStoreConfig {
                backend: chronik_storage::object_store::config::StorageBackend::Local { 
                    path: data_dir.join("segments").to_string_lossy().to_string() 
                },
                bucket: "chronik".to_string(),
                prefix: None,
                connection: Default::default(),
                performance: Default::default(),
                retry: Default::default(),
                auth: Default::default(),
                default_metadata: None,
                encryption: None,
            },
            segment_writer_config: chronik_storage::SegmentWriterConfig {
                data_dir: data_dir.join("segments"),
                compression_codec: "snappy".to_string(),
                max_segment_size: 1024 * 1024 * 1024, // 1GB
            },
            segment_reader_config: SegmentReaderConfig::default(),
        };
        let storage_service = Arc::new(StorageService::new(storage_config.clone()).await?);
        
        // Create segment reader
        let segment_reader = Arc::new(SegmentReader::new(
            storage_config.segment_reader_config.clone(),
            storage_service.object_store(),
        ));
        
        // Create produce handler
        let produce_config = ProduceHandlerConfig {
            node_id: 1, // TODO: Make configurable
            storage_config: storage_config.clone(),
            enable_indexing: true,
            enable_idempotence: true,
            enable_transactions: false,
            max_in_flight_requests: 5,
            batch_size: 1000,
            linger_ms: 100,
            compression_type: chronik_storage::kafka_records::CompressionType::None,
            request_timeout_ms: 30000,
            buffer_memory: 32 * 1024 * 1024, // 32MB
        };
        let produce_handler = Arc::new(
            ProduceHandler::new(produce_config, storage_service.object_store(), metadata_store.clone()).await?
        );
        
        // Create Kafka protocol handler
        let kafka_handler = Arc::new(
            KafkaProtocolHandler::new(
                produce_handler,
                segment_reader,
                metadata_store.clone(),
                1, // node_id
                config.listen_addr.ip().to_string(),
                config.listen_addr.port() as i32,
            ).await?
        );
        
        // Setup TLS if configured
        let tls_acceptor = if let Some(tls_config) = &config.tls_config {
            use chronik_auth::tls::{TlsAcceptor, TlsConfig as AuthTlsConfig};
            
            let auth_tls_config = AuthTlsConfig {
                cert_file: tls_config.cert_path.to_string_lossy().to_string(),
                key_file: tls_config.key_path.to_string_lossy().to_string(),
                ca_file: tls_config.ca_path.as_ref().map(|p| p.to_string_lossy().to_string()),
                verify_client: tls_config.require_client_cert,
                min_version: "1.2".to_string(),
            };
            
            let acceptor = TlsAcceptor::new(&auth_tls_config)
                .map_err(|e| Error::Configuration(format!("Failed to create TLS acceptor: {}", e)))?;
            
            Some(Arc::new(acceptor))
        } else {
            None
        };
        
        // Initialize metrics
        let metrics = Arc::new(ServerMetrics::new());
        
        Ok(Self {
            kafka_handler,
            metadata_store,
            running: Arc::new(AtomicBool::new(false)),
            connection_count: Arc::new(AtomicUsize::new(0)),
            connections: Arc::new(RwLock::new(HashMap::new())),
            connection_limiter: Arc::new(Semaphore::new(config.max_connections)),
            ip_connections: Arc::new(RwLock::new(HashMap::new())),
            ip_rate_limiter: Arc::new(RwLock::new(HashMap::new())),
            metrics,
            tls_acceptor,
            config,
        })
    }
    
    /// Start the server
    #[instrument(skip(self))]
    pub async fn start(&self) -> Result<JoinHandle<Result<()>>> {
        if self.running.load(Ordering::SeqCst) {
            return Err(Error::Internal("Server already running".into()));
        }
        
        self.running.store(true, Ordering::SeqCst);
        
        let listener = TcpListener::bind(self.config.listen_addr).await
            .map_err(|e| Error::Network(format!("Failed to bind: {}", e)))?;
        
        info!("Ingest server listening on {}", self.config.listen_addr);
        if self.tls_acceptor.is_some() {
            info!("TLS enabled");
        }
        
        // Start metrics collection task
        let metrics_handle = self.start_metrics_collection();
        
        // Start connection cleanup task
        let cleanup_handle = self.start_connection_cleanup();
        
        let running = Arc::clone(&self.running);
        let config = self.config.clone();
        let kafka_handler = Arc::clone(&self.kafka_handler);
        let connections = Arc::clone(&self.connections);
        let connection_count = Arc::clone(&self.connection_count);
        let connection_limiter = Arc::clone(&self.connection_limiter);
        let ip_connections = Arc::clone(&self.ip_connections);
        let ip_rate_limiter = Arc::clone(&self.ip_rate_limiter);
        let metrics = Arc::clone(&self.metrics);
        let tls_acceptor = self.tls_acceptor.clone();
        
        let handle = tokio::spawn(async move {
            let mut shutdown_handles = vec![metrics_handle, cleanup_handle];
            
            loop {
                if !running.load(Ordering::SeqCst) {
                    break;
                }
                
                tokio::select! {
                    result = listener.accept() => {
                        match result {
                            Ok((stream, addr)) => {
                                debug!("New connection from {}", addr);
                                
                                // Check rate limits
                                if let Err(e) = check_rate_limit(&ip_rate_limiter, &addr, &config.pool_config).await {
                                    warn!("Rate limit exceeded for {}: {}", addr, e);
                                    continue;
                                }
                                
                                // Try to acquire connection permit
                                let permit = match connection_limiter.clone().try_acquire_owned() {
                                    Ok(permit) => permit,
                                    Err(_) => {
                                        warn!("Max connections reached, rejecting {}", addr);
                                        metrics.connections_rejected.inc();
                                        continue;
                                    }
                                };
                                
                                // Check per-IP limits
                                if let Err(e) = check_ip_limit(&ip_connections, &addr, &config.pool_config).await {
                                    warn!("Per-IP limit exceeded for {}: {}", addr, e);
                                    metrics.connections_rejected.inc();
                                    continue;
                                }
                                
                                // Create connection info
                                let conn_id = Uuid::new_v4();
                                let now = Instant::now();
                                let conn_info = Arc::new(ConnectionInfo {
                                    id: conn_id,
                                    peer_addr: addr,
                                    connected_at: now,
                                    last_activity: AtomicU64::new(0),
                                    request_count: AtomicUsize::new(0),
                                    bytes_sent: AtomicUsize::new(0),
                                    bytes_received: AtomicUsize::new(0),
                                    is_tls: tls_acceptor.is_some(),
                                    start_time: now,
                                });
                                
                                // Track connection
                                let (shutdown_tx, shutdown_rx) = oneshot::channel();
                                {
                                    let mut conns = connections.write().await;
                                    conns.insert(conn_id, ConnectionState {
                                        info: Arc::clone(&conn_info),
                                        pending_requests: AtomicUsize::new(0),
                                        shutdown_tx: Some(shutdown_tx),
                                    });
                                    
                                    let mut ip_conns = ip_connections.write().await;
                                    ip_conns.entry(addr.ip().to_string())
                                        .or_insert_with(Vec::new)
                                        .push(conn_id);
                                }
                                
                                connection_count.fetch_add(1, Ordering::SeqCst);
                                metrics.connections_active.set(connection_count.load(Ordering::SeqCst) as f64);
                                metrics.connections_total.inc();
                                
                                // Spawn connection handler
                                let config = config.clone();
                                let kafka_handler = kafka_handler.clone();
                                let connections = connections.clone();
                                let connection_count = connection_count.clone();
                                let ip_connections = ip_connections.clone();
                                let metrics = metrics.clone();
                                let tls_acceptor = tls_acceptor.clone();
                                
                                tokio::spawn(async move {
                                    let result = handle_connection(
                                        stream,
                                        conn_info,
                                        config,
                                        kafka_handler,
                                        connections.clone(),
                                        metrics,
                                        tls_acceptor.clone(),
                                        shutdown_rx,
                                        permit,
                                    ).await;
                                    
                                    // Cleanup connection
                                    {
                                        let mut conns = connections.write().await;
                                        conns.remove(&conn_id);
                                        
                                        let mut ip_conns = ip_connections.write().await;
                                        if let Some(conns) = ip_conns.get_mut(&addr.ip().to_string()) {
                                            conns.retain(|&id| id != conn_id);
                                            if conns.is_empty() {
                                                ip_conns.remove(&addr.ip().to_string());
                                            }
                                        }
                                    }
                                    
                                    connection_count.fetch_sub(1, Ordering::SeqCst);
                                    
                                    if let Err(e) = result {
                                        error!("Connection error from {}: {}", addr, e);
                                    }
                                });
                            }
                            Err(e) => {
                                error!("Accept error: {}", e);
                                // Brief pause to avoid tight loop on persistent errors
                                tokio::time::sleep(Duration::from_millis(100)).await;
                            }
                        }
                    }
                    _ = tokio::signal::ctrl_c() => {
                        info!("Received shutdown signal");
                        running.store(false, Ordering::SeqCst);
                        break;
                    }
                }
            }
            
            // Graceful shutdown
            info!("Starting graceful shutdown");
            
            // Stop accepting new connections
            drop(listener);
            
            // Send shutdown signal to all connections
            {
                let mut conns = connections.write().await;
                for (_, mut state) in conns.drain() {
                    if let Some(tx) = state.shutdown_tx.take() {
                        let _ = tx.send(());
                    }
                }
            }
            
            // Wait for connections to close with timeout
            let shutdown_deadline = Instant::now() + config.shutdown_timeout;
            while connection_count.load(Ordering::SeqCst) > 0 && Instant::now() < shutdown_deadline {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            
            if connection_count.load(Ordering::SeqCst) > 0 {
                warn!("Forced shutdown with {} active connections", connection_count.load(Ordering::SeqCst));
            }
            
            // Stop background tasks
            for handle in shutdown_handles.drain(..) {
                handle.abort();
            }
            
            info!("Server shutdown complete");
            Ok(())
        });
        
        Ok(handle)
    }
    
    /// Stop the server
    pub async fn stop(&self) -> Result<()> {
        self.running.store(false, Ordering::SeqCst);
        Ok(())
    }
    
    /// Start metrics collection task
    fn start_metrics_collection(&self) -> JoinHandle<()> {
        let running = Arc::clone(&self.running);
        let metrics = Arc::clone(&self.metrics);
        let interval_duration = self.config.metrics_interval;
        let connections = Arc::clone(&self.connections);
        
        tokio::spawn(async move {
            let mut interval = interval(interval_duration);
            
            while running.load(Ordering::SeqCst) {
                interval.tick().await;
                
                // Collect connection metrics
                let conns = connections.read().await;
                let mut total_pending = 0;
                let mut total_requests = 0;
                
                for (_, state) in conns.iter() {
                    total_pending += state.pending_requests.load(Ordering::SeqCst);
                    total_requests += state.info.request_count.load(Ordering::SeqCst);
                }
                
                metrics.pending_requests.set(total_pending as f64);
                
                debug!(
                    "Server metrics - connections: {}, pending requests: {}, total requests: {}",
                    conns.len(),
                    total_pending,
                    total_requests
                );
            }
        })
    }
    
    /// Start connection cleanup task
    fn start_connection_cleanup(&self) -> JoinHandle<()> {
        let running = Arc::clone(&self.running);
        let connections = Arc::clone(&self.connections);
        let idle_timeout = self.config.idle_timeout;
        
        tokio::spawn(async move {
            let mut interval = interval(Duration::from_secs(30));
            
            while running.load(Ordering::SeqCst) {
                interval.tick().await;
                
                let now = Instant::now();
                let mut to_remove = Vec::new();
                
                {
                    let conns = connections.read().await;
                    for (id, state) in conns.iter() {
                        let last_activity_ms = state.info.last_activity.load(Ordering::SeqCst);
                        let last_activity = state.info.start_time + Duration::from_millis(last_activity_ms);
                        if now.duration_since(last_activity) > idle_timeout {
                            to_remove.push(*id);
                        }
                    }
                }
                
                if !to_remove.is_empty() {
                    let mut conns = connections.write().await;
                    for id in to_remove {
                        if let Some(mut state) = conns.remove(&id) {
                            info!("Closing idle connection {}", id);
                            if let Some(tx) = state.shutdown_tx.take() {
                                let _ = tx.send(());
                            }
                        }
                    }
                }
            }
        })
    }
}

/// Handle a single client connection
async fn handle_connection(
    stream: TcpStream,
    info: Arc<ConnectionInfo>,
    config: ServerConfig,
    kafka_handler: Arc<KafkaProtocolHandler>,
    connections: Arc<RwLock<HashMap<Uuid, ConnectionState>>>,
    metrics: Arc<ServerMetrics>,
    tls_acceptor: Option<Arc<TlsAcceptor>>,
    mut shutdown_rx: oneshot::Receiver<()>,
    _permit: tokio::sync::OwnedSemaphorePermit,
) -> Result<()> {
    // Set TCP options
    stream.set_nodelay(true)
        .map_err(|e| Error::Network(format!("Failed to set TCP_NODELAY: {}", e)))?;
    
    // Handle TLS if configured
    let stream = if let Some(acceptor) = tls_acceptor {
        match tokio::time::timeout(
            Duration::from_secs(10),
            acceptor.inner().accept(stream)
        ).await {
            Ok(Ok(tls_stream)) => {
                info!("TLS handshake completed");
                ConnectionStream::Tls(Box::pin(tls_stream))
            }
            Ok(Err(e)) => {
                error!("TLS handshake failed: {}", e);
                return Err(Error::Network(format!("TLS handshake failed: {}", e)));
            }
            Err(_) => {
                error!("TLS handshake timeout");
                return Err(Error::Network("TLS handshake timeout".to_string()));
            }
        }
    } else {
        ConnectionStream::Plain(stream)
    };
    
    // Create framed codec
    let mut framed = Framed::new(stream, KafkaFrameCodec::new());
    
    info!("Connection established");
    
    loop {
        tokio::select! {
            // Handle incoming requests
            frame_result = framed.next() => {
                match frame_result {
                    Some(Ok(frame_data)) => {
                        // Update activity time
                        info.last_activity.store(
                            Instant::now().duration_since(info.start_time).as_millis() as u64,
                            Ordering::SeqCst
                        );
                        
                        // Check backpressure
                        if let Some(state) = connections.read().await.get(&info.id) {
                            let pending = state.pending_requests.load(Ordering::SeqCst);
                            if pending >= config.backpressure_threshold {
                                warn!("Backpressure threshold reached: {} pending requests", pending);
                                metrics.backpressure_events.inc();
                                // Send error response (need to extract correlation ID from request)
                                let mut req_buf = frame_data.clone();
                                if req_buf.len() >= 8 {
                                    req_buf.advance(4); // Skip api_key and api_version
                                    let correlation_id = req_buf.get_i32();
                                    let error_response = create_error_response_with_correlation_id(35, correlation_id); // NOT_COORDINATOR
                                    
                                    let mut complete_response = BytesMut::new();
                                    chronik_protocol::parser::write_response_header(&mut complete_response, &error_response.header);
                                    complete_response.extend_from_slice(&error_response.body);
                                    
                                    framed.send(complete_response.freeze()).await
                                        .map_err(|e| Error::Network(format!("Failed to send backpressure response: {}", e)))?;
                                } else {
                                    return Err(Error::Protocol("Invalid request - too short".into()));
                                }
                                continue;
                            }
                            state.pending_requests.fetch_add(1, Ordering::SeqCst);
                        }
                        
                        // Update metrics
                        info.request_count.fetch_add(1, Ordering::SeqCst);
                        info.bytes_received.fetch_add(frame_data.len(), Ordering::SeqCst);
                        metrics.requests_total.inc();
                        metrics.bytes_received.inc_by(frame_data.len() as f64);
                        
                        // Process request
                        let start_time = Instant::now();
                        let kafka_handler = kafka_handler.clone();
                        let connections = connections.clone();
                        let conn_id = info.id;
                        let metrics = metrics.clone();
                        let timeout_duration = config.request_timeout;
                        
                        // Handle request
                        let response = match timeout(timeout_duration, kafka_handler.handle_request(&frame_data)).await {
                            Ok(Ok(resp)) => resp,
                            Ok(Err(e)) => {
                                error!("Request handling error: {}", e);
                                metrics.request_errors.inc();
                                // Try to extract correlation ID from request
                                let mut req_buf = frame_data.clone();
                                let correlation_id = if req_buf.len() >= 8 {
                                    req_buf.advance(4); // Skip api_key and api_version
                                    req_buf.get_i32()
                                } else {
                                    0
                                };
                                create_error_response_with_correlation_id(1, correlation_id) // UNKNOWN_ERROR
                            }
                            Err(_) => {
                                warn!("Request timeout");
                                metrics.request_timeouts.inc();
                                // Try to extract correlation ID from request
                                let mut req_buf = frame_data.clone();
                                let correlation_id = if req_buf.len() >= 8 {
                                    req_buf.advance(4); // Skip api_key and api_version
                                    req_buf.get_i32()
                                } else {
                                    0
                                };
                                create_error_response_with_correlation_id(7, correlation_id) // REQUEST_TIMED_OUT
                            }
                        };
                        
                        // Build complete response with correlation ID
                        let mut complete_response = BytesMut::new();
                        chronik_protocol::parser::write_response_header(&mut complete_response, &response.header);
                        complete_response.extend_from_slice(&response.body);
                        
                        let response_bytes = complete_response.freeze();
                        let response_size = response_bytes.len() + 4; // +4 for length prefix
                        
                        if let Err(e) = framed.send(response_bytes).await {
                            error!("Failed to send response: {}", e);
                        } else {
                            // Update metrics
                            if let Some(state) = connections.read().await.get(&conn_id) {
                                state.info.bytes_sent.fetch_add(response_size, Ordering::SeqCst);
                                state.pending_requests.fetch_sub(1, Ordering::SeqCst);
                            }
                            metrics.bytes_sent.inc_by(response_size as f64);
                            
                            let duration = start_time.elapsed();
                            metrics.request_duration.observe(duration.as_secs_f64());
                            
                            debug!("Request processed in {:?}", duration);
                        }
                    }
                    Some(Err(e)) => {
                        error!("Frame error: {}", e);
                        metrics.frame_errors.inc();
                        break;
                    }
                    None => {
                        info!("Connection closed by client");
                        break;
                    }
                }
            }
            
            // Handle shutdown signal
            _ = &mut shutdown_rx => {
                info!("Shutdown signal received");
                break;
            }
            
            // Handle idle timeout
            _ = tokio::time::sleep(config.idle_timeout) => {
                let last_activity_ms = info.last_activity.load(Ordering::SeqCst);
                let last_activity = info.start_time + Duration::from_millis(last_activity_ms);
                if Instant::now().duration_since(last_activity) >= config.idle_timeout {
                    info!("Connection idle timeout");
                    break;
                }
            }
        }
    }
    
    info!("Connection closed");
    Ok(())
}

/// Connection stream type (plain or TLS)
enum ConnectionStream {
    Plain(TcpStream),
    Tls(Pin<Box<tokio_rustls::server::TlsStream<TcpStream>>>),
}

// Implement AsyncRead/AsyncWrite for ConnectionStream
impl tokio::io::AsyncRead for ConnectionStream {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match &mut *self {
            ConnectionStream::Plain(s) => std::pin::Pin::new(s).poll_read(cx, buf),
            ConnectionStream::Tls(s) => s.as_mut().poll_read(cx, buf),
        }
    }
}

impl tokio::io::AsyncWrite for ConnectionStream {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        match &mut *self {
            ConnectionStream::Plain(s) => std::pin::Pin::new(s).poll_write(cx, buf),
            ConnectionStream::Tls(s) => s.as_mut().poll_write(cx, buf),
        }
    }
    
    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match &mut *self {
            ConnectionStream::Plain(s) => std::pin::Pin::new(s).poll_flush(cx),
            ConnectionStream::Tls(s) => s.as_mut().poll_flush(cx),
        }
    }
    
    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match &mut *self {
            ConnectionStream::Plain(s) => std::pin::Pin::new(s).poll_shutdown(cx),
            ConnectionStream::Tls(s) => s.as_mut().poll_shutdown(cx),
        }
    }
}

/// Check rate limit for IP
async fn check_rate_limit(
    rate_limiter: &Arc<RwLock<HashMap<String, RateLimitInfo>>>,
    addr: &SocketAddr,
    config: &ConnectionPoolConfig,
) -> Result<()> {
    let ip = addr.ip().to_string();
    let now = Instant::now();
    
    let mut limiter = rate_limiter.write().await;
    let info = limiter.entry(ip.clone()).or_insert_with(|| RateLimitInfo {
        last_reset: now,
        connection_count: 0,
        banned_until: None,
    });
    
    // Check if banned
    if let Some(banned_until) = info.banned_until {
        if now < banned_until {
            return Err(Error::Network(format!("IP {} is rate limited", ip)));
        }
        info.banned_until = None;
    }
    
    // Reset counter if needed
    if now.duration_since(info.last_reset) >= Duration::from_secs(1) {
        info.last_reset = now;
        info.connection_count = 0;
    }
    
    // Check rate limit
    info.connection_count += 1;
    if info.connection_count > config.rate_limit {
        info.banned_until = Some(now + config.ban_duration);
        return Err(Error::Network(format!("Rate limit exceeded for IP {}", ip)));
    }
    
    Ok(())
}

/// Check per-IP connection limit
async fn check_ip_limit(
    ip_connections: &Arc<RwLock<HashMap<String, Vec<Uuid>>>>,
    addr: &SocketAddr,
    config: &ConnectionPoolConfig,
) -> Result<()> {
    let ip = addr.ip().to_string();
    
    let ip_conns = ip_connections.read().await;
    if let Some(conns) = ip_conns.get(&ip) {
        if conns.len() >= config.max_per_ip {
            return Err(Error::Network(format!("Per-IP limit exceeded for {}", ip)));
        }
    }
    
    Ok(())
}

/// Create error response bytes with proper correlation ID
fn create_error_response_with_correlation_id(error_code: i16, correlation_id: i32) -> chronik_protocol::handler::Response {
    use chronik_protocol::parser::{Encoder, ResponseHeader};
    
    let mut buf = BytesMut::new();
    let mut encoder = Encoder::new(&mut buf);
    
    // Just write error code
    encoder.write_i16(error_code);
    
    chronik_protocol::handler::Response {
        header: ResponseHeader { correlation_id },
        body: buf.freeze(),
    }
}

/// Create error response bytes (for cases where we can't extract correlation ID)
fn create_error_response(error_code: i16) -> chronik_protocol::handler::Response {
    create_error_response_with_correlation_id(error_code, 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[tokio::test]
    async fn test_server_lifecycle() {
        let config = ServerConfig {
            listen_addr: "127.0.0.1:0".parse().unwrap(),
            ..Default::default()
        };
        
        let data_dir = tempfile::tempdir().unwrap();
        let server = IngestServer::new(config, data_dir.path().to_path_buf()).await.unwrap();
        
        // Start server
        let handle = server.start().await.unwrap();
        
        // Verify it's running
        assert!(server.running.load(Ordering::SeqCst));
        
        // Stop server
        server.stop().await.unwrap();
        
        // Wait for shutdown
        let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;
        
        assert!(!server.running.load(Ordering::SeqCst));
    }
}