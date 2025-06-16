//! Resource generation for Chronik Stream operator.

use crate::crd::{ChronikCluster, StorageBackend};
use k8s_openapi::{
    api::{
        apps::v1::{StatefulSet, StatefulSetSpec, StatefulSetUpdateStrategy, RollingUpdateStatefulSetStrategy},
        core::v1::{
            ConfigMap, Container, ContainerPort, EmptyDirVolumeSource, EnvVar, EnvVarSource,
            PersistentVolumeClaim, PersistentVolumeClaimSpec, PersistentVolumeClaimVolumeSource,
            PodSpec, PodTemplateSpec, Probe, ResourceRequirements as K8sResourceRequirements,
            Secret, SecretKeySelector, Service, ServicePort, ServiceSpec, Volume, VolumeMount,
            HTTPGetAction, PodSecurityContext as K8sPodSecurityContext,
            SecurityContext as K8sSecurityContext,
        },
        policy::v1::{PodDisruptionBudget, PodDisruptionBudgetSpec as K8sPDBSpec},
        autoscaling::v2::{HorizontalPodAutoscaler, HorizontalPodAutoscalerSpec, MetricSpec,
            ResourceMetricSource, MetricTarget, CrossVersionObjectReference},
    },
    apimachinery::pkg::{
        api::resource::Quantity,
        apis::meta::v1::{LabelSelector, ObjectMeta, OwnerReference},
        util::intstr::IntOrString,
    },
};
use std::collections::BTreeMap;

const CONTROLLER_PORT: i32 = 9090;
const INGEST_PORT: i32 = 9092;
const SEARCH_PORT: i32 = 9093;
const METRICS_PORT: i32 = 8080;
const HEALTH_PORT: i32 = 8081;

/// Generate all resources for a ChronikCluster
pub struct ResourceGenerator<'a> {
    cluster: &'a ChronikCluster,
    owner_ref: OwnerReference,
}

impl<'a> ResourceGenerator<'a> {
    pub fn new(cluster: &'a ChronikCluster) -> Self {
        let owner_ref = OwnerReference {
            api_version: ChronikCluster::api_version(&()).to_string(),
            kind: ChronikCluster::kind(&()).to_string(),
            name: cluster.metadata.name.as_ref().unwrap().clone(),
            uid: cluster.metadata.uid.as_ref().unwrap().clone(),
            controller: Some(true),
            block_owner_deletion: Some(true),
        };
        
        Self { cluster, owner_ref }
    }
    
    /// Generate ConfigMap for cluster configuration
    pub fn config_map(&self) -> ConfigMap {
        let name = format!("{}-config", self.cluster.metadata.name.as_ref().unwrap());
        let namespace = self.cluster.metadata.namespace.as_ref().unwrap();
        
        let mut data = BTreeMap::new();
        
        // Add storage configuration
        match &self.cluster.spec.storage.backend {
            StorageBackend::S3 { bucket, region, endpoint } => {
                data.insert("storage.backend".to_string(), "s3".to_string());
                data.insert("storage.s3.bucket".to_string(), bucket.clone());
                data.insert("storage.s3.region".to_string(), region.clone());
                if let Some(endpoint) = endpoint {
                    data.insert("storage.s3.endpoint".to_string(), endpoint.clone());
                }
            }
            StorageBackend::Gcs { bucket } => {
                data.insert("storage.backend".to_string(), "gcs".to_string());
                data.insert("storage.gcs.bucket".to_string(), bucket.clone());
            }
            StorageBackend::Azure { container, account } => {
                data.insert("storage.backend".to_string(), "azure".to_string());
                data.insert("storage.azure.container".to_string(), container.clone());
                data.insert("storage.azure.account".to_string(), account.clone());
            }
            StorageBackend::Local => {
                data.insert("storage.backend".to_string(), "local".to_string());
            }
        }
        
        // Add metastore configuration
        data.insert("metastore.host".to_string(), self.cluster.spec.metastore.connection.host.clone());
        data.insert("metastore.port".to_string(), self.cluster.spec.metastore.connection.port.to_string());
        data.insert("metastore.database".to_string(), self.cluster.spec.metastore.connection.database.clone());
        
        // Add monitoring configuration
        if let Some(monitoring) = &self.cluster.spec.monitoring {
            data.insert("monitoring.prometheus.enabled".to_string(), monitoring.prometheus.to_string());
            data.insert("monitoring.tracing.enabled".to_string(), monitoring.tracing.to_string());
            if let Some(endpoint) = &monitoring.otlp_endpoint {
                data.insert("monitoring.otlp.endpoint".to_string(), endpoint.clone());
            }
        }
        
        ConfigMap {
            metadata: ObjectMeta {
                name: Some(name),
                namespace: Some(namespace.clone()),
                labels: Some(self.common_labels()),
                owner_references: Some(vec![self.owner_ref.clone()]),
                ..Default::default()
            },
            data: Some(data),
            ..Default::default()
        }
    }
    
