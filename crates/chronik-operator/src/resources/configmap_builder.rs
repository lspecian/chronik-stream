use std::collections::BTreeMap;

use k8s_openapi::api::core::v1::ConfigMap;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;

/// Build a ConfigMap containing environment overrides for a standalone instance.
///
/// The ConfigMap holds supplementary config data. The primary configuration is
/// passed via environment variables directly on the Pod. This ConfigMap is
/// available for any extra config files that may be mounted.
pub fn build_standalone_configmap(
    name: &str,
    namespace: &str,
    labels: BTreeMap<String, String>,
    owner_ref: k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference,
) -> ConfigMap {
    let cm_name = format!("{name}-config");

    ConfigMap {
        metadata: ObjectMeta {
            name: Some(cm_name),
            namespace: Some(namespace.into()),
            labels: Some(labels),
            owner_references: Some(vec![owner_ref]),
            ..Default::default()
        },
        data: Some(BTreeMap::new()),
        ..Default::default()
    }
}

/// Build a ConfigMap containing the TOML cluster config for a specific node.
///
/// This ConfigMap is mounted into the Pod at `/config/cluster.toml` and
/// contains the node-specific cluster configuration.
pub fn build_cluster_node_configmap(
    cluster_name: &str,
    namespace: &str,
    node_id: u64,
    toml_config: &str,
    labels: BTreeMap<String, String>,
    owner_ref: k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference,
) -> ConfigMap {
    let cm_name = format!("{cluster_name}-{node_id}-config");

    let mut data = BTreeMap::new();
    data.insert("cluster.toml".into(), toml_config.to_string());

    ConfigMap {
        metadata: ObjectMeta {
            name: Some(cm_name),
            namespace: Some(namespace.into()),
            labels: Some(labels),
            owner_references: Some(vec![owner_ref]),
            ..Default::default()
        },
        data: Some(data),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn test_build_standalone_configmap() {
        let cm =
            build_standalone_configmap("my-chronik", "default", BTreeMap::new(), test_owner_ref());
        assert_eq!(cm.metadata.name.as_deref(), Some("my-chronik-config"));
        assert_eq!(cm.metadata.namespace.as_deref(), Some("default"));
        assert!(cm.data.as_ref().unwrap().is_empty());
    }

    #[test]
    fn test_build_cluster_node_configmap() {
        let toml = "enabled = true\nnode_id = 1\n";
        let cm = build_cluster_node_configmap(
            "prod",
            "default",
            1,
            toml,
            BTreeMap::new(),
            test_owner_ref(),
        );

        assert_eq!(cm.metadata.name.as_deref(), Some("prod-1-config"));
        let data = cm.data.as_ref().unwrap();
        assert!(data.contains_key("cluster.toml"));
        assert!(data["cluster.toml"].contains("node_id = 1"));
    }
}
