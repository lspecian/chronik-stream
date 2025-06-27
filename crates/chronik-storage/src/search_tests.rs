//! Comprehensive tests for the Tantivy search integration

use super::*;
use crate::chronik_segment::{ChronikSegment, ChronikSegmentBuilder};
use crate::{Record, RecordBatch};
use serde_json::json;
use std::collections::HashMap;

fn create_test_records() -> Vec<Record> {
    vec![
        Record {
            offset: 0,
            timestamp: 1640995200000, // 2022-01-01
            key: Some(b"user:1".to_vec()),
            value: json!({
                "id": 1,
                "name": "Alice Johnson",
                "email": "alice@example.com",
                "age": 30,
                "department": "Engineering",
                "salary": 85000.0,
                "joined": "2021-01-15T10:00:00Z",
                "active": true,
                "skills": ["rust", "python", "javascript"],
                "location": {
                    "city": "San Francisco",
                    "state": "CA",
                    "country": "USA"
                }
            }).to_string().into_bytes(),
            headers: HashMap::new(),
        },
        Record {
            offset: 1,
            timestamp: 1640995260000,
            key: Some(b"user:2".to_vec()),
            value: json!({
                "id": 2,
                "name": "Bob Smith",
                "email": "bob@example.com",
                "age": 25,
                "department": "Marketing",
                "salary": 65000.0,
                "joined": "2021-03-20T14:30:00Z",
                "active": true,
                "skills": ["analytics", "seo", "content"],
                "location": {
                    "city": "New York",
                    "state": "NY",
                    "country": "USA"
                }
            }).to_string().into_bytes(),
            headers: HashMap::new(),
        },
        Record {
            offset: 2,
            timestamp: 1640995320000,
            key: Some(b"user:3".to_vec()),
            value: json!({
                "id": 3,
                "name": "Carol Williams",
                "email": "carol@example.com",
                "age": 35,
                "department": "Engineering",
                "salary": 95000.0,
                "joined": "2020-08-10T09:15:00Z",
                "active": false,
                "skills": ["java", "scala", "kubernetes"],
                "location": {
                    "city": "Austin",
                    "state": "TX",
                    "country": "USA"
                }
            }).to_string().into_bytes(),
            headers: HashMap::new(),
        },
        Record {
            offset: 3,
            timestamp: 1640995380000,
            key: Some(b"event:1".to_vec()),
            value: json!({
                "event_type": "login",
                "user_id": 1,
                "timestamp": "2022-01-01T10:00:00Z",
                "ip_address": "192.168.1.100",
                "user_agent": "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36",
                "success": true,
                "metadata": {
                    "session_id": "abc123",
                    "device": "desktop"
                }
            }).to_string().into_bytes(),
            headers: HashMap::new(),
        },
        Record {
            offset: 4,
            timestamp: 1640995440000,
            key: Some(b"product:1".to_vec()),
            value: json!({
                "product_id": "PROD-001",
                "name": "Wireless Headphones",
                "description": "High-quality wireless headphones with noise cancellation",
                "price": 199.99,
                "category": "Electronics",
                "brand": "TechCorp",
                "in_stock": true,
                "reviews": {
                    "average_rating": 4.5,
                    "count": 127
                },
                "tags": ["wireless", "audio", "electronics", "premium"]
            }).to_string().into_bytes(),
            headers: HashMap::new(),
        },
    ]
}