    /// Generate controller StatefulSet
    pub fn controller_stateful_set(&self) -> StatefulSet {
        let name = format!("{}-controller", self.cluster.metadata.name.as_ref().unwrap());
        let namespace = self.cluster.metadata.namespace.as_ref().unwrap();
        
        let labels = self.component_labels("controller");
        
        StatefulSet {
            metadata: ObjectMeta {
                name: Some(name.clone()),
                namespace: Some(namespace.clone()),
                labels: Some(labels.clone()),
                owner_references: Some(vec![self.owner_ref.clone()]),
                ..Default::default()
            },
            spec: Some(StatefulSetSpec {
                replicas: Some(self.cluster.spec.controllers),
                selector: LabelSelector {
                    match_labels: Some(labels.clone()),
                    ..Default::default()
                },
                service_name: name.clone(),
                template: self.controller_pod_template(labels),
                volume_claim_templates: Some(self.volume_claim_templates()),
                pod_management_policy: Some("Parallel".to_string()),
                update_strategy: Some(StatefulSetUpdateStrategy {
                    type_: Some("RollingUpdate".to_string()),
                    rolling_update: Some(RollingUpdateStatefulSetStrategy {
                        partition: Some(0),
                        ..Default::default()
                    }),
                }),
                ..Default::default()
            }),
            ..Default::default()
        }
    }
    
    /// Generate ingest StatefulSet
    pub fn ingest_stateful_set(&self) -> StatefulSet {
        let name = format!("{}-ingest", self.cluster.metadata.name.as_ref().unwrap());
        let namespace = self.cluster.metadata.namespace.as_ref().unwrap();
        
        let labels = self.component_labels("ingest");
        
        StatefulSet {
            metadata: ObjectMeta {
                name: Some(name.clone()),
                namespace: Some(namespace.clone()),
                labels: Some(labels.clone()),
                owner_references: Some(vec![self.owner_ref.clone()]),
                ..Default::default()
            },
            spec: Some(StatefulSetSpec {
                replicas: Some(self.cluster.spec.ingest_nodes),
                selector: LabelSelector {
                    match_labels: Some(labels.clone()),
                    ..Default::default()
                },
                service_name: name.clone(),
                template: self.ingest_pod_template(labels),
                volume_claim_templates: Some(self.volume_claim_templates()),
                pod_management_policy: Some("Parallel".to_string()),
                update_strategy: Some(StatefulSetUpdateStrategy {
                    type_: Some("RollingUpdate".to_string()),
                    rolling_update: Some(RollingUpdateStatefulSetStrategy {
                        partition: Some(0),
                        ..Default::default()
                    }),
                }),
                ..Default::default()
            }),
            ..Default::default()
        }
    }
    
    /// Generate search StatefulSet (if enabled)
    pub fn search_stateful_set(&self) -> Option<StatefulSet> {
        let search_nodes = self.cluster.spec.search_nodes.unwrap_or(0);
        if search_nodes == 0 {
            return None;
        }
        
        let name = format!("{}-search", self.cluster.metadata.name.as_ref().unwrap());
        let namespace = self.cluster.metadata.namespace.as_ref().unwrap();
        
        let labels = self.component_labels("search");
        
        Some(StatefulSet {
            metadata: ObjectMeta {
                name: Some(name.clone()),
                namespace: Some(namespace.clone()),
                labels: Some(labels.clone()),
                owner_references: Some(vec![self.owner_ref.clone()]),
                ..Default::default()
            },
            spec: Some(StatefulSetSpec {
                replicas: Some(search_nodes),
                selector: LabelSelector {
                    match_labels: Some(labels.clone()),
                    ..Default::default()
                },
                service_name: name.clone(),
                template: self.search_pod_template(labels),
                volume_claim_templates: Some(self.volume_claim_templates()),
                pod_management_policy: Some("Parallel".to_string()),
                update_strategy: Some(StatefulSetUpdateStrategy {
                    type_: Some("RollingUpdate".to_string()),
                    rolling_update: Some(RollingUpdateStatefulSetStrategy {
                        partition: Some(0),
                        ..Default::default()
                    }),
                }),
                ..Default::default()
            }),
            ..Default::default()
        })
    }
    
