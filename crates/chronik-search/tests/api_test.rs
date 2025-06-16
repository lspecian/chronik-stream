//! Integration tests for the search API.

use chronik_search::{SearchApi, IndexerConfig};
use axum::http::StatusCode;
use axum_test::TestServer;
use serde_json::json;
use std::sync::Arc;

/// Create a test server
async fn create_test_server() -> TestServer {
    let api = Arc::new(SearchApi::new().unwrap());
    let app = api.router();
    TestServer::new(app).unwrap()
}

#[tokio::test]
async fn test_health_endpoint() {
    let server = create_test_server().await;
    
    let response = server.get("/health").await;
    response.assert_status(StatusCode::OK);
    
    let body: serde_json::Value = response.json();
    assert_eq!(body["status"], "green");
}

#[tokio::test]
async fn test_create_and_search_index() {
    let server = create_test_server().await;
    
    // Create index with mapping
    let mapping = json!({
        "mappings": {
            "properties": {
                "_id": {"type": "keyword", "store": true},
                "title": {"type": "text", "store": true},
                "content": {"type": "text", "store": true},
                "timestamp": {"type": "long", "store": true}
            }
        }
    });
    
    let response = server.put("/test-index")
        .json(&mapping)
        .await;
    response.assert_status(StatusCode::OK);
    
    // Index a document
    let doc = json!({
        "title": "Test Document",
        "content": "This is a test document for search",
        "timestamp": 1640000000000i64
    });
    
    let response = server.post("/test-index/_doc/doc1")
        .json(&doc)
        .await;
    response.assert_status(StatusCode::CREATED);
    
    // Search for the document
    let query = json!({
        "query": {
            "match": {
                "content": "test"
            }
        }
    });
    
    let response = server.post("/test-index/_search")
        .json(&query)
        .await;
    response.assert_status(StatusCode::OK);
    
    let body: serde_json::Value = response.json();
    assert_eq!(body["hits"]["total"]["value"], 1);
    assert_eq!(body["hits"]["hits"][0]["_id"], "doc1");
}

#[tokio::test]
async fn test_get_document() {
    let server = create_test_server().await;
    
    // Create index
    let mapping = json!({
        "mappings": {
            "properties": {
                "_id": {"type": "keyword", "store": true},
                "name": {"type": "text", "store": true}
            }
        }
    });
    
    server.put("/test-index2")
        .json(&mapping)
        .await
        .assert_status(StatusCode::OK);
    
    // Index a document
    let doc = json!({
        "name": "Test Name"
    });
    
    server.post("/test-index2/_doc/doc2")
        .json(&doc)
        .await
        .assert_status(StatusCode::CREATED);
    
    // Get the document
    let response = server.get("/test-index2/_doc/doc2").await;
    response.assert_status(StatusCode::OK);
    
    let body: serde_json::Value = response.json();
    assert_eq!(body["found"], true);
    assert_eq!(body["_id"], "doc2");
    assert_eq!(body["_source"]["name"], "Test Name");
}

#[tokio::test]
async fn test_delete_document() {
    let server = create_test_server().await;
    
    // Create index
    let mapping = json!({
        "mappings": {
            "properties": {
                "_id": {"type": "keyword", "store": true},
                "data": {"type": "text", "store": true}
            }
        }
    });
    
    server.put("/test-index3")
        .json(&mapping)
        .await
        .assert_status(StatusCode::OK);
    
    // Index a document
    let doc = json!({
        "data": "To be deleted"
    });
    
    server.post("/test-index3/_doc/doc3")
        .json(&doc)
        .await
        .assert_status(StatusCode::CREATED);
    
    // Delete the document
    let response = server.delete("/test-index3/_doc/doc3").await;
    response.assert_status(StatusCode::OK);
    
    let body: serde_json::Value = response.json();
    assert_eq!(body["result"], "deleted");
    
    // Verify it's gone
    let response = server.get("/test-index3/_doc/doc3").await;
    let body: serde_json::Value = response.json();
    assert_eq!(body["found"], false);
}

#[tokio::test]
async fn test_bool_query() {
    let server = create_test_server().await;
    
    // Create index
    let mapping = json!({
        "mappings": {
            "properties": {
                "_id": {"type": "keyword", "store": true},
                "category": {"type": "keyword", "store": true},
                "title": {"type": "text", "store": true},
                "status": {"type": "keyword", "store": true}
            }
        }
    });
    
    server.put("/test-index4")
        .json(&mapping)
        .await
        .assert_status(StatusCode::OK);
    
    // Index multiple documents
    let docs = vec![
        ("doc1", json!({"category": "tech", "title": "Introduction to Rust", "status": "published"})),
        ("doc2", json!({"category": "tech", "title": "Advanced Rust Patterns", "status": "draft"})),
        ("doc3", json!({"category": "science", "title": "Rust in Scientific Computing", "status": "published"})),
    ];
    
    for (id, doc) in docs {
        server.post(&format!("/test-index4/_doc/{}", id))
            .json(&doc)
            .await
            .assert_status(StatusCode::CREATED);
    }
    
    // Bool query: tech category AND published status
    let query = json!({
        "query": {
            "bool": {
                "must": [
                    {"term": {"category": "tech"}},
                    {"term": {"status": "published"}}
                ]
            }
        }
    });
    
    let response = server.post("/test-index4/_search")
        .json(&query)
        .await;
    response.assert_status(StatusCode::OK);
    
    let body: serde_json::Value = response.json();
    assert_eq!(body["hits"]["total"]["value"], 1);
    assert_eq!(body["hits"]["hits"][0]["_id"], "doc1");
}

#[tokio::test]
async fn test_cat_indices() {
    let server = create_test_server().await;
    
    // Create a few indices
    for i in 1..=3 {
        let mapping = json!({
            "mappings": {
                "properties": {
                    "_id": {"type": "keyword", "store": true}
                }
            }
        });
        
        server.put(&format!("/cat-test-{}", i))
            .json(&mapping)
            .await
            .assert_status(StatusCode::OK);
    }
    
    // Get indices list
    let response = server.get("/_cat/indices").await;
    response.assert_status(StatusCode::OK);
    
    let body: Vec<serde_json::Value> = response.json();
    assert!(body.len() >= 3);
    
    // Check that our indices are in the list
    let index_names: Vec<String> = body.iter()
        .filter_map(|idx| idx["index"].as_str())
        .map(|s| s.to_string())
        .collect();
    
    assert!(index_names.contains(&"cat-test-1".to_string()));
    assert!(index_names.contains(&"cat-test-2".to_string()));
    assert!(index_names.contains(&"cat-test-3".to_string()));
}

#[tokio::test]
async fn test_error_responses() {
    let server = create_test_server().await;
    
    // Try to search non-existent index
    let query = json!({
        "query": {"match_all": {}}
    });
    
    let response = server.post("/non-existent/_search")
        .json(&query)
        .await;
    response.assert_status(StatusCode::NOT_FOUND);
    
    let body: serde_json::Value = response.json();
    assert_eq!(body["error"]["type"], "index_not_found_exception");
    
    // Try to get non-existent document
    let response = server.get("/non-existent/_doc/123").await;
    response.assert_status(StatusCode::NOT_FOUND);
    
    let body: serde_json::Value = response.json();
    assert_eq!(body["error"]["type"], "index_not_found_exception");
}