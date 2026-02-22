use std::collections::BTreeMap;

use k8s_openapi::api::core::v1::{PersistentVolumeClaim, PersistentVolumeClaimSpec as K8sPvcSpec};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;

use crate::crds::common::StorageSpec;
use crate::crds::defaults;

/// Build a PVC for a standalone Chronik instance.
pub fn build_standalone_pvc(
    name: &str,
    namespace: &str,
    storage: Option<&StorageSpec>,
    labels: BTreeMap<String, String>,
    owner_ref: k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference,
) -> PersistentVolumeClaim {
    let pvc_name = format!("{name}-data");

    let (storage_class, size, access_modes) = match storage {
        Some(s) => (
            s.storage_class.clone(),
            s.size.clone(),
            s.access_modes.clone(),
        ),
        None => (
            defaults::storage_class(),
            defaults::storage_size(),
            defaults::access_modes(),
        ),
    };

    PersistentVolumeClaim {
        metadata: ObjectMeta {
            name: Some(pvc_name),
            namespace: Some(namespace.into()),
            labels: Some(labels),
            owner_references: Some(vec![owner_ref]),
            ..Default::default()
        },
        spec: Some(K8sPvcSpec {
            storage_class_name: Some(storage_class),
            access_modes: Some(access_modes),
            resources: Some(k8s_openapi::api::core::v1::ResourceRequirements {
                requests: Some(BTreeMap::from([("storage".into(), Quantity(size))])),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// Build a PVC for a cluster node.
pub fn build_cluster_node_pvc(
    cluster_name: &str,
    namespace: &str,
    node_id: u64,
    storage: Option<&StorageSpec>,
    labels: BTreeMap<String, String>,
    owner_ref: k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference,
) -> PersistentVolumeClaim {
    let pvc_name = format!("{cluster_name}-{node_id}-data");

    let (storage_class, size, access_modes) = match storage {
        Some(s) => (
            s.storage_class.clone(),
            s.size.clone(),
            s.access_modes.clone(),
        ),
        None => (
            defaults::storage_class(),
            defaults::storage_size(),
            defaults::access_modes(),
        ),
    };

    PersistentVolumeClaim {
        metadata: ObjectMeta {
            name: Some(pvc_name),
            namespace: Some(namespace.into()),
            labels: Some(labels),
            owner_references: Some(vec![owner_ref]),
            ..Default::default()
        },
        spec: Some(K8sPvcSpec {
            storage_class_name: Some(storage_class),
            access_modes: Some(access_modes),
            resources: Some(k8s_openapi::api::core::v1::ResourceRequirements {
                requests: Some(BTreeMap::from([("storage".into(), Quantity(size))])),
                ..Default::default()
            }),
            ..Default::default()
        }),
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
    fn test_build_pvc_defaults() {
        let pvc = build_standalone_pvc(
            "my-chronik",
            "default",
            None,
            BTreeMap::new(),
            test_owner_ref(),
        );

        assert_eq!(pvc.metadata.name.as_deref(), Some("my-chronik-data"));
        assert_eq!(pvc.metadata.namespace.as_deref(), Some("default"));

        let spec = pvc.spec.as_ref().unwrap();
        assert_eq!(spec.storage_class_name.as_deref(), Some("standard"));
        assert_eq!(spec.access_modes.as_ref().unwrap(), &["ReadWriteOnce"]);

        let requests = spec.resources.as_ref().unwrap().requests.as_ref().unwrap();
        assert_eq!(requests.get("storage").unwrap().0, "50Gi");
    }

    #[test]
    fn test_build_pvc_custom() {
        let storage = StorageSpec {
            storage_class: "ssd".into(),
            size: "200Gi".into(),
            access_modes: vec!["ReadWriteOnce".into()],
        };
        let pvc = build_standalone_pvc(
            "big",
            "prod",
            Some(&storage),
            BTreeMap::new(),
            test_owner_ref(),
        );

        let spec = pvc.spec.as_ref().unwrap();
        assert_eq!(spec.storage_class_name.as_deref(), Some("ssd"));
        let requests = spec.resources.as_ref().unwrap().requests.as_ref().unwrap();
        assert_eq!(requests.get("storage").unwrap().0, "200Gi");
    }

    #[test]
    fn test_pvc_has_owner_ref() {
        let pvc = build_standalone_pvc("test", "ns", None, BTreeMap::new(), test_owner_ref());
        let refs = pvc.metadata.owner_references.as_ref().unwrap();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].kind, "ChronikStandalone");
    }

    #[test]
    fn test_build_cluster_node_pvc() {
        let pvc = build_cluster_node_pvc(
            "prod",
            "default",
            2,
            None,
            BTreeMap::new(),
            test_owner_ref(),
        );

        assert_eq!(pvc.metadata.name.as_deref(), Some("prod-2-data"));
        let spec = pvc.spec.as_ref().unwrap();
        assert_eq!(spec.storage_class_name.as_deref(), Some("standard"));
    }
}
