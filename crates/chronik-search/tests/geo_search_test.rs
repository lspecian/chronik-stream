//! Integration tests for geo-spatial search functionality

use chronik_search::{SearchApi, api::{IndexMapping, FieldMapping}};
use std::{collections::HashMap, sync::Arc};
use serde_json::json;
use axum_test::TestServer;

#[tokio::test]
async fn test_geo_point_indexing() {
    // Create search API
    let api = Arc::new(SearchApi::new().unwrap());
    
    // Create mapping with geo_point field
    let mut properties = HashMap::new();
    properties.insert("name".to_string(), FieldMapping {
        field_type: "text".to_string(),
        analyzer: None,
        store: Some(true),
        index: Some(true),
    });
    properties.insert("location".to_string(), FieldMapping {
        field_type: "geo_point".to_string(),
        analyzer: None,
        store: Some(true),
        index: Some(true),
    });
    
    let mapping = IndexMapping { properties };
    
    // Create index
    api.create_index_with_mapping("test_geo".to_string(), mapping).await.unwrap();
    
    // Verify the schema has lat/lon fields
    let state = api.indices.get("test_geo").unwrap();
    assert!(state.schema.get_field("location_lat").is_ok());
    assert!(state.schema.get_field("location_lon").is_ok());
}

#[tokio::test]
async fn test_geo_distance_query() {
    let api = Arc::new(SearchApi::new().unwrap());
    let app = api.router();
    let server = TestServer::new(app).unwrap();
    
    // Create index with geo mapping
    let mapping = json!({
        "mappings": {
            "properties": {
                "name": {"type": "text"},
                "location": {"type": "geo_point"}
            }
        }
    });
    
    server.put("/test_places")
        .json(&mapping)
        .await
        .assert_status_ok();
    
    // Index some documents
    let docs = vec![
        ("1", json!({"name": "Near", "location": {"lat": 40.7128, "lon": -74.0060}})),
        ("2", json!({"name": "Far", "location": {"lat": 51.5074, "lon": -0.1278}})),
    ];
    
    for (id, doc) in docs {
        server.put(&format!("/test_places/_doc/{}", id))
            .json(&json!({"document": doc}))
            .await
            .assert_status_success();
    }
    
    // Search with geo distance query
    let query = json!({
        "query": {
            "geo_distance": {
                "field": "location",
                "distance": "100km",
                "lat": 40.7128,
                "lon": -74.0060
            }
        }
    });
    
    let response = server.post("/test_places/_search")
        .json(&query)
        .await;
    
    response.assert_status_ok();
    // In a complete test, we'd verify the response contains the expected documents
}

#[tokio::test]
async fn test_geo_bounding_box_query() {
    let api = Arc::new(SearchApi::new().unwrap());
    let app = api.router();
    let server = TestServer::new(app).unwrap();
    
    // Create index
    server.put("/test_bbox")
        .json(&json!({
            "mappings": {
                "properties": {
                    "location": {"type": "geo_point"}
                }
            }
        }))
        .await
        .assert_status_ok();
    
    // Test bounding box query
    let query = json!({
        "query": {
            "geo_bounding_box": {
                "field": "location",
                "top_left": {"lat": 41.0, "lon": -75.0},
                "bottom_right": {"lat": 40.0, "lon": -73.0}
            }
        }
    });
    
    let response = server.post("/test_bbox/_search")
        .json(&query)
        .await;
    
    response.assert_status_ok();
}

#[tokio::test]
async fn test_geo_polygon_query() {
    let api = Arc::new(SearchApi::new().unwrap());
    let app = api.router();
    let server = TestServer::new(app).unwrap();
    
    // Create index
    server.put("/test_polygon")
        .json(&json!({
            "mappings": {
                "properties": {
                    "location": {"type": "geo_point"}
                }
            }
        }))
        .await
        .assert_status_ok();
    
    // Test polygon query
    let query = json!({
        "query": {
            "geo_polygon": {
                "field": "location",
                "points": [
                    {"lat": 40.0, "lon": -74.0},
                    {"lat": 41.0, "lon": -74.0},
                    {"lat": 40.5, "lon": -73.0}
                ]
            }
        }
    });
    
    let response = server.post("/test_polygon/_search")
        .json(&query)
        .await;
    
    response.assert_status_ok();
}