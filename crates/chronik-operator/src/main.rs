//! Kubernetes operator for Chronik Stream.

use anyhow::Result;
use tracing::info;

mod crd;
mod controller;
mod reconciler;
mod resources;
mod finalizer;

#[cfg(test)]
mod tests;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    
    info!("Starting Chronik Stream operator");
    
    // Create Kubernetes client
    let client = kube::Client::try_default().await?;
    
    // Install CRDs
    crd::install_crds(client.clone()).await?;
    
    // Start controller
    controller::run(client).await?;
    
    Ok(())
}