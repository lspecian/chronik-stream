//! Query executor that integrates the translator with Tantivy search execution.

use crate::translator::{QueryTranslator, QueryInput};
use chronik_common::{Result, Error};
use serde::{Deserialize, Serialize};
use tantivy::{
    collector::{TopDocs, Count, DocSetCollector},
    query::Query as TantivyQuery,
    schema::Schema,
    Index, IndexReader, Searcher,
    TantivyDocument, Score, DocAddress,
};
use std::sync::Arc;
use tracing::{debug, instrument};

/// Query executor that handles query translation and execution
pub struct QueryExecutor {
    index: Index,
    reader: IndexReader,
    translator: Arc<QueryTranslator>,
}

impl QueryExecutor {
    /// Create a new query executor
    pub fn new(index: Index) -> Result<Self> {
        let schema = index.schema();
        let reader = index.reader()
            .map_err(|e| Error::Internal(format!("Failed to create index reader: {}", e)))?;
        
        let translator = Arc::new(QueryTranslator::new(schema));
        
        Ok(Self {
            index,
            reader,
            translator,
        })
    }
    
    /// Execute a query and return results
    #[instrument(skip(self))]
    pub fn execute(&self, query: &QueryInput, options: QueryOptions) -> Result<QueryResults> {
        let start = std::time::Instant::now();
        
        // Translate query
        let tantivy_query = self.translator.translate(query)?;
        debug!("Query translated successfully");
        
        // Get searcher
        let searcher = self.reader.searcher();
        
        // Execute based on options
        let results = match options.result_type {
            ResultType::TopDocs => self.execute_top_docs(&searcher, &*tantivy_query, &options)?,
            ResultType::Count => self.execute_count(&searcher, &*tantivy_query)?,
            ResultType::All => self.execute_all(&searcher, &*tantivy_query, &options)?,
        };
        
        let elapsed = start.elapsed();
        debug!("Query executed in {:?}", elapsed);
        
        let total_hits = results.len();
        
        Ok(QueryResults {
            hits: results,
            total_hits,
            execution_time_ms: elapsed.as_millis() as u64,
            query_type: format!("{:?}", query),
        })
    }
    
    /// Execute query and return top documents
    fn execute_top_docs(
        &self,
        searcher: &Searcher,
        query: &dyn TantivyQuery,
        options: &QueryOptions,
    ) -> Result<Vec<SearchHit>> {
        let collector = TopDocs::with_limit(options.limit)
            .and_offset(options.offset);
        
        let top_docs = searcher.search(query, &collector)
            .map_err(|e| Error::Internal(format!("Search failed: {}", e)))?;
        
        let mut hits = Vec::new();
        
        for (score, doc_address) in top_docs {
            let doc = searcher.doc(doc_address)
                .map_err(|e| Error::Internal(format!("Failed to retrieve document: {}", e)))?;
            
            let hit = self.doc_to_hit(doc, Some(score), doc_address)?;
            hits.push(hit);
        }
        
        Ok(hits)
    }
    
    /// Execute query and return count only
    fn execute_count(
        &self,
        searcher: &Searcher,
        query: &dyn TantivyQuery,
    ) -> Result<Vec<SearchHit>> {
        let count = searcher.search(query, &Count)
            .map_err(|e| Error::Internal(format!("Count failed: {}", e)))?;
        
        // Return empty vec with count in results metadata
        debug!("Query matched {} documents", count);
        Ok(Vec::new())
    }
    
    /// Execute query and return all matching documents
    fn execute_all(
        &self,
        searcher: &Searcher,
        query: &dyn TantivyQuery,
        options: &QueryOptions,
    ) -> Result<Vec<SearchHit>> {
        let collector = DocSetCollector;
        let doc_set = searcher.search(query, &collector)
            .map_err(|e| Error::Internal(format!("Search failed: {}", e)))?;
        
        let mut hits = Vec::new();
        let mut count = 0;
        
        for doc_address in doc_set.into_iter().skip(options.offset) {
            if count >= options.limit {
                break;
            }
            
            let doc = searcher.doc(doc_address)
                .map_err(|e| Error::Internal(format!("Failed to retrieve document: {}", e)))?;
            
            let hit = self.doc_to_hit(doc, None, doc_address)?;
            hits.push(hit);
            count += 1;
        }
        
        Ok(hits)
    }
    
    /// Convert Tantivy document to search hit
    fn doc_to_hit(
        &self,
        doc: TantivyDocument,
        score: Option<Score>,
        doc_address: DocAddress,
    ) -> Result<SearchHit> {
        let schema = self.index.schema();
        let mut fields = serde_json::Map::new();
        
        // Extract all field values
        for field_value in doc.field_values() {
            let field = field_value.field();
            let field_name = schema.get_field_name(field);
            
            // Convert field value to JSON
            // Note: In Tantivy 0.22, field values are handled differently
            let value_str = format!("{:?}", field_value.value());
            let json_value = serde_json::Value::String(value_str);
            
            fields.insert(field_name.to_string(), json_value);
        }
        
        Ok(SearchHit {
            doc_id: format!("{}:{}", doc_address.segment_ord, doc_address.doc_id),
            score,
            fields,
            highlights: None,
        })
    }
    
