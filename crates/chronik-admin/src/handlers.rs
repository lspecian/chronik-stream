//! Request handlers and middleware.

use axum::{
    extract::{Request, State},
    http::{header, StatusCode},
    middleware::Next,
    response::Response,
};
use chronik_auth::Claims;

/// JWT authentication middleware
pub async fn auth_middleware(
    State(state): State<crate::state::AppState>,
    mut req: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    // Skip auth for health and login endpoints
    let path = req.uri().path();
    if path == "/api/v1/cluster/health" || path == "/api/v1/auth/login" {
        return Ok(next.run(req).await);
    }
    
    // Extract token from Authorization header
    let auth_header = req.headers()
        .get(header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok());
    
    let token = match auth_header {
        Some(h) if h.starts_with("Bearer ") => &h[7..],
        _ => return Err(StatusCode::UNAUTHORIZED),
    };
    
    // Verify token
    let claims = state.jwt_manager
        .verify_token(token)
        .map_err(|_| StatusCode::UNAUTHORIZED)?;
    
    // Add claims to request extensions
    req.extensions_mut().insert(claims);
    
    Ok(next.run(req).await)
}

/// Extract claims from request
pub fn extract_claims(req: &Request) -> Option<&Claims> {
    req.extensions().get::<Claims>()
}