//! Authorization: mapping Kafka requests onto ACL checks.
//!
//! # Why this exists
//!
//! [`crate::acl`] has been a complete-looking authorizer with **zero call
//! sites** since it was written — it compiled into the binary and never ran, so
//! `DescribeAcls`/`CreateAcls`/`DeleteAcls` answered `SECURITY_DISABLED` and no
//! request was ever checked. It could not have worked either, because until
//! Phase 0 there was no principal to authorize: the dispatch had no idea which
//! connection a request arrived on.
//!
//! This module is the missing half. It turns "connection X issued API Y naming
//! resource Z" into an ACL decision, following Kafka's own operation table.
//!
//! # Where checks happen
//!
//! Not at the dispatch. A Kafka request names its resources *inside* the body
//! (topic names in Produce, a group id in JoinGroup), so a dispatch-level check
//! would have to parse every request a second time. Instead each handler
//! authorizes once its request is parsed and the names are known — which is also
//! where a correctly shaped denial can be built.
//!
//! # Denials must be legible
//!
//! A denial cannot be reported through `ErrorHandler::build_error_response()`:
//! that function ignores the error code for Produce, Fetch, Metadata and
//! CreateTopics and emits an empty **success** body, so a denied Produce would
//! reach the client as "accepted, zero results". Handlers therefore build a
//! per-resource denial from the parsed request, carrying
//! `TOPIC_AUTHORIZATION_FAILED` (29) or `GROUP_AUTHORIZATION_FAILED` (30) on
//! each entry, exactly as Kafka does.

use std::sync::Arc;

use chronik_protocol::describe_acls_types::{AclOperation, ResourceType};
use tracing::warn;

use crate::acl::AclStore;
use crate::connection::ConnectionContext;

/// Kafka error code: the principal may not act on this topic.
pub const ERROR_TOPIC_AUTHORIZATION_FAILED: i16 = 29;
/// Kafka error code: the principal may not act on this consumer group.
pub const ERROR_GROUP_AUTHORIZATION_FAILED: i16 = 30;
/// Kafka error code: the principal may not perform this cluster operation.
pub const ERROR_CLUSTER_AUTHORIZATION_FAILED: i16 = 31;
/// Kafka error code: the principal may not use this transactional id.
pub const ERROR_TRANSACTIONAL_ID_AUTHORIZATION_FAILED: i16 = 53;

/// The principal used when authorization is enabled but the connection did not
/// authenticate.
///
/// This is only reachable when ACLs are on and SASL is off — a deployment that
/// wants authorization without authentication. Everything is then one anonymous
/// principal, which is what Kafka calls it too. It is a legitimate
/// configuration for host-based rules, and it is not a way to bypass anything:
/// `User:ANONYMOUS` matches only ACLs written for it.
pub const ANONYMOUS_PRINCIPAL: &str = "User:ANONYMOUS";

/// Authorizes requests on behalf of a connection.
#[derive(Clone)]
pub struct Authorizer {
    acls: Arc<AclStore>,
}

impl Authorizer {
    pub fn new(acls: Arc<AclStore>) -> Self {
        Self { acls }
    }

    /// Whether any check will actually be performed.
    ///
    /// Handlers use this to skip building resource lists on the hot path when
    /// authorization is off, which is the default.
    pub fn is_enabled(&self) -> bool {
        self.acls.is_enabled()
    }

    pub fn store(&self) -> &Arc<AclStore> {
        &self.acls
    }

    /// Resolve the principal for a connection.
    async fn principal(&self, ctx: &ConnectionContext) -> String {
        ctx.principal()
            .await
            .unwrap_or_else(|| ANONYMOUS_PRINCIPAL.to_string())
    }

    /// Authorize one operation against one resource.
    pub async fn authorize(
        &self,
        ctx: &ConnectionContext,
        resource_type: ResourceType,
        resource_name: &str,
        operation: AclOperation,
    ) -> bool {
        if !self.acls.is_enabled() {
            return true;
        }

        let principal = self.principal(ctx).await;
        let host = ctx.peer_addr().ip().to_string();

        let allowed = self
            .acls
            .authorize(&principal, &host, resource_type, resource_name, operation)
            .await;

        if !allowed {
            // Denials are the security-relevant event and must be visible; an
            // operator cannot debug an ACL policy from a client-side error.
            warn!(
                "DENIED {} ({}) {:?} on {:?} '{}'",
                principal, ctx.id(), operation, resource_type, resource_name
            );
        }

        allowed
    }

