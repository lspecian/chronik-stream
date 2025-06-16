//! Integration tests for the Chronik Stream operator.
//! These tests require a Kubernetes cluster (can use kind or minikube).

use chronik_operator::crd::{ChronikCluster, ChronikClusterSpec, StorageSpec, StorageBackend, 
    MetastoreSpec, DatabaseType, ConnectionSpec};
use k8s_openapi::api::apps::v1::StatefulSet;
use k8s_openapi::api::core::v1::{ConfigMap, Service};
use kube::{
    api::{Api, DeleteParams, PostParams},
    Client, Config,
};
use tokio::time::{sleep, Duration};

/// Test cluster name
const TEST_CLUSTER_NAME: &str = "chronik-test";
/// Test namespace
const TEST_NAMESPACE: &str = "default";

/// Create a test client
async fn test_client() -> Client {
    // Try to create client from kubeconfig
    let config = Config::infer().await.expect("Failed to infer kube config");
    Client::try_from(config).expect("Failed to create kube client")
}

/// Create a test cluster spec
fn test_cluster_spec() -> ChronikClusterSpec {
    ChronikClusterSpec {
        controllers: 1,
        ingest_nodes: 1,
        search_nodes: Some(1),
        storage: StorageSpec {
            backend: StorageBackend::Local,
            size: "1Gi".to_string(),
            storage_class: None,
        },
        metastore: MetastoreSpec {
            database: DatabaseType::Postgres,
            connection: ConnectionSpec {
                host: "postgres.default.svc.cluster.local".to_string(),
                port: 5432,
                database: "chronik_test".to_string(),
                credentials_secret: "postgres-test-credentials".to_string(),
            },
        },
        resources: None,
        image: None,
        monitoring: None,
        network: None,
        security: None,
        autoscaling: None,
        pod_disruption_budget: None,
        pod_annotations: None,
        pod_labels: None,
        node_selector: None,
        tolerations: None,
        affinity: None,
    }
}

#[tokio::test]
#[ignore = "requires kubernetes cluster"]
async fn test_cluster_lifecycle() {
    let client = test_client().await;
    
    // Create APIs
    let cluster_api: Api<ChronikCluster> = Api::namespaced(client.clone(), TEST_NAMESPACE);
    let sts_api: Api<StatefulSet> = Api::namespaced(client.clone(), TEST_NAMESPACE);
    let svc_api: Api<Service> = Api::namespaced(client.clone(), TEST_NAMESPACE);
    let cm_api: Api<ConfigMap> = Api::namespaced(client.clone(), TEST_NAMESPACE);
    
    // Clean up any existing test cluster
    let _ = cluster_api.delete(TEST_CLUSTER_NAME, &DeleteParams::default()).await;
    sleep(Duration::from_secs(5)).await;
    
    // Create test cluster
    let cluster = ChronikCluster::new(TEST_CLUSTER_NAME, test_cluster_spec());
    let created = cluster_api.create(&PostParams::default(), &cluster).await
        .expect("Failed to create ChronikCluster");
    
    assert_eq!(created.metadata.name, Some(TEST_CLUSTER_NAME.to_string()));
    
    // Wait for reconciliation
    sleep(Duration::from_secs(10)).await;
    
    // Verify StatefulSets were created
    let controller_sts = sts_api.get(&format!("{}-controller", TEST_CLUSTER_NAME)).await
        .expect("Controller StatefulSet should exist");
    assert_eq!(controller_sts.spec.as_ref().unwrap().replicas, Some(1));
    
    let ingest_sts = sts_api.get(&format!("{}-ingest", TEST_CLUSTER_NAME)).await
        .expect("Ingest StatefulSet should exist");
    assert_eq!(ingest_sts.spec.as_ref().unwrap().replicas, Some(1));
    
    let search_sts = sts_api.get(&format!("{}-search", TEST_CLUSTER_NAME)).await
        .expect("Search StatefulSet should exist");
    assert_eq!(search_sts.spec.as_ref().unwrap().replicas, Some(1));
    
    // Verify Services were created
    let controller_svc = svc_api.get(&format!("{}-controller", TEST_CLUSTER_NAME)).await
        .expect("Controller Service should exist");
    assert_eq!(controller_svc.spec.as_ref().unwrap().cluster_ip, Some("None".to_string()));
    
    let ingest_svc = svc_api.get(&format!("{}-ingest", TEST_CLUSTER_NAME)).await
        .expect("Ingest Service should exist");
    assert!(ingest_svc.spec.is_some());
    
    // Verify ConfigMap was created
    let config_map = cm_api.get(&format!("{}-config", TEST_CLUSTER_NAME)).await
        .expect("ConfigMap should exist");
    let data = config_map.data.unwrap();
    assert_eq!(data.get("storage.backend"), Some(&"local".to_string()));
    
    // Update cluster (scale up)
    let mut updated_cluster = created.clone();
    updated_cluster.spec.controllers = 2;
    updated_cluster.spec.ingest_nodes = 2;
    
    let _ = cluster_api.replace(TEST_CLUSTER_NAME, &PostParams::default(), &updated_cluster).await
        .expect("Failed to update ChronikCluster");
    
    // Wait for reconciliation
    sleep(Duration::from_secs(5)).await;
    
    // Verify StatefulSets were updated
    let controller_sts = sts_api.get(&format!("{}-controller", TEST_CLUSTER_NAME)).await
        .expect("Controller StatefulSet should exist");
    assert_eq!(controller_sts.spec.as_ref().unwrap().replicas, Some(2));
    
    let ingest_sts = sts_api.get(&format!("{}-ingest", TEST_CLUSTER_NAME)).await
        .expect("Ingest StatefulSet should exist");
    assert_eq!(ingest_sts.spec.as_ref().unwrap().replicas, Some(2));
    
    // Check cluster status
    let cluster_with_status = cluster_api.get(TEST_CLUSTER_NAME).await
        .expect("Failed to get ChronikCluster");
    
    if let Some(status) = cluster_with_status.status {
        // Status should be populated
        assert!(!status.conditions.is_empty());
        assert_eq!(status.observed_generation, Some(1));
    }
    
    // Delete cluster
    cluster_api.delete(TEST_CLUSTER_NAME, &DeleteParams::default()).await
        .expect("Failed to delete ChronikCluster");
    
    // Wait for deletion
    sleep(Duration::from_secs(10)).await;
    
    // Verify resources were deleted (should return 404)
    let controller_sts_result = sts_api.get(&format!("{}-controller", TEST_CLUSTER_NAME)).await;
    assert!(controller_sts_result.is_err());
    
    let ingest_sts_result = sts_api.get(&format!("{}-ingest", TEST_CLUSTER_NAME)).await;
    assert!(ingest_sts_result.is_err());
    
    let config_map_result = cm_api.get(&format!("{}-config", TEST_CLUSTER_NAME)).await;
    assert!(config_map_result.is_err());
}

