//! Embedding provider factory.
//!
//! Creates the appropriate embedding provider based on configuration.

use anyhow::{anyhow, Result};
use std::sync::Arc;

use crate::config::{EmbeddingModelConfig, EmbeddingProvider as ProviderType, VectorSearchConfig};
use crate::external::ExternalProvider;
use crate::openai::OpenAIProvider;
use crate::provider::EmbeddingProvider;

/// Create an embedding provider from configuration.
///
/// # Arguments
/// * `config` - Vector search configuration with embedding settings
///
/// # Returns
/// A boxed provider implementing the EmbeddingProvider trait
///
/// # Example
/// ```ignore
/// let config = VectorSearchConfig::from_topic_config(&topic_config)?;
/// let provider = create_provider(&config)?;
/// let embedding = provider.embed("hello world").await?;
/// ```
pub fn create_provider(config: &VectorSearchConfig) -> Result<Arc<dyn EmbeddingProvider>> {
    create_provider_from_model_config(&config.embedding)
}

/// Create a provider from model configuration.
pub fn create_provider_from_model_config(
    config: &EmbeddingModelConfig,
) -> Result<Arc<dyn EmbeddingProvider>> {
    match config.provider {
        ProviderType::OpenAI => create_openai_provider(config),
        ProviderType::External => create_external_provider(config),
        ProviderType::Local => create_local_provider(config),
    }
}

/// Create OpenAI provider from config.
fn create_openai_provider(config: &EmbeddingModelConfig) -> Result<Arc<dyn EmbeddingProvider>> {
    // Get API key from environment
    let api_key = std::env::var("OPENAI_API_KEY")
        .map_err(|_| anyhow!("OPENAI_API_KEY environment variable not set"))?;

    let mut provider = OpenAIProvider::new(api_key, &config.model)?;

    // Apply custom dimensions if specified
    provider = provider.with_dimensions(config.dimensions);

    // Apply custom endpoint if specified (for Azure OpenAI or compatible APIs)
    if let Some(endpoint) = &config.endpoint {
        provider = provider.with_endpoint(endpoint);
    }

    Ok(Arc::new(provider))
}

/// Create External provider from config.
fn create_external_provider(config: &EmbeddingModelConfig) -> Result<Arc<dyn EmbeddingProvider>> {
    let endpoint = config
        .endpoint
        .as_ref()
        .ok_or_else(|| anyhow!("External provider requires vector.endpoint"))?;

    let mut provider = ExternalProvider::new(endpoint, config.dimensions)?
        .with_model(&config.model);

    // Check for API key in environment
    if let Ok(api_key) = std::env::var("EMBEDDING_API_KEY") {
        provider = provider.with_api_key(api_key);
    } else if let Ok(bearer_token) = std::env::var("EMBEDDING_BEARER_TOKEN") {
        provider = provider.with_bearer_token(bearer_token);
    }

    Ok(Arc::new(provider))
}

/// Create Local provider from config (requires local-models feature).
#[allow(unused_variables)]
fn create_local_provider(config: &EmbeddingModelConfig) -> Result<Arc<dyn EmbeddingProvider>> {
    #[cfg(feature = "local-models")]
    {
        use crate::local::LocalProvider;
        let provider = LocalProvider::new(&config.model)?;
        Ok(Arc::new(provider))
    }

    #[cfg(not(feature = "local-models"))]
    {
        Err(anyhow!(
            "Local provider requires 'local-models' feature. Rebuild with --features local-models"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn test_create_openai_provider_no_api_key() {
        // Clear any existing API key
        std::env::remove_var("OPENAI_API_KEY");

        let config = VectorSearchConfig::default();
        let result = create_provider(&config);

        assert!(result.is_err());
        let err = result.err().expect("Expected error");
        assert!(err.to_string().contains("OPENAI_API_KEY"));
    }

    #[test]
    fn test_create_openai_provider_with_api_key() {
        std::env::set_var("OPENAI_API_KEY", "sk-test-key");

        let config = VectorSearchConfig::default();
        let result = create_provider(&config);

        assert!(result.is_ok());
        let provider = result.unwrap();
        assert_eq!(provider.name(), "openai");
        assert_eq!(provider.dimensions(), 1536);

        std::env::remove_var("OPENAI_API_KEY");
    }

    #[test]
    fn test_create_external_provider_no_endpoint() {
        let mut topic_config = HashMap::new();
        topic_config.insert("vector.provider".to_string(), "external".to_string());

        let config = VectorSearchConfig::from_topic_config(&topic_config).unwrap();
        let result = create_provider(&config);

        assert!(result.is_err());
        let err = result.err().expect("Expected error");
        assert!(err.to_string().contains("endpoint"));
    }

    #[test]
    fn test_create_external_provider_with_endpoint() {
        let mut topic_config = HashMap::new();
        topic_config.insert("vector.provider".to_string(), "external".to_string());
        topic_config.insert("vector.endpoint".to_string(), "http://localhost:8000/embed".to_string());
        topic_config.insert("vector.dimensions".to_string(), "384".to_string());

        let config = VectorSearchConfig::from_topic_config(&topic_config).unwrap();
        let result = create_provider(&config);

        assert!(result.is_ok());
        let provider = result.unwrap();
        assert_eq!(provider.name(), "external");
        assert_eq!(provider.dimensions(), 384);
    }

    #[test]
    fn test_create_local_provider_without_feature() {
        let mut topic_config = HashMap::new();
        topic_config.insert("vector.provider".to_string(), "local".to_string());
        topic_config.insert("vector.model".to_string(), "all-MiniLM-L6-v2".to_string());

        let config = VectorSearchConfig::from_topic_config(&topic_config).unwrap();
        let result = create_provider(&config);

        // Without the feature flag, this should fail
        #[cfg(not(feature = "local-models"))]
        assert!(result.is_err());
    }

    #[test]
    fn test_create_openai_with_custom_dimensions() {
        std::env::set_var("OPENAI_API_KEY", "sk-test-key");

        let mut topic_config = HashMap::new();
        topic_config.insert("vector.provider".to_string(), "openai".to_string());
        topic_config.insert("vector.model".to_string(), "text-embedding-3-small".to_string());
        topic_config.insert("vector.dimensions".to_string(), "512".to_string());

        let config = VectorSearchConfig::from_topic_config(&topic_config).unwrap();
        let result = create_provider(&config);

        assert!(result.is_ok());
        let provider = result.unwrap();
        assert_eq!(provider.dimensions(), 512);

        std::env::remove_var("OPENAI_API_KEY");
    }
}