    /// Generate controller service
    pub fn controller_service(&self) -> Service {
        let name = format!("{}-controller", self.cluster.metadata.name.as_ref().unwrap());
        let namespace = self.cluster.metadata.namespace.as_ref().unwrap();
        
        let labels = self.component_labels("controller");
        
        Service {
            metadata: ObjectMeta {
                name: Some(name),
                namespace: Some(namespace.clone()),
                labels: Some(labels.clone()),
                owner_references: Some(vec![self.owner_ref.clone()]),
                ..Default::default()
            },
            spec: Some(ServiceSpec {
                selector: Some(labels),
                cluster_ip: Some("None".to_string()), // Headless service for StatefulSet
                ports: Some(vec![
                    ServicePort {
                        name: Some("grpc".to_string()),
                        port: CONTROLLER_PORT,
                        target_port: Some(IntOrString::Int(CONTROLLER_PORT)),
                        ..Default::default()
                    },
                    ServicePort {
                        name: Some("metrics".to_string()),
                        port: METRICS_PORT,
                        target_port: Some(IntOrString::Int(METRICS_PORT)),
                        ..Default::default()
                    },
                ]),
                ..Default::default()
            }),
            ..Default::default()
        }
    }
    
    /// Generate ingest service
    pub fn ingest_service(&self) -> Service {
        let name = format!("{}-ingest", self.cluster.metadata.name.as_ref().unwrap());
        let namespace = self.cluster.metadata.namespace.as_ref().unwrap();
        
        let labels = self.component_labels("ingest");
        
        let service_type = self.cluster.spec.network.as_ref()
            .and_then(|n| n.service_type.clone())
            .unwrap_or_else(|| "ClusterIP".to_string());
        
        Service {
            metadata: ObjectMeta {
                name: Some(name),
                namespace: Some(namespace.clone()),
                labels: Some(labels.clone()),
                owner_references: Some(vec![self.owner_ref.clone()]),
                ..Default::default()
            },
            spec: Some(ServiceSpec {
                selector: Some(labels),
                type_: Some(service_type),
                ports: Some(vec![
                    ServicePort {
                        name: Some("kafka".to_string()),
                        port: INGEST_PORT,
                        target_port: Some(IntOrString::Int(INGEST_PORT)),
                        ..Default::default()
                    },
                    ServicePort {
                        name: Some("metrics".to_string()),
                        port: METRICS_PORT,
                        target_port: Some(IntOrString::Int(METRICS_PORT)),
                        ..Default::default()
                    },
                ]),
                load_balancer_source_ranges: self.cluster.spec.network.as_ref()
                    .and_then(|n| n.load_balancer_source_ranges.clone()),
                ..Default::default()
            }),
            ..Default::default()
        }
    }
    
    /// Generate search service (if enabled)
    pub fn search_service(&self) -> Option<Service> {
        if self.cluster.spec.search_nodes.unwrap_or(0) == 0 {
            return None;
        }
        
        let name = format!("{}-search", self.cluster.metadata.name.as_ref().unwrap());
        let namespace = self.cluster.metadata.namespace.as_ref().unwrap();
        
        let labels = self.component_labels("search");
        
        Some(Service {
            metadata: ObjectMeta {
                name: Some(name),
                namespace: Some(namespace.clone()),
                labels: Some(labels.clone()),
                owner_references: Some(vec![self.owner_ref.clone()]),
                ..Default::default()
            },
            spec: Some(ServiceSpec {
                selector: Some(labels),
                cluster_ip: Some("None".to_string()), // Headless service for StatefulSet
                ports: Some(vec![
                    ServicePort {
                        name: Some("grpc".to_string()),
                        port: SEARCH_PORT,
                        target_port: Some(IntOrString::Int(SEARCH_PORT)),
                        ..Default::default()
                    },
                    ServicePort {
                        name: Some("metrics".to_string()),
                        port: METRICS_PORT,
                        target_port: Some(IntOrString::Int(METRICS_PORT)),
                        ..Default::default()
                    },
                ]),
                ..Default::default()
            }),
            ..Default::default()
        })
    }
    
