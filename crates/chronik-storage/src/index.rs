//! Search index integration for segments.

use bytes::Bytes;
use chronik_common::{Result, Error};
use serde_json::Value as JsonValue;
use std::collections::HashMap;
use tantivy::{
    doc,
    schema::{Schema, Field, TEXT, STORED, NumericOptions, Value as TantivyValue},
    Index, IndexWriter, IndexReader, ReloadPolicy,
    collector::TopDocs,
    query::QueryParser,
};

/// Index builder for creating search indexes from Kafka records
pub struct IndexBuilder {
    /// Tantivy index
    index: Index,
    /// Index writer
    index_writer: IndexWriter,
    /// Schema
    schema: Schema,
    /// Field mappings
    field_map: HashMap<String, Field>,
    /// System fields
    offset_field: Field,
    timestamp_field: Field,
    /// Default text field for dynamic content
    content_field: Field,
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
    pub fn new() -> Result<Self> {
        // Create schema with system fields
        let mut schema_builder = Schema::builder();
        
        // System fields
        let offset_field = schema_builder.add_i64_field("_offset", NumericOptions::default().set_indexed().set_stored().set_fast());
        let timestamp_field = schema_builder.add_i64_field("_timestamp", NumericOptions::default().set_indexed().set_stored().set_fast());
        
        // Dynamic content field for JSON data
        let content_field = schema_builder.add_text_field("_content", TEXT | STORED);
        
        let schema = schema_builder.build();
        
        // Create in-memory index
        let index = Index::create_in_ram(schema.clone());
        
        // Create index writer with 50MB heap
        let index_writer = index.writer(50_000_000)
            .map_err(|e| Error::Internal(format!("Failed to create index writer: {}", e)))?;
        
        Ok(Self {
            index,
            index_writer,
            schema,
            field_map: HashMap::new(),
            offset_field,
            timestamp_field,
            content_field,
        })
    }
    
    /// Add a JSON document to the index
    pub fn add_json(&mut self, offset: i64, timestamp: i64, json: &JsonValue) -> Result<()> {
        // Create Tantivy document
        let mut doc = doc!(
            self.offset_field => offset,
            self.timestamp_field => timestamp
        );
        
        // Convert JSON to searchable text
        let json_text = serde_json::to_string(json)
            .map_err(|e| Error::Internal(format!("Failed to serialize JSON: {}", e)))?;
        doc.add_text(self.content_field, &json_text);
        
        // Also extract and index specific fields
        self.extract_and_index_fields("", json, &mut doc)?;
        
        // Add document to index
        self.index_writer.add_document(doc)
            .map_err(|e| Error::Internal(format!("Failed to add document: {}", e)))?;
        
        Ok(())
    }
    
    /// Extract and index fields from JSON
    fn extract_and_index_fields(&mut self, prefix: &str, value: &JsonValue, doc: &mut tantivy::TantivyDocument) -> Result<()> {
        match value {
            JsonValue::Object(map) => {
                for (key, val) in map {
                    let field_name = if prefix.is_empty() {
                        key.clone()
                    } else {
                        format!("{}.{}", prefix, key)
                    };
                    self.extract_and_index_fields(&field_name, val, doc)?;
                }
            }
            JsonValue::String(s) => {
                // Get or create field for this path
                let field = self.get_or_create_text_field(prefix)?;
                doc.add_text(field, s);
            }
            JsonValue::Number(n) => {
                if let Some(i) = n.as_i64() {
                    let field = self.get_or_create_i64_field(prefix)?;
                    doc.add_i64(field, i);
                } else if let Some(f) = n.as_f64() {
                    let field = self.get_or_create_f64_field(prefix)?;
                    doc.add_f64(field, f);
                }
            }
            JsonValue::Bool(b) => {
                let field = self.get_or_create_text_field(prefix)?;
                doc.add_text(field, &b.to_string());
            }
            JsonValue::Array(arr) => {
                // For arrays, concatenate values
                let values: Vec<String> = arr.iter()
                    .filter_map(|v| match v {
                        JsonValue::String(s) => Some(s.clone()),
                        JsonValue::Number(n) => Some(n.to_string()),
                        JsonValue::Bool(b) => Some(b.to_string()),
                        _ => None
                    })
                    .collect();
                if !values.is_empty() {
                    let field = self.get_or_create_text_field(prefix)?;
                    doc.add_text(field, &values.join(" "));
                }
            }
            JsonValue::Null => {
                // Skip null values
            }
        }
        Ok(())
    }
    
    /// Get or create a text field
    fn get_or_create_text_field(&mut self, name: &str) -> Result<Field> {
        if let Some(field) = self.field_map.get(name) {
            Ok(*field)
        } else {
            // For dynamic schema, we can't add fields after index creation
            // So we'll index everything in the content field
            Ok(self.content_field)
        }
    }
    
    /// Get or create an i64 field
    fn get_or_create_i64_field(&mut self, _name: &str) -> Result<Field> {
        // For simplicity, convert numbers to text in content field
        Ok(self.content_field)
    }
    
    /// Get or create an f64 field
    fn get_or_create_f64_field(&mut self, _name: &str) -> Result<Field> {
        // For simplicity, convert numbers to text in content field
        Ok(self.content_field)
    }
    
