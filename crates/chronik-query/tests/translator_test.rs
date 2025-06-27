//! Comprehensive tests for the query translation layer.

use chronik_query::{QueryTranslator, QueryInput, KafkaQuery, ValueQuery};
use serde_json::json;
use tantivy::schema::{Schema, TEXT, STORED, STRING, NumericOptions};

/// Create a test schema that represents a typical Kafka message index
fn create_kafka_schema() -> Schema {
    let mut schema_builder = Schema::builder();
    
    // Core Kafka fields
    schema_builder.add_text_field("topic", STRING | STORED);
    schema_builder.add_i64_field("partition", NumericOptions::default().set_indexed().set_stored());
    schema_builder.add_i64_field("offset", NumericOptions::default().set_indexed().set_stored());
    schema_builder.add_i64_field("timestamp", NumericOptions::default().set_indexed().set_stored());
    schema_builder.add_text_field("key", TEXT | STORED);
    schema_builder.add_text_field("value", TEXT | STORED);
    schema_builder.add_text_field("headers", TEXT | STORED);
    
    // Additional fields for testing
    schema_builder.add_text_field("message", TEXT | STORED);
    schema_builder.add_text_field("user", STRING | STORED);
    schema_builder.add_text_field("event_type", STRING | STORED);
    schema_builder.add_f64_field("score", NumericOptions::default().set_indexed().set_stored());
    schema_builder.add_u64_field("count", NumericOptions::default().set_indexed().set_stored());
    
    schema_builder.build()
}

#[test]
fn test_elasticsearch_match_query() {
    let schema = create_kafka_schema();
    let translator = QueryTranslator::new(schema);
    
    // Simple match query
    let query = json!({
        "match": {
            "value": "error occurred"
        }
    });
    
    let result = translator.translate_json_query(&query);
    assert!(result.is_ok(), "Match query should translate successfully");
    
    // Match query with options
    let query_with_options = json!({
        "match": {
            "value": {
                "query": "error occurred",
                "operator": "and",
                "boost": 2.0
            }
        }
    });
    
    let result = translator.translate_json_query(&query_with_options);
    assert!(result.is_ok(), "Match query with options should translate successfully");
}

#[test]
fn test_elasticsearch_term_query() {
    let schema = create_kafka_schema();
    let translator = QueryTranslator::new(schema);
    
    // String term query
    let query = json!({
        "term": {
            "topic": "events"
        }
    });
    
    let result = translator.translate_json_query(&query);
    assert!(result.is_ok(), "String term query should translate successfully");
    
    // Numeric term query
    let numeric_query = json!({
        "term": {
            "partition": 0
        }
    });
    
    let result = translator.translate_json_query(&numeric_query);
    assert!(result.is_ok(), "Numeric term query should translate successfully");
    
    // Boolean term query
    let bool_query = json!({
        "term": {
            "key": true
        }
    });
    
    let result = translator.translate_json_query(&bool_query);
    assert!(result.is_ok(), "Boolean term query should translate successfully");
}

#[test]
fn test_elasticsearch_terms_query() {
    let schema = create_kafka_schema();
    let translator = QueryTranslator::new(schema);
    
    let query = json!({
        "terms": {
            "event_type": ["login", "logout", "signup"]
        }
    });
    
    let result = translator.translate_json_query(&query);
    assert!(result.is_ok(), "Terms query should translate successfully");
}

#[test]
fn test_elasticsearch_range_query() {
    let schema = create_kafka_schema();
    let translator = QueryTranslator::new(schema);
    
    // Timestamp range query
    let query = json!({
        "range": {
            "timestamp": {
                "gte": 1000000,
                "lte": 2000000
            }
        }
    });
    
    let result = translator.translate_json_query(&query);
    assert!(result.is_ok(), "Timestamp range query should translate successfully");
    
    // Offset range with exclusive bounds
    let exclusive_query = json!({
        "range": {
            "offset": {
                "gt": 100,
                "lt": 200
            }
        }
    });
    
    let result = translator.translate_json_query(&exclusive_query);
    assert!(result.is_ok(), "Exclusive range query should translate successfully");
    
    // Open-ended range
    let open_query = json!({
        "range": {
            "score": {
                "gte": 0.5
            }
        }
    });
    
    let result = translator.translate_json_query(&open_query);
    assert!(result.is_ok(), "Open-ended range query should translate successfully");
}

#[test]
fn test_elasticsearch_bool_query() {
    let schema = create_kafka_schema();
    let translator = QueryTranslator::new(schema);
    
    let complex_query = json!({
        "bool": {
            "must": [
                {"term": {"topic": "events"}},
                {"range": {"timestamp": {"gte": 1000}}}
            ],
            "should": [
                {"match": {"value": "error"}},
                {"match": {"value": "warning"}}
            ],
            "must_not": [
                {"term": {"user": "system"}}
            ],
            "filter": [
                {"term": {"partition": 0}}
            ]
        }
    });
    
    let result = translator.translate_json_query(&complex_query);
    assert!(result.is_ok(), "Complex bool query should translate successfully");
}