    /// Partition a list of topics into (allowed, denied).
    ///
    /// Kafka authorizes per topic and reports per topic, so a request naming
    /// several topics can be partly served. Returning both halves lets the
    /// handler do exactly that instead of failing the whole request.
    pub async fn partition_topics(
        &self,
        ctx: &ConnectionContext,
        topics: &[String],
        operation: AclOperation,
    ) -> (Vec<String>, Vec<String>) {
        if !self.acls.is_enabled() {
            return (topics.to_vec(), Vec::new());
        }

        let mut allowed = Vec::with_capacity(topics.len());
        let mut denied = Vec::new();
        for topic in topics {
            if self
                .authorize(ctx, ResourceType::Topic, topic, operation)
                .await
            {
                allowed.push(topic.clone());
            } else {
                denied.push(topic.clone());
            }
        }
        (allowed, denied)
    }

    /// Authorize a consumer group operation.
    pub async fn authorize_group(
        &self,
        ctx: &ConnectionContext,
        group_id: &str,
        operation: AclOperation,
    ) -> bool {
        self.authorize(ctx, ResourceType::Group, group_id, operation)
            .await
    }

    /// Authorize a cluster-wide operation.
    ///
    /// Kafka names the cluster resource `kafka-cluster`; using the same literal
    /// means ACLs written by `kafka-acls.sh` apply unchanged.
    pub async fn authorize_cluster(
        &self,
        ctx: &ConnectionContext,
        operation: AclOperation,
    ) -> bool {
        self.authorize(ctx, ResourceType::Cluster, CLUSTER_RESOURCE_NAME, operation)
            .await
    }

    /// Authorize use of a transactional id.
    pub async fn authorize_transactional_id(
        &self,
        ctx: &ConnectionContext,
        transactional_id: &str,
        operation: AclOperation,
    ) -> bool {
        self.authorize(
            ctx,
            ResourceType::TransactionalId,
            transactional_id,
            operation,
        )
        .await
    }
}

/// Kafka's name for the cluster resource.
pub const CLUSTER_RESOURCE_NAME: &str = "kafka-cluster";

/// The operation an API requires on its primary resource.
///
/// This mirrors Kafka's `AclOperation` table (see `KafkaApis` upstream). The
/// non-obvious entries are worth stating explicitly:
///
/// - `Fetch` needs **Read** on the topic, not Describe.
/// - Consumer-group APIs need **Read** on the group; only `DescribeGroups` and
///   `ListGroups` are Describe.
/// - `ListOffsets` needs **Describe** on the topic.
/// - Transactional APIs need **Write** on the TransactionalId.
/// - `OffsetCommit` needs Read on the group *and* Read on each topic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequiredAccess {
    /// Operation on named topics.
    Topic(AclOperation),
    /// Operation on the named consumer group.
    Group(AclOperation),
    /// Operation on the cluster.
    Cluster(AclOperation),
    /// Operation on a transactional id.
    TransactionalId(AclOperation),
    /// No authorization required (protocol negotiation and authentication).
    None,
}

