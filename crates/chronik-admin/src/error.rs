//! Error types for admin API.

use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;
use thiserror::Error;

/// Admin API error
#[derive(Error, Debug)]
pub enum AdminError {
    #[error("Not found: {0}")]
    NotFound(String),
    
    #[error("Bad request: {0}")]
    BadRequest(String),
    
    #[error("Unauthorized")]
    Unauthorized,
    
    #[error("Forbidden")]
    Forbidden,
    
    #[error("Conflict: {0}")]
    Conflict(String),
    
    #[error("Internal error: {0}")]
    Internal(String),
    
    #[error("Service unavailable: {0}")]
    ServiceUnavailable(String),
    
    #[error("Metadata error: {0}")]
    Metadata(String),
    
    #[error("Auth error: {0}")]
    Auth(#[from] chronik_auth::AuthError),
    
    #[error("Controller error: {0}")]
    Controller(String),
}

/// Admin result type
pub type AdminResult<T> = Result<T, AdminError>;

impl IntoResponse for AdminError {
    fn into_response(self) -> Response {
        let (status, error_message) = match self {
            AdminError::NotFound(msg) => (StatusCode::NOT_FOUND, msg),
            AdminError::BadRequest(msg) => (StatusCode::BAD_REQUEST, msg),
            AdminError::Unauthorized => (StatusCode::UNAUTHORIZED, "Unauthorized".to_string()),
            AdminError::Forbidden => (StatusCode::FORBIDDEN, "Forbidden".to_string()),
            AdminError::Conflict(msg) => (StatusCode::CONFLICT, msg),
            AdminError::Internal(msg) => (StatusCode::INTERNAL_SERVER_ERROR, msg),
            AdminError::ServiceUnavailable(msg) => (StatusCode::SERVICE_UNAVAILABLE, msg),
            AdminError::Metadata(msg) => (StatusCode::INTERNAL_SERVER_ERROR, msg),
            AdminError::Auth(e) => match e {
                chronik_auth::AuthError::InvalidCredentials => (StatusCode::UNAUTHORIZED, "Invalid credentials".to_string()),
                chronik_auth::AuthError::TokenExpired => (StatusCode::UNAUTHORIZED, "Token expired".to_string()),
                chronik_auth::AuthError::TokenInvalid(_) => (StatusCode::UNAUTHORIZED, "Invalid token".to_string()),
                chronik_auth::AuthError::PermissionDenied => (StatusCode::FORBIDDEN, "Permission denied".to_string()),
                _ => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
            },
            AdminError::Controller(msg) => (StatusCode::BAD_GATEWAY, msg),
        };
        
        let body = Json(json!({
            "error": error_message,
            "status": status.as_u16(),
        }));
        
        (status, body).into_response()
    }
}