#[test]
fn test_elasticsearch_wildcard_query() {
    let schema = create_kafka_schema();
    let translator = QueryTranslator::new(schema);
    
    let query = json!({
        "wildcard": {
            "key": "user:*"
        }
    });
    
    let result = translator.translate_json_query(&query);
    assert!(result.is_ok(), "Wildcard query should translate successfully");
}

#[test]
fn test_elasticsearch_regexp_query() {
    let schema = create_kafka_schema();
    let translator = QueryTranslator::new(schema);
    
    let query = json!({
        "regexp": {
            "value": "error\\s+[0-9]+"
        }
    });
    
    let result = translator.translate_json_query(&query);
    assert!(result.is_ok(), "Regexp query should translate successfully");
}

#[test]
fn test_elasticsearch_match_phrase_query() {
    let schema = create_kafka_schema();
    let translator = QueryTranslator::new(schema);
    
    let query = json!({
        "match_phrase": {
            "message": "exact phrase match"
        }
    });
    
    let result = translator.translate_json_query(&query);
    assert!(result.is_ok(), "Match phrase query should translate successfully");
}

#[test]
fn test_elasticsearch_fuzzy_query() {
    let schema = create_kafka_schema();
    let translator = QueryTranslator::new(schema);
    
    // Simple fuzzy query
    let query = json!({
        "fuzzy": {
            "user": "johm"
        }
    });
    
    let result = translator.translate_json_query(&query);
    assert!(result.is_ok(), "Simple fuzzy query should translate successfully");
    
    // Fuzzy query with options
    let query_with_options = json!({
        "fuzzy": {
            "user": {
                "value": "johm",
                "fuzziness": 2,
                "prefix_length": 1
            }
        }
    });
    
    let result = translator.translate_json_query(&query_with_options);
    assert!(result.is_ok(), "Fuzzy query with options should translate successfully");
}

#[test]
fn test_elasticsearch_exists_query() {
    let schema = create_kafka_schema();
    let translator = QueryTranslator::new(schema);
    
    let query = json!({
        "exists": {
            "field": "headers"
        }
    });
    
    let result = translator.translate_json_query(&query);
    assert!(result.is_ok(), "Exists query should translate successfully");
}

#[test]
fn test_sql_simple_queries() {
    let schema = create_kafka_schema();
    let translator = QueryTranslator::new(schema);
    
    // Simple equality
    let sql = "SELECT * FROM messages WHERE topic = 'events'";
    let query = QueryInput::Sql(sql.to_string());
    let result = translator.translate(&query);
    assert!(result.is_ok(), "Simple SQL equality should translate");
    
    // Numeric comparison
    let sql = "SELECT * FROM messages WHERE partition > 0";
    let query = QueryInput::Sql(sql.to_string());
    let result = translator.translate(&query);
    assert!(result.is_ok(), "SQL numeric comparison should translate");
    
    // LIKE query
    let sql = "SELECT * FROM messages WHERE value LIKE '%error%'";
    let query = QueryInput::Sql(sql.to_string());
    let result = translator.translate(&query);
    assert!(result.is_ok(), "SQL LIKE query should translate");
    
    // IN clause
    let sql = "SELECT * FROM messages WHERE event_type IN ('login', 'logout')";
    let query = QueryInput::Sql(sql.to_string());
    let result = translator.translate(&query);
    assert!(result.is_ok(), "SQL IN clause should translate");
    
    // BETWEEN clause
    let sql = "SELECT * FROM messages WHERE timestamp BETWEEN 1000 AND 2000";
    let query = QueryInput::Sql(sql.to_string());
    let result = translator.translate(&query);
    assert!(result.is_ok(), "SQL BETWEEN clause should translate");
}

#[test]
fn test_sql_complex_queries() {
    let schema = create_kafka_schema();
    let translator = QueryTranslator::new(schema);
    
    // AND/OR combinations
    let sql = "SELECT * FROM messages WHERE topic = 'events' AND partition = 0 OR user = 'admin'";
    let query = QueryInput::Sql(sql.to_string());
    let result = translator.translate(&query);
    assert!(result.is_ok(), "SQL with AND/OR should translate");
    
    // Multiple conditions
    let sql = "SELECT * FROM messages WHERE topic = 'events' AND timestamp > 1000 AND value LIKE '%error%'";
    let query = QueryInput::Sql(sql.to_string());
    let result = translator.translate(&query);
    assert!(result.is_ok(), "SQL with multiple conditions should translate");
    
    // NOT EQUAL
    let sql = "SELECT * FROM messages WHERE user != 'system'";
    let query = QueryInput::Sql(sql.to_string());
    let result = translator.translate(&query);
    assert!(result.is_ok(), "SQL NOT EQUAL should translate");
}

