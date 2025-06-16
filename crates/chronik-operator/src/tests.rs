//! Tests for the Chronik Stream operator.

#[cfg(test)]
mod tests {
    use crate::crd::*;
    use crate::resources::ResourceGenerator;
    use k8s_openapi::api::apps::v1::StatefulSet;
    use k8s_openapi::api::core::v1::{ConfigMap, Service};
    use std::collections::BTreeMap;

    /// Create a test ChronikCluster
    fn test_cluster() -> ChronikCluster {
        ChronikCluster {
            metadata: k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta {
                name: Some("test-cluster".to_string()),
                namespace: Some("default".to_string()),
                uid: Some("test-uid".to_string()),
                generation: Some(1),
                ..Default::default()
            },
            spec: ChronikClusterSpec {
                controllers: 3,
                ingest_nodes: 2,
                search_nodes: Some(1),
                storage: StorageSpec {
                    backend: StorageBackend::S3 {
                        bucket: "test-bucket".to_string(),
                        region: "us-east-1".to_string(),
                        endpoint: None,
                    },
                    size: "10Gi".to_string(),
                    storage_class: Some("fast-ssd".to_string()),
                },
                metastore: MetastoreSpec {
                    database: DatabaseType::Postgres,
                    connection: ConnectionSpec {
                        host: "postgres.default.svc.cluster.local".to_string(),
                        port: 5432,
                        database: "chronik".to_string(),
                        credentials_secret: "postgres-credentials".to_string(),
                    },
                },
                resources: Some(ResourceRequirements {
                    controller: ResourceSpec {
                        cpu: "500m".to_string(),
                        memory: "1Gi".to_string(),
                        cpu_limit: Some("1000m".to_string()),
                        memory_limit: Some("2Gi".to_string()),
                    },
                    ingest: ResourceSpec {
                        cpu: "1000m".to_string(),
                        memory: "2Gi".to_string(),
                        cpu_limit: Some("2000m".to_string()),
                        memory_limit: Some("4Gi".to_string()),
                    },
                    search: Some(ResourceSpec {
                        cpu: "2000m".to_string(),
                        memory: "4Gi".to_string(),
                        cpu_limit: Some("4000m".to_string()),
                        memory_limit: Some("8Gi".to_string()),
                    }),
                }),
                image: Some(ImageSpec {
                    controller: "chronik/controller:v1.0.0".to_string(),
                    ingest: "chronik/ingest:v1.0.0".to_string(),
                    search: Some("chronik/search:v1.0.0".to_string()),
                    pull_policy: Some("IfNotPresent".to_string()),
                    pull_secrets: Some(vec!["dockerhub-secret".to_string()]),
                }),
                monitoring: Some(MonitoringSpec {
                    prometheus: true,
                    tracing: true,
                    otlp_endpoint: Some("http://otel-collector:4317".to_string()),
                }),
                network: Some(NetworkSpec {
                    service_type: Some("LoadBalancer".to_string()),
                    load_balancer_source_ranges: Some(vec!["10.0.0.0/8".to_string()]),
                    host_network: Some(false),
                    dns_policy: Some("ClusterFirst".to_string()),
                }),
                security: Some(SecuritySpec {
                    tls_enabled: true,
                    tls_secret: Some("chronik-tls".to_string()),
                    mtls_enabled: Some(false),
                    pod_security_context: Some(PodSecurityContext {
                        run_as_user: Some(1000),
                        run_as_group: Some(1000),
                        fs_group: Some(1000),
                        run_as_non_root: Some(true),
                    }),
                    container_security_context: Some(ContainerSecurityContext {
                        allow_privilege_escalation: Some(false),
                        privileged: Some(false),
                        read_only_root_filesystem: Some(true),
                        run_as_non_root: Some(true),
                        run_as_user: Some(1000),
                    }),
                }),
                autoscaling: Some(AutoscalingSpec {
                    controllers: Some(AutoscalingConfig {
                        min_replicas: 3,
                        max_replicas: 10,
                        target_cpu_utilization_percentage: Some(80),
                        target_memory_utilization_percentage: Some(80),
                        custom_metrics: None,
                    }),
                    ingest_nodes: Some(AutoscalingConfig {
                        min_replicas: 2,
                        max_replicas: 20,
                        target_cpu_utilization_percentage: Some(70),
                        target_memory_utilization_percentage: None,
                        custom_metrics: None,
                    }),
                    search_nodes: None,
                }),
                pod_disruption_budget: Some(PodDisruptionBudgetSpec {
                    min_available: Some(IntOrString::Int(1)),
                    max_unavailable: None,
                }),
                pod_annotations: Some({
                    let mut annotations = BTreeMap::new();
                    annotations.insert("prometheus.io/scrape".to_string(), "true".to_string());
                    annotations.insert("prometheus.io/port".to_string(), "8080".to_string());
                    annotations
                }),
                pod_labels: Some({
                    let mut labels = BTreeMap::new();
                    labels.insert("env".to_string(), "production".to_string());
                    labels
                }),
                node_selector: Some({
                    let mut selector = BTreeMap::new();
                    selector.insert("node-type".to_string(), "compute".to_string());
                    selector
                }),
                tolerations: Some(vec![
                    Toleration {
                        key: Some("dedicated".to_string()),
                        operator: Some("Equal".to_string()),
                        value: Some("chronik".to_string()),
                        effect: Some("NoSchedule".to_string()),
                        toleration_seconds: None,
                    },
                ]),
                affinity: None,
            },
            status: None,
        }
    }

