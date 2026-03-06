//! JSON Schema Inference for Columnar Storage
//!
//! Automatically infers Arrow schema from JSON message values, enabling
//! typed columns instead of raw `_value` binary blobs.
//!
//! ## Usage
//!
//! ```rust,ignore
//! let schema = InferredJsonSchema::infer(&values, 64)?;
//! let fields = schema.to_arrow_fields();
//! // fields: [Field("user_id", Int64), Field("name", Utf8), ...]
//! ```

use std::collections::BTreeMap;

use arrow_schema::{DataType, Field};
use serde_json::Value;

/// Maximum number of records to sample for schema inference.
const DEFAULT_SAMPLE_SIZE: usize = 100;

/// Maximum number of JSON fields to include in the inferred schema.
const DEFAULT_MAX_FIELDS: usize = 64;

/// Inferred type for a JSON field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JsonFieldType {
    /// JSON string → Arrow Utf8
    String,
    /// JSON integer → Arrow Int64
    Int64,
    /// JSON float → Arrow Float64
    Float64,
    /// JSON boolean → Arrow Boolean
    Boolean,
    /// Nested object → serialized as Arrow Utf8 (JSON string)
    Object,
    /// Array → serialized as Arrow Utf8 (JSON string)
    Array,
}

impl JsonFieldType {
    /// Convert to Arrow DataType.
    pub fn to_arrow_type(&self) -> DataType {
        match self {
            JsonFieldType::String => DataType::Utf8,
            JsonFieldType::Int64 => DataType::Int64,
            JsonFieldType::Float64 => DataType::Float64,
            JsonFieldType::Boolean => DataType::Boolean,
            JsonFieldType::Object => DataType::Utf8,
            JsonFieldType::Array => DataType::Utf8,
        }
    }

    /// Widen type when conflicting types are detected across samples.
    /// Int64 + Float64 → Float64, anything + String → String.
    fn widen(&self, other: &JsonFieldType) -> JsonFieldType {
        if self == other {
            return self.clone();
        }
        match (self, other) {
            (JsonFieldType::Int64, JsonFieldType::Float64)
            | (JsonFieldType::Float64, JsonFieldType::Int64) => JsonFieldType::Float64,
            _ => JsonFieldType::String,
        }
    }
}

/// Inferred field: name, type, and nullability.
#[derive(Debug, Clone)]
pub struct InferredField {
    pub name: String,
    pub field_type: JsonFieldType,
    pub nullable: bool,
    /// How many samples had this field present.
    pub occurrence_count: usize,
}

/// Schema inferred from JSON message values.
#[derive(Debug, Clone)]
pub struct InferredJsonSchema {
    pub fields: Vec<InferredField>,
}

impl InferredJsonSchema {
    /// Infer schema from a sample of raw byte values.
    ///
    /// Samples up to `DEFAULT_SAMPLE_SIZE` records, detects JSON objects,
    /// and infers field types. Non-JSON values are silently skipped.
    ///
    /// `max_fields` limits the number of top-level fields included.
    pub fn infer(values: &[&[u8]], max_fields: usize) -> Option<Self> {
        let max_fields = if max_fields == 0 { DEFAULT_MAX_FIELDS } else { max_fields };
        let sample_count = values.len().min(DEFAULT_SAMPLE_SIZE);
        if sample_count == 0 {
            return None;
        }

        // Track: field_name → (type, occurrence_count)
        let mut field_map: BTreeMap<String, (JsonFieldType, usize)> = BTreeMap::new();
        let mut valid_samples = 0usize;

        for value in values.iter().take(sample_count) {
            let parsed: Value = match serde_json::from_slice(value) {
                Ok(v) => v,
                Err(_) => continue, // Not valid JSON
            };

            let obj = match parsed.as_object() {
                Some(o) => o,
                None => continue, // Not a JSON object (could be array, string, etc.)
            };

            valid_samples += 1;

            for (key, val) in obj {
                let detected_type = detect_type(val);
                field_map
                    .entry(key.clone())
                    .and_modify(|(existing_type, count)| {
                        *existing_type = existing_type.widen(&detected_type);
                        *count += 1;
                    })
                    .or_insert((detected_type, 1));
            }
        }

        if valid_samples == 0 || field_map.is_empty() {
            return None;
        }

        // Sort by occurrence (descending), then alphabetically for stability
        let mut fields: Vec<InferredField> = field_map
            .into_iter()
            .map(|(name, (field_type, occurrence_count))| InferredField {
                nullable: occurrence_count < valid_samples,
                name,
                field_type,
                occurrence_count,
            })
            .collect();

        fields.sort_by(|a, b| {
            b.occurrence_count
                .cmp(&a.occurrence_count)
                .then(a.name.cmp(&b.name))
        });
        fields.truncate(max_fields);

        Some(InferredJsonSchema { fields })
    }