#[test]
fn test_simple_text_query() {
    let schema = create_kafka_schema();
    let translator = QueryTranslator::new(schema);
    
    let query = QueryInput::Simple("error warning".to_string());
    let result = translator.translate(&query);
    assert!(result.is_ok(), "Simple text query should translate");
}

#[test]
fn test_kafka_specific_query() {
    let schema = create_kafka_schema();
    let translator = QueryTranslator::new(schema);
    
    // Basic Kafka query
    let kafka_query = KafkaQuery {
        topic: Some("events".to_string()),
        partition: Some(0),
        start_offset: Some(100),
        end_offset: Some(200),
        start_timestamp: Some(1000000),
        end_timestamp: Some(2000000),
        key_pattern: Some("user:*"),
        value_query: Some(ValueQuery::Text("error".to_string())),
        headers: vec![
            ("type".to_string(), "error".to_string()),
            ("severity".to_string(), "high".to_string()),
        ].into_iter().collect(),
    };
    
    let query = QueryInput::Kafka(kafka_query);
    let result = translator.translate(&query);
    assert!(result.is_ok(), "Kafka query should translate successfully");
    
    // Kafka query with JSON value
    let json_value_query = KafkaQuery {
        topic: Some("events".to_string()),
        partition: None,
        start_offset: None,
        end_offset: None,
        start_timestamp: None,
        end_timestamp: None,
        key_pattern: None,
        value_query: Some(ValueQuery::Json(json!({
            "match": {
                "message": "error"
            }
        }))),
        headers: Default::default(),
    };
    
    let query = QueryInput::Kafka(json_value_query);
    let result = translator.translate(&query);
    assert!(result.is_ok(), "Kafka query with JSON value should translate");
    
    // Kafka query with regex value
    let regex_value_query = KafkaQuery {
        topic: Some("logs".to_string()),
        partition: None,
        start_offset: None,
        end_offset: None,
        start_timestamp: None,
        end_timestamp: None,
        key_pattern: None,
        value_query: Some(ValueQuery::Regex(r"ERROR\s+\d{3}".to_string())),
        headers: Default::default(),
    };
    
    let query = QueryInput::Kafka(regex_value_query);
    let result = translator.translate(&query);
    assert!(result.is_ok(), "Kafka query with regex value should translate");
}

#[test]
fn test_edge_cases() {
    let schema = create_kafka_schema();
    let translator = QueryTranslator::new(schema);
    
    // Empty bool query
    let empty_bool = json!({
        "bool": {}
    });
    let result = translator.translate_json_query(&empty_bool);
    assert!(result.is_ok(), "Empty bool query should translate");
    
    // Match all query
    let match_all = json!({
        "match_all": {}
    });
    let result = translator.translate_json_query(&match_all);
    assert!(result.is_ok(), "Match all query should translate");
    
    // Empty terms array
    let empty_terms = json!({
        "terms": {
            "topic": []
        }
    });
    let result = translator.translate_json_query(&empty_terms);
    assert!(result.is_ok(), "Empty terms query should translate");
    
    // Invalid field name
    let invalid_field = json!({
        "term": {
            "non_existent_field": "value"
        }
    });
    let result = translator.translate_json_query(&invalid_field);
    assert!(result.is_err(), "Query with invalid field should fail");
}

#[test]
fn test_nested_queries() {
    let schema = create_kafka_schema();
    let translator = QueryTranslator::new(schema);
    
    // Deeply nested bool query
    let nested_query = json!({
        "bool": {
            "must": [{
                "bool": {
                    "should": [
                        {"term": {"topic": "events"}},
                        {"term": {"topic": "logs"}}
                    ]
                }
            }],
            "filter": [{
                "bool": {
                    "must": [
                        {"range": {"timestamp": {"gte": 1000}}},
                        {"term": {"partition": 0}}
                    ]
                }
            }]
        }
    });
    
    let result = translator.translate_json_query(&nested_query);
    assert!(result.is_ok(), "Deeply nested query should translate successfully");
}

#[test]
fn test_performance_considerations() {
    let schema = create_kafka_schema();
    let translator = QueryTranslator::new(schema);
    
    // Query that would benefit from optimization
    let query = json!({
        "bool": {
            "should": [
                {"wildcard": {"value": "*error*"}},
                {"wildcard": {"value": "*warning*"}},
                {"wildcard": {"value": "*critical*"}}
            ],
            "minimum_should_match": 1
        }
    });
    
    let result = translator.translate_json_query(&query);
    assert!(result.is_ok(), "Query with multiple wildcards should translate");
    
    // Note: In production, such queries should be optimized or warned about
}