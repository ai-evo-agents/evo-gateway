use crate::error::GatewayError;
use crate::github_copilot::CopilotAuth;
use crate::tmux::TmuxManager;
use evo_common::config::{
    GatewayConfig, ModelMetadata, ProviderConfig, ProviderType, ReliabilityConfig, RoutingConfig,
};
use reqwest::Client;
use socketioxide::SocketIo;
use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::sync::RwLock;

/// Round-robin pool of API tokens for a single provider.
pub struct TokenPool {
    tokens: Vec<String>,
    next: AtomicUsize,
}

impl TokenPool {
    /// Resolve env-var names to actual token values at startup.
    pub fn from_envs(envs: &[String]) -> Self {
        let tokens = envs
            .iter()
            .filter(|e| !e.is_empty())
            .filter_map(|e| std::env::var(e).ok())
            .collect();
        Self {
            tokens,
            next: AtomicUsize::new(0),
        }
    }

    /// Returns the next token in round-robin order, or `None` if the pool is empty.
    pub fn next_token(&self) -> Option<&str> {
        if self.tokens.is_empty() {
            return None;
        }
        let idx = self.next.fetch_add(1, Ordering::Relaxed) % self.tokens.len();
        Some(&self.tokens[idx])
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.tokens.is_empty()
    }

    pub fn len(&self) -> usize {
        self.tokens.len()
    }
}

/// One configured LLM provider with its token pool pre-resolved.
pub struct ProviderPool {
    pub config: ProviderConfig,
    pub token_pool: TokenPool,
}

impl ProviderPool {
    pub fn new(config: ProviderConfig) -> Self {
        let token_pool = TokenPool::from_envs(&config.api_key_envs);
        Self { config, token_pool }
    }

    pub fn name(&self) -> &str {
        &self.config.name
    }

    pub fn provider_type(&self) -> &ProviderType {
        &self.config.provider_type
    }
}

/// Shared application state, held behind `Arc` so handlers can clone cheaply.
#[derive(Clone)]
pub struct AppState {
    /// Provider pools keyed by provider name, protected for hot-reload.
    pub pools: Arc<RwLock<HashMap<String, Arc<ProviderPool>>>>,
    pub http_client: Arc<Client>,
    /// Cache of dynamically discovered CLI provider models.
    /// Key: provider name, Value: (discovered_at, model_ids).
    #[allow(clippy::type_complexity)]
    pub cli_models_cache: Arc<RwLock<HashMap<String, (Instant, Vec<String>)>>>,
    /// Tmux session manager (None if tmux unavailable or disabled).
    pub tmux_manager: Option<Arc<TmuxManager>>,
    /// Socket.IO handle for broadcasting session events.
    pub io: Option<SocketIo>,
    /// Reliability config for retry/fallback (None = single-attempt mode).
    pub reliability: Option<ReliabilityConfig>,
    /// Hint-based model routing config (None = no hint routing).
    pub routing: Option<RoutingConfig>,
    /// GitHub Copilot token manager (initialized when a GithubCopilot provider exists).
    pub copilot_auth: Option<Arc<CopilotAuth>>,
    /// Per-model WebSocket preference from WHAM discovery.
    /// Key: model slug (e.g. "gpt-5.3-codex"), Value: prefer_websockets flag.
    pub codex_auth_transport: Arc<RwLock<HashMap<String, bool>>>,
    /// WHAM-discovered model metadata (context_window, reasoning, etc.).
    /// Key: model slug, Value: metadata.
    pub codex_auth_model_metadata: Arc<RwLock<HashMap<String, ModelMetadata>>>,
}

