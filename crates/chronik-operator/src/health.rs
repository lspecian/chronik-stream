use tracing::warn;

use crate::admin_client::{AdminClient, HealthResponse};

/// Poll health status of all cluster nodes.
///
/// Returns a vec of (node_id, health_response) for nodes that responded,
/// and skips nodes that are unreachable.
pub async fn poll_all_nodes(
    admin_client: &AdminClient,
    cluster_name: &str,
    namespace: &str,
    node_ids: &[u64],
) -> Vec<(u64, HealthResponse)> {
    let mut results = Vec::new();

    for &node_id in node_ids {
        let url = AdminClient::admin_url_from_dns(cluster_name, node_id, namespace);
        match admin_client.health(&url).await {
            Ok(health) => {
                results.push((node_id, health));
            }
            Err(e) => {
                warn!(node_id, "Health check failed: {e}");
            }
        }
    }

    results
}

/// Find the leader among health responses.
pub fn find_leader(responses: &[(u64, HealthResponse)]) -> Option<u64> {
    responses
        .iter()
        .find(|(_, h)| h.is_leader)
        .map(|(id, _)| *id)
}

/// Count ready nodes (those that responded with status "ok").
pub fn count_ready(responses: &[(u64, HealthResponse)]) -> i32 {
    responses.iter().filter(|(_, h)| h.status == "ok").count() as i32
}