    /// Generate PodDisruptionBudget for a component
    pub fn pod_disruption_budget(&self, component: &str) -> Option<PodDisruptionBudget> {
        let pdb_spec = self.cluster.spec.pod_disruption_budget.as_ref()?;
        
        let name = format!("{}-{}-pdb", self.cluster.metadata.name.as_ref().unwrap(), component);
        let namespace = self.cluster.metadata.namespace.as_ref().unwrap();
        
        let labels = self.component_labels(component);
        
        Some(PodDisruptionBudget {
            metadata: ObjectMeta {
                name: Some(name),
                namespace: Some(namespace.clone()),
                labels: Some(labels.clone()),
                owner_references: Some(vec![self.owner_ref.clone()]),
                ..Default::default()
            },
            spec: Some(K8sPDBSpec {
                selector: Some(LabelSelector {
                    match_labels: Some(labels),
                    ..Default::default()
                }),
                min_available: pdb_spec.min_available.as_ref().map(|v| match v {
                    crate::crd::IntOrString::Int(i) => IntOrString::Int(*i),
                    crate::crd::IntOrString::String(s) => IntOrString::String(s.clone()),
                }),
                max_unavailable: pdb_spec.max_unavailable.as_ref().map(|v| match v {
                    crate::crd::IntOrString::Int(i) => IntOrString::Int(*i),
                    crate::crd::IntOrString::String(s) => IntOrString::String(s.clone()),
                }),
                ..Default::default()
            }),
            ..Default::default()
        })
    }
    
    /// Generate HorizontalPodAutoscaler for a component
    pub fn horizontal_pod_autoscaler(&self, component: &str) -> Option<HorizontalPodAutoscaler> {
        let autoscaling = self.cluster.spec.autoscaling.as_ref()?;
        
        let config = match component {
            "controller" => autoscaling.controllers.as_ref()?,
            "ingest" => autoscaling.ingest_nodes.as_ref()?,
            "search" => autoscaling.search_nodes.as_ref()?,
            _ => return None,
        };
        
        let name = format!("{}-{}-hpa", self.cluster.metadata.name.as_ref().unwrap(), component);
        let namespace = self.cluster.metadata.namespace.as_ref().unwrap();
        
        let labels = self.component_labels(component);
        
        let mut metrics = Vec::new();
        
        // CPU metric
        if let Some(cpu_target) = config.target_cpu_utilization_percentage {
            metrics.push(MetricSpec {
                type_: "Resource".to_string(),
                resource: Some(ResourceMetricSource {
                    name: "cpu".to_string(),
                    target: MetricTarget {
                        type_: "Utilization".to_string(),
                        average_utilization: Some(cpu_target),
                        ..Default::default()
                    },
                }),
                ..Default::default()
            });
        }
        
        // Memory metric
        if let Some(memory_target) = config.target_memory_utilization_percentage {
            metrics.push(MetricSpec {
                type_: "Resource".to_string(),
                resource: Some(ResourceMetricSource {
                    name: "memory".to_string(),
                    target: MetricTarget {
                        type_: "Utilization".to_string(),
                        average_utilization: Some(memory_target),
                        ..Default::default()
                    },
                }),
                ..Default::default()
            });
        }
        
        Some(HorizontalPodAutoscaler {
            metadata: ObjectMeta {
                name: Some(name),
                namespace: Some(namespace.clone()),
                labels: Some(labels),
                owner_references: Some(vec![self.owner_ref.clone()]),
                ..Default::default()
            },
            spec: Some(HorizontalPodAutoscalerSpec {
                scale_target_ref: CrossVersionObjectReference {
                    api_version: Some("apps/v1".to_string()),
                    kind: "StatefulSet".to_string(),
                    name: format!("{}-{}", self.cluster.metadata.name.as_ref().unwrap(), component),
                },
                min_replicas: Some(config.min_replicas),
                max_replicas: config.max_replicas,
                metrics: Some(metrics),
                ..Default::default()
            }),
            ..Default::default()
        })
    }
    
    // Helper methods
    
    fn common_labels(&self) -> BTreeMap<String, String> {
        let mut labels = BTreeMap::new();
        labels.insert("app.kubernetes.io/name".to_string(), "chronik-stream".to_string());
        labels.insert("app.kubernetes.io/instance".to_string(), self.cluster.metadata.name.as_ref().unwrap().clone());
        labels.insert("app.kubernetes.io/managed-by".to_string(), "chronik-operator".to_string());
        labels.insert("chronik.stream/cluster".to_string(), self.cluster.metadata.name.as_ref().unwrap().clone());
        labels
    }
    
    fn component_labels(&self, component: &str) -> BTreeMap<String, String> {
        let mut labels = self.common_labels();
        labels.insert("app.kubernetes.io/component".to_string(), component.to_string());
        labels.insert("chronik.stream/component".to_string(), component.to_string());
        labels
    }
    
