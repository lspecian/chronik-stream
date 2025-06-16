//! Search index integration for segments.

use bytes::Bytes;
use chronik_common::Result;
use serde_json::Value;
use std::collections::HashMap;

/// Index builder for creating search indexes from Kafka records
pub struct IndexBuilder {
    /// Dynamic schema based on JSON fields
    fields: HashMap<String, FieldType>,
    /// Documents to be indexed
    documents: Vec<Document>,
}

/// Document to be indexed
#[derive(Debug, Clone)]
pub struct Document {
    pub fields: HashMap<String, FieldValue>,
}

/// Field types in the index
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldType {
    Text,
    String,
    I64,
    F64,
    Bool,
    DateTime,
    Bytes,
}

/// Field values
#[derive(Debug, Clone)]
pub enum FieldValue {
    Text(String),
    String(String),
    I64(i64),
    F64(f64),
    Bool(bool),
    DateTime(i64), // Unix timestamp millis
    Bytes(Vec<u8>),
}

impl IndexBuilder {
    /// Create a new index builder
    pub fn new() -> Self {
        Self {
            fields: HashMap::new(),
            documents: Vec::new(),
        }
    }
    
    /// Add a JSON document to the index
    pub fn add_json(&mut self, offset: i64, timestamp: i64, json: &Value) -> Result<()> {
        let mut doc = Document {
            fields: HashMap::new(),
        };
        
        // Add system fields
        doc.fields.insert("_offset".to_string(), FieldValue::I64(offset));
        doc.fields.insert("_timestamp".to_string(), FieldValue::DateTime(timestamp));
        
        // Extract fields from JSON
        self.extract_fields("", json, &mut doc)?;
        
        self.documents.push(doc);
        Ok(())
    }
    
    /// Extract fields recursively from JSON
    fn extract_fields(&mut self, prefix: &str, value: &Value, doc: &mut Document) -> Result<()> {
        match value {
            Value::Object(map) => {
                for (key, val) in map {
                    let field_name = if prefix.is_empty() {
                        key.clone()
                    } else {
                        format!("{}.{}", prefix, key)
                    };
                    self.extract_fields(&field_name, val, doc)?;
                }
            }
            Value::Array(arr) => {
                // For arrays, we index each element
                for (i, val) in arr.iter().enumerate() {
                    let field_name = format!("{}.{}", prefix, i);
                    self.extract_fields(&field_name, val, doc)?;
                }
            }
            Value::String(s) => {
                self.add_field(prefix, FieldType::Text, FieldValue::Text(s.clone()), doc);
            }
            Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    self.add_field(prefix, FieldType::I64, FieldValue::I64(i), doc);
                } else if let Some(f) = n.as_f64() {
                    self.add_field(prefix, FieldType::F64, FieldValue::F64(f), doc);
                }
            }
            Value::Bool(b) => {
                self.add_field(prefix, FieldType::Bool, FieldValue::Bool(*b), doc);
            }
            Value::Null => {
                // Skip null values
            }
        }
        Ok(())
    }
    
    /// Add a field to the document and update schema
    fn add_field(&mut self, name: &str, field_type: FieldType, value: FieldValue, doc: &mut Document) {
        self.fields.entry(name.to_string()).or_insert(field_type);
        doc.fields.insert(name.to_string(), value);
    }
    
    /// Build the index and serialize to bytes
    pub fn build(self) -> Result<Bytes> {
        // TODO: Integrate with Tantivy once we resolve the dependency issues
        // For now, return a placeholder
        let index_data = serde_json::json!({
            "fields": self.fields.iter().map(|(k, v)| (k, format!("{:?}", v))).collect::<HashMap<_, _>>(),
            "doc_count": self.documents.len(),
        });
        
        Ok(Bytes::from(serde_json::to_vec(&index_data)?))
    }
    
    /// Get the number of documents
    pub fn doc_count(&self) -> usize {
        self.documents.len()
    }
}

/// Search within a segment's index
pub struct SegmentSearcher {
    // TODO: Add Tantivy searcher once integrated
}

impl SegmentSearcher {
    /// Create a searcher from index data
    pub fn from_bytes(_data: Bytes) -> Result<Self> {
        // TODO: Deserialize Tantivy index
        Ok(Self {})
    }
    
    /// Search for documents matching a query
    pub fn search(&self, _query: &str, _limit: usize) -> Result<Vec<SearchHit>> {
        // TODO: Implement search using Tantivy
        Ok(vec![])
    }
}

/// Search result
#[derive(Debug, Clone)]
pub struct SearchHit {
    pub offset: i64,
    pub score: f32,
    pub fields: HashMap<String, FieldValue>,
}