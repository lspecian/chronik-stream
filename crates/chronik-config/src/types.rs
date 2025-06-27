//! Configuration types and structures.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Generic configuration value that can hold different types
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ConfigValue {
    Null,
    Bool(bool),
    Integer(i64),
    Float(f64),
    String(String),
    Array(Vec<ConfigValue>),
    Object(HashMap<String, ConfigValue>),
}

impl ConfigValue {
    /// Try to get value as string
    pub fn as_str(&self) -> Option<&str> {
        match self {
            ConfigValue::String(s) => Some(s),
            _ => None,
        }
    }
    
    /// Try to get value as integer
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            ConfigValue::Integer(i) => Some(*i),
            _ => None,
        }
    }
    
    /// Try to get value as float
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            ConfigValue::Float(f) => Some(*f),
            ConfigValue::Integer(i) => Some(*i as f64),
            _ => None,
        }
    }
    
    /// Try to get value as bool
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            ConfigValue::Bool(b) => Some(*b),
            _ => None,
        }
    }
    
    /// Try to get value as array
    pub fn as_array(&self) -> Option<&Vec<ConfigValue>> {
        match self {
            ConfigValue::Array(a) => Some(a),
            _ => None,
        }
    }
    
    /// Try to get value as object
    pub fn as_object(&self) -> Option<&HashMap<String, ConfigValue>> {
        match self {
            ConfigValue::Object(o) => Some(o),
            _ => None,
        }
    }
    
    /// Get nested value by dot-separated path
    pub fn get_path(&self, path: &str) -> Option<&ConfigValue> {
        let parts: Vec<&str> = path.split('.').collect();
        let mut current = self;
        
        for part in parts {
            match current {
                ConfigValue::Object(map) => {
                    current = map.get(part)?;
                }
                _ => return None,
            }
        }
        
        Some(current)
    }
}

/// Configuration source information
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ConfigSource {
    /// Source type (file, env, api, etc.)
    pub source_type: SourceType,
    /// Source identifier (file path, env prefix, etc.)
    pub identifier: String,
    /// Priority (higher values override lower)
    pub priority: i32,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SourceType {
    File,
    Environment,
    Api,
    Default,
}

/// Configuration metadata
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigMetadata {
    /// When the configuration was loaded
    pub loaded_at: chrono::DateTime<chrono::Utc>,
    /// Version of the configuration
    pub version: Option<String>,
    /// Sources that contributed to this configuration
    pub sources: Vec<ConfigSource>,
    /// Checksum of the configuration
    pub checksum: Option<String>,
}

impl Default for ConfigMetadata {
    fn default() -> Self {
        Self {
            loaded_at: chrono::Utc::now(),
            version: None,
            sources: Vec::new(),
            checksum: None,
        }
    }
}