//! REST API for query service.

use crate::search::{SearchEngine, SearchQuery};
use crate::aggregation::{AggregationEngine, AggregationQuery};
use axum::{
    extract::{Path, Query as AxumQuery, State},
    http::StatusCode,
    response::{IntoResponse, Json},
    routing::{delete, get, post},
    Router,
};
use chronik_common::Result;
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;

/// Query API configuration
#[derive(Debug, Clone)]
pub struct QueryConfig {
    /// Listen address
    pub listen_addr: SocketAddr,
    /// Index directory path
    pub index_path: PathBuf,
    /// Enable CORS
    pub enable_cors: bool,
}

/// Query API server
pub struct QueryApi {
    config: QueryConfig,
    search_engine: Arc<SearchEngine>,
    aggregation_engine: Arc<AggregationEngine>,
}

/// API state
#[derive(Clone)]
struct ApiState {
    search_engine: Arc<SearchEngine>,
    aggregation_engine: Arc<AggregationEngine>,
}

impl QueryApi {
    /// Create a new query API
    pub fn new(config: QueryConfig) -> Result<Self> {
        let search_engine = Arc::new(SearchEngine::new(&config.index_path)?);
        let aggregation_engine = Arc::new(AggregationEngine::new());
        
        Ok(Self {
            config,
            search_engine,
            aggregation_engine,
        })
    }
    
    /// Start the API server
    pub async fn start(self) -> Result<()> {
        let listen_addr = self.config.listen_addr;
        
        // Start aggregation engine
        self.aggregation_engine.clone().start().await?;
        
        let app = self.build_router();
        
        tracing::info!("Starting Query API on {}", listen_addr);
        
        let listener = tokio::net::TcpListener::bind(&listen_addr).await?;
        axum::serve(listener, app).await
            .map_err(|e| chronik_common::Error::Io(std::io::Error::new(std::io::ErrorKind::Other, e)))?;
        
        Ok(())
    }
    
    /// Build the router
    fn build_router(self) -> Router {
        let state = ApiState {
            search_engine: self.search_engine,
            aggregation_engine: self.aggregation_engine,
        };
        
        let mut router = Router::new()
            .route("/health", get(health_check))
            .route("/search", get(search_handler))
            .route("/search", post(search_post_handler))
            .route("/aggregations", post(create_aggregation_handler))
            .route("/aggregations/:id", get(get_aggregation_handler))
            .route("/aggregations/:id", delete(delete_aggregation_handler))
            .with_state(state);
        
        // Add middleware
        router = router.layer(TraceLayer::new_for_http());
        
        if self.config.enable_cors {
            router = router.layer(CorsLayer::permissive());
        }
        
        router
    }
}

/// Health check handler
async fn health_check() -> impl IntoResponse {
    StatusCode::OK
}

/// Search query parameters
#[derive(Debug, Deserialize)]
struct SearchParams {
    q: Option<String>,
    topic: Option<String>,
    partition: Option<i32>,
    start_time: Option<i64>,
    end_time: Option<i64>,
    limit: Option<usize>,
    offset: Option<usize>,
}

/// Search handler (GET)
async fn search_handler(
    State(state): State<ApiState>,
    AxumQuery(params): AxumQuery<SearchParams>,
) -> impl IntoResponse {
    let query = SearchQuery {
        query: params.q.unwrap_or_default(),
        topic: params.topic,
        partition: params.partition,
        start_time: params.start_time,
        end_time: params.end_time,
        limit: params.limit.unwrap_or(100),
        offset: params.offset.unwrap_or(0),
    };
    
    match state.search_engine.search(query).await {
        Ok(result) => (StatusCode::OK, Json(ApiResponse::success(result))).into_response(),
        Err(e) => {
            tracing::error!("Search failed: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, Json(ApiResponse::<()>::error(e.to_string()))).into_response()
        }
    }
}

/// Search request body
#[derive(Debug, Deserialize)]
struct SearchRequest {
    query: SearchQuery,
}

/// Search handler (POST)
async fn search_post_handler(
    State(state): State<ApiState>,
    Json(request): Json<SearchRequest>,
) -> impl IntoResponse {
    match state.search_engine.search(request.query).await {
        Ok(result) => (StatusCode::OK, Json(ApiResponse::success(result))).into_response(),
        Err(e) => {
            tracing::error!("Search failed: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, Json(ApiResponse::<()>::error(e.to_string()))).into_response()
        }
    }
}

/// API response wrapper
#[derive(Debug, Serialize)]
struct ApiResponse<T> {
    success: bool,
    data: Option<T>,
    error: Option<String>,
}

impl<T> ApiResponse<T> {
    fn success(data: T) -> Self {
        Self {
            success: true,
            data: Some(data),
            error: None,
        }
    }
    
    fn error(message: String) -> Self {
        Self {
            success: false,
            data: None,
            error: Some(message),
        }
    }
}

/// Create aggregation request
#[derive(Debug, Deserialize)]
struct CreateAggregationRequest {
    id: String,
    query: AggregationQuery,
}

/// Create aggregation handler
async fn create_aggregation_handler(
    State(state): State<ApiState>,
    Json(request): Json<CreateAggregationRequest>,
) -> impl IntoResponse {
    match state.aggregation_engine.register_aggregation(request.id, request.query).await {
        Ok(()) => (StatusCode::CREATED, Json(ApiResponse::success(()))).into_response(),
        Err(e) => {
            tracing::error!("Failed to create aggregation: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, Json(ApiResponse::<()>::error(e.to_string()))).into_response()
        }
    }
}

/// Get aggregation results handler
async fn get_aggregation_handler(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match state.aggregation_engine.get_results(&id).await {
        Ok(results) => (StatusCode::OK, Json(ApiResponse::success(results))).into_response(),
        Err(e) => {
            tracing::error!("Failed to get aggregation results: {}", e);
            (StatusCode::NOT_FOUND, Json(ApiResponse::<()>::error(e.to_string()))).into_response()
        }
    }
}

/// Delete aggregation handler
async fn delete_aggregation_handler(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match state.aggregation_engine.remove_aggregation(&id).await {
        Ok(()) => (StatusCode::NO_CONTENT, Json(ApiResponse::success(()))).into_response(),
        Err(e) => {
            tracing::error!("Failed to delete aggregation: {}", e);
            (StatusCode::NOT_FOUND, Json(ApiResponse::<()>::error(e.to_string()))).into_response()
        }
    }
}