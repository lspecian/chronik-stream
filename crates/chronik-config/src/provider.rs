//! Configuration providers for different sources.

use crate::{
    types::{ConfigValue, ConfigSource, SourceType},
    Result, ConfigError,
};
use async_trait::async_trait;
use std::{
    collections::HashMap,
    env,
    path::{Path, PathBuf},
};
use tokio::fs;
use tracing::{debug, info};

/// Trait for configuration providers
#[async_trait]
pub trait ConfigProvider: Send + Sync {
    /// Load configuration from this provider
    async fn load(&self) -> Result<ConfigValue>;
    
    /// Get source information
    fn source(&self) -> ConfigSource;
}

/// File-based configuration provider
pub struct FileProvider {
    path: PathBuf,
    format: FileFormat,
    priority: i32,
}

#[derive(Debug, Clone, Copy)]
pub enum FileFormat {
    Json,
    Yaml,
    Toml,
}

impl FileProvider {
    /// Create a new file provider
    pub fn new(path: impl AsRef<Path>, priority: i32) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let format = detect_format(&path)?;
        
        Ok(Self {
            path,
            format,
            priority,
        })
    }
    
    /// Create with explicit format
    pub fn with_format(path: impl AsRef<Path>, format: FileFormat, priority: i32) -> Self {
        Self {
            path: path.as_ref().to_path_buf(),
            format,
            priority,
        }
    }
}

#[async_trait]
impl ConfigProvider for FileProvider {
    async fn load(&self) -> Result<ConfigValue> {
        let content = fs::read_to_string(&self.path).await?;
        
        let value = match self.format {
            FileFormat::Json => {
                let json: serde_json::Value = serde_json::from_str(&content)
                    .map_err(|e| ConfigError::Parse(e.to_string()))?;
                json_to_config_value(json)
            }
            FileFormat::Yaml => {
                let yaml: serde_yaml::Value = serde_yaml::from_str(&content)
                    .map_err(|e| ConfigError::Parse(e.to_string()))?;
                yaml_to_config_value(yaml)
            }
            FileFormat::Toml => {
                let toml: toml::Value = toml::from_str(&content)
                    .map_err(|e| ConfigError::Parse(e.to_string()))?;
                toml_to_config_value(toml)
            }
        };
        
        info!("Loaded configuration from {:?}", self.path);
        Ok(value)
    }
    
    fn source(&self) -> ConfigSource {
        ConfigSource {
            source_type: SourceType::File,
            identifier: self.path.display().to_string(),
            priority: self.priority,
        }
    }
}

/// Environment variable configuration provider
pub struct EnvironmentProvider {
    prefix: String,
    separator: String,
    priority: i32,
}

impl EnvironmentProvider {
    /// Create a new environment provider
    pub fn new(prefix: impl Into<String>, priority: i32) -> Self {
        Self {
            prefix: prefix.into(),
            separator: "__".to_string(),
            priority,
        }
    }
    
    /// Set the separator for nested keys
    pub fn with_separator(mut self, separator: impl Into<String>) -> Self {
        self.separator = separator.into();
        self
    }
}

#[async_trait]
impl ConfigProvider for EnvironmentProvider {
    async fn load(&self) -> Result<ConfigValue> {
        let mut config = HashMap::new();
        
        for (key, value) in env::vars() {
            if key.starts_with(&self.prefix) {
                let key_without_prefix = key.strip_prefix(&self.prefix)
                    .unwrap()
                    .trim_start_matches('_');
                
                if !key_without_prefix.is_empty() {
                    let path_parts: Vec<String> = key_without_prefix
                        .split(&self.separator)
                        .map(|s| s.to_lowercase())
                        .collect();
                    
                    let path_refs: Vec<&str> = path_parts.iter().map(|s| s.as_str()).collect();
                    insert_nested_value(&mut config, &path_refs, parse_env_value(&value));
                    debug!("Loaded env var: {} = {}", key, value);
                }
            }
        }
        
        info!("Loaded {} environment variables with prefix '{}'", 
              config.len(), self.prefix);
        Ok(ConfigValue::Object(config))
    }
    
    fn source(&self) -> ConfigSource {
        ConfigSource {
            source_type: SourceType::Environment,
            identifier: self.prefix.clone(),
            priority: self.priority,
        }
    }
}