    #[test]
    fn test_config_map_generation() {
        let cluster = test_cluster();
        let generator = ResourceGenerator::new(&cluster);
        let config_map = generator.config_map();

        assert_eq!(config_map.metadata.name, Some("test-cluster-config".to_string()));
        assert_eq!(config_map.metadata.namespace, Some("default".to_string()));
        
        let data = config_map.data.unwrap();
        assert_eq!(data.get("storage.backend"), Some(&"s3".to_string()));
        assert_eq!(data.get("storage.s3.bucket"), Some(&"test-bucket".to_string()));
        assert_eq!(data.get("storage.s3.region"), Some(&"us-east-1".to_string()));
        assert_eq!(data.get("metastore.host"), Some(&"postgres.default.svc.cluster.local".to_string()));
        assert_eq!(data.get("metastore.port"), Some(&"5432".to_string()));
        assert_eq!(data.get("metastore.database"), Some(&"chronik".to_string()));
        assert_eq!(data.get("monitoring.prometheus.enabled"), Some(&"true".to_string()));
        assert_eq!(data.get("monitoring.tracing.enabled"), Some(&"true".to_string()));
        assert_eq!(data.get("monitoring.otlp.endpoint"), Some(&"http://otel-collector:4317".to_string()));
    }

    #[test]
    fn test_controller_stateful_set_generation() {
        let cluster = test_cluster();
        let generator = ResourceGenerator::new(&cluster);
        let stateful_set = generator.controller_stateful_set();

        assert_eq!(stateful_set.metadata.name, Some("test-cluster-controller".to_string()));
        assert_eq!(stateful_set.metadata.namespace, Some("default".to_string()));
        
        let spec = stateful_set.spec.unwrap();
        assert_eq!(spec.replicas, Some(3));
        assert_eq!(spec.service_name, "test-cluster-controller".to_string());
        
        let template = spec.template;
        let pod_spec = template.spec.unwrap();
        assert_eq!(pod_spec.containers.len(), 1);
        
        let container = &pod_spec.containers[0];
        assert_eq!(container.name, "controller");
        assert_eq!(container.image, Some("chronik/controller:v1.0.0".to_string()));
        assert_eq!(container.image_pull_policy, Some("IfNotPresent".to_string()));
        
        // Check resources
        let resources = container.resources.as_ref().unwrap();
        let requests = resources.requests.as_ref().unwrap();
        assert_eq!(requests.get("cpu").unwrap().0, "500m");
        assert_eq!(requests.get("memory").unwrap().0, "1Gi");
        
        let limits = resources.limits.as_ref().unwrap();
        assert_eq!(limits.get("cpu").unwrap().0, "1000m");
        assert_eq!(limits.get("memory").unwrap().0, "2Gi");
        
        // Check volume claim templates
        let volume_claims = spec.volume_claim_templates.unwrap();
        assert_eq!(volume_claims.len(), 1);
        assert_eq!(volume_claims[0].metadata.name, Some("data".to_string()));
    }

