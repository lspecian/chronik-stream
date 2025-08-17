//! Configuration manager for dynamic updates.

use crate::{
    provider::{ConfigProvider, FileProvider},
    types::{ConfigValue, ConfigMetadata, ConfigSource},
    validation::ConfigValidator,
    watcher::ConfigWatcher,
    Result, ConfigError,
};
use arc_swap::ArcSwap;
use dashmap::DashMap;
use std::{
    collections::HashMap,
    path::Path,
    sync::Arc,
};
use tokio::sync::{broadcast, RwLock};
use tracing::{debug, error, info, warn};

/// Configuration update notification
#[derive(Debug, Clone)]
pub struct ConfigUpdate {
    /// Path that was updated (dot-separated)
    pub path: String,
    /// Old value (if any)
    pub old_value: Option<ConfigValue>,
    /// New value
    pub new_value: ConfigValue,
    /// Update timestamp
    pub timestamp: chrono::DateTime<chrono::Utc>,
}

/// Dynamic configuration manager
pub struct ConfigManager {
    /// Current configuration
    config: Arc<ArcSwap<ConfigValue>>,
    /// Configuration metadata
    metadata: Arc<RwLock<ConfigMetadata>>,
    /// Configuration providers
    providers: Vec<Box<dyn ConfigProvider>>,
    /// Validators
    validators: Vec<Box<dyn ConfigValidator>>,
    /// Update channel
    update_tx: broadcast::Sender<ConfigUpdate>,
    /// Watchers for auto-reload
    watchers: Arc<RwLock<Vec<ConfigWatcher>>>,
    /// Configuration cache by path
    cache: Arc<DashMap<String, ConfigValue>>,
}

impl ConfigManager {
    /// Create a new configuration manager
    pub fn new() -> Self {
        let (update_tx, _) = broadcast::channel(1024);
        
        Self {
            config: Arc::new(ArcSwap::from_pointee(ConfigValue::Object(HashMap::new()))),
            metadata: Arc::new(RwLock::new(ConfigMetadata::default())),
            providers: Vec::new(),
            validators: Vec::new(),
            update_tx,
            watchers: Arc::new(RwLock::new(Vec::new())),
            cache: Arc::new(DashMap::new()),
        }
    }
    
    /// Add a configuration provider
    pub fn add_provider(&mut self, provider: Box<dyn ConfigProvider>) {
        self.providers.push(provider);
    }
    
    /// Add a configuration validator
    pub fn add_validator(&mut self, validator: Box<dyn ConfigValidator>) {
        self.validators.push(validator);
    }
    
    /// Load configuration from all providers
    pub async fn load(&self) -> Result<()> {
        let mut merged_config = ConfigValue::Object(HashMap::new());
        let mut sources = Vec::new();
        
        // Load from each provider in order
        for provider in &self.providers {
            match provider.load().await {
                Ok(config) => {
                    info!("Loaded configuration from {:?}", provider.source());
                    sources.push(provider.source());
                    merged_config = merge_configs(merged_config, config);
                }
                Err(e) => {
                    warn!("Failed to load from {:?}: {}", provider.source(), e);
                }
            }
        }
        
        // Validate the merged configuration
        for validator in &self.validators {
            validator.validate(&merged_config)?;
        }
        
        // Update metadata
        let mut metadata = self.metadata.write().await;
        metadata.loaded_at = chrono::Utc::now();
        metadata.sources = sources;
        
        // Store the new configuration
        let old_config = self.config.load();
        self.config.store(Arc::new(merged_config.clone()));
        
        // Clear cache
        self.cache.clear();
        
        // Notify listeners of changes
        self.notify_changes(&old_config, &merged_config).await;
        
        info!("Configuration loaded successfully");
        Ok(())
    }
    
    /// Reload configuration
    pub async fn reload(&self) -> Result<()> {
        info!("Reloading configuration...");
        self.load().await
    }
    
    /// Get configuration value by path
    pub fn get(&self, path: &str) -> Option<ConfigValue> {
        // Check cache first
        if let Some(cached) = self.cache.get(path) {
            return Some(cached.clone());
        }
        
        // Get from configuration
        let config = self.config.load();
        if let Some(value) = config.get_path(path) {
            let cloned = value.clone();
            self.cache.insert(path.to_string(), cloned.clone());
            Some(cloned)
        } else {
            None
        }
    }
    
