use crate::error::GatewayError;
use evo_common::config::{GatewayConfig, ProviderConfig};
use reqwest::Client;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Shared application state, wrapped in Arc for thread-safe access across handlers.
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<RwLock<GatewayConfig>>,
    pub http_client: Arc<Client>,
}

impl AppState {
    pub fn new(config: GatewayConfig) -> Self {
        let http_client = Client::builder()
            .timeout(std::time::Duration::from_secs(60))
            .build()
            .expect("Failed to build HTTP client");

        Self {
            config: Arc::new(RwLock::new(config)),
            http_client: Arc::new(http_client),
        }
    }

    /// Get a provider by name, returning error if not found or disabled.
    pub async fn get_provider(&self, name: &str) -> Result<ProviderConfig, GatewayError> {
        let config = self.config.read().await;
        config
            .providers
            .iter()
            .find(|p| p.name == name)
            .cloned()
            .ok_or_else(|| GatewayError::ProviderNotFound(name.to_string()))
            .and_then(|p| {
                if p.enabled {
                    Ok(p)
                } else {
                    Err(GatewayError::ProviderDisabled(name.to_string()))
                }
            })
    }

    /// Get the first enabled provider matching any of the given names, in priority order.
    pub async fn get_preferred_provider(
        &self,
        preferred: &[&str],
    ) -> Result<ProviderConfig, GatewayError> {
        let config = self.config.read().await;
        for name in preferred {
            if let Some(p) = config.providers.iter().find(|p| p.name == *name && p.enabled) {
                return Ok(p.clone());
            }
        }
        // Fallback to any enabled provider
        config
            .providers
            .iter()
            .find(|p| p.enabled)
            .cloned()
            .ok_or_else(|| GatewayError::ProviderNotFound("any enabled provider".to_string()))
    }

    /// Reload config in-place (called by king after config swap via /reload endpoint).
    #[allow(dead_code)]
    pub async fn reload_config(&self, new_config: GatewayConfig) {
        let mut config = self.config.write().await;
        *config = new_config;
    }
}