    /// Convert to Arrow fields for schema extension.
    pub fn to_arrow_fields(&self) -> Vec<Field> {
        self.fields
            .iter()
            .map(|f| Field::new(&f.name, f.field_type.to_arrow_type(), f.nullable))
            .collect()
    }

    /// Serialize to a compact JSON string for storage in topic config.
    pub fn to_config_string(&self) -> String {
        let entries: Vec<String> = self
            .fields
            .iter()
            .map(|f| {
                let type_str = match f.field_type {
                    JsonFieldType::String => "string",
                    JsonFieldType::Int64 => "int64",
                    JsonFieldType::Float64 => "float64",
                    JsonFieldType::Boolean => "boolean",
                    JsonFieldType::Object => "object",
                    JsonFieldType::Array => "array",
                };
                format!(
                    "{}:{}{}",
                    f.name,
                    type_str,
                    if f.nullable { "?" } else { "" }
                )
            })
            .collect();
        entries.join(",")
    }

    /// Parse from config string (reverse of `to_config_string`).
    pub fn from_config_string(s: &str) -> Option<Self> {
        if s.is_empty() {
            return None;
        }

        let mut fields = Vec::new();
        for entry in s.split(',') {
            let entry = entry.trim();
            if entry.is_empty() {
                continue;
            }
            let (name_and_type, nullable) = if entry.ends_with('?') {
                (&entry[..entry.len() - 1], true)
            } else {
                (entry, false)
            };

            let parts: Vec<&str> = name_and_type.splitn(2, ':').collect();
            if parts.len() != 2 {
                continue;
            }

            let field_type = match parts[1] {
                "string" => JsonFieldType::String,
                "int64" => JsonFieldType::Int64,
                "float64" => JsonFieldType::Float64,
                "boolean" => JsonFieldType::Boolean,
                "object" => JsonFieldType::Object,
                "array" => JsonFieldType::Array,
                _ => continue,
            };

            fields.push(InferredField {
                name: parts[0].to_string(),
                field_type,
                nullable,
                occurrence_count: 0,
            });
        }

        if fields.is_empty() {
            None
        } else {
            Some(InferredJsonSchema { fields })
        }
    }
}

/// Detect the JSON type of a serde_json::Value.
fn detect_type(val: &Value) -> JsonFieldType {
    match val {
        Value::Null => JsonFieldType::String, // Null → nullable string
        Value::Bool(_) => JsonFieldType::Boolean,
        Value::Number(n) => {
            if n.is_i64() || n.is_u64() {
                JsonFieldType::Int64
            } else {
                JsonFieldType::Float64
            }
        }
        Value::String(_) => JsonFieldType::String,
        Value::Array(_) => JsonFieldType::Array,
        Value::Object(_) => JsonFieldType::Object,
    }
}

