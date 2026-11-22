//! Cluster catalog startup: the catalog's raft node and its gRPC endpoint.
//!
//! The raft loop runs on its own thread (`RaftDriver`). Its network I/O runs
//! on a small dedicated runtime, so request handling on the main runtime can
//! never starve heartbeats into a spurious election (issue #58).

use anyhow::{Context, Result};
use std::sync::{Arc, OnceLock};
use tracing::info;

use super::config::ClusterInitConfig;
use crate::cluster_catalog::{ClusterCatalog, ClusterCatalogConfig};

/// Runtime for catalog raft transport (outbound sends and the gRPC server).
pub fn catalog_runtime() -> &'static tokio::runtime::Runtime {
    static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("catalog-net")
            .enable_all()
            .build()
            .expect("catalog transport runtime")
    })
}

/// Peers in the config plus this node, with their raft addresses.
fn all_peers(init_config: &ClusterInitConfig) -> Vec<(u64, String)> {
    let mut peers: Vec<(u64, String)> = init_config
        .raft_peers
        .iter()
        .filter(|(id, _)| *id != init_config.node_id)
        .cloned()
        .collect();
    peers.push((init_config.node_id, init_config.self_raft_addr.clone()));
    peers.sort();
    peers
}

/// `CHRONIK_CLUSTER_JOIN`: this node was added to a running cluster.
pub fn is_joining() -> bool {
    std::env::var("CHRONIK_CLUSTER_JOIN").map(|v| v == "true" || v == "1").unwrap_or(false)
}

/// Open this node's catalog log and start its raft node. A brand-new cluster
/// is founded by the nodes in the config; every founding node must list the
/// same set.
pub async fn start_cluster_catalog(
    init_config: &ClusterInitConfig,
    object_store: Option<&chronik_storage::object_store::ObjectStoreConfig>,
) -> Result<Arc<ClusterCatalog>> {
    let peers = all_peers(init_config);
    // A node added to a running cluster (`cluster add-node`) must not found a
    // cluster of its own: it starts blank and the leader sends it a snapshot.
    let joining = is_joining();
    let voters: Vec<u64> = if joining { Vec::new() } else { peers.iter().map(|(id, _)| *id).collect() };
    // Whole-cluster loss: with CHRONIK_DR_RESTORE=true every blank founding
    // node founds the cluster on the same catalog image from the object store.
    let bootstrap = if joining {
        None
    } else {
        crate::metadata_dr::restore_if_blank(&init_config.data_dir, init_config.node_id, Some(&voters), object_store)
            .await
            .context("restoring metadata from the object store")?
    };
    info!(
        "Starting cluster catalog (node {}, {})",
        init_config.node_id,
        if joining { "joining an existing cluster".to_string() } else { format!("founding voters {:?}", voters) }
    );
    let mut config = ClusterCatalogConfig::new(
        init_config.node_id,
        init_config.data_dir.join("catalog-raft"),
        peers,
        voters,
    );
    config.bootstrap = bootstrap;
    let catalog = ClusterCatalog::start(config, catalog_runtime().handle().clone())
        .context("starting the cluster catalog")?;
    Ok(Arc::new(catalog))
}

/// Serve the catalog's raft endpoint on `bind_addr`.
pub async fn start_catalog_grpc_server(catalog: Arc<ClusterCatalog>, bind_addr: &str) -> Result<()> {
    let addr: std::net::SocketAddr = bind_addr
        .parse()
        .with_context(|| format!("invalid raft bind address {}", bind_addr))?;
    // Bind here so a port conflict fails startup instead of a background task.
    let listener = catalog_runtime()
        .spawn(async move { tokio::net::TcpListener::bind(addr).await })
        .await?
        .with_context(|| format!("binding the catalog raft endpoint on {}", addr))?;
    let service = catalog.service();
    catalog_runtime().spawn(async move {
        // TCP_NODELAY on every accepted connection: raft messages are small, and
        // Nagle against delayed ACKs held each round trip ~40 ms.
        let incoming = tonic::transport::server::TcpIncoming::from(listener).with_nodelay(Some(true));
        if let Err(e) = tonic::transport::Server::builder().add_service(service).serve_with_incoming(incoming).await {
            tracing::error!("Catalog raft endpoint stopped: {}", e);
        }
    });
    info!("Catalog raft endpoint listening on {}", addr);
    Ok(())
}