impl AppState {
    pub fn new(
        config: GatewayConfig,
        tmux_manager: Option<Arc<TmuxManager>>,
        io: Option<SocketIo>,
    ) -> Self {
        // Initialize Copilot auth if any GithubCopilot provider is configured
        let copilot_auth = config
            .providers
            .iter()
            .any(|p| p.provider_type == ProviderType::GithubCopilot && p.enabled)
            .then(|| Arc::new(CopilotAuth::new()));

        let pools = build_pools(config.providers);
        let http_client = Client::builder()
            .connect_timeout(std::time::Duration::from_secs(30))
            .build()
            .expect("Failed to build HTTP client");

        Self {
            pools: Arc::new(RwLock::new(pools)),
            http_client: Arc::new(http_client),
            cli_models_cache: Arc::new(RwLock::new(HashMap::new())),
            tmux_manager,
            io,
            reliability: config.reliability,
            routing: config.routing,
            copilot_auth,
            codex_auth_transport: Arc::new(RwLock::new(HashMap::new())),
            codex_auth_model_metadata: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Look up a provider pool by exact name.
    pub async fn get_pool(&self, name: &str) -> Result<Arc<ProviderPool>, GatewayError> {
        let pools = self.pools.read().await;
        pools
            .get(name)
            .cloned()
            .ok_or_else(|| GatewayError::ProviderNotFound(name.to_string()))
            .and_then(|p| {
                if p.config.enabled {
                    Ok(p)
                } else {
                    Err(GatewayError::ProviderDisabled(p.name().to_string()))
                }
            })
    }

    /// Return the first available pool from a priority list, then any enabled pool.
    pub async fn get_preferred_pool(
        &self,
        preferred: &[&str],
    ) -> Result<Arc<ProviderPool>, GatewayError> {
        let pools = self.pools.read().await;
        for name in preferred {
            if let Some(p) = pools.get(*name)
                && p.config.enabled
            {
                return Ok(Arc::clone(p));
            }
        }
        pools
            .values()
            .find(|p| p.config.enabled)
            .cloned()
            .ok_or_else(|| GatewayError::ProviderNotFound("any enabled provider".to_string()))
    }

    /// All enabled pools for listing models.
    pub async fn all_enabled_pools(&self) -> Vec<Arc<ProviderPool>> {
        let pools = self.pools.read().await;
        pools
            .values()
            .filter(|p| p.config.enabled)
            .cloned()
            .collect()
    }

    /// Hot-reload: replace all pools from a new config (called by king).
    #[allow(dead_code)]
    pub async fn reload(&self, config: GatewayConfig) {
        let new_pools = build_pools(config.providers);
        let mut pools = self.pools.write().await;
        *pools = new_pools;
    }

    /// Get cached CLI models if still within TTL.
    pub async fn get_cached_models(&self, provider: &str, ttl: Duration) -> Option<Vec<String>> {
        let cache = self.cli_models_cache.read().await;
        cache.get(provider).and_then(|(discovered_at, models)| {
            if discovered_at.elapsed() < ttl && !models.is_empty() {
                Some(models.clone())
            } else {
                None
            }
        })
    }

    /// Store discovered CLI models in cache.
    pub async fn set_cached_models(&self, provider: &str, models: Vec<String>) {
        let mut cache = self.cli_models_cache.write().await;
        cache.insert(provider.to_string(), (Instant::now(), models));
    }

    /// Clear all cached model discovery data, forcing re-fetch on next request.
    pub async fn clear_all_cached_models(&self) {
        let mut cache = self.cli_models_cache.write().await;
        cache.clear();
    }

    /// Get WHAM transport preference for a codex-auth model.
    pub async fn get_model_transport(&self, model: &str) -> Option<bool> {
        self.codex_auth_transport.read().await.get(model).copied()
    }

    /// Store WHAM transport preferences for codex-auth models.
    pub async fn set_model_transports(&self, map: HashMap<String, bool>) {
        *self.codex_auth_transport.write().await = map;
    }

    /// Get WHAM-discovered metadata for a codex-auth model.
    #[allow(dead_code)]
    pub async fn get_codex_auth_metadata(&self, model: &str) -> Option<ModelMetadata> {
        self.codex_auth_model_metadata
            .read()
            .await
            .get(model)
            .cloned()
    }

    /// Store WHAM-discovered model metadata.
    pub async fn set_codex_auth_metadata(&self, map: HashMap<String, ModelMetadata>) {
        *self.codex_auth_model_metadata.write().await = map;
    }
}

fn build_pools(providers: Vec<ProviderConfig>) -> HashMap<String, Arc<ProviderPool>> {
    providers
        .into_iter()
        .map(|p| {
            let name = p.name.clone();
            (name, Arc::new(ProviderPool::new(p)))
        })
        .collect()
}
