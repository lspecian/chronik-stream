//! API routes and OpenAPI documentation.

pub mod auth;
pub mod brokers;
pub mod topics;
pub mod consumer_groups;
pub mod cluster;

use crate::state::AppState;
use axum::{
    routing::{get, post, put, delete},
    Router,
};
use utoipa::OpenApi;
use utoipa_swagger_ui::SwaggerUi;

/// Create API router
pub fn create_router(state: AppState) -> Router {
    Router::new()
        // Auth routes
        .route("/api/v1/auth/login", post(auth::login))
        .route("/api/v1/auth/refresh", post(auth::refresh))
        
        // Cluster routes
        .route("/api/v1/cluster/info", get(cluster::get_info))
        .route("/api/v1/cluster/health", get(cluster::health_check))
        .route("/api/v1/cluster/metrics", get(cluster::get_metrics))
        
        // Broker routes
        .route("/api/v1/brokers", get(brokers::list_brokers))
        .route("/api/v1/brokers/:id", get(brokers::get_broker))
        .route("/api/v1/brokers/:id/config", get(brokers::get_broker_config))
        .route("/api/v1/brokers/:id/config", put(brokers::update_broker_config))
        
        // Topic routes
        .route("/api/v1/topics", get(topics::list_topics))
        .route("/api/v1/topics", post(topics::create_topic))
        .route("/api/v1/topics/:name", get(topics::get_topic))
        .route("/api/v1/topics/:name", delete(topics::delete_topic))
        .route("/api/v1/topics/:name/config", get(topics::get_topic_config))
        .route("/api/v1/topics/:name/config", put(topics::update_topic_config))
        .route("/api/v1/topics/:name/partitions", get(topics::list_partitions))
        .route("/api/v1/topics/:name/partitions", put(topics::update_partitions))
        
        // Consumer group routes
        .route("/api/v1/consumer-groups", get(consumer_groups::list_groups))
        .route("/api/v1/consumer-groups/:id", get(consumer_groups::get_group))
        .route("/api/v1/consumer-groups/:id", delete(consumer_groups::delete_group))
        .route("/api/v1/consumer-groups/:id/members", get(consumer_groups::list_members))
        .route("/api/v1/consumer-groups/:id/offsets", get(consumer_groups::get_offsets))
        .route("/api/v1/consumer-groups/:id/offsets", put(consumer_groups::reset_offsets))
        
        // OpenAPI documentation
        .merge(SwaggerUi::new("/swagger-ui").url("/api-doc/openapi.json", ApiDoc::openapi()))
        
        .with_state(state)
}

/// OpenAPI documentation
#[derive(OpenApi)]
#[openapi(
    paths(
        auth::login,
        auth::refresh,
        cluster::get_info,
        cluster::health_check,
        cluster::get_metrics,
        brokers::list_brokers,
        brokers::get_broker,
        topics::list_topics,
        topics::create_topic,
        topics::get_topic,
        topics::delete_topic,
        consumer_groups::list_groups,
        consumer_groups::get_group,
        consumer_groups::delete_group,
    ),
    components(
        schemas(
            auth::LoginRequest,
            auth::LoginResponse,
            auth::RefreshRequest,
            auth::RefreshResponse,
            cluster::ClusterInfo,
            cluster::HealthStatus,
            cluster::ClusterMetrics,
            brokers::Broker,
            brokers::BrokerConfig,
            topics::Topic,
            topics::CreateTopicRequest,
            topics::TopicConfig,
            topics::Partition,
            consumer_groups::ConsumerGroup,
            consumer_groups::GroupMember,
            consumer_groups::GroupOffset,
        )
    ),
    tags(
        (name = "auth", description = "Authentication endpoints"),
        (name = "cluster", description = "Cluster management endpoints"),
        (name = "brokers", description = "Broker management endpoints"),
        (name = "topics", description = "Topic management endpoints"),
        (name = "consumer-groups", description = "Consumer group management endpoints"),
    )
)]
pub struct ApiDoc;