    #[test]
    fn test_ingest_stateful_set_generation() {
        let cluster = test_cluster();
        let generator = ResourceGenerator::new(&cluster);
        let stateful_set = generator.ingest_stateful_set();

        assert_eq!(stateful_set.metadata.name, Some("test-cluster-ingest".to_string()));
        assert_eq!(stateful_set.metadata.namespace, Some("default".to_string()));
        
        let spec = stateful_set.spec.unwrap();
        assert_eq!(spec.replicas, Some(2));
        assert_eq!(spec.service_name, "test-cluster-ingest".to_string());
    }

    #[test]
    fn test_search_stateful_set_generation() {
        let cluster = test_cluster();
        let generator = ResourceGenerator::new(&cluster);
        let stateful_set = generator.search_stateful_set();

        assert!(stateful_set.is_some());
        let stateful_set = stateful_set.unwrap();
        
        assert_eq!(stateful_set.metadata.name, Some("test-cluster-search".to_string()));
        assert_eq!(stateful_set.metadata.namespace, Some("default".to_string()));
        
        let spec = stateful_set.spec.unwrap();
        assert_eq!(spec.replicas, Some(1));
    }

    #[test]
    fn test_controller_service_generation() {
        let cluster = test_cluster();
        let generator = ResourceGenerator::new(&cluster);
        let service = generator.controller_service();

        assert_eq!(service.metadata.name, Some("test-cluster-controller".to_string()));
        assert_eq!(service.metadata.namespace, Some("default".to_string()));
        
        let spec = service.spec.unwrap();
        assert_eq!(spec.cluster_ip, Some("None".to_string())); // Headless service
        
        let ports = spec.ports.unwrap();
        assert_eq!(ports.len(), 2);
        assert_eq!(ports[0].name, Some("grpc".to_string()));
        assert_eq!(ports[0].port, 9090);
        assert_eq!(ports[1].name, Some("metrics".to_string()));
        assert_eq!(ports[1].port, 8080);
    }

    #[test]
    fn test_ingest_service_generation() {
        let cluster = test_cluster();
        let generator = ResourceGenerator::new(&cluster);
        let service = generator.ingest_service();

        assert_eq!(service.metadata.name, Some("test-cluster-ingest".to_string()));
        assert_eq!(service.metadata.namespace, Some("default".to_string()));
        
        let spec = service.spec.unwrap();
        assert_eq!(spec.type_, Some("LoadBalancer".to_string()));
        assert_eq!(spec.load_balancer_source_ranges, Some(vec!["10.0.0.0/8".to_string()]));
    }

    #[test]
    fn test_pod_disruption_budget_generation() {
        let cluster = test_cluster();
        let generator = ResourceGenerator::new(&cluster);
        let pdb = generator.pod_disruption_budget("controller");

        assert!(pdb.is_some());
        let pdb = pdb.unwrap();
        
        assert_eq!(pdb.metadata.name, Some("test-cluster-controller-pdb".to_string()));
        assert_eq!(pdb.metadata.namespace, Some("default".to_string()));
        
        let spec = pdb.spec.unwrap();
        assert_eq!(spec.min_available, Some(k8s_openapi::apimachinery::pkg::util::intstr::IntOrString::Int(1)));
    }

