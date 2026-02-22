use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use tracing::debug;

use crate::error::OperatorError;

/// HTTP client for Chronik topic and user management.
///
/// Uses the admin API (port 10000 + node_id) for topic/user operations.
/// The operator resolves the target cluster's admin URL from the
/// ChronikCluster or ChronikStandalone status.
pub struct KafkaAdminClient {
    http: reqwest::Client,
    api_key: Option<String>,
}

impl KafkaAdminClient {
    pub fn new(api_key: Option<String>) -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .expect("HTTP client should build"),
            api_key,
        }
    }

    /// Create a topic.
    pub async fn create_topic(
        &self,
        base_url: &str,
        request: &CreateTopicRequest,
    ) -> Result<CreateTopicResponse, OperatorError> {
        let url = format!("{base_url}/admin/topics");
        debug!("Create topic {}: {url}", request.name);
        let resp = self.send_post(&url, request).await?;
        Ok(resp)
    }

    /// Delete a topic.
    pub async fn delete_topic(
        &self,
        base_url: &str,
        topic_name: &str,
    ) -> Result<DeleteTopicResponse, OperatorError> {
        let url = format!("{base_url}/admin/topics/{topic_name}");
        debug!("Delete topic {topic_name}: {url}");
        let mut req = self.http.delete(&url);
        if let Some(ref key) = self.api_key {
            req = req.header("X-API-Key", key);
        }
        let resp = req
            .send()
            .await?
            .error_for_status()
            .map_err(|e| OperatorError::AdminApi(format!("Delete topic failed: {e}")))?;
        Ok(resp.json().await?)
    }

    /// Get topic metadata (check if topic exists, get partition count, etc.).
    pub async fn describe_topic(
        &self,
        base_url: &str,
        topic_name: &str,
    ) -> Result<Option<TopicDescription>, OperatorError> {
        let url = format!("{base_url}/admin/topics/{topic_name}");
        debug!("Describe topic {topic_name}: {url}");
        let mut req = self.http.get(&url);
        if let Some(ref key) = self.api_key {
            req = req.header("X-API-Key", key);
        }
        let resp = req.send().await?;

        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }

        let resp = resp
            .error_for_status()
            .map_err(|e| OperatorError::AdminApi(format!("Describe topic failed: {e}")))?;
        Ok(Some(resp.json().await?))
    }

    /// Update topic configuration.
    pub async fn update_topic_config(
        &self,
        base_url: &str,
        topic_name: &str,
        config: &BTreeMap<String, String>,
    ) -> Result<UpdateConfigResponse, OperatorError> {
        let url = format!("{base_url}/admin/topics/{topic_name}/config");
        debug!("Update topic config {topic_name}: {url}");
        let request = UpdateConfigRequest {
            config: config.clone(),
        };
        self.send_post(&url, &request).await
    }

    /// Increase partition count for a topic.
    pub async fn increase_partitions(
        &self,
        base_url: &str,
        topic_name: &str,
        new_count: i32,
    ) -> Result<UpdatePartitionsResponse, OperatorError> {
        let url = format!("{base_url}/admin/topics/{topic_name}/partitions");
        debug!("Increase partitions for {topic_name} to {new_count}: {url}");
        let request = UpdatePartitionsRequest { count: new_count };
        self.send_post(&url, &request).await
    }

    /// Create a user.
    pub async fn create_user(
        &self,
        base_url: &str,
        request: &CreateUserRequest,
    ) -> Result<CreateUserResponse, OperatorError> {
        let url = format!("{base_url}/admin/users");
        debug!("Create user {}: {url}", request.username);
        self.send_post(&url, request).await
    }

    /// Delete a user.
    pub async fn delete_user(
        &self,
        base_url: &str,
        username: &str,
    ) -> Result<DeleteUserResponse, OperatorError> {
        let url = format!("{base_url}/admin/users/{username}");
        debug!("Delete user {username}: {url}");
        let mut req = self.http.delete(&url);
        if let Some(ref key) = self.api_key {
            req = req.header("X-API-Key", key);
        }
        let resp = req
            .send()
            .await?
            .error_for_status()
            .map_err(|e| OperatorError::AdminApi(format!("Delete user failed: {e}")))?;
        Ok(resp.json().await?)
    }

    /// Set ACLs for a user (replaces all existing ACLs).
    pub async fn set_acls(
        &self,
        base_url: &str,
        request: &SetAclsRequest,
    ) -> Result<SetAclsResponse, OperatorError> {
        let url = format!("{base_url}/admin/users/{}/acls", request.username);
        debug!("Set ACLs for {}: {url}", request.username);
        let mut req = self.http.put(&url).json(request);
        if let Some(ref key) = self.api_key {
            req = req.header("X-API-Key", key);
        }
        let resp = req.send().await?.error_for_status().map_err(|e| {
            OperatorError::AdminApi(format!("Set ACLs for {} failed: {e}", request.username))
        })?;
        Ok(resp.json().await?)
    }

    /// Helper to send authenticated POST requests.
    async fn send_post<Req: Serialize, Resp: for<'de> Deserialize<'de>>(
        &self,
        url: &str,
        body: &Req,
    ) -> Result<Resp, OperatorError> {
        let mut req = self.http.post(url).json(body);
        if let Some(ref key) = self.api_key {
            req = req.header("X-API-Key", key);
        }
        let resp = req
            .send()
            .await?
            .error_for_status()
            .map_err(|e| OperatorError::AdminApi(format!("Request failed: {e}")))?;
        Ok(resp.json().await?)
    }
}