    fn controller_pod_template(&self, labels: BTreeMap<String, String>) -> PodTemplateSpec {
        let container = self.controller_container();
        
        PodTemplateSpec {
            metadata: Some(ObjectMeta {
                labels: Some(self.merge_labels(labels)),
                annotations: self.cluster.spec.pod_annotations.clone(),
                ..Default::default()
            }),
            spec: Some(PodSpec {
                containers: vec![container],
                volumes: Some(self.volumes()),
                service_account_name: Some(format!("{}-controller", self.cluster.metadata.name.as_ref().unwrap())),
                security_context: self.pod_security_context(),
                node_selector: self.cluster.spec.node_selector.clone(),
                tolerations: self.tolerations(),
                affinity: self.affinity(),
                image_pull_secrets: self.image_pull_secrets(),
                ..Default::default()
            }),
        }
    }
    
    fn ingest_pod_template(&self, labels: BTreeMap<String, String>) -> PodTemplateSpec {
        let container = self.ingest_container();
        
        PodTemplateSpec {
            metadata: Some(ObjectMeta {
                labels: Some(self.merge_labels(labels)),
                annotations: self.cluster.spec.pod_annotations.clone(),
                ..Default::default()
            }),
            spec: Some(PodSpec {
                containers: vec![container],
                volumes: Some(self.volumes()),
                service_account_name: Some(format!("{}-ingest", self.cluster.metadata.name.as_ref().unwrap())),
                security_context: self.pod_security_context(),
                node_selector: self.cluster.spec.node_selector.clone(),
                tolerations: self.tolerations(),
                affinity: self.affinity(),
                image_pull_secrets: self.image_pull_secrets(),
                ..Default::default()
            }),
        }
    }
    
    fn search_pod_template(&self, labels: BTreeMap<String, String>) -> PodTemplateSpec {
        let container = self.search_container();
        
        PodTemplateSpec {
            metadata: Some(ObjectMeta {
                labels: Some(self.merge_labels(labels)),
                annotations: self.cluster.spec.pod_annotations.clone(),
                ..Default::default()
            }),
            spec: Some(PodSpec {
                containers: vec![container],
                volumes: Some(self.volumes()),
                service_account_name: Some(format!("{}-search", self.cluster.metadata.name.as_ref().unwrap())),
                security_context: self.pod_security_context(),
                node_selector: self.cluster.spec.node_selector.clone(),
                tolerations: self.tolerations(),
                affinity: self.affinity(),
                image_pull_secrets: self.image_pull_secrets(),
                ..Default::default()
            }),
        }
    }
    
    fn controller_container(&self) -> Container {
        Container {
            name: "controller".to_string(),
            image: Some(
                self.cluster.spec.image.as_ref()
                    .map(|i| i.controller.clone())
                    .unwrap_or_else(|| "chronik/controller:latest".to_string())
            ),
            image_pull_policy: self.cluster.spec.image.as_ref()
                .and_then(|i| i.pull_policy.clone()),
            ports: Some(vec![
                ContainerPort {
                    container_port: CONTROLLER_PORT,
                    name: Some("grpc".to_string()),
                    protocol: Some("TCP".to_string()),
                },
                ContainerPort {
                    container_port: METRICS_PORT,
                    name: Some("metrics".to_string()),
                    protocol: Some("TCP".to_string()),
                },
                ContainerPort {
                    container_port: HEALTH_PORT,
                    name: Some("health".to_string()),
                    protocol: Some("TCP".to_string()),
                },
            ]),
            env: Some(self.controller_env()),
            env_from: Some(vec![]),
            resources: self.cluster.spec.resources.as_ref().map(|r| {
                self.build_k8s_resources(&r.controller)
            }),
            volume_mounts: Some(self.volume_mounts()),
            liveness_probe: Some(self.liveness_probe()),
            readiness_probe: Some(self.readiness_probe()),
            security_context: self.container_security_context(),
            ..Default::default()
        }
    }
    
    fn ingest_container(&self) -> Container {
        Container {
            name: "ingest".to_string(),
            image: Some(
                self.cluster.spec.image.as_ref()
                    .map(|i| i.ingest.clone())
                    .unwrap_or_else(|| "chronik/ingest:latest".to_string())
            ),
            image_pull_policy: self.cluster.spec.image.as_ref()
                .and_then(|i| i.pull_policy.clone()),
            ports: Some(vec![
                ContainerPort {
                    container_port: INGEST_PORT,
                    name: Some("kafka".to_string()),
                    protocol: Some("TCP".to_string()),
                },
                ContainerPort {
                    container_port: METRICS_PORT,
                    name: Some("metrics".to_string()),
                    protocol: Some("TCP".to_string()),
                },
                ContainerPort {
                    container_port: HEALTH_PORT,
                    name: Some("health".to_string()),
                    protocol: Some("TCP".to_string()),
                },
            ]),
            env: Some(self.ingest_env()),
            env_from: Some(vec![]),
            resources: self.cluster.spec.resources.as_ref().map(|r| {
                self.build_k8s_resources(&r.ingest)
            }),
            volume_mounts: Some(self.volume_mounts()),
            liveness_probe: Some(self.liveness_probe()),
            readiness_probe: Some(self.readiness_probe()),
            security_context: self.container_security_context(),
            ..Default::default()
        }
    }
    