    #[test]
    fn test_horizontal_pod_autoscaler_generation() {
        let cluster = test_cluster();
        let generator = ResourceGenerator::new(&cluster);
        
        // Test controller HPA
        let hpa = generator.horizontal_pod_autoscaler("controller");
        assert!(hpa.is_some());
        let hpa = hpa.unwrap();
        
        assert_eq!(hpa.metadata.name, Some("test-cluster-controller-hpa".to_string()));
        assert_eq!(hpa.metadata.namespace, Some("default".to_string()));
        
        let spec = hpa.spec.unwrap();
        assert_eq!(spec.min_replicas, Some(3));
        assert_eq!(spec.max_replicas, 10);
        
        let metrics = spec.metrics.unwrap();
        assert_eq!(metrics.len(), 2); // CPU and memory
        
        // Test ingest HPA
        let hpa = generator.horizontal_pod_autoscaler("ingest");
        assert!(hpa.is_some());
        let hpa = hpa.unwrap();
        
        let spec = hpa.spec.unwrap();
        assert_eq!(spec.min_replicas, Some(2));
        assert_eq!(spec.max_replicas, 20);
        
        let metrics = spec.metrics.unwrap();
        assert_eq!(metrics.len(), 1); // Only CPU
        
        // Test search HPA (should be None as autoscaling not configured)
        let hpa = generator.horizontal_pod_autoscaler("search");
        assert!(hpa.is_none());
    }

    #[test]
    fn test_pod_labels_and_annotations() {
        let cluster = test_cluster();
        let generator = ResourceGenerator::new(&cluster);
        let stateful_set = generator.controller_stateful_set();
        
        let spec = stateful_set.spec.unwrap();
        let pod_metadata = spec.template.metadata.unwrap();
        
        // Check labels
        let labels = pod_metadata.labels.unwrap();
        assert_eq!(labels.get("env"), Some(&"production".to_string()));
        assert_eq!(labels.get("app.kubernetes.io/name"), Some(&"chronik-stream".to_string()));
        assert_eq!(labels.get("app.kubernetes.io/component"), Some(&"controller".to_string()));
        
        // Check annotations
        let annotations = pod_metadata.annotations.unwrap();
        assert_eq!(annotations.get("prometheus.io/scrape"), Some(&"true".to_string()));
        assert_eq!(annotations.get("prometheus.io/port"), Some(&"8080".to_string()));
    }

    #[test]
    fn test_security_context() {
        let cluster = test_cluster();
        let generator = ResourceGenerator::new(&cluster);
        let stateful_set = generator.controller_stateful_set();
        
        let spec = stateful_set.spec.unwrap();
        let pod_spec = spec.template.spec.unwrap();
        
        // Check pod security context
        let pod_security_context = pod_spec.security_context.unwrap();
        assert_eq!(pod_security_context.run_as_user, Some(1000));
        assert_eq!(pod_security_context.run_as_group, Some(1000));
        assert_eq!(pod_security_context.fs_group, Some(1000));
        assert_eq!(pod_security_context.run_as_non_root, Some(true));
        
        // Check container security context
        let container = &pod_spec.containers[0];
        let container_security_context = container.security_context.as_ref().unwrap();
        assert_eq!(container_security_context.allow_privilege_escalation, Some(false));
        assert_eq!(container_security_context.privileged, Some(false));
        assert_eq!(container_security_context.read_only_root_filesystem, Some(true));
        assert_eq!(container_security_context.run_as_non_root, Some(true));
        assert_eq!(container_security_context.run_as_user, Some(1000));
    }

    #[test]
    fn test_node_selector_and_tolerations() {
        let cluster = test_cluster();
        let generator = ResourceGenerator::new(&cluster);
        let stateful_set = generator.controller_stateful_set();
        
        let spec = stateful_set.spec.unwrap();
        let pod_spec = spec.template.spec.unwrap();
        
        // Check node selector
        let node_selector = pod_spec.node_selector.unwrap();
        assert_eq!(node_selector.get("node-type"), Some(&"compute".to_string()));
        
        // Check tolerations
        let tolerations = pod_spec.tolerations.unwrap();
        assert_eq!(tolerations.len(), 1);
        assert_eq!(tolerations[0].key, Some("dedicated".to_string()));
        assert_eq!(tolerations[0].operator, Some("Equal".to_string()));
        assert_eq!(tolerations[0].value, Some("chronik".to_string()));
        assert_eq!(tolerations[0].effect, Some("NoSchedule".to_string()));
    }

