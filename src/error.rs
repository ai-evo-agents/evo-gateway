use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde_json::json;
use thiserror::Error;

#[derive(Debug, Error)]
#[allow(dead_code)] // RateLimitExceeded used when rate limiting middleware is wired in
pub enum GatewayError {
    #[error("Provider '{0}' not found")]
    ProviderNotFound(String),

    #[error("Provider '{0}' is disabled")]
    ProviderDisabled(String),

    #[error("Upstream request failed: {0}")]
    UpstreamError(String),

    #[error("Configuration error: {0}")]
    ConfigError(String),

    #[error("Rate limit exceeded for provider '{0}'")]
    RateLimitExceeded(String),

    #[error("Unauthorized: {0}")]
    Unauthorized(String),

    #[error("Internal error: {0}")]
    Internal(String),

    #[error("Tmux error: {0}")]
    TmuxError(String),

    #[error("Session not found: {0}")]
    SessionNotFound(String),

    #[error("Session timeout: {0}")]
    SessionTimeout(String),
}

impl IntoResponse for GatewayError {
    fn into_response(self) -> Response {
        let (status, code, message) = match &self {
            Self::ProviderNotFound(_) => (
                StatusCode::NOT_FOUND,
                "PROVIDER_NOT_FOUND",
                self.to_string(),
            ),
            Self::ProviderDisabled(_) => (
                StatusCode::SERVICE_UNAVAILABLE,
                "PROVIDER_DISABLED",
                self.to_string(),
            ),
            Self::UpstreamError(_) => (StatusCode::BAD_GATEWAY, "UPSTREAM_ERROR", self.to_string()),
            Self::ConfigError(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "CONFIG_ERROR",
                self.to_string(),
            ),
            Self::RateLimitExceeded(_) => (
                StatusCode::TOO_MANY_REQUESTS,
                "RATE_LIMIT_EXCEEDED",
                self.to_string(),
            ),
            Self::Unauthorized(_) => (StatusCode::UNAUTHORIZED, "UNAUTHORIZED", self.to_string()),
            Self::Internal(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "INTERNAL_ERROR",
                self.to_string(),
            ),
            Self::TmuxError(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "TMUX_ERROR",
                self.to_string(),
            ),
            Self::SessionNotFound(_) => {
                (StatusCode::NOT_FOUND, "SESSION_NOT_FOUND", self.to_string())
            }
            Self::SessionTimeout(_) => (
                StatusCode::GATEWAY_TIMEOUT,
                "SESSION_TIMEOUT",
                self.to_string(),
            ),
        };

        let body = Json(json!({
            "error": {
                "code": code,
                "message": message,
            }
        }));

        (status, body).into_response()
    }
}

impl From<reqwest::Error> for GatewayError {
    fn from(e: reqwest::Error) -> Self {
        Self::UpstreamError(e.to_string())
    }
}

impl From<anyhow::Error> for GatewayError {
    fn from(e: anyhow::Error) -> Self {
        Self::Internal(e.to_string())
    }
}
