//! Controller implementation for Chronik Stream operator.

use crate::crd::ChronikCluster;
use crate::reconciler::Reconciler;
use futures::StreamExt;
use kube::{
    api::Api,
    client::Client,
    runtime::{
        controller::{Action, Controller},
        watcher::Config,
    },
};
use std::sync::Arc;
use std::time::Duration;
use tracing::{error, info};

/// Controller context
#[derive(Clone)]
pub struct Context {
    pub client: Client,
}

/// Run the controller
pub async fn run(client: Client) -> Result<(), kube::Error> {
    info!("Starting ChronikCluster controller");
    
    let context = Arc::new(Context { client: client.clone() });
    let api = Api::<ChronikCluster>::all(client);
    
    Controller::new(api.clone(), Config::default())
        .run(reconcile, error_policy, context)
        .for_each(|res| async move {
            match res {
                Ok(o) => info!("Reconciled {:?}", o),
                Err(e) => error!("Reconcile failed: {:?}", e),
            }
        })
        .await;
    
    Ok(())
}

/// Reconcile function
async fn reconcile(
    cluster: Arc<ChronikCluster>,
    ctx: Arc<Context>,
) -> Result<Action, kube::Error> {
    let name = cluster.metadata.name.as_ref().unwrap();
    let namespace = cluster.metadata.namespace.as_ref().unwrap();
    
    info!("Reconciling ChronikCluster {}/{}", namespace, name);
    
    let reconciler = Reconciler::new(ctx.client.clone());
    
    match reconciler.reconcile(cluster.as_ref()).await {
        Ok(action) => Ok(action),
        Err(e) => {
            error!("Reconciliation failed for {}/{}: {}", namespace, name, e);
            Err(e)
        }
    }
}

/// Error policy - how to handle errors
fn error_policy(
    _cluster: Arc<ChronikCluster>,
    error: &kube::Error,
    _ctx: Arc<Context>,
) -> Action {
    error!("Reconciliation error: {:?}", error);
    Action::requeue(Duration::from_secs(30))
}