    /// Get configuration value as string
    pub fn get_string(&self, path: &str) -> Option<String> {
        self.get(path).and_then(|v| v.as_str().map(|s| s.to_string()))
    }
    
    /// Get configuration value as integer
    pub fn get_i64(&self, path: &str) -> Option<i64> {
        self.get(path).and_then(|v| v.as_i64())
    }
    
    /// Get configuration value as float
    pub fn get_f64(&self, path: &str) -> Option<f64> {
        self.get(path).and_then(|v| v.as_f64())
    }
    
    /// Get configuration value as bool
    pub fn get_bool(&self, path: &str) -> Option<bool> {
        self.get(path).and_then(|v| v.as_bool())
    }
    
    /// Update configuration value at runtime
    pub async fn set(&self, path: &str, value: ConfigValue) -> Result<()> {
        let mut config = self.config.load().as_ref().clone();
        
        // Parse path and update value
        let parts: Vec<&str> = path.split('.').collect();
        if let ConfigValue::Object(ref mut map) = config {
            set_nested_value(map, &parts, value.clone())?;
        } else {
            return Err(ConfigError::TypeMismatch {
                expected: "object".to_string(),
                actual: format!("{:?}", config),
            });
        }
        
        // Validate the updated configuration
        for validator in &self.validators {
            validator.validate(&config)?;
        }
        
        // Store the updated configuration
        let old_config = self.config.load();
        self.config.store(Arc::new(config));
        
        // Clear cache for this path and its parents
        self.invalidate_cache(path);
        
        // Notify listeners
        self.notify_changes(&old_config, &config).await;
        
        info!("Configuration updated: {} = {:?}", path, value);
        Ok(())
    }
    
    /// Watch a file for changes
    pub async fn watch_file(&self, path: impl AsRef<Path>) -> Result<()> {
        let path = path.as_ref();
        let watcher = ConfigWatcher::new(path)?;
        
        // Clone necessary components for the watcher task
        let manager = self.clone_for_watcher();
        let file_path = path.to_path_buf();
        
        // Start watching
        watcher.watch(move || {
            let manager = manager.clone();
            let file_path = file_path.clone();
            
            tokio::spawn(async move {
                info!("Configuration file changed: {:?}", file_path);
                if let Err(e) = manager.reload().await {
                    error!("Failed to reload configuration: {}", e);
                }
            });
        });
        
        self.watchers.write().await.push(watcher);
        info!("Watching configuration file: {:?}", path);
        Ok(())
    }
    
    /// Subscribe to configuration updates
    pub fn subscribe(&self) -> broadcast::Receiver<ConfigUpdate> {
        self.update_tx.subscribe()
    }
    
    /// Get current configuration metadata
    pub async fn metadata(&self) -> ConfigMetadata {
        self.metadata.read().await.clone()
    }
    
    /// Export current configuration
    pub fn export(&self) -> ConfigValue {
        self.config.load().as_ref().clone()
    }
    
    /// Invalidate cache for a path and its parents
    fn invalidate_cache(&self, path: &str) {
        self.cache.remove(path);
        
        // Also invalidate parent paths
        let parts: Vec<&str> = path.split('.').collect();
        for i in 1..parts.len() {
            let parent_path = parts[..i].join(".");
            self.cache.remove(&parent_path);
        }
    }
    
    /// Notify listeners of configuration changes
    async fn notify_changes(&self, old_config: &ConfigValue, new_config: &ConfigValue) {
        let changes = find_changes(old_config, new_config, "");
        
        for (path, old_value, new_value) in changes {
            let update = ConfigUpdate {
                path,
                old_value,
                new_value,
                timestamp: chrono::Utc::now(),
            };
            
            // Send update, ignoring if no receivers
            let _ = self.update_tx.send(update);
        }
    }
    
    /// Clone manager components for watcher
    fn clone_for_watcher(&self) -> Arc<Self> {
        // This is a simplified approach - in production you'd want
        // a more sophisticated way to share the manager
        unimplemented!("Implement proper cloning for watcher")
    }
}

