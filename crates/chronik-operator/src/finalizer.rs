//! Finalizer handling for ChronikCluster resources.

use crate::crd::ChronikCluster;
use kube::{
    api::{Api, Patch, PatchParams},
    Resource,
};
use serde_json::json;
use tracing::info;

/// Finalizer name for ChronikCluster resources
pub const FINALIZER_NAME: &str = "chronik.stream/finalizer";

/// Add finalizer to a ChronikCluster resource
pub async fn add_finalizer(api: &Api<ChronikCluster>, name: &str) -> Result<(), kube::Error> {
    info!("Adding finalizer to ChronikCluster {}", name);
    
    let patch = json!({
        "metadata": {
            "finalizers": [FINALIZER_NAME]
        }
    });
    
    let patch_params = PatchParams::default();
    api.patch(name, &patch_params, &Patch::Merge(patch)).await?;
    
    Ok(())
}

/// Remove finalizer from a ChronikCluster resource
pub async fn remove_finalizer(api: &Api<ChronikCluster>, name: &str) -> Result<(), kube::Error> {
    info!("Removing finalizer from ChronikCluster {}", name);
    
    // Get the current resource to check finalizers
    let cluster = api.get(name).await?;
    let mut finalizers = cluster.finalizers();
    
    // Remove our finalizer
    finalizers.retain(|f| f != FINALIZER_NAME);
    
    let patch = json!({
        "metadata": {
            "finalizers": finalizers
        }
    });
    
    let patch_params = PatchParams::default();
    api.patch(name, &patch_params, &Patch::Merge(patch)).await?;
    
    Ok(())
}