/// Map an API key to the access it requires.
///
/// Returned for documentation and for the coarse cluster-level checks that can
/// be made without parsing a body; per-resource checks happen in the handlers,
/// which know the names.
pub fn required_access(api_key: i16) -> RequiredAccess {
    use AclOperation::*;
    match api_key {
        0 => RequiredAccess::Topic(Write),        // Produce
        1 => RequiredAccess::Topic(Read),         // Fetch
        2 => RequiredAccess::Topic(Describe),     // ListOffsets
        3 => RequiredAccess::Topic(Describe),     // Metadata
        8 => RequiredAccess::Group(Read),         // OffsetCommit
        9 => RequiredAccess::Group(Describe),     // OffsetFetch
        10 => RequiredAccess::Group(Describe),    // FindCoordinator
        11 => RequiredAccess::Group(Read),        // JoinGroup
        12 => RequiredAccess::Group(Read),        // Heartbeat
        13 => RequiredAccess::Group(Read),        // LeaveGroup
        14 => RequiredAccess::Group(Read),        // SyncGroup
        15 => RequiredAccess::Group(Describe),    // DescribeGroups
        16 => RequiredAccess::Cluster(Describe),  // ListGroups
        17 | 18 | 36 => RequiredAccess::None,     // SaslHandshake/ApiVersions/SaslAuthenticate
        19 => RequiredAccess::Cluster(Create),    // CreateTopics
        20 => RequiredAccess::Topic(Delete),      // DeleteTopics
        21 => RequiredAccess::Topic(Delete),      // DeleteRecords
        22 => RequiredAccess::TransactionalId(Write), // InitProducerId
        23 => RequiredAccess::Topic(Describe),    // OffsetForLeaderEpoch
        24 => RequiredAccess::TransactionalId(Write), // AddPartitionsToTxn
        25 => RequiredAccess::TransactionalId(Write), // AddOffsetsToTxn
        26 => RequiredAccess::TransactionalId(Write), // EndTxn
        28 => RequiredAccess::TransactionalId(Write), // TxnOffsetCommit
        29 | 30 | 31 => RequiredAccess::Cluster(Alter), // Describe/Create/DeleteAcls
        32 => RequiredAccess::Cluster(DescribeConfigs), // DescribeConfigs
        33 => RequiredAccess::Cluster(AlterConfigs), // AlterConfigs
        35 => RequiredAccess::Cluster(Describe),  // DescribeLogDirs
        37 => RequiredAccess::Topic(Alter),       // CreatePartitions
        42 => RequiredAccess::Group(Delete),      // DeleteGroups
        44 => RequiredAccess::Cluster(AlterConfigs), // IncrementalAlterConfigs
        47 => RequiredAccess::Group(Delete),      // OffsetDelete
        // Anything unmapped is treated as a cluster operation requiring
        // ClusterAction. Failing closed on an unknown API is the safe default:
        // a new API added without an entry here is refused rather than silently
        // unauthorized.
        _ => RequiredAccess::Cluster(ClusterAction),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acl::AclBinding;
    use chronik_protocol::describe_acls_types::{AclPermissionType, PatternType};

    fn binding(
        principal: &str,
        resource_type: ResourceType,
        name: &str,
        operation: AclOperation,
        permission: AclPermissionType,
    ) -> AclBinding {
        AclBinding {
            resource_type,
            resource_name: name.to_string(),
            pattern_type: PatternType::Literal,
            principal: principal.to_string(),
            host: "*".to_string(),
            operation,
            permission_type: permission,
        }
    }

    /// A denied topic and an allowed topic in one request must be separated,
    /// not collapsed into a whole-request failure.
    #[tokio::test]
    async fn partition_topics_splits_allowed_and_denied() {
        let store = Arc::new(AclStore::with_config(true, false, Vec::new()));

        store
            .create_acl(binding(
                "User:alice",
                ResourceType::Topic,
                "allowed-topic",
                AclOperation::Write,
                AclPermissionType::Allow,
            ))
            .await
            .unwrap();

        let authorizer = Authorizer::new(store);
        let ctx = ConnectionContext::internal();

        // ConnectionContext::internal() has auth disabled, so it authorizes as
        // ANONYMOUS - which has no ACLs and must be denied both topics.
        let (allowed, denied) = authorizer
            .partition_topics(
                &ctx,
                &["allowed-topic".to_string(), "other-topic".to_string()],
                AclOperation::Write,
            )
            .await;
        assert!(allowed.is_empty());
        assert_eq!(denied.len(), 2);
    }

    /// An API with no entry in the table must fail closed, not open.
    #[test]
    fn unmapped_api_requires_cluster_action() {
        assert_eq!(
            required_access(9999),
            RequiredAccess::Cluster(AclOperation::ClusterAction)
        );
    }

    /// Regression guards for the entries most often got wrong.
    #[test]
    fn operation_table_matches_kafka() {
        assert_eq!(required_access(0), RequiredAccess::Topic(AclOperation::Write)); // Produce
        assert_eq!(required_access(1), RequiredAccess::Topic(AclOperation::Read)); // Fetch, not Describe
        assert_eq!(required_access(11), RequiredAccess::Group(AclOperation::Read)); // JoinGroup
        assert_eq!(
            required_access(19),
            RequiredAccess::Cluster(AclOperation::Create)
        ); // CreateTopics
        assert_eq!(required_access(18), RequiredAccess::None); // ApiVersions
        assert_eq!(required_access(17), RequiredAccess::None); // SaslHandshake
        assert_eq!(required_access(36), RequiredAccess::None); // SaslAuthenticate
    }

    /// With ACLs disabled nothing is checked - the default must stay free.
    #[tokio::test]
    async fn disabled_authorizer_allows_everything() {
        let authorizer = Authorizer::new(Arc::new(AclStore::with_config(false, true, Vec::new())));
        assert!(!authorizer.is_enabled());

        let ctx = ConnectionContext::internal();
        assert!(
            authorizer
                .authorize(&ctx, ResourceType::Topic, "any", AclOperation::Write)
                .await
        );
        let (allowed, denied) = authorizer
            .partition_topics(&ctx, &["a".to_string()], AclOperation::Write)
            .await;
        assert_eq!(allowed.len(), 1);
        assert!(denied.is_empty());
    }
}
