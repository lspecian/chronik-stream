//! Configuration validation framework.

use crate::types::ConfigValue;
use std::collections::HashMap;
use thiserror::Error;
use validator::{Validate, ValidationError as ValidatorError};

#[derive(Debug, Error)]
pub enum ValidationError {
    #[error("Required field missing: {0}")]
    RequiredFieldMissing(String),
    
    #[error("Invalid type for field {field}: expected {expected}, got {actual}")]
    InvalidType {
        field: String,
        expected: String,
        actual: String,
    },
    
    #[error("Value out of range for field {field}: {message}")]
    OutOfRange { field: String, message: String },
    
    #[error("Invalid format for field {field}: {message}")]
    InvalidFormat { field: String, message: String },
    
    #[error("Custom validation failed: {0}")]
    Custom(String),
    
    #[error("Validation errors: {0:?}")]
    Multiple(Vec<ValidationError>),
}

/// Trait for configuration validators
pub trait ConfigValidator: Send + Sync {
    /// Validate a configuration
    fn validate(&self, config: &ConfigValue) -> Result<(), ValidationError>;
}

/// Schema-based validator
pub struct SchemaValidator {
    schema: Schema,
}

impl SchemaValidator {
    /// Create a new schema validator
    pub fn new(schema: Schema) -> Self {
        Self { schema }
    }
}

impl ConfigValidator for SchemaValidator {
    fn validate(&self, config: &ConfigValue) -> Result<(), ValidationError> {
        self.schema.validate(config)
    }
}

/// Configuration schema definition
#[derive(Debug, Clone)]
pub struct Schema {
    fields: HashMap<String, FieldSchema>,
}

impl Schema {
    /// Create a new schema
    pub fn new() -> Self {
        Self {
            fields: HashMap::new(),
        }
    }
    
    /// Add a field to the schema
    pub fn field(mut self, name: impl Into<String>, field: FieldSchema) -> Self {
        self.fields.insert(name.into(), field);
        self
    }
    
    /// Validate a configuration against this schema
    pub fn validate(&self, config: &ConfigValue) -> Result<(), ValidationError> {
        let mut errors = Vec::new();
        
        if let ConfigValue::Object(map) = config {
            // Check required fields
            for (name, field_schema) in &self.fields {
                if field_schema.required && !map.contains_key(name) {
                    errors.push(ValidationError::RequiredFieldMissing(name.clone()));
                    continue;
                }
                
                if let Some(value) = map.get(name) {
                    if let Err(e) = field_schema.validate(value, name) {
                        errors.push(e);
                    }
                }
            }
        } else {
            return Err(ValidationError::InvalidType {
                field: "root".to_string(),
                expected: "object".to_string(),
                actual: format!("{:?}", config),
            });
        }
        
        if errors.is_empty() {
            Ok(())
        } else if errors.len() == 1 {
            Err(errors.into_iter().next().unwrap())
        } else {
            Err(ValidationError::Multiple(errors))
        }
    }
}

/// Field schema definition
#[derive(Debug, Clone)]
pub struct FieldSchema {
    pub field_type: FieldType,
    pub required: bool,
    pub validators: Vec<Box<dyn FieldValidator>>,
}

impl FieldSchema {
    /// Create a new field schema
    pub fn new(field_type: FieldType) -> Self {
        Self {
            field_type,
            required: false,
            validators: Vec::new(),
        }
    }
    
    /// Set field as required
    pub fn required(mut self) -> Self {
        self.required = true;
        self
    }
    
    /// Add a validator
    pub fn validator(mut self, validator: Box<dyn FieldValidator>) -> Self {
        self.validators.push(validator);
        self
    }
    
    /// Validate a value against this field schema
    fn validate(&self, value: &ConfigValue, field_name: &str) -> Result<(), ValidationError> {
        // Check type
        if !self.field_type.matches(value) {
            return Err(ValidationError::InvalidType {
                field: field_name.to_string(),
                expected: self.field_type.to_string(),
                actual: format!("{:?}", value),
            });
        }
        
        // Run validators
        for validator in &self.validators {
            validator.validate(value, field_name)?;
        }
        
        Ok(())
    }
}

/// Field type enumeration
#[derive(Debug, Clone)]
pub enum FieldType {
    String,
    Integer,
    Float,
    Bool,
    Array(Box<FieldType>),
    Object(Schema),
}

impl FieldType {
    /// Check if a value matches this type
    fn matches(&self, value: &ConfigValue) -> bool {
        match (self, value) {
            (FieldType::String, ConfigValue::String(_)) => true,
            (FieldType::Integer, ConfigValue::Integer(_)) => true,
            (FieldType::Float, ConfigValue::Float(_)) => true,
            (FieldType::Float, ConfigValue::Integer(_)) => true, // Allow integer as float
            (FieldType::Bool, ConfigValue::Bool(_)) => true,
            (FieldType::Array(elem_type), ConfigValue::Array(arr)) => {
                arr.iter().all(|v| elem_type.matches(v))
            }
            (FieldType::Object(schema), ConfigValue::Object(_)) => {
                // Type check only, validation happens separately
                true
            }
            _ => false,
        }
    }
}

