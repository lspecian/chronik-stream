//! Integration tests for the operator reconciliation logic

use chronik_operator::crd::{ChronikCluster, ChronikClusterSpec, StorageSpec, StorageBackend, MetastoreSpec, DatabaseType, ConnectionSpec, ClusterPhase};
use chronik_operator::reconciler::Reconciler;
use k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::CustomResourceDefinition;
use kube::{
    api::{Api, PostParams, DeleteParams},
    client::Client,
    CustomResourceExt,
};
use std::time::Duration;
use tokio::time::sleep;

/// Test the full reconciliation logic
#[tokio::test]
#[ignore] // This test requires a running Kubernetes cluster
async fn test_reconciler_full_flow() -> Result<(), Box<dyn std::error::Error>> {
    let client = Client::try_default().await?;
    let namespace = "default";
    let cluster_name = "test-reconciler-cluster";
    
    // Install CRD first
    let crds: Api<CustomResourceDefinition> = Api::all(client.clone());
    let crd = ChronikCluster::crd();
    
    match crds.create(&PostParams::default(), &crd).await {
        Ok(_) => println!("Created ChronikCluster CRD"),
        Err(kube::Error::Api(e)) if e.code == 409 => {
            println!("CRD already exists, continuing...");
        },
        Err(e) => return Err(e.into()),
    }
    
    // Wait for CRD to be ready
    sleep(Duration::from_secs(2)).await;
    
    let api: Api<ChronikCluster> = Api::namespaced(client.clone(), namespace);
    
    // Create a test cluster
    let cluster = ChronikCluster::new(
        cluster_name,
        ChronikClusterSpec {
            controllers: 3,
            ingest_nodes: 3,
            search_nodes: Some(2),
            storage: StorageSpec {
                backend: StorageBackend::Local,
                size: "10Gi".to_string(),
                storage_class: None,
                s3: None,
                gcs: None,
                azure: None,
            },
            metastore: MetastoreSpec {
                database: DatabaseType::Postgres,
                connection: ConnectionSpec {
                    host: "postgres".to_string(),
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
    println!("Created cluster: {}", created_cluster.metadata.name.as_ref().unwrap());
    
    // Test reconciliation
    let reconciler = Reconciler::new(client.clone());
    
    // First reconciliation should handle the new cluster
    let action = reconciler.reconcile(&created_cluster).await?;
    println!("First reconciliation action: {:?}", action);
    
    // Get updated cluster
    let updated_cluster = api.get(cluster_name).await?;
    
    // Check that status was updated
    if let Some(status) = &updated_cluster.status {
        assert!(matches!(status.phase, ClusterPhase::Creating | ClusterPhase::Running | ClusterPhase::Updating));
        println!("Cluster phase: {:?}", status.phase);
    }
    
    // Test second reconciliation (should be idempotent)
    let action2 = reconciler.reconcile(&updated_cluster).await?;
    println!("Second reconciliation action: {:?}", action2);
    
    // Clean up
    api.delete(cluster_name, &DeleteParams::default()).await?;
    println!("Deleted test cluster");
    
    Ok(())
}

/// Test reconciler with invalid cluster spec
#[tokio::test]
#[ignore] // This test requires a running Kubernetes cluster  
async fn test_reconciler_validation_error() -> Result<(), Box<dyn std::error::Error>> {
    let client = Client::try_default().await?;
    let namespace = "default";
    let cluster_name = "test-invalid-cluster";
    
    let api: Api<ChronikCluster> = Api::namespaced(client.clone(), namespace);
    
    // Create a cluster with invalid configuration
    let cluster = ChronikCluster::new(
        cluster_name,
        ChronikClusterSpec {
            controllers: -1, // Invalid: negative controllers
            ingest_nodes: 1,
            search_nodes: Some(1),
            storage: StorageSpec {
                backend: StorageBackend::Local,
                size: "10Gi".to_string(),
                storage_class: None,
                s3: None,
                gcs: None,
                azure: None,
            },
            metastore: MetastoreSpec {
                database: DatabaseType::Postgres,
                connection: ConnectionSpec {
                    host: "postgres".to_string(),
                    port: 5432,
                    database: "chronik_test".to_string(),
                    credentials_secret: "postgres-secret".to_string(),
                },
            },
            ..Default::default()
        },
    );
    
    // This should fail at the Kubernetes level due to validation
    let result = api.create(&PostParams::default(), &cluster).await;
    assert!(result.is_err(), "Expected creation to fail due to invalid spec");
    
    Ok(())
}

/// Test reconciler error handling
#[tokio::test]
#[ignore] // This test requires a running Kubernetes cluster
async fn test_reconciler_error_handling() -> Result<(), Box<dyn std::error::Error>> {
    let client = Client::try_default().await?;
    let _namespace = "nonexistent-namespace"; // This should cause errors
    let cluster_name = "test-error-cluster";
    
    // Create a cluster in a non-existent namespace
    let cluster = ChronikCluster::new(
        cluster_name,
        ChronikClusterSpec::default(),
    );
    
    let reconciler = Reconciler::new(client.clone());
    
    // This should handle the error gracefully
    let result = reconciler.reconcile(&cluster).await;
    
    // We expect this to fail, but the reconciler should handle it gracefully
    assert!(result.is_err(), "Expected reconciliation to fail for non-existent namespace");
    
    Ok(())
}

#[cfg(test)]
mod unit_tests {
    use super::*;
    use chronik_operator::crd::*;
    
    /// Test cluster spec validation
    #[test]
    fn test_cluster_validation() {
        // Test valid cluster
        let valid_cluster = ChronikClusterSpec {
            controllers: 3,
            ingest_nodes: 2,
            search_nodes: Some(1),
            ..Default::default()
        };
        
        // This would be tested if we could access the validation method directly
        // For now, we test that the spec can be created
        assert_eq!(valid_cluster.controllers, 3);
        assert_eq!(valid_cluster.ingest_nodes, 2);
        assert_eq!(valid_cluster.search_nodes, Some(1));
    }
    
    /// Test storage backend configurations
    #[test]
    fn test_storage_configurations() {
        // Test different storage backends
        let local_storage = StorageSpec {
            backend: StorageBackend::Local,
            size: "10Gi".to_string(),
            storage_class: None,
            s3: None,
            gcs: None,
            azure: None,
        };
        
        let s3_storage = StorageSpec {
            backend: StorageBackend::S3,
            size: "100Gi".to_string(),
            storage_class: Some("fast-ssd".to_string()),
            s3: Some(S3Config {
                bucket: "test-bucket".to_string(),
                region: "us-west-2".to_string(),
                endpoint: None,
            }),
            gcs: None,
            azure: None,
        };
        
        assert!(matches!(local_storage.backend, StorageBackend::Local));
        assert!(matches!(s3_storage.backend, StorageBackend::S3));
        assert_eq!(s3_storage.s3.as_ref().unwrap().bucket, "test-bucket");
    }
    
    /// Test cluster phases
    #[test]
    fn test_cluster_phases() {
        assert!(matches!(ClusterPhase::default(), ClusterPhase::Pending));
        
        let phases = vec![
            ClusterPhase::Pending,
            ClusterPhase::Creating,
            ClusterPhase::Running,
            ClusterPhase::Updating,
            ClusterPhase::Failed,
            ClusterPhase::Deleting,
        ];
        
        // Test that all phases can be serialized/deserialized
        for phase in phases {
            let serialized = serde_json::to_string(&phase).unwrap();
            let deserialized: ClusterPhase = serde_json::from_str(&serialized).unwrap();
            assert_eq!(format!("{:?}", phase), format!("{:?}", deserialized));
        }
    }
    
    /// Test condition creation
    #[test]
    fn test_condition_creation() {
        let condition = Condition {
            condition_type: "Ready".to_string(),
            status: "True".to_string(),
            last_transition_time: "2024-01-01T00:00:00Z".to_string(),
            reason: "AllComponentsReady".to_string(),
            message: "All components are running".to_string(),
        };
        
        assert_eq!(condition.condition_type, "Ready");
        assert_eq!(condition.status, "True");
        assert_eq!(condition.reason, "AllComponentsReady");
    }
    
    /// Test ready replicas calculation
    #[test]
    fn test_ready_replicas() {
        let ready_replicas = ReadyReplicas {
            controllers: 3,
            ingest_nodes: 3,
            search_nodes: 2,
        };
        
        assert_eq!(ready_replicas.controllers, 3);
        assert_eq!(ready_replicas.ingest_nodes, 3);
        assert_eq!(ready_replicas.search_nodes, 2);
    }
}