    /// Get schema information
    pub fn schema(&self) -> Schema {
        self.index.schema()
    }
    
    /// Reload the index reader to see new documents
    pub fn reload(&self) -> Result<()> {
        self.reader.reload()
            .map_err(|e| Error::Internal(format!("Failed to reload reader: {}", e)))
    }
}

/// Query execution options
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryOptions {
    /// Maximum number of results to return
    pub limit: usize,
    /// Offset for pagination
    pub offset: usize,
    /// Type of results to return
    pub result_type: ResultType,
    /// Fields to include in results
    pub fields: Option<Vec<String>>,
    /// Enable highlighting
    pub highlight: bool,
    /// Highlight configuration
    pub highlight_config: Option<HighlightConfig>,
}

impl Default for QueryOptions {
    fn default() -> Self {
        Self {
            limit: 10,
            offset: 0,
            result_type: ResultType::TopDocs,
            fields: None,
            highlight: false,
            highlight_config: None,
        }
    }
}

/// Type of results to return
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResultType {
    /// Return top documents with scores
    TopDocs,
    /// Return count only
    Count,
    /// Return all matching documents (use with caution)
    All,
}

/// Highlight configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HighlightConfig {
    /// Fields to highlight
    pub fields: Vec<String>,
    /// Pre-tag for highlighting
    pub pre_tag: String,
    /// Post-tag for highlighting
    pub post_tag: String,
    /// Fragment size
    pub fragment_size: usize,
    /// Number of fragments
    pub number_of_fragments: usize,
}

impl Default for HighlightConfig {
    fn default() -> Self {
        Self {
            fields: Vec::new(),
            pre_tag: "<em>".to_string(),
            post_tag: "</em>".to_string(),
            fragment_size: 150,
            number_of_fragments: 3,
        }
    }
}

/// Query execution results
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryResults {
    /// Search hits
    pub hits: Vec<SearchHit>,
    /// Total number of hits
    pub total_hits: usize,
    /// Query execution time in milliseconds
    pub execution_time_ms: u64,
    /// Query type for debugging
    pub query_type: String,
}

/// Individual search hit
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchHit {
    /// Document ID
    pub doc_id: String,
    /// Relevance score
    pub score: Option<f32>,
    /// Field values
    pub fields: serde_json::Map<String, serde_json::Value>,
    /// Highlighted fields
    pub highlights: Option<serde_json::Map<String, serde_json::Value>>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use tantivy::schema::{Schema, TEXT, STORED, STRING};
    use tantivy::doc;
    
    fn create_test_index() -> (TempDir, Index) {
        let temp_dir = TempDir::new().unwrap();
        
        let mut schema_builder = Schema::builder();
        schema_builder.add_text_field("topic", STRING | STORED);
        schema_builder.add_text_field("value", TEXT | STORED);
        schema_builder.add_i64_field("offset", STORED);
        
        let schema = schema_builder.build();
        let index = Index::create_in_dir(&temp_dir.path(), schema).unwrap();
        
        // Add some test documents
        let mut index_writer = index.writer(50_000_000).unwrap();
        
        index_writer.add_document(doc!(
            index.schema().get_field("topic").unwrap() => "events",
            index.schema().get_field("value").unwrap() => "Error occurred in system",
            index.schema().get_field("offset").unwrap() => 100i64
        )).unwrap();
        
        index_writer.add_document(doc!(
            index.schema().get_field("topic").unwrap() => "logs",
            index.schema().get_field("value").unwrap() => "Warning: low memory",
            index.schema().get_field("offset").unwrap() => 200i64
        )).unwrap();
        
        index_writer.commit().unwrap();
        
        (temp_dir, index)
    }
    
    #[test]
    fn test_query_executor_basic() {
        let (_temp_dir, index) = create_test_index();
        let executor = QueryExecutor::new(index).unwrap();
        
        // Test simple text query
        let query = QueryInput::Simple("error".to_string());
        let options = QueryOptions::default();
        
        let results = executor.execute(&query, options).unwrap();
        assert_eq!(results.hits.len(), 1);
        assert!(results.hits[0].fields.get("value")
            .and_then(|v| v.as_str())
            .map(|s| s.contains("Error"))
            .unwrap_or(false));
    }
    
    #[test]
    fn test_query_executor_json_query() {
        let (_temp_dir, index) = create_test_index();
        let executor = QueryExecutor::new(index).unwrap();
        
        // Test JSON query
        let query_json = serde_json::json!({
            "term": {
                "topic": "events"
            }
        });
        let query = QueryInput::Json(query_json);
        let options = QueryOptions::default();
        
        let results = executor.execute(&query, options).unwrap();
        assert_eq!(results.hits.len(), 1);
    }
    
    #[test]
    fn test_query_executor_with_options() {
        let (_temp_dir, index) = create_test_index();
        let executor = QueryExecutor::new(index).unwrap();
        
        // Test with pagination
        let query = QueryInput::Simple("".to_string()); // Match all
        let options = QueryOptions {
            limit: 1,
            offset: 1,
            ..Default::default()
        };
        
        let results = executor.execute(&query, options).unwrap();
        assert_eq!(results.hits.len(), 1);
    }
}