//! Tantivy search integration for ChronikSegment
//!
//! This module provides full-text search capabilities by creating and managing
//! Tantivy indexes from segment data.

use crate::chronik_segment::ChronikSegment;
use crate::Record;
use chronik_common::{Result, Error};
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use flate2::Compression;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use tantivy::{
    collector::TopDocs,
    directory::MmapDirectory,
    doc,
    query::QueryParser,
    schema::*,
    Index, IndexReader, ReloadPolicy, Searcher,
};
use tar::{Archive, Builder};
use tempfile::TempDir;
use tracing::{debug, info};

/// Field type detection for schema building
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DetectedFieldType {
    Text,
    String,
    U64,
    I64,
    F64,
    Date,
    Bytes,
    Bool,
}

/// Search index wrapper for Tantivy
pub struct SearchIndex {
    index: Index,
    reader: IndexReader,
    schema: Schema,
    field_map: HashMap<String, Field>,
}

/// Search results container
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResults {
    pub total_hits: usize,
    pub hits: Vec<SearchHit>,
    pub took_ms: u64,
}

/// Individual search hit
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchHit {
    pub score: f32,
    pub doc_id: u32,
    pub source: Value,
}

impl SearchIndex {
    /// Create a new search index from a ChronikSegment
    pub fn create_from_segment(segment: &ChronikSegment) -> Result<Self> {
        info!("Creating search index from segment");
        let start = std::time::Instant::now();
        
        // Analyze fields to build schema
        let field_types = Self::analyze_segment_fields(segment)?;
        let (schema, field_map) = Self::build_schema(&field_types)?;
        
        // Create in-memory index
        let index = Index::create_in_ram(schema.clone());
        
        // Index all records
        let mut index_writer = index.writer(50_000_000)
            .map_err(|e| Error::Search(e.to_string()))?;
        let mut doc_count = 0;
        
        for record in segment.iter_records()? {
            if let Some(doc) = Self::convert_record_to_document(&record, &schema, &field_map)? {
                index_writer.add_document(doc)
                    .map_err(|e| Error::Search(e.to_string()))?;
                doc_count += 1;
                
                if doc_count % 10000 == 0 {
                    debug!("Indexed {} documents", doc_count);
                }
            }
        }
        
        index_writer.commit()
            .map_err(|e| Error::Search(e.to_string()))?;
        
        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::OnCommitWithDelay)
            .try_into()
            .map_err(|e| Error::Search(e.to_string()))?;
        
        info!(
            "Created index with {} documents in {:?}",
            doc_count,
            start.elapsed()
        );
        
