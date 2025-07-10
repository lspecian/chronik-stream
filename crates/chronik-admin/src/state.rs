//! Application state.

use crate::config::AdminConfig;
use chronik_auth::{JwtManager, JwtConfig};
use chronik_controller::client::ControllerClient;
use chronik_common::metadata::{MetadataStore, TiKVMetadataStore};
use std::sync::Arc;

/// Application state
#[derive(Clone)]
pub struct AppState {
    /// Configuration
    pub config: Arc<AdminConfig>,
    
    /// Metadata store
    pub metadata_store: Arc<dyn MetadataStore>,
    
    /// Controller client
    pub controller: Arc<ControllerClient>,
    
    /// JWT manager
    pub jwt_manager: Arc<JwtManager>,
}

impl AppState {
    /// Create new app state
    pub async fn new(config: AdminConfig) -> anyhow::Result<Self> {
        // Create metadata store using TiKV
        let pd_endpoints = std::env::var("TIKV_PD_ENDPOINTS")
            .unwrap_or_else(|_| "localhost:2379".to_string());
        let endpoints = pd_endpoints
            .split(',')
            .map(|s| s.to_string())
            .collect::<Vec<String>>();
        let metadata_store: Arc<dyn MetadataStore> = Arc::new(TiKVMetadataStore::new(endpoints).await?);
        
        // Initialize metadata store
        metadata_store.init_system_state().await?;
        
        // Create controller client
        let controller = Arc::new(
            ControllerClient::new(config.controller.endpoints.clone())
        );
        
        // Create JWT manager
        let jwt_config = JwtConfig {
            secret: config.auth.jwt_secret.clone(),
            expiration_secs: config.auth.token_expiration_secs,
            issuer: "chronik-admin".to_string(),
            audience: "chronik-api".to_string(),
        };
        let jwt_manager = Arc::new(JwtManager::new(jwt_config));
        
        Ok(Self {
            config: Arc::new(config),
            metadata_store,
            controller,
            jwt_manager,
        })
    }
}