/// Extract a JSON field value as a string, int, float, or bool from raw bytes.
///
/// Returns `None` if the value is not valid JSON or the field is missing.
pub fn extract_json_field(value: &[u8], field_name: &str) -> Option<Value> {
    let parsed: Value = serde_json::from_slice(value).ok()?;
    parsed.get(field_name).cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_infer_basic_types() {
        let records: Vec<&[u8]> = vec![
            br#"{"name":"Alice","age":30,"score":9.5,"active":true}"#,
            br#"{"name":"Bob","age":25,"score":8.0,"active":false}"#,
        ];
        let schema = InferredJsonSchema::infer(&records, 64).unwrap();
        assert_eq!(schema.fields.len(), 4);

        let by_name: BTreeMap<_, _> = schema.fields.iter().map(|f| (f.name.as_str(), f)).collect();
        assert_eq!(by_name["name"].field_type, JsonFieldType::String);
        assert_eq!(by_name["age"].field_type, JsonFieldType::Int64);
        assert_eq!(by_name["score"].field_type, JsonFieldType::Float64);
        assert_eq!(by_name["active"].field_type, JsonFieldType::Boolean);

        // All fields present in all samples → not nullable
        for field in &schema.fields {
            assert!(!field.nullable);
        }
    }

    #[test]
    fn test_infer_nullable_fields() {
        let records: Vec<&[u8]> = vec![
            br#"{"name":"Alice","age":30}"#,
            br#"{"name":"Bob"}"#,
        ];
        let schema = InferredJsonSchema::infer(&records, 64).unwrap();

        let by_name: BTreeMap<_, _> = schema.fields.iter().map(|f| (f.name.as_str(), f)).collect();
        assert!(!by_name["name"].nullable); // Present in all
        assert!(by_name["age"].nullable); // Missing in second record
    }

    #[test]
    fn test_infer_type_widening() {
        let records: Vec<&[u8]> = vec![
            br#"{"value":42}"#,      // Int64
            br#"{"value":3.14}"#,    // Float64 → widens to Float64
        ];
        let schema = InferredJsonSchema::infer(&records, 64).unwrap();
        assert_eq!(schema.fields[0].field_type, JsonFieldType::Float64);
    }

    #[test]
    fn test_infer_nested_objects_and_arrays() {
        let records: Vec<&[u8]> = vec![
            br#"{"user":{"id":1},"tags":["a","b"],"name":"test"}"#,
        ];
        let schema = InferredJsonSchema::infer(&records, 64).unwrap();

        let by_name: BTreeMap<_, _> = schema.fields.iter().map(|f| (f.name.as_str(), f)).collect();
        assert_eq!(by_name["user"].field_type, JsonFieldType::Object);
        assert_eq!(by_name["tags"].field_type, JsonFieldType::Array);
        assert_eq!(by_name["name"].field_type, JsonFieldType::String);
    }

    #[test]
    fn test_infer_skips_non_json() {
        let records: Vec<&[u8]> = vec![
            b"not json at all",
            br#"{"name":"Alice"}"#,
            b"12345",
        ];
        let schema = InferredJsonSchema::infer(&records, 64).unwrap();
        assert_eq!(schema.fields.len(), 1);
        assert_eq!(schema.fields[0].name, "name");
    }

    #[test]
    fn test_infer_all_non_json_returns_none() {
        let records: Vec<&[u8]> = vec![b"binary data", b"\x00\x01\x02"];
        assert!(InferredJsonSchema::infer(&records, 64).is_none());
    }

    #[test]
    fn test_infer_empty_returns_none() {
        let records: Vec<&[u8]> = vec![];
        assert!(InferredJsonSchema::infer(&records, 64).is_none());
    }

    #[test]
    fn test_max_fields_truncation() {
        // Create JSON with 10 fields
        let json = br#"{"a":1,"b":2,"c":3,"d":4,"e":5,"f":6,"g":7,"h":8,"i":9,"j":10}"#;
        let records: Vec<&[u8]> = vec![json.as_ref()];
        let schema = InferredJsonSchema::infer(&records, 5).unwrap();
        assert_eq!(schema.fields.len(), 5);
    }

    #[test]
    fn test_to_arrow_fields() {
        let records: Vec<&[u8]> = vec![
            br#"{"name":"Alice","age":30,"score":9.5}"#,
        ];
        let schema = InferredJsonSchema::infer(&records, 64).unwrap();
        let fields = schema.to_arrow_fields();

        assert_eq!(fields.len(), 3);
        let by_name: BTreeMap<_, _> = fields.iter().map(|f| (f.name().as_str(), f)).collect();
        assert_eq!(*by_name["name"].data_type(), DataType::Utf8);
        assert_eq!(*by_name["age"].data_type(), DataType::Int64);
        assert_eq!(*by_name["score"].data_type(), DataType::Float64);
    }

    #[test]
    fn test_config_string_roundtrip() {
        let records: Vec<&[u8]> = vec![
            br#"{"name":"Alice","age":30,"score":9.5,"active":true}"#,
            br#"{"name":"Bob","age":25}"#,
        ];
        let schema = InferredJsonSchema::infer(&records, 64).unwrap();
        let config_str = schema.to_config_string();
        let restored = InferredJsonSchema::from_config_string(&config_str).unwrap();

        assert_eq!(schema.fields.len(), restored.fields.len());
        for (orig, rest) in schema.fields.iter().zip(restored.fields.iter()) {
            assert_eq!(orig.name, rest.name);
            assert_eq!(orig.field_type, rest.field_type);
            assert_eq!(orig.nullable, rest.nullable);
        }
    }

    #[test]
    fn test_from_config_string_empty() {
        assert!(InferredJsonSchema::from_config_string("").is_none());
    }

    #[test]
    fn test_extract_json_field() {
        let value = br#"{"name":"Alice","age":30}"#;
        assert_eq!(
            extract_json_field(value, "name"),
            Some(Value::String("Alice".to_string()))
        );
        assert_eq!(
            extract_json_field(value, "age"),
            Some(Value::Number(30.into()))
        );
        assert_eq!(extract_json_field(value, "missing"), None);
        assert_eq!(extract_json_field(b"not json", "name"), None);
    }

    #[test]
    fn test_null_field_handling() {
        let records: Vec<&[u8]> = vec![
            br#"{"name":"Alice","value":null}"#,
            br#"{"name":"Bob","value":"hello"}"#,
        ];
        let schema = InferredJsonSchema::infer(&records, 64).unwrap();
        let by_name: BTreeMap<_, _> = schema.fields.iter().map(|f| (f.name.as_str(), f)).collect();
        // null widens with String → String
        assert_eq!(by_name["value"].field_type, JsonFieldType::String);
    }
}
