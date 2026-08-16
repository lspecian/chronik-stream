//! Integration tests for Admin API (v2.6.0 - Priority 2 Step 2)
//!
//! Tests the HTTP Admin API for cluster management:
//! - Health endpoint
//! - Add node endpoint
//! - Leader discovery

use std::time::Duration;

/// Admin API lives on the Unified API. The port-per-node scheme these tests used
/// (`10000 + node_id`) is the deprecated legacy listener kept only for cluster
/// backward compatibility.
const ADMIN_PORTS: [u16; 3] = [6092, 6093, 6094];

fn admin_url(port: u16, path: &str) -> String {
    format!("http://localhost:{}/admin/{}", port, path)
}

/// Health endpoint reports the fields an operator needs to find the leader.
#[tokio::test]
#[ignore = "requires a running cluster: ./tests/cluster/start.sh, then --ignored"]
async fn test_admin_api_health_endpoint() {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .unwrap();

    let response = client
        .get(admin_url(ADMIN_PORTS[0], "health"))
        .send()
        .await
        .expect("admin health endpoint unreachable");

    assert!(
        response.status().is_success(),
        "admin health returned {}",
        response.status()
    );

    let health: serde_json::Value = response.json().await.expect("health response is not JSON");
    for field in ["node_id", "is_leader", "cluster_nodes"] {
        assert!(
            health.get(field).is_some(),
            "health response is missing `{}`: {}",
            field,
            health
        );
    }
}

/// Test Admin API add-node endpoint (requires running cluster)
#[tokio::test]
#[ignore = "requires a running 3-node cluster: ./tests/cluster/start.sh, then --ignored"]
async fn test_admin_api_add_node_endpoint() {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();

    // Step 1: Find the leader
    let mut leader_port = None;
    for node_id in 1..=3 {
        let admin_port = ADMIN_PORTS[(node_id - 1) as usize];
        let health_url = admin_url(admin_port, "health");

        match client.get(&health_url).send().await {
            Ok(response) if response.status().is_success() => {
                let health: serde_json::Value = response.json().await.unwrap();
                if health["is_leader"].as_bool().unwrap_or(false) {
                    leader_port = Some(admin_port);
                    println!("Found leader at port {}", admin_port);
                    break;
                }
            }
            _ => continue,
        }
    }

    assert!(leader_port.is_some(), "No leader found in cluster");

    // Step 2: Send add-node request to leader
    let add_node_url = admin_url(leader_port.unwrap(), "add-node");

    let request_body = serde_json::json!({
        "node_id": 4,
        "kafka_addr": "localhost:9095",
        "wal_addr": "localhost:9294",
        "raft_addr": "localhost:5004",
    });

    let response = client
        .post(&add_node_url)
        .json(&request_body)
        .send()
        .await
        .expect("Failed to send add-node request");

    assert!(
        response.status().is_success(),
        "Add node request failed: {}",
        response.status()
    );

    let resp: serde_json::Value = response.json().await.unwrap();
    println!("Add node response: {:#?}", resp);

    assert!(resp["success"].as_bool().unwrap_or(false));
    assert_eq!(resp["node_id"].as_u64(), Some(4));
}

/// Test leader discovery logic
#[tokio::test]
#[ignore = "requires a running 3-node cluster: ./tests/cluster/start.sh, then --ignored"]
async fn test_leader_discovery() {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();

    let mut leader_found = false;
    let mut follower_count = 0;

    // Query all nodes
    for node_id in 1..=3 {
        let admin_port = ADMIN_PORTS[(node_id - 1) as usize];
        let health_url = admin_url(admin_port, "health");

        match client.get(&health_url).send().await {
            Ok(response) if response.status().is_success() => {
                let health: serde_json::Value = response.json().await.unwrap();
                let is_leader = health["is_leader"].as_bool().unwrap_or(false);

                println!(
                    "Node {} (port {}): is_leader={}",
                    node_id, admin_port, is_leader
                );

                if is_leader {
                    leader_found = true;
                } else {
                    follower_count += 1;
                }
            }
            Ok(response) => {
                println!("Node {} returned {}", node_id, response.status());
            }
            Err(e) => {
                println!("Node {} unreachable: {}", node_id, e);
            }
        }
    }

    assert!(leader_found, "No leader found in cluster");
    assert!(follower_count >= 1, "Expected at least 1 follower");
    println!(
        "✓ Leader discovery successful: 1 leader, {} followers",
        follower_count
    );
}

/// Test error handling for invalid requests
#[tokio::test]
#[ignore = "requires a running cluster: ./tests/cluster/start.sh, then --ignored"]
async fn test_admin_api_error_handling() {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();

    // Find leader
    let mut leader_port = None;
    for node_id in 1..=3 {
        let admin_port = ADMIN_PORTS[(node_id - 1) as usize];
        let health_url = admin_url(admin_port, "health");

        match client.get(&health_url).send().await {
            Ok(response) if response.status().is_success() => {
                let health: serde_json::Value = response.json().await.unwrap();
                if health["is_leader"].as_bool().unwrap_or(false) {
                    leader_port = Some(admin_port);
                    break;
                }
            }
            _ => continue,
        }
    }

    assert!(leader_port.is_some(), "No leader found");
    let add_node_url = admin_url(leader_port.unwrap(), "add-node");

    // Test 1: Invalid address format (missing port)
    let invalid_request = serde_json::json!({
        "node_id": 5,
        "kafka_addr": "localhost",  // Missing port!
        "wal_addr": "localhost:9295",
        "raft_addr": "localhost:5005",
    });

    let response = client
        .post(&add_node_url)
        .json(&invalid_request)
        .send()
        .await
        .expect("Failed to send request");

    let resp: serde_json::Value = response.json().await.unwrap();
    assert!(!resp["success"].as_bool().unwrap_or(true));
    assert!(resp["message"].as_str().unwrap().contains("Invalid"));

    // Test 2: Duplicate node ID (node 1 already exists)
    let duplicate_request = serde_json::json!({
        "node_id": 1,  // Already exists!
        "kafka_addr": "localhost:9099",
        "wal_addr": "localhost:9299",
        "raft_addr": "localhost:5009",
    });

    let response = client
        .post(&add_node_url)
        .json(&duplicate_request)
        .send()
        .await
        .expect("Failed to send request");

    let resp: serde_json::Value = response.json().await.unwrap();
    assert!(!resp["success"].as_bool().unwrap_or(true));
    assert!(resp["message"].as_str().unwrap().contains("already exists"));

    println!("✓ Error handling tests passed");
}
