//! Integration tests for ChronikCluster CRD operations

use chronik_operator::crd::{ChronikCluster, ChronikClusterSpec, StorageSpec, StorageBackend, MetastoreSpec, DatabaseType, ConnectionSpec};
use k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::CustomResourceDefinition;
use kube::{
    api::{Api, PostParams, DeleteParams, Patch, PatchParams},
    client::Client,
    CustomResourceExt,
};
use serde_json::json;
use std::time::Duration;
use tokio::time::sleep;

/// Test CRD installation and basic operations
#[tokio::test]
#[ignore] // This test requires a running Kubernetes cluster
async fn test_crd_installation() -> Result<(), Box<dyn std::error::Error>> {
    let client = Client::try_default().await?;
    
    // Install CRD
    let crds: Api<CustomResourceDefinition> = Api::all(client.clone());
    let crd = ChronikCluster::crd();
    
    // Try to create the CRD
    match crds.create(&PostParams::default(), &crd).await {
        Ok(_) => println!("Created ChronikCluster CRD"),
        Err(kube::Error::Api(e)) if e.code == 409 => {
            println!("CRD already exists, continuing...");
        },
        Err(e) => return Err(e.into()),
    }
    
    // Wait for CRD to be ready
    sleep(Duration::from_secs(2)).await;
    
    // Verify CRD exists
    let crd_name = "chronikclusters.chronik.stream";
    let retrieved_crd = crds.get(crd_name).await?;
    assert_eq!(retrieved_crd.metadata.name, Some(crd_name.to_string()));
    
    Ok(())
}

/// Test creating a ChronikCluster resource
#[tokio::test]
#[ignore] // This test requires a running Kubernetes cluster
async fn test_create_chronik_cluster() -> Result<(), Box<dyn std::error::Error>> {
    let client = Client::try_default().await?;
    let namespace = "default";
    let cluster_name = "test-cluster";
    
    let api: Api<ChronikCluster> = Api::namespaced(client, namespace);
    
    // Create a test cluster
    let cluster = ChronikCluster::new(
        cluster_name,
        ChronikClusterSpec {
            controllers: 1,
            ingest_nodes: 1,
            search_nodes: None,
            storage: StorageSpec {
                backend: StorageBackend::Local,
                size: "1Gi".to_string(),
                storage_class: None,
                s3: None,
                gcs: None,
                azure: None,
            },
            metastore: MetastoreSpec {
                database: DatabaseType::Postgres,
                connection: ConnectionSpec {
                    host: "localhost".to_string(),
                    port: 5432,
                    database: "chronik_test".to_string(),
                    credentials_secret: "postgres-secret".to_string(),
                },
            },
            ..Default::default()
        },
    );
    
    // Create the cluster
    let created_cluster = api.create(&PostParams::default(), &cluster).await?;
    assert_eq!(created_cluster.metadata.name, Some(cluster_name.to_string()));
    assert_eq!(created_cluster.spec.controllers, 1);
    assert_eq!(created_cluster.spec.ingest_nodes, 1);
    
    // Clean up
    api.delete(cluster_name, &DeleteParams::default()).await?;
    
    Ok(())
}

/// Test updating a ChronikCluster resource
#[tokio::test]
#[ignore] // This test requires a running Kubernetes cluster
async fn test_update_chronik_cluster() -> Result<(), Box<dyn std::error::Error>> {
    let client = Client::try_default().await?;
    let namespace = "default";
    let cluster_name = "test-update-cluster";
    
    let api: Api<ChronikCluster> = Api::namespaced(client, namespace);
    
    // Create a test cluster
    let cluster = ChronikCluster::new(
        cluster_name,
        ChronikClusterSpec {
            controllers: 1,
            ingest_nodes: 1,
            search_nodes: None,
            storage: StorageSpec {
                backend: StorageBackend::Local,
                size: "1Gi".to_string(),
                storage_class: None,
                s3: None,
                gcs: None,
                azure: None,
            },
            metastore: MetastoreSpec {
                database: DatabaseType::Postgres,
                connection: ConnectionSpec {
                    host: "localhost".to_string(),
                    port: 5432,
                    database: "chronik_test".to_string(),
                    credentials_secret: "postgres-secret".to_string(),
                },
            },
            ..Default::default()
        },
    );
    
    // Create the cluster
    let _created_cluster = api.create(&PostParams::default(), &cluster).await?;
    
    // Update the cluster
    let patch = json!({
        "spec": {
            "controllers": 3,
            "ingestNodes": 3
        }
    });
    
    let patch_params = PatchParams::default();
    let updated_cluster = api.patch(cluster_name, &patch_params, &Patch::Merge(patch)).await?;
    
    assert_eq!(updated_cluster.spec.controllers, 3);
    assert_eq!(updated_cluster.spec.ingest_nodes, 3);
    
    // Clean up
    api.delete(cluster_name, &DeleteParams::default()).await?;
    
    Ok(())
}

