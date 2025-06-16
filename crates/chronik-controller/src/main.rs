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
    
    let metadata_path = std::env::var("METADATA_PATH")
        .unwrap_or_else(|_| "/var/chronik/metadata".to_string());
    
    // Create controller node config
    let config = ControllerConfig {
        node_id,
        peers,
        tick_interval: Duration::from_millis(100),
        election_timeout: (150, 300),
        metadata_path,
    };
    
    let node = ControllerNode::new(config)?;
    
    // Start the node
    node.start().await?;
    
    // Keep running
    tokio::signal::ctrl_c().await?;
    tracing::info!("Shutting down controller");
    
    Ok(())
}