        Ok(Self {
            index,
            reader,
            schema,
            field_map,
        })
    }
    
    /// Deserialize a search index from compressed bytes
    pub fn deserialize_from_bytes(data: &[u8]) -> Result<Self> {
        debug!("Deserializing search index from {} bytes", data.len());
        
        let temp_dir = TempDir::new()?;
        let index_path = temp_dir.path();
        
        // Decompress and unpack
        let tar = GzDecoder::new(data);
        let mut archive = Archive::new(tar);
        archive.unpack(index_path)?;
        
        // Open the index
        let index = Index::open_in_dir(index_path)
            .map_err(|e| Error::Search(e.to_string()))?;
        let schema = index.schema();
        
        // Rebuild field map
        let mut field_map = HashMap::new();
        for (field, field_entry) in schema.fields() {
            field_map.insert(field_entry.name().to_string(), field);
        }
        
        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::Manual)
            .try_into()
            .map_err(|e| Error::Search(e.to_string()))?;
        
        Ok(Self {
            index,
            reader,
            schema,
            field_map,
        })
    }
    
    /// Execute a search query
    pub fn search(&self, query_str: &str, limit: usize) -> Result<SearchResults> {
        let start = std::time::Instant::now();
        let searcher = self.reader.searcher();
        
        // Get all searchable fields
        let default_fields: Vec<Field> = self.schema
            .fields()
            .filter_map(|(field, entry)| {
                if entry.is_indexed() && entry.field_type().value_type() == Type::Str {
                    Some(field)
                } else {
                    None
                }
            })
            .collect();
        
        if default_fields.is_empty() {
            return Err(Error::Internal("No searchable fields in index".into()));
        }
        
        // Parse query
        let query_parser = QueryParser::for_index(&self.index, default_fields);
        let query = query_parser.parse_query(query_str)
            .map_err(|e| Error::Search(e.to_string()))?;
        
        // Execute search
        let top_docs = searcher.search(&query, &TopDocs::with_limit(limit))
            .map_err(|e| Error::Search(e.to_string()))?;
        let total_hits = searcher.search(&query, &tantivy::collector::Count)
            .map_err(|e| Error::Search(e.to_string()))?;
        
        let mut hits = Vec::new();
        for (score, doc_address) in top_docs {
            let retrieved_doc = searcher.doc(doc_address)
                .map_err(|e| Error::Search(e.to_string()))?;
            let doc_json = self.convert_document_to_json(&retrieved_doc)?;
            
            hits.push(SearchHit {
                score,
                doc_id: doc_address.doc_id,
                source: doc_json,
            });
        }
        
        Ok(SearchResults {
            total_hits,
            hits,
            took_ms: start.elapsed().as_millis() as u64,
        })
    }
    
    /// Serialize the index to compressed bytes
    pub fn serialize_to_bytes(&self) -> Result<Vec<u8>> {
        debug!("Serializing search index");
        
        let mut buffer = Vec::new();
        {
            let encoder = GzEncoder::new(&mut buffer, Compression::default());
            let mut tar = Builder::new(encoder);
            
            // For in-memory indexes, we need to save to a temp directory first
            let temp_dir = TempDir::new()?;
            let temp_path = temp_dir.path();
            
            // Clone the index to disk
            let mmap_dir = MmapDirectory::open(temp_path)
                .map_err(|e| Error::Search(e.to_string()))?;
            let disk_index = Index::open_or_create(mmap_dir, self.schema.clone())
                .map_err(|e| Error::Search(e.to_string()))?;
            
            // Copy all documents
            let mut writer = disk_index.writer(50_000_000)
                .map_err(|e| Error::Search(e.to_string()))?;
            let searcher = self.reader.searcher();
            
            for segment_reader in searcher.segment_readers() {
                for doc_id in 0..segment_reader.num_docs() {
                    if let Ok(doc) = segment_reader.doc(doc_id) {
                        writer.add_document(doc)
                            .map_err(|e| Error::Search(e.to_string()))?;
                    }
                }
            }
            writer.commit()
                .map_err(|e| Error::Search(e.to_string()))?;
            
            // Now tar the directory
            tar.append_dir_all(".", temp_path)?;
            tar.finish()?;
        }
        
        debug!("Serialized index to {} bytes", buffer.len());
        Ok(buffer)
    }
    
    /// Get the searcher for advanced queries
    pub fn searcher(&self) -> Searcher {
        self.reader.searcher()
    }
    
    /// Get the schema
    pub fn schema(&self) -> &Schema {
        &self.schema
    }
    
    /// Get a field by name
    pub fn get_field(&self, name: &str) -> Option<Field> {
        self.field_map.get(name).copied()
    }
    
    /// Analyze fields in a segment to determine types
    pub fn analyze_segment_fields(segment: &ChronikSegment) -> Result<HashMap<String, DetectedFieldType>> {
        let mut field_types = HashMap::new();
        let mut sample_count = 0;
        const MAX_SAMPLES: usize = 1000;
        
        for record in segment.iter_records()? {
            if sample_count >= MAX_SAMPLES {
                break;
            }
            
            if let Some(value) = record.value {
                if let Ok(json) = serde_json::from_slice::<Value>(&value) {
                    Self::analyze_json_fields(&json, "", &mut field_types);
                    sample_count += 1;
                }
            }
        }
        
        debug!("Analyzed {} fields from {} samples", field_types.len(), sample_count);
        Ok(field_types)
    }
    
    /// Recursively analyze JSON fields
    pub fn analyze_json_fields(
        json: &Value,
        prefix: &str,
        field_types: &mut HashMap<String, DetectedFieldType>,
    ) {
        match json {
            Value::Object(map) => {
                for (key, value) in map {
                    let field_name = if prefix.is_empty() {
                        key.clone()
                    } else {
                        format!("{}.{}", prefix, key)
                    };
                    Self::analyze_json_fields(value, &field_name, field_types);
                }
            }
            Value::String(s) => {
                // Detect if it's a date
                if chrono::DateTime::parse_from_rfc3339(s).is_ok() {
                    field_types.entry(prefix.to_string())
                        .or_insert(DetectedFieldType::Date);
                } else if s.len() <= 256 && !s.contains(' ') {
                    // Short strings without spaces are likely keywords
                    field_types.entry(prefix.to_string())
                        .or_insert(DetectedFieldType::String);
                } else {
                    field_types.entry(prefix.to_string())
                        .or_insert(DetectedFieldType::Text);
                }
            }
            Value::Number(n) => {
                if n.is_i64() {
                    field_types.entry(prefix.to_string())
                        .or_insert(DetectedFieldType::I64);
                } else if n.is_u64() {
                    field_types.entry(prefix.to_string())
                        .or_insert(DetectedFieldType::U64);
                } else {
                    field_types.entry(prefix.to_string())
                        .or_insert(DetectedFieldType::F64);
                }
            }
            Value::Bool(_) => {
                field_types.entry(prefix.to_string())
                    .or_insert(DetectedFieldType::Bool);
            }
            _ => {}
        }
    }
    
    /// Build Tantivy schema from detected field types
    fn build_schema(
        field_types: &HashMap<String, DetectedFieldType>,
    ) -> Result<(Schema, HashMap<String, Field>)> {
        let mut schema_builder = Schema::builder();
        let mut field_map = HashMap::new();
        
        // Add system fields
        let offset_field = schema_builder.add_u64_field("_offset", INDEXED | STORED);
        field_map.insert("_offset".to_string(), offset_field);
        
        let timestamp_field = schema_builder.add_date_field("_timestamp", INDEXED | STORED);
        field_map.insert("_timestamp".to_string(), timestamp_field);
        
        let key_field = schema_builder.add_text_field("_key", STRING | STORED);
        field_map.insert("_key".to_string(), key_field);
        
        // Add detected fields
        for (field_name, field_type) in field_types {
            let field = match field_type {
                DetectedFieldType::Text => {
                    schema_builder.add_text_field(field_name, TEXT | STORED)
                }
                DetectedFieldType::String => {
                    schema_builder.add_text_field(field_name, STRING | STORED)
                }
                DetectedFieldType::U64 => {
                    schema_builder.add_u64_field(field_name, INDEXED | STORED)
                }
                DetectedFieldType::I64 => {
                    schema_builder.add_i64_field(field_name, INDEXED | STORED)
                }
                DetectedFieldType::F64 => {
                    schema_builder.add_f64_field(field_name, INDEXED | STORED)
                }
                DetectedFieldType::Date => {
                    schema_builder.add_date_field(field_name, INDEXED | STORED)
                }
                DetectedFieldType::Bytes => {
                    schema_builder.add_bytes_field(field_name, STORED)
                }
                DetectedFieldType::Bool => {
                    schema_builder.add_u64_field(field_name, INDEXED | STORED) // Store as 0/1
                }
            };
            
            field_map.insert(field_name.clone(), field);
        }
        
        Ok((schema_builder.build(), field_map))
    }
    
    /// Convert a segment record to a Tantivy document
    fn convert_record_to_document(
        record: &Record,
        schema: &Schema,
        field_map: &HashMap<String, Field>,
    ) -> Result<Option<tantivy::Document>> {
        let mut doc = doc!();
        
        // Add system fields
        if let Some(offset_field) = field_map.get("_offset") {
            doc.add_u64(*offset_field, record.offset as u64);
        }
        
        if let Some(timestamp_field) = field_map.get("_timestamp") {
            if record.timestamp > 0 {
                let datetime = tantivy::DateTime::from_timestamp_millis(record.timestamp);
                doc.add_date(*timestamp_field, datetime);
            }
        }
        
        if let Some(key) = &record.key {
            if let Some(key_field) = field_map.get("_key") {
                if let Ok(key_str) = std::str::from_utf8(key) {
                    doc.add_text(*key_field, key_str);
                }
            }
        }
        
        // Add fields from JSON value
        if let Some(value) = &record.value {
            if let Ok(json) = serde_json::from_slice::<Value>(value) {
                Self::add_json_fields_to_document(&json, "", &mut doc, field_map)?;
            }
        }
        
        Ok(Some(doc))
    }
    
    /// Recursively add JSON fields to document
    fn add_json_fields_to_document(
        json: &Value,
        prefix: &str,
        doc: &mut tantivy::Document,
        field_map: &HashMap<String, Field>,
    ) -> Result<()> {
        match json {
            Value::Object(map) => {
                for (key, value) in map {
                    let field_name = if prefix.is_empty() {
                        key.clone()
                    } else {
                        format!("{}.{}", prefix, key)
                    };
                    Self::add_json_fields_to_document(value, &field_name, doc, field_map)?;
                }
            }
            Value::String(s) => {
                if let Some(field) = field_map.get(prefix) {
                    if let Ok(datetime) = chrono::DateTime::parse_from_rfc3339(s) {
                        let tantivy_dt = tantivy::DateTime::from_timestamp_millis(
                            datetime.timestamp_millis()
                        );
                        doc.add_date(*field, tantivy_dt);
                    } else {
                        doc.add_text(*field, s);
                    }
                }
            }
            Value::Number(n) => {
                if let Some(field) = field_map.get(prefix) {
                    if let Some(i) = n.as_i64() {
                        doc.add_i64(*field, i);
                    } else if let Some(u) = n.as_u64() {
                        doc.add_u64(*field, u);
                    } else if let Some(f) = n.as_f64() {
                        doc.add_f64(*field, f);
                    }
                }
            }
            Value::Bool(b) => {
                if let Some(field) = field_map.get(prefix) {
                    doc.add_u64(*field, if *b { 1 } else { 0 });
                }
            }
            _ => {}
        }
        
        Ok(())
    }
    
    /// Convert Tantivy document back to JSON
    fn convert_document_to_json(&self, doc: &tantivy::Document) -> Result<Value> {
        let mut json_map = serde_json::Map::new();
        
        for field_value in doc.field_values() {
            let field = field_value.field();
            let field_entry = self.schema.get_field_entry(field);
            let field_name = field_entry.name();
            
            let json_value = match field_value.value() {
                tantivy::schema::Value::Str(s) => Value::String(s.to_string()),
                tantivy::schema::Value::U64(u) => {
                    // Check if this is a boolean field
                    if field_name.ends_with("_bool") {
                        Value::Bool(*u != 0)
                    } else {
                        Value::Number(serde_json::Number::from(*u))
                    }
                }
                tantivy::schema::Value::I64(i) => {
                    Value::Number(serde_json::Number::from(*i))
                }
                tantivy::schema::Value::F64(f) => {
                    if let Some(n) = serde_json::Number::from_f64(*f) {
                        Value::Number(n)
                    } else {
                        Value::Null
                    }
                }
                tantivy::schema::Value::Date(d) => {
                    Value::String(d.to_rfc3339())
                }
                tantivy::schema::Value::Bytes(b) => {
                    Value::String(base64::prelude::Engine::encode(&base64::prelude::BASE64_STANDARD, b))
                }
                _ => continue,
            };
            
            // Handle nested field names
            if field_name.contains('.') {
                Self::set_nested_field(&mut json_map, field_name, json_value);
            } else if !field_name.starts_with('_') {
                // Skip system fields in output
                json_map.insert(field_name.to_string(), json_value);
            }
        }
        
        Ok(Value::Object(json_map))
    }
    
    /// Set a nested field in a JSON map
    fn set_nested_field(map: &mut serde_json::Map<String, Value>, path: &str, value: Value) {
        let parts: Vec<&str> = path.split('.').collect();
        let mut current = map;
        
        for (i, part) in parts.iter().enumerate() {
            if i == parts.len() - 1 {
                current.insert(part.to_string(), value.clone());
            } else {
                let entry = current
                    .entry(part.to_string())
                    .or_insert_with(|| Value::Object(serde_json::Map::new()));
                
                if let Value::Object(obj) = entry {
                    current = obj;
                } else {
                    break; // Can't traverse further
                }
            }
        }
    }
}

impl SearchResults {
    /// Create new empty search results
    pub fn new() -> Self {
        Self {
            total_hits: 0,
            hits: Vec::new(),
            took_ms: 0,
        }
    }
    
    /// Add a hit to the results
    pub fn add_hit(&mut self, hit: SearchHit) {
        self.hits.push(hit);
    }
}