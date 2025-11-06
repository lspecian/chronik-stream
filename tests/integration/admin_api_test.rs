//! Integration tests for Admin API (v2.6.0 - Priority 2 Step 2)
//!
//! Tests the HTTP Admin API for cluster management:
//! - Health endpoint
//! - Add node endpoint
//! - Leader discovery

use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;

/// Test Admin API health endpoint
#[tokio::test]
async fn test_admin_api_health_endpoint() {
    // This test requires a running cluster node
    // For now, we'll create a minimal test that verifies the endpoint structure

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .unwrap();

    // Try to connect to a hypothetical node (will fail if no cluster running)
    // This is intentionally a smoke test
    match client.get("http://localhost:10001/admin/health").send().await {
        Ok(response) => {
            println!("Health endpoint responded with status: {}", response.status());
            if response.status().is_success() {
                let text = response.text().await.unwrap();
                println!("Health response: {}", text);
                // Verify JSON structure
                assert!(text.contains("node_id"));
                assert!(text.contains("is_leader"));
                assert!(text.contains("cluster_nodes"));
            }
        }
        Err(e) => {
            println!("Expected failure (no cluster running): {}", e);
            // This is expected if no cluster is running
        }
    }
}

/// Test Admin API add-node endpoint (requires running cluster)
#[tokio::test]
#[ignore] // Requires running 3-node cluster
async fn test_admin_api_add_node_endpoint() {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();

    // Step 1: Find the leader
    let mut leader_port = None;
    for node_id in 1..=3 {
        let admin_port = 10000 + node_id;
        let health_url = format!("http://localhost:{}/admin/health", admin_port);

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
    let add_node_url = format!("http://localhost:{}/admin/add-node", leader_port.unwrap());

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
#[ignore] // Requires running 3-node cluster
async fn test_leader_discovery() {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();

    let mut leader_found = false;
    let mut follower_count = 0;

    // Query all nodes
    for node_id in 1..=3 {
        let admin_port = 10000 + node_id;
        let health_url = format!("http://localhost:{}/admin/health", admin_port);

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
#[ignore] // Requires running cluster
async fn test_admin_api_error_handling() {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();

    // Find leader
    let mut leader_port = None;
    for node_id in 1..=3 {
        let admin_port = 10000 + node_id;
        let health_url = format!("http://localhost:{}/admin/health", admin_port);

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
    let add_node_url = format!("http://localhost:{}/admin/add-node", leader_port.unwrap());

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