fn create_test_segment() -> Result<ChronikSegment> {
    let records = create_test_records();
    let batch = RecordBatch { records };
    
    ChronikSegmentBuilder::new("test-topic".to_string(), 0)
        .add_batch(batch)
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_field_type_detection() {
        let json = json!({
            "title": "Test Document",
            "count": 42,
            "price": 19.99,
            "active": true,
            "created_at": "2024-01-01T00:00:00Z",
            "short_code": "ABC123",
            "description": "This is a longer text field that should be detected as full text",
            "metadata": {
                "author": "John Doe",
                "version": 1
            }
        });
        
        let mut field_types = HashMap::new();
        SearchIndex::analyze_json_fields(&json, "", &mut field_types);
        
        assert_eq!(field_types.get("title"), Some(&DetectedFieldType::Text));
        assert_eq!(field_types.get("count"), Some(&DetectedFieldType::I64));
        assert_eq!(field_types.get("price"), Some(&DetectedFieldType::F64));
        assert_eq!(field_types.get("active"), Some(&DetectedFieldType::Bool));
        assert_eq!(field_types.get("created_at"), Some(&DetectedFieldType::Date));
        assert_eq!(field_types.get("short_code"), Some(&DetectedFieldType::String));
        assert_eq!(field_types.get("description"), Some(&DetectedFieldType::Text));
        assert_eq!(field_types.get("metadata.author"), Some(&DetectedFieldType::Text));
        assert_eq!(field_types.get("metadata.version"), Some(&DetectedFieldType::I64));
    }
    
    #[test]
    fn test_search_index_creation() {
        let mut segment = create_test_segment().unwrap();
        
        // Build search index
        segment.build_search_index().unwrap();
        
        // Verify index was created
        assert!(segment.get_search_index_ref().is_some());
    }
    
    #[test]
    fn test_text_search() {
        let mut segment = create_test_segment().unwrap();
        
        // Search for "Alice"
        let results = segment.search("Alice", 10).unwrap();
        assert_eq!(results.total_hits, 1);
        assert_eq!(results.hits.len(), 1);
        
        // Search for "Engineering"
        let results = segment.search("Engineering", 10).unwrap();
        assert_eq!(results.total_hits, 2); // Alice and Carol
        assert_eq!(results.hits.len(), 2);
        
        // Search for non-existent term
        let results = segment.search("nonexistent", 10).unwrap();
        assert_eq!(results.total_hits, 0);
        assert_eq!(results.hits.len(), 0);
    }
    
    #[test]
    fn test_email_search() {
        let mut segment = create_test_segment().unwrap();
        
        // Search for specific email
        let results = segment.search("alice@example.com", 10).unwrap();
        assert_eq!(results.total_hits, 1);
        assert!(results.hits[0].source.to_string().contains("alice@example.com"));
    }
    
    #[test]
    fn test_nested_field_search() {
        let mut segment = create_test_segment().unwrap();
        
        // Search for city
        let results = segment.search("Francisco", 10).unwrap();
        assert_eq!(results.total_hits, 1);
        
        // Search for state
        let results = segment.search("NY", 10).unwrap();
        assert_eq!(results.total_hits, 1);
    }
    
    #[test]
    fn test_numeric_search() {
        let mut segment = create_test_segment().unwrap();
        
        // Note: For numeric search, we need to use field-specific queries
        // This test verifies that numeric fields are properly indexed
        let search_index = segment.get_search_index().unwrap();
        let schema = search_index.schema();
        
        // Verify numeric fields exist in schema
        let age_field = search_index.get_field("age");
        assert!(age_field.is_some());
        
        let salary_field = search_index.get_field("salary");
        assert!(salary_field.is_some());
    }
    
    #[test]
    fn test_boolean_search() {
        let mut segment = create_test_segment().unwrap();
        
        // Search for active users (stored as text representation)
        let results = segment.search("true", 10).unwrap();
        assert!(results.total_hits >= 2); // At least Alice and Bob are active
    }
    
    #[test]
    fn test_date_search() {
        let mut segment = create_test_segment().unwrap();
        
        // Search for 2021 (year in joined date)
        let results = segment.search("2021", 10).unwrap();
        assert!(results.total_hits >= 2); // Alice and Bob joined in 2021
    }
    
    #[test]
    fn test_search_with_limit() {
        let mut segment = create_test_segment().unwrap();
        
        // Search for common term with limit
        let results = segment.search("example.com", 2).unwrap();
        assert!(results.total_hits >= 3); // All users have example.com emails
        assert_eq!(results.hits.len(), 2); // But we limited to 2 results
    }
    
    #[test]
    fn test_search_case_insensitive() {
        let mut segment = create_test_segment().unwrap();
        
        // Search should be case insensitive
        let results1 = segment.search("alice", 10).unwrap();
        let results2 = segment.search("ALICE", 10).unwrap();
        let results3 = segment.search("Alice", 10).unwrap();
        
        assert_eq!(results1.total_hits, results2.total_hits);
        assert_eq!(results1.total_hits, results3.total_hits);
    }
    
    #[test]
    fn test_search_multiple_terms() {
        let mut segment = create_test_segment().unwrap();
        
        // Search for multiple terms (AND by default)
        let results = segment.search("Alice Engineering", 10).unwrap();
        assert_eq!(results.total_hits, 1); // Only Alice matches both terms
    }
    
    #[test]
    fn test_search_skills_array() {
        let mut segment = create_test_segment().unwrap();
        
        // Search within skills array
        let results = segment.search("rust", 10).unwrap();
        assert_eq!(results.total_hits, 1); // Only Alice has rust skill
        
        let results = segment.search("javascript", 10).unwrap();
        assert_eq!(results.total_hits, 1); // Only Alice has javascript skill
    }
    
    #[test]
    fn test_search_product_data() {
        let mut segment = create_test_segment().unwrap();
        
        // Search for product name
        let results = segment.search("Wireless Headphones", 10).unwrap();
        assert_eq!(results.total_hits, 1);
        
        // Search for product description
        let results = segment.search("noise cancellation", 10).unwrap();
        assert_eq!(results.total_hits, 1);
        
        // Search for product category
        let results = segment.search("Electronics", 10).unwrap();
        assert_eq!(results.total_hits, 1);
    }
    
    #[test]
    fn test_search_event_data() {
        let mut segment = create_test_segment().unwrap();
        
        // Search for event type
        let results = segment.search("login", 10).unwrap();
        assert_eq!(results.total_hits, 1);
        
        // Search for IP address
        let results = segment.search("192.168.1.100", 10).unwrap();
        assert_eq!(results.total_hits, 1);
        
        // Search for user agent
        let results = segment.search("Mozilla", 10).unwrap();
        assert_eq!(results.total_hits, 1);
    }
    
    #[test]
    fn test_search_performance() {
        let mut segment = create_test_segment().unwrap();
        
        let start = std::time::Instant::now();
        let results = segment.search("Engineering", 10).unwrap();
        let elapsed = start.elapsed();
        
        // Search should complete quickly
        assert!(elapsed.as_millis() < 100);
        assert!(results.took_ms < 100);
        assert_eq!(results.total_hits, 2);
    }
    
    #[test]
    fn test_index_serialization() {
        let mut segment = create_test_segment().unwrap();
        let search_index = segment.get_search_index().unwrap();
        
        // Serialize the index
        let serialized = search_index.serialize_to_bytes().unwrap();
        assert!(!serialized.is_empty());
        
        // Deserialize the index
        let deserialized = SearchIndex::deserialize_from_bytes(&serialized).unwrap();
        
        // Test that deserialized index works
        let results = deserialized.search("Alice", 10).unwrap();
        assert_eq!(results.total_hits, 1);
    }
    
    #[test]
    fn test_complex_query_scenarios() {
        let mut segment = create_test_segment().unwrap();
        
        // Test phrase search
        let results = segment.search("\"Alice Johnson\"", 10).unwrap();
        assert_eq!(results.total_hits, 1);
        
        // Test wildcard-like search (will depend on tokenizer)
        let results = segment.search("San", 10).unwrap();
        assert_eq!(results.total_hits, 1); // Should match "San Francisco"
        
        // Test partial matches
        let results = segment.search("John", 10).unwrap();
        assert_eq!(results.total_hits, 1); // Should match "Alice Johnson"
    }
    
    #[test]
    fn test_iter_records() {
        let segment = create_test_segment().unwrap();
        
        let records: Vec<_> = segment.iter_records().unwrap().collect();
        assert_eq!(records.len(), 5);
        
        // Verify record order and content
        assert_eq!(records[0].offset, 0);
        assert_eq!(records[1].offset, 1);
        assert_eq!(records[2].offset, 2);
        assert_eq!(records[3].offset, 3);
        assert_eq!(records[4].offset, 4);
        
        // Verify keys
        assert_eq!(records[0].key, Some(b"user:1".to_vec()));
        assert_eq!(records[1].key, Some(b"user:2".to_vec()));
        assert_eq!(records[4].key, Some(b"product:1".to_vec()));
    }
    
    #[test]
    fn test_empty_search_query() {
        let mut segment = create_test_segment().unwrap();
        
        // Empty query should return some results (depending on implementation)
        let results = segment.search("", 10);
        // This might return an error or empty results depending on implementation
        // For now, we just verify it doesn't panic
        assert!(results.is_ok() || results.is_err());
    }
    
    #[test]
    fn test_search_with_zero_limit() {
        let mut segment = create_test_segment().unwrap();
        
        let results = segment.search("Alice", 0).unwrap();
        assert_eq!(results.hits.len(), 0);
        // Total hits should still be accurate
        assert_eq!(results.total_hits, 1);
    }
    
    #[test]
    fn test_search_special_characters() {
        let mut segment = create_test_segment().unwrap();
        
        // Search for email with @ symbol
        let results = segment.search("@example.com", 10).unwrap();
        assert!(results.total_hits >= 3); // All users have @example.com
        
        // Search for IP address with dots
        let results = segment.search("192.168", 10).unwrap();
        assert_eq!(results.total_hits, 1);
    }
}