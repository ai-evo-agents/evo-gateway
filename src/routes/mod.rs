pub mod anthropic;
pub mod local_llm;
pub mod openai;

use crate::state::AppState;
use axum::{
    Router,
    routing::{get, post},
};
use std::sync::Arc;

/// Returns a router with all API routes.
/// State is NOT applied here — caller applies `.with_state()` at the top level.
pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        // OpenAI-compatible endpoints
        .route("/v1/chat/completions", post(openai::chat_completions))
        .route("/v1/embeddings", post(openai::embeddings))
        .route("/v1/models", get(openai::list_models))
        .route("/v1/models/{provider}", get(openai::list_provider_models))
        // Anthropic-compatible endpoint
        .route("/v1/messages", post(anthropic::messages))
        // Local LLM endpoints (Ollama-compatible)
        .route("/api/generate", post(local_llm::generate))
        .route("/api/chat", post(local_llm::chat))
}