#[tokio::test]
#[ignore = "requires kubernetes cluster"]
async fn test_cluster_with_autoscaling() {
    use chronik_operator::crd::{AutoscalingSpec, AutoscalingConfig};
    use k8s_openapi::api::autoscaling::v2::HorizontalPodAutoscaler;
    
    let client = test_client().await;
    let cluster_api: Api<ChronikCluster> = Api::namespaced(client.clone(), TEST_NAMESPACE);
    let hpa_api: Api<HorizontalPodAutoscaler> = Api::namespaced(client.clone(), TEST_NAMESPACE);
    
    // Create cluster with autoscaling
    let mut spec = test_cluster_spec();
    spec.autoscaling = Some(AutoscalingSpec {
        controllers: Some(AutoscalingConfig {
            min_replicas: 1,
            max_replicas: 5,
            target_cpu_utilization_percentage: Some(80),
            target_memory_utilization_percentage: None,
            custom_metrics: None,
        }),
        ingest_nodes: Some(AutoscalingConfig {
            min_replicas: 1,
            max_replicas: 10,
            target_cpu_utilization_percentage: Some(70),
            target_memory_utilization_percentage: Some(80),
            custom_metrics: None,
        }),
        search_nodes: None,
    });
    
    let cluster = ChronikCluster::new("chronik-autoscale-test", spec);
    let _ = cluster_api.create(&PostParams::default(), &cluster).await
        .expect("Failed to create ChronikCluster with autoscaling");
    
    // Wait for reconciliation
    sleep(Duration::from_secs(5)).await;
    
    // Verify HPAs were created
    let controller_hpa = hpa_api.get("chronik-autoscale-test-controller-hpa").await
        .expect("Controller HPA should exist");
    assert_eq!(controller_hpa.spec.as_ref().unwrap().min_replicas, Some(1));
    assert_eq!(controller_hpa.spec.as_ref().unwrap().max_replicas, 5);
    
    let ingest_hpa = hpa_api.get("chronik-autoscale-test-ingest-hpa").await
        .expect("Ingest HPA should exist");
    assert_eq!(ingest_hpa.spec.as_ref().unwrap().min_replicas, Some(1));
    assert_eq!(ingest_hpa.spec.as_ref().unwrap().max_replicas, 10);
    
    // Clean up
    let _ = cluster_api.delete("chronik-autoscale-test", &DeleteParams::default()).await;
}

#[tokio::test]
#[ignore = "requires kubernetes cluster"]
async fn test_cluster_with_pod_disruption_budget() {
    use chronik_operator::crd::{PodDisruptionBudgetSpec, IntOrString};
    use k8s_openapi::api::policy::v1::PodDisruptionBudget;
    
    let client = test_client().await;
    let cluster_api: Api<ChronikCluster> = Api::namespaced(client.clone(), TEST_NAMESPACE);
    let pdb_api: Api<PodDisruptionBudget> = Api::namespaced(client.clone(), TEST_NAMESPACE);
    
    // Create cluster with PDB
    let mut spec = test_cluster_spec();
    spec.pod_disruption_budget = Some(PodDisruptionBudgetSpec {
        min_available: Some(IntOrString::Int(1)),
        max_unavailable: None,
    });
    
    let cluster = ChronikCluster::new("chronik-pdb-test", spec);
    let _ = cluster_api.create(&PostParams::default(), &cluster).await
        .expect("Failed to create ChronikCluster with PDB");
    
    // Wait for reconciliation
    sleep(Duration::from_secs(5)).await;
    
    // Verify PDBs were created
    let controller_pdb = pdb_api.get("chronik-pdb-test-controller-pdb").await
        .expect("Controller PDB should exist");
    assert!(controller_pdb.spec.is_some());
    
    let ingest_pdb = pdb_api.get("chronik-pdb-test-ingest-pdb").await
        .expect("Ingest PDB should exist");
    assert!(ingest_pdb.spec.is_some());
    
    // Clean up
    let _ = cluster_api.delete("chronik-pdb-test", &DeleteParams::default()).await;
}