    fn search_container(&self) -> Container {
        Container {
            name: "search".to_string(),
            image: Some(
                self.cluster.spec.image.as_ref()
                    .and_then(|i| i.search.clone())
                    .unwrap_or_else(|| "chronik/search:latest".to_string())
            ),
            image_pull_policy: self.cluster.spec.image.as_ref()
                .and_then(|i| i.pull_policy.clone()),
            ports: Some(vec![
                ContainerPort {
                    container_port: SEARCH_PORT,
                    name: Some("grpc".to_string()),
                    protocol: Some("TCP".to_string()),
                },
                ContainerPort {
                    container_port: METRICS_PORT,
                    name: Some("metrics".to_string()),
                    protocol: Some("TCP".to_string()),
                },
                ContainerPort {
                    container_port: HEALTH_PORT,
                    name: Some("health".to_string()),
                    protocol: Some("TCP".to_string()),
                },
            ]),
            env: Some(self.search_env()),
            env_from: Some(vec![]),
            resources: self.cluster.spec.resources.as_ref()
                .and_then(|r| r.search.as_ref())
                .map(|r| self.build_k8s_resources(r)),
            volume_mounts: Some(self.volume_mounts()),
            liveness_probe: Some(self.liveness_probe()),
            readiness_probe: Some(self.readiness_probe()),
            security_context: self.container_security_context(),
            ..Default::default()
        }
    }
    
    fn controller_env(&self) -> Vec<EnvVar> {
        let mut env = vec![
            EnvVar {
                name: "CHRONIK_ROLE".to_string(),
                value: Some("controller".to_string()),
                ..Default::default()
            },
            EnvVar {
                name: "CHRONIK_CLUSTER_NAME".to_string(),
                value: Some(self.cluster.metadata.name.as_ref().unwrap().clone()),
                ..Default::default()
            },
            EnvVar {
                name: "CHRONIK_NAMESPACE".to_string(),
                value: Some(self.cluster.metadata.namespace.as_ref().unwrap().clone()),
                ..Default::default()
            },
        ];
        
        // Add config map references
        env.extend(self.config_map_env());
        
        // Add secret references
        env.extend(self.secret_env());
        
        env
    }
    
    fn ingest_env(&self) -> Vec<EnvVar> {
        let mut env = vec![
            EnvVar {
                name: "CHRONIK_ROLE".to_string(),
                value: Some("ingest".to_string()),
                ..Default::default()
            },
            EnvVar {
                name: "CHRONIK_CLUSTER_NAME".to_string(),
                value: Some(self.cluster.metadata.name.as_ref().unwrap().clone()),
                ..Default::default()
            },
            EnvVar {
                name: "CHRONIK_NAMESPACE".to_string(),
                value: Some(self.cluster.metadata.namespace.as_ref().unwrap().clone()),
                ..Default::default()
            },
            EnvVar {
                name: "CHRONIK_CONTROLLER_SERVICE".to_string(),
                value: Some(format!(
                    "{}-controller.{}.svc.cluster.local:{}",
                    self.cluster.metadata.name.as_ref().unwrap(),
                    self.cluster.metadata.namespace.as_ref().unwrap(),
                    CONTROLLER_PORT
                )),
                ..Default::default()
            },
        ];
        
        // Add config map references
        env.extend(self.config_map_env());
        
        // Add secret references
        env.extend(self.secret_env());
        
        env
    }
    
    fn search_env(&self) -> Vec<EnvVar> {
        let mut env = vec![
            EnvVar {
                name: "CHRONIK_ROLE".to_string(),
                value: Some("search".to_string()),
                ..Default::default()
            },
            EnvVar {
                name: "CHRONIK_CLUSTER_NAME".to_string(),
                value: Some(self.cluster.metadata.name.as_ref().unwrap().clone()),
                ..Default::default()
            },
            EnvVar {
                name: "CHRONIK_NAMESPACE".to_string(),
                value: Some(self.cluster.metadata.namespace.as_ref().unwrap().clone()),
                ..Default::default()
            },
            EnvVar {
                name: "CHRONIK_CONTROLLER_SERVICE".to_string(),
                value: Some(format!(
                    "{}-controller.{}.svc.cluster.local:{}",
                    self.cluster.metadata.name.as_ref().unwrap(),
                    self.cluster.metadata.namespace.as_ref().unwrap(),
                    CONTROLLER_PORT
                )),
                ..Default::default()
            },
        ];
        
        // Add config map references
        env.extend(self.config_map_env());
        
        // Add secret references
        env.extend(self.secret_env());
        
        env
    }
    
