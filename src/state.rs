use crate::error::GatewayError;
use evo_common::config::{GatewayConfig, ProviderConfig, ProviderType};
use reqwest::Client;
use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
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
}

impl AppState {
    pub fn new(config: GatewayConfig) -> Self {
        let pools = build_pools(config.providers);
        let http_client = Client::builder()
            .timeout(std::time::Duration::from_secs(60))
            .build()
            .expect("Failed to build HTTP client");

        Self {
            pools: Arc::new(RwLock::new(pools)),
            http_client: Arc::new(http_client),
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
