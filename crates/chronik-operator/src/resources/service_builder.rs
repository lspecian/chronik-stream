use std::collections::BTreeMap;

use k8s_openapi::api::core::v1::{Service, ServicePort, ServiceSpec};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, OwnerReference};
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;

use crate::constants;

/// Build a Service for a standalone Chronik instance.
#[allow(clippy::too_many_arguments)]
pub fn build_standalone_service(
    name: &str,
    namespace: &str,
    kafka_port: i32,
    unified_api_port: i32,
    service_type: &str,
    labels: BTreeMap<String, String>,
    annotations: Option<BTreeMap<String, String>>,
    owner_ref: k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference,
) -> Service {
    let ports = vec![
        ServicePort {
            name: Some("kafka".into()),
            port: kafka_port,
            target_port: Some(IntOrString::Int(kafka_port)),
            ..Default::default()
        },
        ServicePort {
            name: Some("unified-api".into()),
            port: unified_api_port,
            target_port: Some(IntOrString::Int(unified_api_port)),
            ..Default::default()
        },
    ];

    Service {
        metadata: ObjectMeta {
            name: Some(name.into()),
            namespace: Some(namespace.into()),
            labels: Some(labels.clone()),
            annotations,
            owner_references: Some(vec![owner_ref]),
            ..Default::default()
        },
        spec: Some(ServiceSpec {
            type_: Some(service_type.into()),
            selector: Some(labels),
            ports: Some(ports),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// Build a headless Service for a cluster (used for stable DNS names).
///
/// A headless Service (clusterIP: None) lets each Pod get its own DNS record:
/// `{pod-name}.{service-name}.{namespace}.svc.cluster.local`
#[allow(clippy::too_many_arguments)]
pub fn build_headless_service(
    cluster_name: &str,
    namespace: &str,
    kafka_port: i32,
    wal_port: i32,
    raft_port: i32,
    unified_api_port: i32,
    selector_labels: BTreeMap<String, String>,
    all_labels: BTreeMap<String, String>,
    owner_ref: OwnerReference,
) -> Service {
    let svc_name = format!("{cluster_name}-headless");

    let ports = vec![
        ServicePort {
            name: Some("kafka".into()),
            port: kafka_port,
            target_port: Some(IntOrString::Int(kafka_port)),
            ..Default::default()
        },
        ServicePort {
            name: Some("wal".into()),
            port: wal_port,
            target_port: Some(IntOrString::Int(wal_port)),
            ..Default::default()
        },
        ServicePort {
            name: Some("raft".into()),
            port: raft_port,
            target_port: Some(IntOrString::Int(raft_port)),
            ..Default::default()
        },
        ServicePort {
            name: Some("unified-api".into()),
            port: unified_api_port,
            target_port: Some(IntOrString::Int(unified_api_port)),
            ..Default::default()
        },
    ];

    Service {
        metadata: ObjectMeta {
            name: Some(svc_name),
            namespace: Some(namespace.into()),
            labels: Some(all_labels),
            owner_references: Some(vec![owner_ref]),
            ..Default::default()
        },
        spec: Some(ServiceSpec {
            type_: Some("ClusterIP".into()),
            cluster_ip: Some("None".into()),
            selector: Some(selector_labels),
            ports: Some(ports),
            publish_not_ready_addresses: Some(true),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// Build a per-node Service for a cluster node.
///
/// Each cluster node gets its own Service so it has a stable DNS name:
/// `{cluster_name}-{node_id}.{namespace}.svc.cluster.local`
#[allow(clippy::too_many_arguments)]
pub fn build_cluster_node_service(
    cluster_name: &str,
    namespace: &str,
    node_id: u64,
    kafka_port: i32,
    wal_port: i32,
    raft_port: i32,
    unified_api_port: i32,
    all_labels: BTreeMap<String, String>,
    owner_ref: OwnerReference,
) -> Service {
    let svc_name = format!("{cluster_name}-{node_id}");
    let admin_port = constants::ports::ADMIN_API_BASE + node_id as i32;

    // Selector must match exactly this node's pod
    let mut selector = BTreeMap::new();
    selector.insert(
        crate::constants::labels::NAME.into(),
        crate::constants::values::APP_NAME.into(),
    );
    selector.insert(
        crate::constants::labels::INSTANCE.into(),
        cluster_name.into(),
    );
    selector.insert(
        crate::constants::labels::NODE_ID.into(),
        node_id.to_string(),
    );

    let ports = vec![
        ServicePort {
            name: Some("kafka".into()),
            port: kafka_port,
            target_port: Some(IntOrString::Int(kafka_port)),
            ..Default::default()
        },
        ServicePort {
            name: Some("wal".into()),
            port: wal_port,
            target_port: Some(IntOrString::Int(wal_port)),
            ..Default::default()
        },
        ServicePort {
            name: Some("raft".into()),
            port: raft_port,
            target_port: Some(IntOrString::Int(raft_port)),
            ..Default::default()
        },
        ServicePort {
            name: Some("unified-api".into()),
            port: unified_api_port,
            target_port: Some(IntOrString::Int(unified_api_port)),
            ..Default::default()
        },
        ServicePort {
            name: Some("admin".into()),
            port: admin_port,
            target_port: Some(IntOrString::Int(admin_port)),
            ..Default::default()
        },
    ];

    Service {
        metadata: ObjectMeta {
            name: Some(svc_name),
            namespace: Some(namespace.into()),
            labels: Some(all_labels),
            owner_references: Some(vec![owner_ref]),
            ..Default::default()
        },
        spec: Some(ServiceSpec {
            type_: Some("ClusterIP".into()),
            selector: Some(selector),
            ports: Some(ports),
            ..Default::default()
        }),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::constants::{labels as lbl, values};

    fn test_labels() -> BTreeMap<String, String> {
        BTreeMap::from([
            (lbl::NAME.into(), values::APP_NAME.into()),
            (lbl::INSTANCE.into(), "my-svc".into()),
        ])
    }

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
    fn test_build_standalone_service_clusterip() {
        let svc = build_standalone_service(
            "my-chronik",
            "default",
            9092,
            6092,
            "ClusterIP",
            test_labels(),
            None,
            test_owner_ref(),
        );

        assert_eq!(svc.metadata.name.as_deref(), Some("my-chronik"));
        let spec = svc.spec.as_ref().unwrap();
        assert_eq!(spec.type_.as_deref(), Some("ClusterIP"));
        let ports = spec.ports.as_ref().unwrap();
        assert_eq!(ports.len(), 2);
        assert_eq!(ports[0].port, 9092);
        assert_eq!(ports[1].port, 6092);
    }

    #[test]
    fn test_build_standalone_service_nodeport() {
        let svc = build_standalone_service(
            "test",
            "ns",
            9092,
            6092,
            "NodePort",
            test_labels(),
            None,
            test_owner_ref(),
        );
        assert_eq!(
            svc.spec.as_ref().unwrap().type_.as_deref(),
            Some("NodePort")
        );
    }

    #[test]
    fn test_service_has_annotations() {
        let mut annotations = BTreeMap::new();
        annotations.insert("foo".into(), "bar".into());
        let svc = build_standalone_service(
            "test",
            "ns",
            9092,
            6092,
            "ClusterIP",
            test_labels(),
            Some(annotations),
            test_owner_ref(),
        );
        assert_eq!(
            svc.metadata
                .annotations
                .as_ref()
                .unwrap()
                .get("foo")
                .unwrap(),
            "bar"
        );
    }

    fn cluster_owner_ref() -> k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference {
        k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference {
            api_version: "chronik.io/v1alpha1".into(),
            kind: "ChronikCluster".into(),
            name: "prod".into(),
            uid: "uid-123".into(),
            controller: Some(true),
            block_owner_deletion: Some(true),
        }
    }

    #[test]
    fn test_build_headless_service() {
        let svc = build_headless_service(
            "prod",
            "default",
            9092,
            9291,
            5001,
            6092,
            test_labels(),
            test_labels(),
            cluster_owner_ref(),
        );

        assert_eq!(svc.metadata.name.as_deref(), Some("prod-headless"));
        let spec = svc.spec.as_ref().unwrap();
        assert_eq!(spec.cluster_ip.as_deref(), Some("None"));
        assert_eq!(spec.publish_not_ready_addresses, Some(true));

        let ports = spec.ports.as_ref().unwrap();
        assert_eq!(ports.len(), 4);
        let port_names: Vec<_> = ports.iter().filter_map(|p| p.name.as_deref()).collect();
        assert!(port_names.contains(&"kafka"));
        assert!(port_names.contains(&"wal"));
        assert!(port_names.contains(&"raft"));
    }

    #[test]
    fn test_build_cluster_node_service() {
        let svc = build_cluster_node_service(
            "prod",
            "default",
            2,
            9092,
            9291,
            5001,
            6092,
            test_labels(),
            cluster_owner_ref(),
        );

        assert_eq!(svc.metadata.name.as_deref(), Some("prod-2"));
        let spec = svc.spec.as_ref().unwrap();
        assert_eq!(spec.type_.as_deref(), Some("ClusterIP"));

        // Check selector targets specific node
        let selector = spec.selector.as_ref().unwrap();
        assert_eq!(selector.get(lbl::NODE_ID).unwrap(), "2");

        // Has admin port
        let ports = spec.ports.as_ref().unwrap();
        let admin = ports
            .iter()
            .find(|p| p.name.as_deref() == Some("admin"))
            .unwrap();
        assert_eq!(admin.port, 10002); // ADMIN_API_BASE + node_id
    }
}