    #[test]
    fn test_image_pull_secrets() {
        let cluster = test_cluster();
        let generator = ResourceGenerator::new(&cluster);
        let stateful_set = generator.controller_stateful_set();
        
        let spec = stateful_set.spec.unwrap();
        let pod_spec = spec.template.spec.unwrap();
        
        let pull_secrets = pod_spec.image_pull_secrets.unwrap();
        assert_eq!(pull_secrets.len(), 1);
        assert_eq!(pull_secrets[0].name, Some("dockerhub-secret".to_string()));
    }

    #[test]
    fn test_volume_mounts() {
        let cluster = test_cluster();
        let generator = ResourceGenerator::new(&cluster);
        let stateful_set = generator.controller_stateful_set();
        
        let spec = stateful_set.spec.unwrap();
        let pod_spec = spec.template.spec.unwrap();
        let container = &pod_spec.containers[0];
        
        let volume_mounts = container.volume_mounts.as_ref().unwrap();
        
        // Check config mount
        let config_mount = volume_mounts.iter().find(|m| m.name == "config").unwrap();
        assert_eq!(config_mount.mount_path, "/etc/chronik/config");
        assert_eq!(config_mount.read_only, Some(true));
        
        // Check data mount
        let data_mount = volume_mounts.iter().find(|m| m.name == "data").unwrap();
        assert_eq!(data_mount.mount_path, "/var/lib/chronik");
        
        // Check TLS mount
        let tls_mount = volume_mounts.iter().find(|m| m.name == "tls").unwrap();
        assert_eq!(tls_mount.mount_path, "/etc/chronik/tls");
        assert_eq!(tls_mount.read_only, Some(true));
    }

    #[test]
    fn test_no_search_nodes() {
        let mut cluster = test_cluster();
        cluster.spec.search_nodes = None;
        
        let generator = ResourceGenerator::new(&cluster);
        
        // Should return None for search resources
        assert!(generator.search_stateful_set().is_none());
        assert!(generator.search_service().is_none());
    }

    #[test]
    fn test_local_storage_backend() {
        let mut cluster = test_cluster();
        cluster.spec.storage.backend = StorageBackend::Local;
        
        let generator = ResourceGenerator::new(&cluster);
        let config_map = generator.config_map();
        
        let data = config_map.data.unwrap();
        assert_eq!(data.get("storage.backend"), Some(&"local".to_string()));
    }

    #[test]
    fn test_gcs_storage_backend() {
        let mut cluster = test_cluster();
        cluster.spec.storage.backend = StorageBackend::Gcs {
            bucket: "gcs-bucket".to_string(),
        };
        
        let generator = ResourceGenerator::new(&cluster);
        let config_map = generator.config_map();
        
        let data = config_map.data.unwrap();
        assert_eq!(data.get("storage.backend"), Some(&"gcs".to_string()));
        assert_eq!(data.get("storage.gcs.bucket"), Some(&"gcs-bucket".to_string()));
    }

    #[test]
    fn test_azure_storage_backend() {
        let mut cluster = test_cluster();
        cluster.spec.storage.backend = StorageBackend::Azure {
            container: "azure-container".to_string(),
            account: "storageaccount".to_string(),
        };
        
        let generator = ResourceGenerator::new(&cluster);
        let config_map = generator.config_map();
        
        let data = config_map.data.unwrap();
        assert_eq!(data.get("storage.backend"), Some(&"azure".to_string()));
        assert_eq!(data.get("storage.azure.container"), Some(&"azure-container".to_string()));
        assert_eq!(data.get("storage.azure.account"), Some(&"storageaccount".to_string()));
    }
}