/// Test status updates
#[tokio::test]
#[ignore] // This test requires a running Kubernetes cluster
async fn test_status_update() -> Result<(), Box<dyn std::error::Error>> {
    let client = Client::try_default().await?;
    let namespace = "default";
    let cluster_name = "test-status-cluster";
    
    let api: Api<ChronikCluster> = Api::namespaced(client, namespace);
    
    // Create a test cluster
    let cluster = ChronikCluster::new(
        cluster_name,
        ChronikClusterSpec::default(),
    );
    
    // Create the cluster
    let _created_cluster = api.create(&PostParams::default(), &cluster).await?;
    
    // Update status
    let status_patch = json!({
        "status": {
            "phase": "Running",
            "lastUpdated": "2024-01-01T00:00:00Z",
            "controllers": ["controller-0"],
            "ingestNodes": ["ingest-0"],
            "searchNodes": [],
            "conditions": []
        }
    });
    
    let patch_params = PatchParams::default();
    let updated_cluster = api.patch_status(cluster_name, &patch_params, &Patch::Merge(status_patch)).await?;
    
    if let Some(status) = updated_cluster.status {
        assert_eq!(status.controllers, vec!["controller-0"]);
        assert_eq!(status.ingest_nodes, vec!["ingest-0"]);
    } else {
        panic!("Status was not updated");
    }
    
    // Clean up
    api.delete(cluster_name, &DeleteParams::default()).await?;
    
    Ok(())
}

/// Test CRD validation
#[tokio::test]
#[ignore] // This test requires a running Kubernetes cluster
async fn test_crd_validation() -> Result<(), Box<dyn std::error::Error>> {
    let client = Client::try_default().await?;
    let namespace = "default";
    let cluster_name = "test-validation-cluster";
    
    let api: Api<ChronikCluster> = Api::namespaced(client, namespace);
    
    // Try to create a cluster with invalid spec (negative replicas)
    let invalid_cluster = ChronikCluster::new(
        cluster_name,
        ChronikClusterSpec {
            controllers: -1, // This should be invalid
            ingest_nodes: 1,
            search_nodes: None,
            storage: StorageSpec {
                backend: StorageBackend::Local,
                size: "1Gi".to_string(),
                storage_class: None,
                s3: None,
                gcs: None,
                azure: None,
            },
            metastore: MetastoreSpec {
                database: DatabaseType::Postgres,
                connection: ConnectionSpec {
                    host: "localhost".to_string(),
                    port: 5432,
                    database: "chronik_test".to_string(),
                    credentials_secret: "postgres-secret".to_string(),
                },
            },
            ..Default::default()
        },
    );
    
    // This should fail due to validation
    let result = api.create(&PostParams::default(), &invalid_cluster).await;
    assert!(result.is_err(), "Expected validation error for negative controllers");
    
    Ok(())
}

/// Test resource listing and selection
#[tokio::test]
#[ignore] // This test requires a running Kubernetes cluster
async fn test_list_chronik_clusters() -> Result<(), Box<dyn std::error::Error>> {
    let client = Client::try_default().await?;
    let namespace = "default";
    
    let api: Api<ChronikCluster> = Api::namespaced(client, namespace);
    
    // Create multiple test clusters
    for i in 0..3 {
        let cluster_name = format!("test-list-cluster-{}", i);
        let cluster = ChronikCluster::new(
            &cluster_name,
            ChronikClusterSpec::default(),
        );
        
        api.create(&PostParams::default(), &cluster).await?;
    }
    
    // List all clusters
    let clusters = api.list(&Default::default()).await?;
    assert!(clusters.items.len() >= 3, "Should have at least 3 clusters");
    
    // Verify cluster names
    let cluster_names: Vec<String> = clusters.items
        .iter()
        .filter_map(|c| c.metadata.name.clone())
        .filter(|n| n.starts_with("test-list-cluster-"))
        .collect();
    assert_eq!(cluster_names.len(), 3);
    
    // Clean up
    for i in 0..3 {
        let cluster_name = format!("test-list-cluster-{}", i);
        let _ = api.delete(&cluster_name, &DeleteParams::default()).await;
    }
    
    Ok(())
}

