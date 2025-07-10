//! Chronik Controller node entry point

use chronik_controller::{ControllerNode, ControllerConfig};
use chronik_common::Result;
use std::time::Duration;

#[tokio::main]  
async fn main() -> Result<()> {
    // Initialize tracing
    tracing_subscriber::fmt::init();
    
    let node_id = std::env::var("NODE_ID")
        .unwrap_or_else(|_| "1".to_string())
        .parse::<u64>()
        .expect("Invalid NODE_ID");
    
    let peers = std::env::var("PEERS")
        .unwrap_or_default()
        .split(',')
        .filter(|s| !s.is_empty())
        .filter_map(|s| s.parse::<u64>().ok())
        .collect::<Vec<_>>();
    
    tracing::info!("Starting Chronik Controller node {} with peers {:?}", node_id, peers);
    
    // For now, use the simple controller but with TiKV metadata if configured
    let metadata_path = std::env::var("METADATA_PATH")
        .unwrap_or_else(|_| "/var/chronik/metadata".to_string());
    
    let config = ControllerConfig {
        node_id,
        peers,
        tick_interval: Duration::from_millis(100),
        election_timeout: (150, 300),
        metadata_path,
    };
    
    // The controller node will use TiKV if TIKV_PD_ENDPOINTS is set
    // This is handled internally in the node implementation
    let node = ControllerNode::new(config)?;
    node.start().await?;
    
    // Keep running
    tokio::signal::ctrl_c().await?;
    tracing::info!("Shutting down controller");
    
    Ok(())
}