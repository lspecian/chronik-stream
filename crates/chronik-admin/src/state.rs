//! Application state.

use crate::config::AdminConfig;
use chronik_auth::{JwtManager, JwtConfig};
use chronik_controller::client::ControllerClient;
use chronik_common::metadata::{MetadataStore, SledMetadataStore};
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
        // Create metadata store
        let metadata_path = std::env::var("METADATA_PATH")
            .unwrap_or_else(|_| config.database.url.clone());
        let metadata_store = Arc::new(SledMetadataStore::new(&metadata_path)?) as Arc<dyn MetadataStore>;
        
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