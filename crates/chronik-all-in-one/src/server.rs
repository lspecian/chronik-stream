use anyhow::Result;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::RwLock;
use tracing::{info, warn, error};

use chronik_ingest::server::IngestServer;
use chronik_admin::server::AdminServer;
use chronik_common::metadata::MetadataStore;
use chronik_storage::{SegmentWriter, SegmentReader};

/// All-in-one server that runs all Chronik components in a single process
pub struct AllInOneServer {
    bind_addr: String,
    kafka_port: u16,
    admin_port: u16,
    metrics_port: u16,
    metadata_store: Arc<RwLock<dyn MetadataStore>>,
    data_dir: PathBuf,
    enable_search: bool,
    advertised_addr: Option<String>,
    memory_limit: Option<usize>,
}

impl AllInOneServer {
    pub fn new(
        bind_addr: &str,
        kafka_port: u16,
        admin_port: u16,
        metrics_port: u16,
        metadata_store: Arc<RwLock<dyn MetadataStore>>,
        data_dir: PathBuf,
        enable_search: bool,
    ) -> Result<Self> {
        Ok(Self {
            bind_addr: bind_addr.to_string(),
            kafka_port,
            admin_port,
            metrics_port,
            metadata_store,
            data_dir,
            enable_search,
            advertised_addr: None,
            memory_limit: None,
        })
    }
    
    pub fn set_advertised_address(&mut self, addr: &str) {
        self.advertised_addr = Some(addr.to_string());
    }
    
    pub fn set_memory_limit(&mut self, limit_mb: usize) {
        self.memory_limit = Some(limit_mb);
    }
    
    pub async fn run(self) -> Result<()> {
        // Create storage directories
        let segments_dir = self.data_dir.join("segments");
        let search_dir = self.data_dir.join("search");
        std::fs::create_dir_all(&segments_dir)?;
        if self.enable_search {
            std::fs::create_dir_all(&search_dir)?;
        }
        
        // Initialize storage
        let segment_writer = Arc::new(SegmentWriter::new(segments_dir.clone())?);
        let segment_reader = Arc::new(SegmentReader::new(segments_dir)?);
        
        // Start metrics server
        let metrics_addr: SocketAddr = format!("{}:{}", self.bind_addr, self.metrics_port).parse()?;
        tokio::spawn(async move {
            if let Err(e) = start_metrics_server(metrics_addr).await {
                error!("Metrics server error: {}", e);
            }
        });
        
        // Start admin API server
        let admin_addr = format!("{}:{}", self.bind_addr, self.admin_port);
        let admin_metadata = self.metadata_store.clone();
        tokio::spawn(async move {
            let admin_server = AdminServer::new(admin_metadata);
            if let Err(e) = admin_server.run(&admin_addr).await {
                error!("Admin server error: {}", e);
            }
        });
        
        // Start search service if enabled
        if self.enable_search {
            let search_metadata = self.metadata_store.clone();
            let search_reader = segment_reader.clone();
            tokio::spawn(async move {
                info!("Search service enabled");
                // Initialize search indexer
                // This would integrate with chronik-search crate
            });
        }
        
        // Start Kafka protocol server (main service)
        let kafka_addr = format!("{}:{}", self.bind_addr, self.kafka_port);
        let ingest_server = IngestServer::new(
            self.metadata_store,
            segment_writer,
            segment_reader,
            self.advertised_addr.unwrap_or(kafka_addr.clone()),
        )?;
        
        info!("Starting Kafka protocol server on {}", kafka_addr);
        ingest_server.run(&kafka_addr).await?;
        
        Ok(())
    }
}

async fn start_metrics_server(addr: SocketAddr) -> Result<()> {
    use prometheus::{Encoder, TextEncoder};
    use hyper::{Body, Request, Response, Server, StatusCode};
    use hyper::service::{make_service_fn, service_fn};
    
    let make_svc = make_service_fn(|_conn| async {
        Ok::<_, std::convert::Infallible>(service_fn(metrics_handler))
    });
    
    info!("Metrics server listening on {}", addr);
    Server::bind(&addr)
        .serve(make_svc)
        .await
        .map_err(|e| anyhow::anyhow!("Metrics server error: {}", e))
}

async fn metrics_handler(_req: Request<Body>) -> Result<Response<Body>, std::convert::Infallible> {
    let encoder = TextEncoder::new();
    let metric_families = prometheus::gather();
    let mut buffer = vec![];
    encoder.encode(&metric_families, &mut buffer).unwrap();
    
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", encoder.format_type())
        .body(Body::from(buffer))
        .unwrap())
}