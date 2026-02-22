use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::common::{Condition, SecretKeyRef};
use super::topic::ClusterRef;

/// Spec for declarative user and ACL management.
#[derive(CustomResource, Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[kube(
    group = "chronik.io",
    version = "v1alpha1",
    kind = "ChronikUser",
    namespaced,
    status = "ChronikUserStatus",
    shortname = "cuser",
    printcolumn = r#"{"name":"Auth","type":"string","jsonPath":".spec.authentication.type"}"#,
    printcolumn = r#"{"name":"ACLs","type":"integer","jsonPath":".status.aclCount"}"#,
    printcolumn = r#"{"name":"Phase","type":"string","jsonPath":".status.phase"}"#,
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct ChronikUserSpec {
    /// Reference to the target Chronik cluster.
    pub cluster_ref: ClusterRef,

    /// Authentication configuration.
    pub authentication: AuthenticationSpec,

    /// Authorization (ACL) configuration.
    #[serde(default)]
    pub authorization: Option<AuthorizationSpec>,

    /// Schema Registry access.
    #[serde(default)]
    pub schema_registry: Option<SchemaRegistryAccessSpec>,
}

/// Authentication configuration for a user.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AuthenticationSpec {
    /// Authentication type: sasl-scram-sha-256, sasl-scram-sha-512, sasl-plain, tls.
    #[serde(rename = "type", default = "super::defaults::auth_type")]
    pub type_: String,

    /// Secret to store/read the password. Operator creates this if it doesn't exist.
    #[serde(default)]
    pub password_secret: Option<SecretKeyRef>,
}

/// Authorization (ACL) configuration.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct AuthorizationSpec {
    /// List of ACL rules.
    #[serde(default)]
    pub acls: Vec<AclRule>,
}

/// A single ACL rule.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct AclRule {
    /// Resource this ACL applies to.
    pub resource: AclResource,

    /// Allowed operations: Read, Write, Create, Delete, Alter, Describe, All.
    pub operations: Vec<String>,

    /// Effect: Allow or Deny.
    #[serde(default = "super::defaults::acl_effect")]
    pub effect: String,
}

/// An ACL resource specification.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AclResource {
    /// Resource type: topic, group, cluster, transactionalId.
    #[serde(rename = "type")]
    pub type_: String,

    /// Resource name or prefix.
    pub name: String,

    /// Pattern type: literal or prefixed.
    #[serde(default = "super::defaults::acl_pattern_type")]
    pub pattern_type: String,
}

/// Schema Registry access configuration for a user.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct SchemaRegistryAccessSpec {
    /// Enable Schema Registry access.
    #[serde(default)]
    pub enabled: bool,

    /// Role: readonly, readwrite.
    #[serde(default = "super::defaults::schema_registry_role")]
    pub role: String,
}

/// Status for ChronikUser.
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ChronikUserStatus {
    /// Current phase: Pending, Ready, Error.
    #[serde(default)]
    pub phase: Option<String>,

    /// Last observed generation.
    #[serde(default)]
    pub observed_generation: Option<i64>,

    /// Name of the Secret containing credentials.
    #[serde(default)]
    pub credentials_secret: Option<String>,

    /// Number of ACL rules applied.
    #[serde(default)]
    pub acl_count: Option<i32>,

    /// Status conditions.
    #[serde(default)]
    pub conditions: Vec<Condition>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use kube::CustomResourceExt;

    #[test]
    fn test_crd_generates_valid_schema() {
        let crd = ChronikUser::crd();
        let yaml = serde_yaml::to_string(&crd).expect("CRD should serialize to YAML");
        assert!(yaml.contains("ChronikUser"));
        assert!(yaml.contains("chronik.io"));
        assert!(yaml.contains("v1alpha1"));
    }

    #[test]
    fn test_spec_defaults() {
        let json = r#"{
            "clusterRef": {"name": "my-cluster"},
            "authentication": {"type": "sasl-scram-sha-256"}
        }"#;
        let spec: ChronikUserSpec = serde_json::from_str(json).unwrap();
        assert_eq!(spec.authentication.type_, "sasl-scram-sha-256");
        assert!(spec.authorization.is_none());
        assert!(spec.schema_registry.is_none());
    }

    #[test]
    fn test_acl_rule_defaults() {
        let json = r#"{
            "resource": {"type": "topic", "name": "orders"},
            "operations": ["Read", "Write"]
        }"#;
        let rule: AclRule = serde_json::from_str(json).unwrap();
        assert_eq!(rule.effect, "Allow");
        assert_eq!(rule.resource.pattern_type, "literal");
    }
}
