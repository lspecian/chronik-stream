//! Authentication endpoints.

use crate::{error::AdminResult, state::AppState};
use axum::{
    extract::State,
    Json,
};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Login request
#[derive(Debug, Deserialize, ToSchema)]
pub struct LoginRequest {
    /// Username
    pub username: String,
    
    /// Password
    pub password: String,
}

/// Login response
#[derive(Debug, Serialize, ToSchema)]
pub struct LoginResponse {
    /// Access token
    pub access_token: String,
    
    /// Token type
    pub token_type: String,
    
    /// Expiration time in seconds
    pub expires_in: i64,
}

/// Refresh token request
#[derive(Debug, Deserialize, ToSchema)]
pub struct RefreshRequest {
    /// Refresh token
    pub refresh_token: String,
}

/// Refresh token response
#[derive(Debug, Serialize, ToSchema)]
pub struct RefreshResponse {
    /// New access token
    pub access_token: String,
    
    /// Token type
    pub token_type: String,
    
    /// Expiration time in seconds
    pub expires_in: i64,
}

/// Login endpoint
#[utoipa::path(
    post,
    path = "/api/v1/auth/login",
    request_body = LoginRequest,
    responses(
        (status = 200, description = "Login successful", body = LoginResponse),
        (status = 401, description = "Invalid credentials"),
    ),
    tag = "auth"
)]
pub async fn login(
    State(state): State<AppState>,
    Json(req): Json<LoginRequest>,
) -> AdminResult<Json<LoginResponse>> {
    // TODO: Verify credentials against database
    // For now, accept admin/admin
    if req.username != "admin" || req.password != "admin" {
        return Err(crate::error::AdminError::Unauthorized);
    }
    
    // Generate JWT token
    let token = state.jwt_manager.generate_token(
        &req.username,
        vec!["admin".to_string()],
    )?;
    
    Ok(Json(LoginResponse {
        access_token: token,
        token_type: "Bearer".to_string(),
        expires_in: state.config.auth.token_expiration_secs,
    }))
}

/// Refresh token endpoint
#[utoipa::path(
    post,
    path = "/api/v1/auth/refresh",
    request_body = RefreshRequest,
    responses(
        (status = 200, description = "Token refreshed", body = RefreshResponse),
        (status = 401, description = "Invalid token"),
    ),
    tag = "auth"
)]
pub async fn refresh(
    State(state): State<AppState>,
    Json(req): Json<RefreshRequest>,
) -> AdminResult<Json<RefreshResponse>> {
    // Refresh the token
    let token = state.jwt_manager.refresh_token(&req.refresh_token)?;
    
    Ok(Json(RefreshResponse {
        access_token: token,
        token_type: "Bearer".to_string(),
        expires_in: state.config.auth.token_expiration_secs,
    }))
}