/// Detect file format from extension
fn detect_format(path: &Path) -> Result<FileFormat> {
    match path.extension().and_then(|ext| ext.to_str()) {
        Some("json") => Ok(FileFormat::Json),
        Some("yaml") | Some("yml") => Ok(FileFormat::Yaml),
        Some("toml") => Ok(FileFormat::Toml),
        _ => Err(ConfigError::Parse(
            format!("Unknown file format for {:?}", path)
        )),
    }
}

/// Convert JSON value to ConfigValue
fn json_to_config_value(json: serde_json::Value) -> ConfigValue {
    match json {
        serde_json::Value::Null => ConfigValue::Null,
        serde_json::Value::Bool(b) => ConfigValue::Bool(b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                ConfigValue::Integer(i)
            } else if let Some(f) = n.as_f64() {
                ConfigValue::Float(f)
            } else {
                ConfigValue::String(n.to_string())
            }
        }
        serde_json::Value::String(s) => ConfigValue::String(s),
        serde_json::Value::Array(arr) => {
            ConfigValue::Array(arr.into_iter().map(json_to_config_value).collect())
        }
        serde_json::Value::Object(obj) => {
            ConfigValue::Object(
                obj.into_iter()
                    .map(|(k, v)| (k, json_to_config_value(v)))
                    .collect()
            )
        }
    }
}

/// Convert YAML value to ConfigValue
fn yaml_to_config_value(yaml: serde_yaml::Value) -> ConfigValue {
    match yaml {
        serde_yaml::Value::Null => ConfigValue::Null,
        serde_yaml::Value::Bool(b) => ConfigValue::Bool(b),
        serde_yaml::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                ConfigValue::Integer(i)
            } else if let Some(f) = n.as_f64() {
                ConfigValue::Float(f)
            } else {
                ConfigValue::String(n.to_string())
            }
        }
        serde_yaml::Value::String(s) => ConfigValue::String(s),
        serde_yaml::Value::Sequence(seq) => {
            ConfigValue::Array(seq.into_iter().map(yaml_to_config_value).collect())
        }
        serde_yaml::Value::Mapping(map) => {
            let mut result = HashMap::new();
            for (k, v) in map {
                if let serde_yaml::Value::String(key) = k {
                    result.insert(key, yaml_to_config_value(v));
                }
            }
            ConfigValue::Object(result)
        }
        _ => ConfigValue::Null,
    }
}

/// Convert TOML value to ConfigValue
fn toml_to_config_value(toml: toml::Value) -> ConfigValue {
    match toml {
        toml::Value::Boolean(b) => ConfigValue::Bool(b),
        toml::Value::Integer(i) => ConfigValue::Integer(i),
        toml::Value::Float(f) => ConfigValue::Float(f),
        toml::Value::String(s) => ConfigValue::String(s),
        toml::Value::Datetime(dt) => ConfigValue::String(dt.to_string()),
        toml::Value::Array(arr) => {
            ConfigValue::Array(arr.into_iter().map(toml_to_config_value).collect())
        }
        toml::Value::Table(table) => {
            ConfigValue::Object(
                table.into_iter()
                    .map(|(k, v)| (k, toml_to_config_value(v)))
                    .collect()
            )
        }
    }
}

/// Parse environment variable value
fn parse_env_value(value: &str) -> ConfigValue {
    // Try to parse as bool
    if let Ok(b) = value.parse::<bool>() {
        return ConfigValue::Bool(b);
    }
    
    // Try to parse as integer
    if let Ok(i) = value.parse::<i64>() {
        return ConfigValue::Integer(i);
    }
    
    // Try to parse as float
    if let Ok(f) = value.parse::<f64>() {
        return ConfigValue::Float(f);
    }
    
    // Try to parse as JSON array or object
    if value.starts_with('[') || value.starts_with('{') {
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(value) {
            return json_to_config_value(json);
        }
    }
    
    // Default to string
    ConfigValue::String(value.to_string())
}

/// Insert a value into a nested structure
fn insert_nested_value(
    map: &mut HashMap<String, ConfigValue>,
    path: &[&str],
    value: ConfigValue,
) {
    if path.is_empty() {
        return;
    }
    
    if path.len() == 1 {
        map.insert(path[0].to_string(), value);
        return;
    }
    
    let key = path[0].to_string();
    let remaining_path = &path[1..];
    
    match map.get_mut(&key) {
        Some(ConfigValue::Object(nested_map)) => {
            insert_nested_value(nested_map, remaining_path, value);
        }
        _ => {
            let mut new_map = HashMap::new();
            insert_nested_value(&mut new_map, remaining_path, value);
            map.insert(key, ConfigValue::Object(new_map));
        }
    }
}