    fn config_map_env(&self) -> Vec<EnvVar> {
        vec![
            // Reference config values from ConfigMap
            EnvVar {
                name: "CHRONIK_CONFIG_PATH".to_string(),
                value: Some("/etc/chronik/config".to_string()),
                ..Default::default()
            },
        ]
    }
    
    fn secret_env(&self) -> Vec<EnvVar> {
        vec![
            // Database credentials from secret
            EnvVar {
                name: "DATABASE_USER".to_string(),
                value_from: Some(EnvVarSource {
                    secret_key_ref: Some(SecretKeySelector {
                        name: Some(self.cluster.spec.metastore.connection.credentials_secret.clone()),
                        key: "username".to_string(),
                        optional: Some(false),
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            },
            EnvVar {
                name: "DATABASE_PASSWORD".to_string(),
                value_from: Some(EnvVarSource {
                    secret_key_ref: Some(SecretKeySelector {
                        name: Some(self.cluster.spec.metastore.connection.credentials_secret.clone()),
                        key: "password".to_string(),
                        optional: Some(false),
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            },
        ]
    }
    
    fn volumes(&self) -> Vec<Volume> {
        let mut volumes = vec![
            // Config volume
            Volume {
                name: "config".to_string(),
                config_map: Some(k8s_openapi::api::core::v1::ConfigMapVolumeSource {
                    name: Some(format!("{}-config", self.cluster.metadata.name.as_ref().unwrap())),
                    ..Default::default()
                }),
                ..Default::default()
            },
            // Temp volume
            Volume {
                name: "temp".to_string(),
                empty_dir: Some(EmptyDirVolumeSource {
                    ..Default::default()
                }),
                ..Default::default()
            },
        ];
        
        // Add TLS volume if enabled
        if let Some(security) = &self.cluster.spec.security {
            if security.tls_enabled {
                if let Some(tls_secret) = &security.tls_secret {
                    volumes.push(Volume {
                        name: "tls".to_string(),
                        secret: Some(k8s_openapi::api::core::v1::SecretVolumeSource {
                            secret_name: Some(tls_secret.clone()),
                            ..Default::default()
                        }),
                        ..Default::default()
                    });
                }
            }
        }
        
        volumes
    }
    
    fn volume_mounts(&self) -> Vec<VolumeMount> {
        let mut mounts = vec![
            VolumeMount {
                name: "config".to_string(),
                mount_path: "/etc/chronik/config".to_string(),
                read_only: Some(true),
                ..Default::default()
            },
            VolumeMount {
                name: "data".to_string(),
                mount_path: "/var/lib/chronik".to_string(),
                ..Default::default()
            },
            VolumeMount {
                name: "temp".to_string(),
                mount_path: "/tmp".to_string(),
                ..Default::default()
            },
        ];
        
        // Add TLS mount if enabled
        if let Some(security) = &self.cluster.spec.security {
            if security.tls_enabled && security.tls_secret.is_some() {
                mounts.push(VolumeMount {
                    name: "tls".to_string(),
                    mount_path: "/etc/chronik/tls".to_string(),
                    read_only: Some(true),
                    ..Default::default()
                });
            }
        }
        
        mounts
    }
    
    fn volume_claim_templates(&self) -> Vec<PersistentVolumeClaim> {
        vec![
            PersistentVolumeClaim {
                metadata: ObjectMeta {
                    name: Some("data".to_string()),
                    ..Default::default()
                },
                spec: Some(PersistentVolumeClaimSpec {
                    access_modes: Some(vec!["ReadWriteOnce".to_string()]),
                    storage_class_name: self.cluster.spec.storage.storage_class.clone(),
                    resources: Some(K8sResourceRequirements {
                        requests: Some({
                            let mut requests = BTreeMap::new();
                            requests.insert("storage".to_string(), Quantity(self.cluster.spec.storage.size.clone()));
                            requests
                        }),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            },
        ]
    }
    
    fn liveness_probe(&self) -> Probe {
        Probe {
            http_get: Some(HTTPGetAction {
                path: Some("/healthz".to_string()),
                port: IntOrString::Int(HEALTH_PORT),
                ..Default::default()
            }),
            initial_delay_seconds: Some(30),
            period_seconds: Some(10),
            timeout_seconds: Some(5),
            failure_threshold: Some(3),
            ..Default::default()
        }
    }
    
    fn readiness_probe(&self) -> Probe {
        Probe {
            http_get: Some(HTTPGetAction {
                path: Some("/readyz".to_string()),
                port: IntOrString::Int(HEALTH_PORT),
                ..Default::default()
            }),
            initial_delay_seconds: Some(10),
            period_seconds: Some(5),
            timeout_seconds: Some(3),
            failure_threshold: Some(3),
            ..Default::default()
        }
    }
    
    fn pod_security_context(&self) -> Option<K8sPodSecurityContext> {
        self.cluster.spec.security.as_ref()
            .and_then(|s| s.pod_security_context.as_ref())
            .map(|psc| K8sPodSecurityContext {
                run_as_user: psc.run_as_user,
                run_as_group: psc.run_as_group,
                fs_group: psc.fs_group,
                run_as_non_root: psc.run_as_non_root,
                ..Default::default()
            })
    }
    
    fn container_security_context(&self) -> Option<K8sSecurityContext> {
        self.cluster.spec.security.as_ref()
            .and_then(|s| s.container_security_context.as_ref())
            .map(|csc| K8sSecurityContext {
                allow_privilege_escalation: csc.allow_privilege_escalation,
                privileged: csc.privileged,
                read_only_root_filesystem: csc.read_only_root_filesystem,
                run_as_non_root: csc.run_as_non_root,
                run_as_user: csc.run_as_user,
                ..Default::default()
            })
    }
    
    fn image_pull_secrets(&self) -> Option<Vec<k8s_openapi::api::core::v1::LocalObjectReference>> {
        self.cluster.spec.image.as_ref()
            .and_then(|i| i.pull_secrets.as_ref())
            .map(|secrets| {
                secrets.iter()
                    .map(|s| k8s_openapi::api::core::v1::LocalObjectReference {
                        name: Some(s.clone()),
                    })
                    .collect()
            })
    }
    
    fn tolerations(&self) -> Option<Vec<k8s_openapi::api::core::v1::Toleration>> {
        self.cluster.spec.tolerations.as_ref()
            .map(|tolerations| {
                tolerations.iter()
                    .map(|t| k8s_openapi::api::core::v1::Toleration {
                        key: t.key.clone(),
                        operator: t.operator.clone(),
                        value: t.value.clone(),
                        effect: t.effect.clone(),
                        toleration_seconds: t.toleration_seconds,
                    })
                    .collect()
            })
    }
    
    fn affinity(&self) -> Option<k8s_openapi::api::core::v1::Affinity> {
        self.cluster.spec.affinity.as_ref()
            .map(|_| {
                // For production use, you would deserialize the serde_json::Value into proper affinity structs
                // For now, we'll create a simple pod anti-affinity rule
                k8s_openapi::api::core::v1::Affinity {
                    pod_anti_affinity: Some(k8s_openapi::api::core::v1::PodAntiAffinity {
                        preferred_during_scheduling_ignored_during_execution: Some(vec![
                            k8s_openapi::api::core::v1::WeightedPodAffinityTerm {
                                weight: 100,
                                pod_affinity_term: k8s_openapi::api::core::v1::PodAffinityTerm {
                                    label_selector: Some(LabelSelector {
                                        match_labels: Some(self.common_labels()),
                                        ..Default::default()
                                    }),
                                    topology_key: "kubernetes.io/hostname".to_string(),
                                    ..Default::default()
                                },
                            }
                        ]),
                        ..Default::default()
                    }),
                    ..Default::default()
                }
            })
    }
    
    fn merge_labels(&self, mut labels: BTreeMap<String, String>) -> BTreeMap<String, String> {
        if let Some(pod_labels) = &self.cluster.spec.pod_labels {
            labels.extend(pod_labels.clone());
        }
        labels
    }
    
    fn build_k8s_resources(&self, spec: &crate::crd::ResourceSpec) -> K8sResourceRequirements {
        let mut requests = BTreeMap::new();
        let mut limits = BTreeMap::new();
        
        requests.insert("cpu".to_string(), Quantity(spec.cpu.clone()));
        requests.insert("memory".to_string(), Quantity(spec.memory.clone()));
        
        if let Some(cpu_limit) = &spec.cpu_limit {
            limits.insert("cpu".to_string(), Quantity(cpu_limit.clone()));
        }
        if let Some(memory_limit) = &spec.memory_limit {
            limits.insert("memory".to_string(), Quantity(memory_limit.clone()));
        }
        
        K8sResourceRequirements {
            requests: Some(requests),
            limits: if limits.is_empty() { None } else { Some(limits) },
            claims: None,
        }
    }
}