/// Merge two configurations, with the second overriding the first
fn merge_configs(mut base: ConfigValue, overlay: ConfigValue) -> ConfigValue {
    match (&mut base, overlay) {
        (ConfigValue::Object(base_map), ConfigValue::Object(overlay_map)) => {
            for (key, value) in overlay_map {
                match base_map.get_mut(&key) {
                    Some(base_value) => {
                        *base_value = merge_configs(base_value.clone(), value);
                    }
                    None => {
                        base_map.insert(key, value);
                    }
                }
            }
            base
        }
        (_, overlay) => overlay,
    }
}

/// Set a nested value in a configuration object
fn set_nested_value(
    map: &mut HashMap<String, ConfigValue>,
    path: &[&str],
    value: ConfigValue,
) -> Result<()> {
    if path.is_empty() {
        return Err(ConfigError::Parse("Empty path".to_string()));
    }
    
    if path.len() == 1 {
        map.insert(path[0].to_string(), value);
        return Ok(());
    }
    
    let key = path[0];
    let remaining_path = &path[1..];
    
    match map.get_mut(key) {
        Some(ConfigValue::Object(nested_map)) => {
            set_nested_value(nested_map, remaining_path, value)
        }
        Some(_) => {
            // Replace non-object with object
            let mut new_map = HashMap::new();
            set_nested_value(&mut new_map, remaining_path, value)?;
            map.insert(key.to_string(), ConfigValue::Object(new_map));
            Ok(())
        }
        None => {
            // Create new nested structure
            let mut new_map = HashMap::new();
            set_nested_value(&mut new_map, remaining_path, value)?;
            map.insert(key.to_string(), ConfigValue::Object(new_map));
            Ok(())
        }
    }
}

/// Find differences between two configurations
fn find_changes(
    old: &ConfigValue,
    new: &ConfigValue,
    path_prefix: &str,
) -> Vec<(String, Option<ConfigValue>, ConfigValue)> {
    let mut changes = Vec::new();
    
    match (old, new) {
        (ConfigValue::Object(old_map), ConfigValue::Object(new_map)) => {
            // Check for modified or new keys
            for (key, new_value) in new_map {
                let path = if path_prefix.is_empty() {
                    key.clone()
                } else {
                    format!("{}.{}", path_prefix, key)
                };
                
                match old_map.get(key) {
                    Some(old_value) if old_value != new_value => {
                        changes.push((path.clone(), Some(old_value.clone()), new_value.clone()));
                        // Recurse for nested changes
                        changes.extend(find_changes(old_value, new_value, &path));
                    }
                    None => {
                        changes.push((path, None, new_value.clone()));
                    }
                    _ => {}
                }
            }
            
            // Check for removed keys
            for (key, old_value) in old_map {
                if !new_map.contains_key(key) {
                    let path = if path_prefix.is_empty() {
                        key.clone()
                    } else {
                        format!("{}.{}", path_prefix, key)
                    };
                    changes.push((path, Some(old_value.clone()), ConfigValue::Null));
                }
            }
        }
        _ if old != new => {
            changes.push((path_prefix.to_string(), Some(old.clone()), new.clone()));
        }
        _ => {}
    }
    
    changes
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_merge_configs() {
        let base = ConfigValue::Object(HashMap::from([
            ("a".to_string(), ConfigValue::Integer(1)),
            ("b".to_string(), ConfigValue::Object(HashMap::from([
                ("c".to_string(), ConfigValue::String("hello".to_string())),
            ]))),
        ]));
        
        let overlay = ConfigValue::Object(HashMap::from([
            ("a".to_string(), ConfigValue::Integer(2)),
            ("b".to_string(), ConfigValue::Object(HashMap::from([
                ("d".to_string(), ConfigValue::String("world".to_string())),
            ]))),
            ("e".to_string(), ConfigValue::Bool(true)),
        ]));
        
        let merged = merge_configs(base, overlay);
        
        if let ConfigValue::Object(map) = merged {
            assert_eq!(map.get("a"), Some(&ConfigValue::Integer(2)));
            assert_eq!(map.get("e"), Some(&ConfigValue::Bool(true)));
            
            if let Some(ConfigValue::Object(b_map)) = map.get("b") {
                assert_eq!(b_map.get("c"), Some(&ConfigValue::String("hello".to_string())));
                assert_eq!(b_map.get("d"), Some(&ConfigValue::String("world".to_string())));
            } else {
                panic!("Expected object at 'b'");
            }
        } else {
            panic!("Expected merged result to be an object");
        }
    }
}