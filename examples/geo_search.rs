//! Example demonstrating geo-spatial search functionality in Chronik Stream

use chronik_search::{SearchApi, api::{IndexMapping, FieldMapping}};
use std::{collections::HashMap, sync::Arc};
use serde_json::json;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize the search API
    let api = Arc::new(SearchApi::new()?);
    
    // Create an index with geo_point mapping
    let mut properties = HashMap::new();
    
    // Add regular fields
    properties.insert("name".to_string(), FieldMapping {
        field_type: "text".to_string(),
        analyzer: Some("standard".to_string()),
        store: Some(true),
        index: Some(true),
    });
    
    properties.insert("category".to_string(), FieldMapping {
        field_type: "keyword".to_string(),
        analyzer: None,
        store: Some(true),
        index: Some(true),
    });
    
    // Add geo_point field for location
    properties.insert("location".to_string(), FieldMapping {
        field_type: "geo_point".to_string(),
        analyzer: None,
        store: Some(true),
        index: Some(true),
    });
    
    let mapping = IndexMapping { properties };
    
    // Create the index
    api.create_index_with_mapping("places".to_string(), mapping).await?;
    println!("Created index 'places' with geo_point mapping");
    
    // Index some sample documents
    let places = vec![
        json!({
            "name": "Statue of Liberty",
            "category": "monument",
            "location": {"lat": 40.6892, "lon": -74.0445}
        }),
        json!({
            "name": "Empire State Building",
            "category": "building",
            "location": {"lat": 40.7484, "lon": -73.9857}
        }),
        json!({
            "name": "Central Park",
            "category": "park",
            "location": {"lat": 40.7829, "lon": -73.9654}
        }),
        json!({
            "name": "Times Square",
            "category": "landmark",
            "location": {"lat": 40.7580, "lon": -73.9855}
        }),
        json!({
            "name": "Brooklyn Bridge",
            "category": "bridge",
            "location": {"lat": 40.7061, "lon": -73.9969}
        }),
    ];
    
    // Index the documents
    for (i, place) in places.iter().enumerate() {
        let url = format!("http://localhost:9200/places/_doc/{}", i + 1);
        println!("Indexing: {}", place["name"]);
        // In a real example, you would use the HTTP API to index documents
    }
    
    // Example queries
    println!("\n=== Example Geo Queries ===\n");
    
    // 1. Geo Distance Query - Find places within 5km of Times Square
    let distance_query = json!({
        "query": {
            "geo_distance": {
                "field": "location",
                "distance": "5km",
                "lat": 40.7580,
                "lon": -73.9855
            }
        }
    });
    println!("1. Distance Query (5km from Times Square):");
    println!("{}", serde_json::to_string_pretty(&distance_query)?);
    
    // 2. Geo Bounding Box Query - Find places in lower Manhattan
    let bbox_query = json!({
        "query": {
            "geo_bounding_box": {
                "field": "location",
                "top_left": {"lat": 40.7500, "lon": -74.0200},
                "bottom_right": {"lat": 40.7000, "lon": -73.9700}
            }
        }
    });
    println!("\n2. Bounding Box Query (Lower Manhattan):");
    println!("{}", serde_json::to_string_pretty(&bbox_query)?);
    
    // 3. Geo Polygon Query - Find places within a triangular area
    let polygon_query = json!({
        "query": {
            "geo_polygon": {
                "field": "location",
                "points": [
                    {"lat": 40.7500, "lon": -74.0000},
                    {"lat": 40.7000, "lon": -74.0000},
                    {"lat": 40.7250, "lon": -73.9500}
                ]
            }
        }
    });
    println!("\n3. Polygon Query (Triangular area):");
    println!("{}", serde_json::to_string_pretty(&polygon_query)?);
    
    // 4. Combined query - Geo distance with category filter
    let combined_query = json!({
        "query": {
            "bool": {
                "must": [
                    {
                        "geo_distance": {
                            "field": "location",
                            "distance": "3km",
                            "lat": 40.7580,
                            "lon": -73.9855
                        }
                    }
                ],
                "filter": [
                    {
                        "term": {
                            "category": "building"
                        }
                    }
                ]
            }
        }
    });
    println!("\n4. Combined Query (Buildings within 3km):");
    println!("{}", serde_json::to_string_pretty(&combined_query)?);
    
    Ok(())
}