    /// Build the index and serialize to bytes
    pub fn build(mut self) -> Result<Bytes> {
        // Commit all documents
        self.index_writer.commit()
            .map_err(|e| Error::Internal(format!("Failed to commit index: {}", e)))?;
        
        // Serialize the index to bytes
        let mut buffer = Vec::new();
        
        // We need to serialize the index data
        // For in-memory index, we'll serialize the segments
        let reader = self.index.reader()
            .map_err(|e| Error::Internal(format!("Failed to create reader: {}", e)))?;
        
        // Get index metadata
        let searcher = reader.searcher();
        let num_docs = searcher.num_docs();
        
        // For now, we'll store the index as a simple format
        // In production, you might want to use Tantivy's native segment format
        let index_meta = serde_json::json!({
            "num_docs": num_docs,
            "schema": self.schema,
        });
        
        buffer.extend_from_slice(&serde_json::to_vec(&index_meta)?);        
        Ok(Bytes::from(buffer))
    }
    
    /// Get the number of documents
    pub fn doc_count(&mut self) -> usize {
        self.index_writer.commit().ok();
        self.index.reader()
            .ok()
            .and_then(|reader| Some(reader.searcher().num_docs() as usize))
            .unwrap_or(0)
    }
}

/// Search within a segment's index
pub struct SegmentSearcher {
    index: Index,
    reader: IndexReader,
    query_parser: QueryParser,
    offset_field: Field,
    timestamp_field: Field,
}

impl SegmentSearcher {
    /// Create a searcher from index data
    pub fn from_bytes(data: Bytes) -> Result<Self> {
        // Deserialize index metadata
        let meta: serde_json::Value = serde_json::from_slice(&data)
            .map_err(|e| Error::Internal(format!("Failed to deserialize index meta: {}", e)))?;
        
        // For this simplified version, we'll recreate a simple index
        // In production, you'd deserialize the actual Tantivy segments
        let _schema_json = &meta["schema"];
        
        // Create a simple schema for searching
        let mut schema_builder = Schema::builder();
        let offset_field = schema_builder.add_i64_field("_offset", NumericOptions::default().set_indexed().set_stored());
        let timestamp_field = schema_builder.add_i64_field("_timestamp", NumericOptions::default().set_indexed().set_stored());
        let content_field = schema_builder.add_text_field("_content", TEXT | STORED);
        let schema = schema_builder.build();
        
        // Create in-memory index
        let index = Index::create_in_ram(schema.clone());
        
        // Create reader
        let reader = index.reader_builder()
            .reload_policy(ReloadPolicy::Manual)
            .try_into()
            .map_err(|e| Error::Internal(format!("Failed to create reader: {}", e)))?;
        
        // Create query parser for content field
        let query_parser = QueryParser::for_index(&index, vec![content_field]);
        
        Ok(Self {
            index,
            reader,
            query_parser,
            offset_field,
            timestamp_field,
        })
    }
    
    /// Search for documents matching a query
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchHit>> {
        let searcher = self.reader.searcher();
        
        // Parse query
        let query = self.query_parser.parse_query(query)
            .map_err(|e| Error::InvalidInput(format!("Invalid query: {}", e)))?;
        
        // Execute search
        let top_docs = searcher.search(&query, &TopDocs::with_limit(limit))
            .map_err(|e| Error::Internal(format!("Search failed: {}", e)))?;
        
        // Collect results
        let mut results = Vec::new();
        for (score, doc_address) in top_docs {
            let doc: tantivy::TantivyDocument = searcher.doc(doc_address)
                .map_err(|e| Error::Internal(format!("Failed to retrieve document: {}", e)))?;
            
            // Extract fields
            let offset = doc.get_first(self.offset_field)
                .and_then(|v| TantivyValue::as_i64(&v))
                .unwrap_or(0);
            
            // Get content field from schema
            let content_field = self.index.schema().get_field("_content")
                .map_err(|_| Error::Internal("Content field not found".to_string()))?;
            let content = doc.get_first(content_field)
                .and_then(|v| TantivyValue::as_str(&v))
                .unwrap_or("");
            
            // Parse content back to extract fields
            let fields = if let Ok(json) = serde_json::from_str::<JsonValue>(content) {
                self.extract_fields_from_json(&json)
            } else {
                HashMap::new()
            };
            
            results.push(SearchHit {
                offset,
                score,
                fields,
            });
        }
        
        Ok(results)
    }
    
    /// Extract fields from JSON for search results
    fn extract_fields_from_json(&self, json: &JsonValue) -> HashMap<String, FieldValue> {
        let mut fields = HashMap::new();
        self.extract_fields_recursive("", json, &mut fields);
        fields
    }
    
    fn extract_fields_recursive(&self, prefix: &str, value: &JsonValue, fields: &mut HashMap<String, FieldValue>) {
        match value {
            JsonValue::Object(map) => {
                for (key, val) in map {
                    let field_name = if prefix.is_empty() {
                        key.clone()
                    } else {
                        format!("{}.{}", prefix, key)
                    };
                    self.extract_fields_recursive(&field_name, val, fields);
                }
            }
            JsonValue::String(s) => {
                fields.insert(prefix.to_string(), FieldValue::Text(s.clone()));
            }
            JsonValue::Number(n) => {
                if let Some(i) = n.as_i64() {
                    fields.insert(prefix.to_string(), FieldValue::I64(i));
                } else if let Some(f) = n.as_f64() {
                    fields.insert(prefix.to_string(), FieldValue::F64(f));
                }
            }
            JsonValue::Bool(b) => {
                fields.insert(prefix.to_string(), FieldValue::Bool(*b));
            }
            _ => {}
        }
    }
}

/// Search result
#[derive(Debug, Clone)]
pub struct SearchHit {
    pub offset: i64,
    pub score: f32,
    pub fields: HashMap<String, FieldValue>,
}