/// Test finalizer handling
#[tokio::test]
#[ignore] // This test requires a running Kubernetes cluster
async fn test_finalizer_handling() -> Result<(), Box<dyn std::error::Error>> {
    let client = Client::try_default().await?;
    let namespace = "default";
    let cluster_name = "test-finalizer-cluster";
    
    let api: Api<ChronikCluster> = Api::namespaced(client, namespace);
    
    // Create a test cluster
    let cluster = ChronikCluster::new(
        cluster_name,
        ChronikClusterSpec::default(),
    );
    
    let _created_cluster = api.create(&PostParams::default(), &cluster).await?;
    
    // Add finalizer
    let finalizer_patch = json!({
        "metadata": {
            "finalizers": ["chronik.stream/finalizer"]
        }
    });
    
    let patch_params = PatchParams::default();
    let updated_cluster = api.patch(cluster_name, &patch_params, &Patch::Merge(finalizer_patch)).await?;
    
    assert!(updated_cluster.metadata.finalizers.as_ref().unwrap().contains(&"chronik.stream/finalizer".to_string()));
    
    // Remove finalizer
    let remove_finalizer_patch = json!({
        "metadata": {
            "finalizers": []
        }
    });
    
    api.patch(cluster_name, &patch_params, &Patch::Merge(remove_finalizer_patch)).await?;
    
    // Clean up
    api.delete(cluster_name, &DeleteParams::default()).await?;
    
    Ok(())
}

#[cfg(test)]
mod unit_tests {
    use super::*;
    use chronik_operator::crd::*;
    
    #[test]
    fn test_chronik_cluster_spec_default() {
        let spec = ChronikClusterSpec::default();
        assert_eq!(spec.controllers, 3);
        assert_eq!(spec.ingest_nodes, 3);
        assert_eq!(spec.search_nodes, Some(3));
        assert!(matches!(spec.storage.backend, StorageBackend::Local));
        assert_eq!(spec.storage.size, "10Gi");
    }
    
    #[test]
    fn test_storage_backend_serialization() {
        // Test Local backend
        let local_backend = StorageBackend::Local;
        let json = serde_json::to_string(&local_backend).unwrap();
        assert_eq!(json, "\"local\"");
        
        // Test S3 backend
        let s3_backend = StorageBackend::S3;
        let json = serde_json::to_string(&s3_backend).unwrap();
        assert_eq!(json, "\"s3\"");
    }
    
    #[test]
    fn test_crd_generation() {
        let crd = ChronikCluster::crd();
        assert_eq!(crd.metadata.name, Some("chronikclusters.chronik.stream".to_string()));
        assert_eq!(crd.spec.group, "chronik.stream");
        assert_eq!(crd.spec.names.kind, "ChronikCluster");
        assert_eq!(crd.spec.names.plural, "chronikclusters");
        assert_eq!(crd.spec.names.short_names, Some(vec!["ck".to_string()]));
    }
    
    #[test]
    fn test_cluster_phase_default() {
        let phase = ClusterPhase::default();
        assert!(matches!(phase, ClusterPhase::Pending));
    }
    
    #[test]
    fn test_storage_spec_with_s3() {
        let storage = StorageSpec {
            backend: StorageBackend::S3,
            size: "100Gi".to_string(),
            storage_class: Some("fast-ssd".to_string()),
            s3: None,
            gcs: None,
            azure: None,
        };
        
        assert!(matches!(storage.backend, StorageBackend::S3));
        assert_eq!(storage.size, "100Gi");
        assert_eq!(storage.storage_class, Some("fast-ssd".to_string()));
    }
}