impl std::fmt::Display for FieldType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FieldType::String => write!(f, "string"),
            FieldType::Integer => write!(f, "integer"),
            FieldType::Float => write!(f, "float"),
            FieldType::Bool => write!(f, "bool"),
            FieldType::Array(elem_type) => write!(f, "array<{}>", elem_type),
            FieldType::Object(_) => write!(f, "object"),
        }
    }
}

/// Trait for field validators
pub trait FieldValidator: Send + Sync + std::fmt::Debug {
    /// Validate a field value
    fn validate(&self, value: &ConfigValue, field_name: &str) -> Result<(), ValidationError>;
    
    /// Clone the validator
    fn clone_box(&self) -> Box<dyn FieldValidator>;
}

impl Clone for Box<dyn FieldValidator> {
    fn clone(&self) -> Self {
        self.clone_box()
    }
}

/// Range validator for numeric fields
#[derive(Debug, Clone)]
pub struct RangeValidator {
    min: Option<f64>,
    max: Option<f64>,
}

impl RangeValidator {
    /// Create a new range validator
    pub fn new() -> Self {
        Self {
            min: None,
            max: None,
        }
    }
    
    /// Set minimum value
    pub fn min(mut self, min: f64) -> Self {
        self.min = Some(min);
        self
    }
    
    /// Set maximum value
    pub fn max(mut self, max: f64) -> Self {
        self.max = Some(max);
        self
    }
}

impl FieldValidator for RangeValidator {
    fn validate(&self, value: &ConfigValue, field_name: &str) -> Result<(), ValidationError> {
        let num = match value {
            ConfigValue::Integer(i) => *i as f64,
            ConfigValue::Float(f) => *f,
            _ => return Ok(()), // Skip non-numeric values
        };
        
        if let Some(min) = self.min {
            if num < min {
                return Err(ValidationError::OutOfRange {
                    field: field_name.to_string(),
                    message: format!("value {} is less than minimum {}", num, min),
                });
            }
        }
        
        if let Some(max) = self.max {
            if num > max {
                return Err(ValidationError::OutOfRange {
                    field: field_name.to_string(),
                    message: format!("value {} is greater than maximum {}", num, max),
                });
            }
        }
        
        Ok(())
    }
    
    fn clone_box(&self) -> Box<dyn FieldValidator> {
        Box::new(self.clone())
    }
}

/// Pattern validator for string fields
#[derive(Debug, Clone)]
pub struct PatternValidator {
    pattern: regex::Regex,
    message: String,
}

impl PatternValidator {
    /// Create a new pattern validator
    pub fn new(pattern: &str, message: impl Into<String>) -> Result<Self, regex::Error> {
        Ok(Self {
            pattern: regex::Regex::new(pattern)?,
            message: message.into(),
        })
    }
}

impl FieldValidator for PatternValidator {
    fn validate(&self, value: &ConfigValue, field_name: &str) -> Result<(), ValidationError> {
        if let ConfigValue::String(s) = value {
            if !self.pattern.is_match(s) {
                return Err(ValidationError::InvalidFormat {
                    field: field_name.to_string(),
                    message: self.message.clone(),
                });
            }
        }
        Ok(())
    }
    
    fn clone_box(&self) -> Box<dyn FieldValidator> {
        Box::new(self.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_schema_validation() {
        let schema = Schema::new()
            .field("name", FieldSchema::new(FieldType::String).required())
            .field("port", FieldSchema::new(FieldType::Integer)
                .required()
                .validator(Box::new(RangeValidator::new().min(1.0).max(65535.0))))
            .field("enabled", FieldSchema::new(FieldType::Bool));
        
        // Valid configuration
        let valid_config = ConfigValue::Object(HashMap::from([
            ("name".to_string(), ConfigValue::String("test".to_string())),
            ("port".to_string(), ConfigValue::Integer(8080)),
            ("enabled".to_string(), ConfigValue::Bool(true)),
        ]));
        
        assert!(schema.validate(&valid_config).is_ok());
        
        // Missing required field
        let missing_field = ConfigValue::Object(HashMap::from([
            ("port".to_string(), ConfigValue::Integer(8080)),
        ]));
        
        assert!(matches!(
            schema.validate(&missing_field),
            Err(ValidationError::RequiredFieldMissing(_))
        ));
        
        // Invalid port range
        let invalid_port = ConfigValue::Object(HashMap::from([
            ("name".to_string(), ConfigValue::String("test".to_string())),
            ("port".to_string(), ConfigValue::Integer(70000)),
        ]));
        
        assert!(matches!(
            schema.validate(&invalid_port),
            Err(ValidationError::OutOfRange { .. })
        ));
    }
}