use std::collections::BTreeMap;

use k8s_openapi::api::core::v1::{
    Container, ContainerPort, EnvVar, LocalObjectReference, PersistentVolumeClaimVolumeSource, Pod,
    PodSpec, Probe, ResourceRequirements, TCPSocketAction, Toleration, Volume, VolumeMount,
};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;

use crate::constants::{self, labels, values};
use crate::crds::standalone::ChronikStandaloneSpec;

/// Build a Pod for a ChronikStandalone instance.
pub fn build_standalone_pod(
    name: &str,
    namespace: &str,
    spec: &ChronikStandaloneSpec,
    owner_ref: k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference,
) -> Pod {
    let labels = standalone_labels(name, &spec.image);
    let pvc_name = format!("{name}-data");
    let svc_dns = format!("{name}.{namespace}.svc.cluster.local");

    let mut env_vars = vec![
        EnvVar {
            name: "CHRONIK_ADVERTISE".into(),
            value: Some(svc_dns),
            ..Default::default()
        },
        EnvVar {
            name: "CHRONIK_KAFKA_PORT".into(),
            value: Some(spec.kafka_port.to_string()),
            ..Default::default()
        },
        EnvVar {
            name: "CHRONIK_UNIFIED_API_PORT".into(),
            value: Some(spec.unified_api_port.to_string()),
            ..Default::default()
        },
        EnvVar {
            name: "CHRONIK_WAL_PROFILE".into(),
            value: Some(spec.wal_profile.clone()),
            ..Default::default()
        },
        EnvVar {
            name: "CHRONIK_PRODUCE_PROFILE".into(),
            value: Some(spec.produce_profile.clone()),
            ..Default::default()
        },
        EnvVar {
            name: "RUST_LOG".into(),
            value: Some(spec.log_level.clone()),
            ..Default::default()
        },
    ];

    if spec.columnar_enabled {
        env_vars.push(EnvVar {
            name: "CHRONIK_HOT_BUFFER_ENABLED".into(),
            value: Some("true".into()),
            ..Default::default()
        });
    }

    // Object store env vars
    if let Some(ref os) = spec.object_store {
        env_vars.push(EnvVar {
            name: "S3_BUCKET".into(),
            value: os.bucket.clone(),
            ..Default::default()
        });
        if let Some(ref region) = os.region {
            env_vars.push(EnvVar {
                name: "S3_REGION".into(),
                value: Some(region.clone()),
                ..Default::default()
            });
        }
        if let Some(ref endpoint) = os.endpoint {
            env_vars.push(EnvVar {
                name: "S3_ENDPOINT".into(),
                value: Some(endpoint.clone()),
                ..Default::default()
            });
        }
    }

    // Vector search env vars
    if let Some(ref vs) = spec.vector_search {
        if vs.enabled {
            env_vars.push(EnvVar {
                name: "CHRONIK_EMBEDDING_PROVIDER".into(),
                value: Some(vs.provider.clone()),
                ..Default::default()
            });
            if let Some(ref secret_ref) = vs.api_key_secret {
                env_vars.push(EnvVar {
                    name: "OPENAI_API_KEY".into(),
                    value_from: Some(k8s_openapi::api::core::v1::EnvVarSource {
                        secret_key_ref: Some(k8s_openapi::api::core::v1::SecretKeySelector {
                            name: Some(secret_ref.name.clone()),
                            key: secret_ref.key.clone(),
                            ..Default::default()
                        }),
                        ..Default::default()
                    }),
                    ..Default::default()
                });
            }
        }
    }

    // User-defined env vars
    for user_env in &spec.env {
        if let Some(ref val) = user_env.value {
            env_vars.push(EnvVar {
                name: user_env.name.clone(),
                value: Some(val.clone()),
                ..Default::default()
            });
        } else if let Some(ref secret_ref) = user_env.value_from_secret {
            env_vars.push(EnvVar {
                name: user_env.name.clone(),
                value_from: Some(k8s_openapi::api::core::v1::EnvVarSource {
                    secret_key_ref: Some(k8s_openapi::api::core::v1::SecretKeySelector {
                        name: Some(secret_ref.name.clone()),
                        key: secret_ref.key.clone(),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            });
        }
    }

    let mut ports = vec![
        ContainerPort {
            name: Some("kafka".into()),
            container_port: spec.kafka_port,
            ..Default::default()
        },
        ContainerPort {
            name: Some("unified-api".into()),
            container_port: spec.unified_api_port,
            ..Default::default()
        },
    ];

    if let Some(metrics_port) = spec.metrics_port {
        ports.push(ContainerPort {
            name: Some("metrics".into()),
            container_port: metrics_port,
            ..Default::default()
        });
    }

    // Resource requirements
    let resources = spec.resources.as_ref().map(|r| {
        let mut k8s_resources = ResourceRequirements::default();
        if let Some(ref req) = r.requests {
            let mut map = BTreeMap::new();
            if let Some(ref cpu) = req.cpu {
                map.insert("cpu".into(), Quantity(cpu.clone()));
            }
            if let Some(ref mem) = req.memory {
                map.insert("memory".into(), Quantity(mem.clone()));
            }
            k8s_resources.requests = Some(map);
        }
        if let Some(ref lim) = r.limits {
            let mut map = BTreeMap::new();
            if let Some(ref cpu) = lim.cpu {
                map.insert("cpu".into(), Quantity(cpu.clone()));
            }
            if let Some(ref mem) = lim.memory {
                map.insert("memory".into(), Quantity(mem.clone()));
            }
            k8s_resources.limits = Some(map);
        }
        k8s_resources
    });

    let liveness_probe = Probe {
        tcp_socket: Some(TCPSocketAction {
            port: IntOrString::Int(spec.kafka_port),
            ..Default::default()
        }),
        initial_delay_seconds: Some(15),
        period_seconds: Some(10),
        timeout_seconds: Some(5),
        failure_threshold: Some(3),
        ..Default::default()
    };

    let readiness_probe = Probe {
        tcp_socket: Some(TCPSocketAction {
            port: IntOrString::Int(spec.kafka_port),
            ..Default::default()
        }),
        initial_delay_seconds: Some(10),
        period_seconds: Some(5),
        timeout_seconds: Some(3),
        failure_threshold: Some(3),
        ..Default::default()
    };

    let container = Container {
        name: "chronik-server".into(),
        image: Some(spec.image.clone()),
        image_pull_policy: Some(spec.image_pull_policy.clone()),
        command: Some(vec!["chronik-server".into()]),
        args: Some(vec![
            "start".into(),
            "--data-dir".into(),
            constants::defaults::DATA_DIR.into(),
        ]),
        ports: Some(ports),
        env: Some(env_vars),
        resources,
        liveness_probe: Some(liveness_probe),
        readiness_probe: Some(readiness_probe),
        volume_mounts: Some(vec![VolumeMount {
            name: "data".into(),
            mount_path: constants::defaults::DATA_DIR.into(),
            ..Default::default()
        }]),
        ..Default::default()
    };

    let tolerations: Option<Vec<Toleration>> = if spec.tolerations.is_empty() {
        None
    } else {
        Some(
            spec.tolerations
                .iter()
                .map(|t| Toleration {
                    key: t.key.clone(),
                    operator: t.operator.clone(),
                    value: t.value.clone(),
                    effect: t.effect.clone(),
                    toleration_seconds: t.toleration_seconds,
                })
                .collect(),
        )
    };

    let image_pull_secrets: Option<Vec<LocalObjectReference>> =
        if spec.image_pull_secrets.is_empty() {
            None
        } else {
            Some(
                spec.image_pull_secrets
                    .iter()
                    .map(|s| LocalObjectReference {
                        name: Some(s.name.clone()),
                    })
                    .collect(),
            )
        };

    Pod {
        metadata: ObjectMeta {
            name: Some(name.into()),
            namespace: Some(namespace.into()),
            labels: Some(labels),
            owner_references: Some(vec![owner_ref]),
            ..Default::default()
        },
        spec: Some(PodSpec {
            containers: vec![container],
            restart_policy: Some("Always".into()),
            node_selector: spec.node_selector.clone(),
            tolerations,
            image_pull_secrets,
            volumes: Some(vec![Volume {
                name: "data".into(),
                persistent_volume_claim: Some(PersistentVolumeClaimVolumeSource {
                    claim_name: pvc_name,
                    ..Default::default()
                }),
                ..Default::default()
            }]),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// Standard labels for standalone resources.
pub fn standalone_labels(name: &str, image: &str) -> BTreeMap<String, String> {
    let version = image.rsplit(':').next().unwrap_or("latest");
    BTreeMap::from([
        (labels::NAME.into(), values::APP_NAME.into()),
        (labels::INSTANCE.into(), name.into()),
        (
            labels::COMPONENT.into(),
            values::COMPONENT_STANDALONE.into(),
        ),
        (labels::MANAGED_BY.into(), values::MANAGED_BY.into()),
        (labels::VERSION.into(), version.into()),
    ])
}

/// Label selector for finding standalone resources.
pub fn standalone_label_selector(name: &str) -> BTreeMap<String, String> {
    BTreeMap::from([
        (labels::NAME.into(), values::APP_NAME.into()),
        (labels::INSTANCE.into(), name.into()),
        (
            labels::COMPONENT.into(),
            values::COMPONENT_STANDALONE.into(),
        ),
        (labels::MANAGED_BY.into(), values::MANAGED_BY.into()),
    ])
}

/// Build a Pod for a cluster node.
pub fn build_cluster_node_pod(
    cluster_name: &str,
    namespace: &str,
    node_id: u64,
    spec: &crate::crds::cluster::ChronikClusterSpec,
    owner_ref: k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference,
) -> Pod {
    let pod_name = format!("{cluster_name}-{node_id}");
    let labels = cluster_node_labels(cluster_name, node_id, &spec.image);
    let pvc_name = format!("{pod_name}-data");
    let cm_name = format!("{pod_name}-config");
    let admin_port = constants::ports::ADMIN_API_BASE + node_id as i32;

    let svc_dns = crate::config_generator::node_dns_name(cluster_name, node_id, namespace);

    let mut env_vars = vec![
        EnvVar {
            name: "CHRONIK_ADVERTISE".into(),
            value: Some(svc_dns),
            ..Default::default()
        },
        EnvVar {
            name: "CHRONIK_WAL_PROFILE".into(),
            value: Some(spec.wal_profile.clone()),
            ..Default::default()
        },
        EnvVar {
            name: "CHRONIK_PRODUCE_PROFILE".into(),
            value: Some(spec.produce_profile.clone()),
            ..Default::default()
        },
        EnvVar {
            name: "RUST_LOG".into(),
            value: Some(spec.log_level.clone()),
            ..Default::default()
        },
    ];

    // Admin API key from secret
    if let Some(ref secret_ref) = spec.admin_api_key_secret {
        env_vars.push(EnvVar {
            name: "CHRONIK_ADMIN_API_KEY".into(),
            value_from: Some(k8s_openapi::api::core::v1::EnvVarSource {
                secret_key_ref: Some(k8s_openapi::api::core::v1::SecretKeySelector {
                    name: Some(secret_ref.name.clone()),
                    key: secret_ref.key.clone(),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        });
    }

    if spec.columnar_enabled {
        env_vars.push(EnvVar {
            name: "CHRONIK_HOT_BUFFER_ENABLED".into(),
            value: Some("true".into()),
            ..Default::default()
        });
    }

    // Object store env vars
    if let Some(ref os) = spec.object_store {
        env_vars.push(EnvVar {
            name: "S3_BUCKET".into(),
            value: os.bucket.clone(),
            ..Default::default()
        });
        if let Some(ref region) = os.region {
            env_vars.push(EnvVar {
                name: "S3_REGION".into(),
                value: Some(region.clone()),
                ..Default::default()
            });
        }
        if let Some(ref endpoint) = os.endpoint {
            env_vars.push(EnvVar {
                name: "S3_ENDPOINT".into(),
                value: Some(endpoint.clone()),
                ..Default::default()
            });
        }
    }

    // Vector search env vars
    if let Some(ref vs) = spec.vector_search {
        if vs.enabled {
            env_vars.push(EnvVar {
                name: "CHRONIK_EMBEDDING_PROVIDER".into(),
                value: Some(vs.provider.clone()),
                ..Default::default()
            });
            if let Some(ref secret_ref) = vs.api_key_secret {
                env_vars.push(EnvVar {
                    name: "OPENAI_API_KEY".into(),
                    value_from: Some(k8s_openapi::api::core::v1::EnvVarSource {
                        secret_key_ref: Some(k8s_openapi::api::core::v1::SecretKeySelector {
                            name: Some(secret_ref.name.clone()),
                            key: secret_ref.key.clone(),
                            ..Default::default()
                        }),
                        ..Default::default()
                    }),
                    ..Default::default()
                });
            }
        }
    }

    // User-defined env vars
    for user_env in &spec.env {
        if let Some(ref val) = user_env.value {
            env_vars.push(EnvVar {
                name: user_env.name.clone(),
                value: Some(val.clone()),
                ..Default::default()
            });
        } else if let Some(ref secret_ref) = user_env.value_from_secret {
            env_vars.push(EnvVar {
                name: user_env.name.clone(),
                value_from: Some(k8s_openapi::api::core::v1::EnvVarSource {
                    secret_key_ref: Some(k8s_openapi::api::core::v1::SecretKeySelector {
                        name: Some(secret_ref.name.clone()),
                        key: secret_ref.key.clone(),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            });
        }
    }

    let kafka_port = spec
        .listeners
        .first()
        .map(|l| l.port)
        .unwrap_or(constants::ports::KAFKA);

    let mut ports = vec![
        ContainerPort {
            name: Some("kafka".into()),
            container_port: kafka_port,
            ..Default::default()
        },
        ContainerPort {
            name: Some("unified-api".into()),
            container_port: spec.unified_api_port,
            ..Default::default()
        },
        ContainerPort {
            name: Some("wal".into()),
            container_port: spec.wal_port,
            ..Default::default()
        },
        ContainerPort {
            name: Some("raft".into()),
            container_port: spec.raft_port,
            ..Default::default()
        },
        ContainerPort {
            name: Some("admin".into()),
            container_port: admin_port,
            ..Default::default()
        },
    ];

    if let Some(metrics_port) = spec.metrics_port {
        ports.push(ContainerPort {
            name: Some("metrics".into()),
            container_port: metrics_port,
            ..Default::default()
        });
    }

    // Resource requirements
    let resources = spec.resources.as_ref().map(|r| {
        let mut k8s_resources = ResourceRequirements::default();
        if let Some(ref req) = r.requests {
            let mut map = BTreeMap::new();
            if let Some(ref cpu) = req.cpu {
                map.insert("cpu".into(), Quantity(cpu.clone()));
            }
            if let Some(ref mem) = req.memory {
                map.insert("memory".into(), Quantity(mem.clone()));
            }
            k8s_resources.requests = Some(map);
        }
        if let Some(ref lim) = r.limits {
            let mut map = BTreeMap::new();
            if let Some(ref cpu) = lim.cpu {
                map.insert("cpu".into(), Quantity(cpu.clone()));
            }
            if let Some(ref mem) = lim.memory {
                map.insert("memory".into(), Quantity(mem.clone()));
            }
            k8s_resources.limits = Some(map);
        }
        k8s_resources
    });

    let liveness_probe = Probe {
        tcp_socket: Some(TCPSocketAction {
            port: IntOrString::Int(kafka_port),
            ..Default::default()
        }),
        initial_delay_seconds: Some(30),
        period_seconds: Some(10),
        timeout_seconds: Some(5),
        failure_threshold: Some(6),
        ..Default::default()
    };

    let readiness_probe = Probe {
        tcp_socket: Some(TCPSocketAction {
            port: IntOrString::Int(kafka_port),
            ..Default::default()
        }),
        initial_delay_seconds: Some(15),
        period_seconds: Some(5),
        timeout_seconds: Some(3),
        failure_threshold: Some(3),
        ..Default::default()
    };

    let container = Container {
        name: "chronik-server".into(),
        image: Some(spec.image.clone()),
        image_pull_policy: Some(spec.image_pull_policy.clone()),
        command: Some(vec!["chronik-server".into()]),
        args: Some(vec![
            "start".into(),
            "--config".into(),
            "/config/cluster.toml".into(),
        ]),
        ports: Some(ports),
        env: Some(env_vars),
        resources,
        liveness_probe: Some(liveness_probe),
        readiness_probe: Some(readiness_probe),
        volume_mounts: Some(vec![
            VolumeMount {
                name: "data".into(),
                mount_path: constants::defaults::DATA_DIR.into(),
                ..Default::default()
            },
            VolumeMount {
                name: "config".into(),
                mount_path: "/config".into(),
                read_only: Some(true),
                ..Default::default()
            },
        ]),
        ..Default::default()
    };

    let tolerations: Option<Vec<Toleration>> = if spec.tolerations.is_empty() {
        None
    } else {
        Some(
            spec.tolerations
                .iter()
                .map(|t| Toleration {
                    key: t.key.clone(),
                    operator: t.operator.clone(),
                    value: t.value.clone(),
                    effect: t.effect.clone(),
                    toleration_seconds: t.toleration_seconds,
                })
                .collect(),
        )
    };

    let image_pull_secrets: Option<Vec<LocalObjectReference>> =
        if spec.image_pull_secrets.is_empty() {
            None
        } else {
            Some(
                spec.image_pull_secrets
                    .iter()
                    .map(|s| LocalObjectReference {
                        name: Some(s.name.clone()),
                    })
                    .collect(),
            )
        };

    // Anti-affinity to spread cluster nodes across K8s nodes
    let affinity = build_anti_affinity(
        cluster_name,
        &spec.anti_affinity,
        &spec.anti_affinity_topology_key,
    );

    Pod {
        metadata: ObjectMeta {
            name: Some(pod_name),
            namespace: Some(namespace.into()),
            labels: Some(labels),
            owner_references: Some(vec![owner_ref]),
            ..Default::default()
        },
        spec: Some(PodSpec {
            containers: vec![container],
            restart_policy: Some("Always".into()),
            node_selector: spec.node_selector.clone(),
            tolerations,
            image_pull_secrets,
            affinity,
            volumes: Some(vec![
                Volume {
                    name: "data".into(),
                    persistent_volume_claim: Some(PersistentVolumeClaimVolumeSource {
                        claim_name: pvc_name,
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                Volume {
                    name: "config".into(),
                    config_map: Some(k8s_openapi::api::core::v1::ConfigMapVolumeSource {
                        name: Some(cm_name),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
            ]),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// Build anti-affinity spec to spread cluster nodes.
fn build_anti_affinity(
    cluster_name: &str,
    mode: &str,
    topology_key: &str,
) -> Option<k8s_openapi::api::core::v1::Affinity> {
    use k8s_openapi::api::core::v1::{
        Affinity, PodAffinityTerm, PodAntiAffinity, WeightedPodAffinityTerm,
    };
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::LabelSelector;

    let selector = LabelSelector {
        match_labels: Some(BTreeMap::from([
            (labels::NAME.into(), values::APP_NAME.to_string()),
            (labels::INSTANCE.into(), cluster_name.to_string()),
            (
                labels::COMPONENT.into(),
                values::COMPONENT_CLUSTER_NODE.to_string(),
            ),
        ])),
        ..Default::default()
    };

    let term = PodAffinityTerm {
        label_selector: Some(selector),
        topology_key: topology_key.to_string(),
        ..Default::default()
    };

    let anti_affinity = if mode == "required" {
        PodAntiAffinity {
            required_during_scheduling_ignored_during_execution: Some(vec![term]),
            ..Default::default()
        }
    } else {
        PodAntiAffinity {
            preferred_during_scheduling_ignored_during_execution: Some(vec![
                WeightedPodAffinityTerm {
                    weight: 100,
                    pod_affinity_term: term,
                },
            ]),
            ..Default::default()
        }
    };

    Some(Affinity {
        pod_anti_affinity: Some(anti_affinity),
        ..Default::default()
    })
}

/// Standard labels for cluster node resources.
pub fn cluster_node_labels(
    cluster_name: &str,
    node_id: u64,
    image: &str,
) -> BTreeMap<String, String> {
    let version = image.rsplit(':').next().unwrap_or("latest");
    BTreeMap::from([
        (labels::NAME.into(), values::APP_NAME.into()),
        (labels::INSTANCE.into(), cluster_name.into()),
        (
            labels::COMPONENT.into(),
            values::COMPONENT_CLUSTER_NODE.into(),
        ),
        (labels::MANAGED_BY.into(), values::MANAGED_BY.into()),
        (labels::VERSION.into(), version.into()),
        (labels::NODE_ID.into(), node_id.to_string()),
        (labels::CLUSTER_NAME.into(), cluster_name.into()),
    ])
}

/// Label selector for all nodes of a cluster (no specific node_id).
pub fn cluster_label_selector(cluster_name: &str) -> BTreeMap<String, String> {
    BTreeMap::from([
        (labels::NAME.into(), values::APP_NAME.into()),
        (labels::INSTANCE.into(), cluster_name.into()),
        (
            labels::COMPONENT.into(),
            values::COMPONENT_CLUSTER_NODE.into(),
        ),
        (labels::MANAGED_BY.into(), values::MANAGED_BY.into()),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crds::standalone::ChronikStandaloneSpec;

    fn test_owner_ref() -> k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference {
        k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference {
            api_version: "chronik.io/v1alpha1".into(),
            kind: "ChronikStandalone".into(),
            name: "test".into(),
            uid: "test-uid".into(),
            controller: Some(true),
            block_owner_deletion: Some(true),
        }
    }

    #[test]
    fn test_build_standalone_pod_basic() {
        let spec: ChronikStandaloneSpec = serde_json::from_str("{}").unwrap();
        let pod = build_standalone_pod("my-chronik", "default", &spec, test_owner_ref());

        assert_eq!(pod.metadata.name.as_deref(), Some("my-chronik"));
        assert_eq!(pod.metadata.namespace.as_deref(), Some("default"));

        let pod_spec = pod.spec.as_ref().unwrap();
        assert_eq!(pod_spec.containers.len(), 1);

        let container = &pod_spec.containers[0];
        assert_eq!(container.name, "chronik-server");
        assert_eq!(
            container.args.as_ref().unwrap(),
            &["start", "--data-dir", "/data"]
        );

        // Check env vars
        let env = container.env.as_ref().unwrap();
        let advertised = env
            .iter()
            .find(|e| e.name == "CHRONIK_ADVERTISE")
            .unwrap();
        assert_eq!(
            advertised.value.as_deref(),
            Some("my-chronik.default.svc.cluster.local")
        );

        let wal_profile = env
            .iter()
            .find(|e| e.name == "CHRONIK_WAL_PROFILE")
            .unwrap();
        assert_eq!(wal_profile.value.as_deref(), Some("medium"));
    }

    #[test]
    fn test_build_standalone_pod_with_resources() {
        let spec: ChronikStandaloneSpec = serde_json::from_str(
            r#"{"resources": {"requests": {"cpu": "500m", "memory": "1Gi"}, "limits": {"cpu": "2", "memory": "4Gi"}}}"#,
        )
        .unwrap();
        let pod = build_standalone_pod("test", "ns", &spec, test_owner_ref());

        let container = &pod.spec.as_ref().unwrap().containers[0];
        let resources = container.resources.as_ref().unwrap();
        let requests = resources.requests.as_ref().unwrap();
        assert_eq!(requests.get("cpu").unwrap().0, "500m");
        assert_eq!(requests.get("memory").unwrap().0, "1Gi");
    }

    #[test]
    fn test_build_standalone_pod_ports() {
        let spec: ChronikStandaloneSpec =
            serde_json::from_str(r#"{"kafkaPort": 9093, "unifiedApiPort": 6093}"#).unwrap();
        let pod = build_standalone_pod("test", "ns", &spec, test_owner_ref());

        let ports = pod.spec.as_ref().unwrap().containers[0]
            .ports
            .as_ref()
            .unwrap();
        assert_eq!(ports.len(), 2);
        assert_eq!(ports[0].container_port, 9093);
        assert_eq!(ports[1].container_port, 6093);
    }

    #[test]
    fn test_standalone_labels() {
        let labels = standalone_labels("my-app", "ghcr.io/chronik-stream/chronik-server:v2.2.23");
        assert_eq!(labels.get(labels::NAME).unwrap(), "chronik-stream");
        assert_eq!(labels.get(labels::INSTANCE).unwrap(), "my-app");
        assert_eq!(labels.get(labels::COMPONENT).unwrap(), "standalone");
        assert_eq!(labels.get(labels::VERSION).unwrap(), "v2.2.23");
    }

    #[test]
    fn test_standalone_pod_has_owner_ref() {
        let spec: ChronikStandaloneSpec = serde_json::from_str("{}").unwrap();
        let pod = build_standalone_pod("test", "ns", &spec, test_owner_ref());
        let refs = pod.metadata.owner_references.as_ref().unwrap();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].kind, "ChronikStandalone");
        assert!(refs[0].controller.unwrap());
    }

    #[test]
    fn test_standalone_pod_probes() {
        let spec: ChronikStandaloneSpec = serde_json::from_str("{}").unwrap();
        let pod = build_standalone_pod("test", "ns", &spec, test_owner_ref());

        let container = &pod.spec.as_ref().unwrap().containers[0];
        let liveness = container.liveness_probe.as_ref().unwrap();
        assert_eq!(liveness.initial_delay_seconds, Some(15));
        assert!(liveness.tcp_socket.is_some());

        let readiness = container.readiness_probe.as_ref().unwrap();
        assert_eq!(readiness.initial_delay_seconds, Some(10));
    }

    #[test]
    fn test_standalone_pod_volume_mount() {
        let spec: ChronikStandaloneSpec = serde_json::from_str("{}").unwrap();
        let pod = build_standalone_pod("test", "ns", &spec, test_owner_ref());

        let container = &pod.spec.as_ref().unwrap().containers[0];
        let mounts = container.volume_mounts.as_ref().unwrap();
        assert_eq!(mounts[0].mount_path, "/data");
        assert_eq!(mounts[0].name, "data");

        let volumes = pod.spec.as_ref().unwrap().volumes.as_ref().unwrap();
        assert_eq!(
            volumes[0]
                .persistent_volume_claim
                .as_ref()
                .unwrap()
                .claim_name,
            "test-data"
        );
    }

    #[test]
    fn test_build_cluster_node_pod() {
        use crate::crds::cluster::ChronikClusterSpec;

        let spec: ChronikClusterSpec = serde_json::from_str("{}").unwrap();
        let owner = k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference {
            api_version: "chronik.io/v1alpha1".into(),
            kind: "ChronikCluster".into(),
            name: "my-cluster".into(),
            uid: "uid-123".into(),
            controller: Some(true),
            block_owner_deletion: Some(true),
        };

        let pod = build_cluster_node_pod("my-cluster", "default", 1, &spec, owner);

        assert_eq!(pod.metadata.name.as_deref(), Some("my-cluster-1"));
        assert_eq!(pod.metadata.namespace.as_deref(), Some("default"));

        let pod_spec = pod.spec.as_ref().unwrap();
        let container = &pod_spec.containers[0];

        // Uses --config for cluster mode
        assert_eq!(
            container.args.as_ref().unwrap(),
            &["start", "--config", "/config/cluster.toml"]
        );

        // Has data + config volume mounts
        let mounts = container.volume_mounts.as_ref().unwrap();
        assert_eq!(mounts.len(), 2);
        assert_eq!(mounts[0].mount_path, "/data");
        assert_eq!(mounts[1].mount_path, "/config");

        // Has WAL, Raft, admin ports
        let ports = container.ports.as_ref().unwrap();
        let port_names: Vec<_> = ports.iter().filter_map(|p| p.name.as_deref()).collect();
        assert!(port_names.contains(&"wal"));
        assert!(port_names.contains(&"raft"));
        assert!(port_names.contains(&"admin"));

        // Admin port = 10000 + node_id
        let admin = ports
            .iter()
            .find(|p| p.name.as_deref() == Some("admin"))
            .unwrap();
        assert_eq!(admin.container_port, 10001);

        // Has anti-affinity
        assert!(pod_spec.affinity.is_some());
    }

    #[test]
    fn test_cluster_node_labels() {
        let labels = cluster_node_labels("prod", 2, "chronik:v2.2.23");
        assert_eq!(labels.get(labels::INSTANCE).unwrap(), "prod");
        assert_eq!(labels.get(labels::COMPONENT).unwrap(), "cluster-node");
        assert_eq!(labels.get(labels::NODE_ID).unwrap(), "2");
        assert_eq!(labels.get(labels::CLUSTER_NAME).unwrap(), "prod");
    }

    #[test]
    fn test_cluster_label_selector() {
        let sel = cluster_label_selector("prod");
        assert_eq!(sel.get(labels::INSTANCE).unwrap(), "prod");
        assert_eq!(sel.get(labels::COMPONENT).unwrap(), "cluster-node");
        assert!(!sel.contains_key(labels::NODE_ID));
    }
}