// --- Request/Response types ---

#[derive(Debug, Serialize)]
pub struct CreateTopicRequest {
    pub name: String,
    pub partitions: i32,
    pub replication_factor: i32,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub config: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
pub struct CreateTopicResponse {
    pub success: bool,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub topic_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct DeleteTopicResponse {
    pub success: bool,
    #[serde(default)]
    pub message: String,
}

#[derive(Debug, Deserialize)]
pub struct TopicDescription {
    pub name: String,
    pub partitions: i32,
    pub replication_factor: i32,
    #[serde(default)]
    pub config: BTreeMap<String, String>,
    #[serde(default)]
    pub topic_id: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct UpdateConfigRequest {
    pub config: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
pub struct UpdateConfigResponse {
    pub success: bool,
    #[serde(default)]
    pub message: String,
}

#[derive(Debug, Serialize)]
pub struct UpdatePartitionsRequest {
    pub count: i32,
}

#[derive(Debug, Deserialize)]
pub struct UpdatePartitionsResponse {
    pub success: bool,
    #[serde(default)]
    pub message: String,
}

#[derive(Debug, Serialize)]
pub struct CreateUserRequest {
    pub username: String,
    pub password: String,
    #[serde(rename = "type")]
    pub auth_type: String,
}

#[derive(Debug, Deserialize)]
pub struct CreateUserResponse {
    pub success: bool,
    #[serde(default)]
    pub message: String,
}

#[derive(Debug, Deserialize)]
pub struct DeleteUserResponse {
    pub success: bool,
    #[serde(default)]
    pub message: String,
}

#[derive(Debug, Serialize)]
pub struct SetAclsRequest {
    pub username: String,
    pub acls: Vec<AclEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AclEntry {
    pub resource_type: String,
    pub resource_name: String,
    pub pattern_type: String,
    pub operation: String,
    pub effect: String,
}

#[derive(Debug, Deserialize)]
pub struct SetAclsResponse {
    pub success: bool,
    #[serde(default)]
    pub acl_count: i32,
    #[serde(default)]
    pub message: String,
}

/// Build the admin API base URL for a standalone instance.
pub fn standalone_admin_url(name: &str, namespace: &str) -> String {
    // Standalone uses port 10001 (ADMIN_API_BASE + 1, single node)
    let port = crate::constants::ports::ADMIN_API_BASE + 1;
    format!("http://{name}.{namespace}.svc.cluster.local:{port}")
}

/// Build the admin API base URL for a cluster (targets node 1 as default).
pub fn cluster_admin_url(cluster_name: &str, namespace: &str) -> String {
    let dns = crate::config_generator::node_dns_name(cluster_name, 1, namespace);
    let port = crate::constants::ports::ADMIN_API_BASE + 1;
    format!("http://{dns}:{port}")
}

/// Expand ACL rules from CRD spec into individual AclEntry items.
///
/// Each CRD AclRule may have multiple operations, which get expanded
/// into one AclEntry per operation.
pub fn expand_acl_rules(rules: &[crate::crds::user::AclRule]) -> Vec<AclEntry> {
    let mut entries = Vec::new();
    for rule in rules {
        for op in &rule.operations {
            entries.push(AclEntry {
                resource_type: rule.resource.type_.clone(),
                resource_name: rule.resource.name.clone(),
                pattern_type: rule.resource.pattern_type.clone(),
                operation: op.clone(),
                effect: rule.effect.clone(),
            });
        }
    }
    entries
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crds::user::{AclResource, AclRule};

    #[test]
    fn test_standalone_admin_url() {
        assert_eq!(
            standalone_admin_url("my-chronik", "default"),
            "http://my-chronik.default.svc.cluster.local:10001"
        );
    }

    #[test]
    fn test_cluster_admin_url() {
        assert_eq!(
            cluster_admin_url("prod", "default"),
            "http://prod-1.default.svc.cluster.local:10001"
        );
    }

    #[test]
    fn test_create_topic_request_serialization() {
        let req = CreateTopicRequest {
            name: "orders".into(),
            partitions: 6,
            replication_factor: 3,
            config: BTreeMap::from([("retention.ms".into(), "604800000".into())]),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"name\":\"orders\""));
        assert!(json.contains("\"partitions\":6"));
        assert!(json.contains("\"retention.ms\""));
    }

    #[test]
    fn test_create_topic_response_deserialization() {
        let json = r#"{"success": true, "message": "Topic created", "topic_id": "abc-123"}"#;
        let resp: CreateTopicResponse = serde_json::from_str(json).unwrap();
        assert!(resp.success);
        assert_eq!(resp.topic_id.as_deref(), Some("abc-123"));
    }

    #[test]
    fn test_topic_description_deserialization() {
        let json = r#"{
            "name": "orders",
            "partitions": 6,
            "replication_factor": 3,
            "config": {"retention.ms": "604800000"},
            "topic_id": "abc-123"
        }"#;
        let desc: TopicDescription = serde_json::from_str(json).unwrap();
        assert_eq!(desc.name, "orders");
        assert_eq!(desc.partitions, 6);
        assert_eq!(desc.config.get("retention.ms").unwrap(), "604800000");
    }

    #[test]
    fn test_create_user_request_serialization() {
        let req = CreateUserRequest {
            username: "app-producer".into(),
            password: "secret123".into(),
            auth_type: "sasl-scram-sha-256".into(),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"username\":\"app-producer\""));
        assert!(json.contains("\"type\":\"sasl-scram-sha-256\""));
    }

    #[test]
    fn test_expand_acl_rules() {
        let rules = vec![
            AclRule {
                resource: AclResource {
                    type_: "topic".into(),
                    name: "orders".into(),
                    pattern_type: "literal".into(),
                },
                operations: vec!["Read".into(), "Write".into()],
                effect: "Allow".into(),
            },
            AclRule {
                resource: AclResource {
                    type_: "group".into(),
                    name: "my-group".into(),
                    pattern_type: "literal".into(),
                },
                operations: vec!["Read".into()],
                effect: "Allow".into(),
            },
        ];

        let entries = expand_acl_rules(&rules);
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].operation, "Read");
        assert_eq!(entries[0].resource_type, "topic");
        assert_eq!(entries[1].operation, "Write");
        assert_eq!(entries[2].resource_type, "group");
    }

    #[test]
    fn test_set_acls_request_serialization() {
        let req = SetAclsRequest {
            username: "app".into(),
            acls: vec![AclEntry {
                resource_type: "topic".into(),
                resource_name: "orders".into(),
                pattern_type: "literal".into(),
                operation: "Read".into(),
                effect: "Allow".into(),
            }],
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"username\":\"app\""));
        assert!(json.contains("\"resource_type\":\"topic\""));
    }
}
