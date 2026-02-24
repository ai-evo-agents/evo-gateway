use axum::{
    extract::{Request, State},
    middleware::Next,
    response::Response,
};
use evo_gateway::auth::AuthStore;
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::error::GatewayError;

/// Shared auth state, held in the middleware layer.
#[derive(Clone)]
pub struct AuthState {
    pub enabled: bool,
    pub store: Arc<RwLock<AuthStore>>,
}

/// Middleware that checks incoming requests for a valid API key.
///
/// Extracts the token from `Authorization: Bearer <key>` or `x-api-key: <key>` headers.
/// When `enabled` is false, all requests pass through unconditionally.
pub async fn auth_middleware(
    State(auth): State<AuthState>,
    req: Request,
    next: Next,
) -> Result<Response, GatewayError> {
    if !auth.enabled {
        return Ok(next.run(req).await);
    }

    // Extract token from Authorization: Bearer <key> or x-api-key: <key>
    let token = req
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .or_else(|| req.headers().get("x-api-key").and_then(|v| v.to_str().ok()));

    let token = token.ok_or_else(|| {
        GatewayError::Unauthorized(
            "missing API key — provide via Authorization: Bearer <key> or x-api-key: <key>"
                .to_string(),
        )
    })?;

    let store = auth.store.read().await;
    if !store.verify(token) {
        return Err(GatewayError::Unauthorized("invalid API key".to_string()));
    }
    drop(store);

    Ok(next.run(req).await)
}
