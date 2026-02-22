use std::collections::BTreeMap;

use k8s_openapi::api::policy::v1::PodDisruptionBudget;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{LabelSelector, ObjectMeta, OwnerReference};
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;

/// Build a PodDisruptionBudget for a Chronik cluster.
pub fn build_cluster_pdb(
    cluster_name: &str,
    namespace: &str,
    max_unavailable: i32,
    selector_labels: BTreeMap<String, String>,
    owner_ref: OwnerReference,
) -> PodDisruptionBudget {
    let pdb_name = format!("{cluster_name}-pdb");

    PodDisruptionBudget {
        metadata: ObjectMeta {
            name: Some(pdb_name),
            namespace: Some(namespace.into()),
            owner_references: Some(vec![owner_ref]),
            ..Default::default()
        },
        spec: Some(k8s_openapi::api::policy::v1::PodDisruptionBudgetSpec {
            max_unavailable: Some(IntOrString::Int(max_unavailable)),
            selector: Some(LabelSelector {
                match_labels: Some(selector_labels),
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
    use crate::constants::{labels, values};

    fn test_owner_ref() -> OwnerReference {
        OwnerReference {
            api_version: "chronik.io/v1alpha1".into(),
            kind: "ChronikCluster".into(),
            name: "test".into(),
            uid: "test-uid".into(),
            controller: Some(true),
            block_owner_deletion: Some(true),
        }
    }

    #[test]
    fn test_build_pdb() {
        let selector = BTreeMap::from([
            (labels::NAME.into(), values::APP_NAME.into()),
            (labels::INSTANCE.into(), "my-cluster".into()),
        ]);

        let pdb = build_cluster_pdb("my-cluster", "default", 1, selector, test_owner_ref());

        assert_eq!(pdb.metadata.name.as_deref(), Some("my-cluster-pdb"));
        let spec = pdb.spec.as_ref().unwrap();
        assert_eq!(spec.max_unavailable, Some(IntOrString::Int(1)));
        let match_labels = spec
            .selector
            .as_ref()
            .unwrap()
            .match_labels
            .as_ref()
            .unwrap();
        assert_eq!(match_labels.get(labels::NAME).unwrap(), values::APP_NAME);
    }
}
