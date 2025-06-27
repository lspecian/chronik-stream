//! Example demonstrating the Tantivy query translation layer.

use chronik_query::{QueryTranslator, QueryInput, KafkaQuery, ValueQuery};
use serde_json::json;
use tantivy::schema::{Schema, TEXT, STORED, STRING, NumericOptions};

fn main() {
    // Create a schema similar to what would be used for Kafka messages
    let schema = create_kafka_schema();
    let translator = QueryTranslator::new(schema);
    
    println!("=== Tantivy Query Translation Examples ===\n");
    
    // Example 1: Elasticsearch-style JSON query
    println!("1. Elasticsearch-style JSON Query:");
    let es_query = json!({
        "bool": {
            "must": [
                {"term": {"topic": "events"}},
                {"range": {"timestamp": {"gte": 1000000, "lte": 2000000}}}
            ],
            "should": [
                {"match": {"value": "error warning"}},
                {"wildcard": {"key": "user:*"}}
            ],
            "must_not": [
                {"term": {"user": "system"}}
            ]
        }
    });
    
    println!("Input: {}", serde_json::to_string_pretty(&es_query).unwrap());
    
    match translator.translate_json_query(&es_query) {
        Ok(_query) => println!("✓ Successfully translated to Tantivy query\n"),
        Err(e) => println!("✗ Translation failed: {}\n", e),
    }
    
    // Example 2: SQL-like query
    println!("2. SQL-like Query:");
    let sql = "SELECT * FROM messages WHERE topic = 'events' AND timestamp > 1000000 AND value LIKE '%error%'";
    println!("Input: {}", sql);
    
    let sql_query = QueryInput::Sql(sql.to_string());
    match translator.translate(&sql_query) {
        Ok(_query) => println!("✓ Successfully translated SQL to Tantivy query\n"),
        Err(e) => println!("✗ Translation failed: {}\n", e),
    }
    
    // Example 3: Kafka-specific query
    println!("3. Kafka-specific Query:");
    let kafka_query = KafkaQuery {
        topic: Some("events".to_string()),
        partition: Some(0),
        start_offset: Some(1000),
        end_offset: Some(2000),
        start_timestamp: Some(1000000),
        end_timestamp: Some(2000000),
        key_pattern: Some("user:*"),
        value_query: Some(ValueQuery::Text("error OR warning".to_string())),
        headers: vec![
            ("type".to_string(), "error".to_string()),
            ("severity".to_string(), "high".to_string()),
        ].into_iter().collect(),
    };
    
    println!("Input: {:#?}", kafka_query);
    
    let query = QueryInput::Kafka(kafka_query);
    match translator.translate(&query) {
        Ok(_query) => println!("✓ Successfully translated Kafka query to Tantivy\n"),
        Err(e) => println!("✗ Translation failed: {}\n", e),
    }
    
    // Example 4: Simple text search
    println!("4. Simple Text Search:");
    let simple_query = QueryInput::Simple("error warning critical".to_string());
    println!("Input: \"error warning critical\"");
    
    match translator.translate(&simple_query) {
        Ok(_query) => println!("✓ Successfully translated simple query to Tantivy\n"),
        Err(e) => println!("✗ Translation failed: {}\n", e),
    }
    
    // Example 5: Complex nested query
    println!("5. Complex Nested Query:");
    let complex_query = json!({
        "bool": {
            "must": [{
                "bool": {
                    "should": [
                        {"match": {"value": "authentication failed"}},
                        {"match": {"value": "access denied"}}
                    ]
                }
            }],
            "filter": [
                {"range": {"timestamp": {"gte": "now-1h"}}},
                {"terms": {"partition": [0, 1, 2]}}
            ],
            "should": [
                {"match_phrase": {"message": "critical error"}},
                {"fuzzy": {"user": {"value": "admim", "fuzziness": 2}}}
            ],
            "minimum_should_match": 1
        }
    });
    
    println!("Input: {}", serde_json::to_string_pretty(&complex_query).unwrap());
    
    match translator.translate_json_query(&complex_query) {
        Ok(_query) => println!("✓ Successfully translated complex query to Tantivy\n"),
        Err(e) => println!("✗ Translation failed: {}\n", e),
    }
    
    // Example 6: Different query types
    println!("6. Various Query Types:");
    
    let query_types = vec![
        ("Match All", json!({"match_all": {}})),
        ("Exists", json!({"exists": {"field": "headers"}})),
        ("Regexp", json!({"regexp": {"value": "ERROR\\s+[0-9]+"}})),
        ("Wildcard", json!({"wildcard": {"key": "session:*:active"}})),
        ("Range", json!({"range": {"score": {"gte": 0.5, "lte": 1.0}}})),
    ];
    
    for (name, query) in query_types {
        println!("  - {}: {}", name, serde_json::to_string(&query).unwrap());
        match translator.translate_json_query(&query) {
            Ok(_) => println!("    ✓ Success"),
            Err(e) => println!("    ✗ Failed: {}", e),
        }
    }
    
    println!("\n=== Translation Complete ===");
}

fn create_kafka_schema() -> Schema {
    let mut schema_builder = Schema::builder();
    
    // Kafka message fields
    schema_builder.add_text_field("topic", STRING | STORED);
    schema_builder.add_i64_field("partition", NumericOptions::default().set_indexed().set_stored());
    schema_builder.add_i64_field("offset", NumericOptions::default().set_indexed().set_stored());
    schema_builder.add_i64_field("timestamp", NumericOptions::default().set_indexed().set_stored());
    schema_builder.add_text_field("key", TEXT | STORED);
    schema_builder.add_text_field("value", TEXT | STORED);
    schema_builder.add_text_field("headers", TEXT | STORED);
    
    // Additional fields
    schema_builder.add_text_field("message", TEXT | STORED);
    schema_builder.add_text_field("user", STRING | STORED);
    schema_builder.add_f64_field("score", NumericOptions::default().set_indexed().set_stored());
    
    schema_builder.build()
}