//! Example client for the Chronik Stream Search API.
//!
//! This example demonstrates how to interact with the Elasticsearch-compatible
//! search API provided by Chronik Stream.

use reqwest::Client;
use serde_json::json;
use std::error::Error;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let base_url = "http://localhost:9200";
    let client = Client::new();

    println!("Chronik Stream Search API Example\n");

    // 1. Check health
    println!("1. Checking server health...");
    let resp = client.get(format!("{}/health", base_url))
        .send()
        .await?
        .json::<serde_json::Value>()
        .await?;
    println!("Health: {}\n", serde_json::to_string_pretty(&resp)?);

    // 2. Create an index with mapping
    println!("2. Creating index 'test-messages' with mapping...");
    let mapping = json!({
        "mappings": {
            "properties": {
                "_id": {
                    "type": "keyword",
                    "store": true
                },
                "topic": {
                    "type": "keyword", 
                    "store": true
                },
                "partition": {
                    "type": "long",
                    "store": true
                },
                "offset": {
                    "type": "long",
                    "store": true
                },
                "timestamp": {
                    "type": "long",
                    "store": true
                },
                "key": {
                    "type": "text",
                    "analyzer": "standard",
                    "store": true
                },
                "value": {
                    "type": "text",
                    "analyzer": "standard",
                    "store": true
                },
                "headers": {
                    "type": "text",
                    "store": true
                }
            }
        }
    });

    let resp = client.put(format!("{}/test-messages", base_url))
        .json(&mapping)
        .send()
        .await?;
    
    if resp.status().is_success() {
        println!("Index created successfully\n");
    } else {
        println!("Failed to create index: {}\n", resp.text().await?);
    }

    // 3. Index some documents
    println!("3. Indexing sample documents...");
    let documents = vec![
        (
            "msg1",
            json!({
                "topic": "orders",
                "partition": 0,
                "offset": 100,
                "timestamp": 1640000000000i64,
                "key": "order-123",
                "value": "Order created for customer John Doe",
                "headers": "{\"type\":\"order_created\"}"
            })
        ),
        (
            "msg2",
            json!({
                "topic": "orders",
                "partition": 0,
                "offset": 101,
                "timestamp": 1640000001000i64,
                "key": "order-124",
                "value": "Order shipped to customer Jane Smith",
                "headers": "{\"type\":\"order_shipped\"}"
            })
        ),
        (
            "msg3",
            json!({
                "topic": "payments",
                "partition": 1,
                "offset": 50,
                "timestamp": 1640000002000i64,
                "key": "payment-456",
                "value": "Payment received for order-123",
                "headers": "{\"type\":\"payment_received\"}"
            })
        ),
    ];

    for (id, doc) in documents {
        let resp = client.post(format!("{}/test-messages/_doc/{}", base_url, id))
            .json(&doc)
            .send()
            .await?;
        
        if resp.status().is_success() {
            println!("Indexed document: {}", id);
        } else {
            println!("Failed to index {}: {}", id, resp.text().await?);
        }
    }
    println!();

    // 4. Search examples
    println!("4. Performing searches...\n");

    // Match all query
    println!("a) Match all documents:");
    let query = json!({
        "query": {
            "match_all": {}
        },
        "size": 10
    });
    
    let resp = client.post(format!("{}/test-messages/_search", base_url))
        .json(&query)
        .send()
        .await?
        .json::<serde_json::Value>()
        .await?;
    
    print_search_results(&resp);

    // Match query
    println!("\nb) Search for 'customer' in value field:");
    let query = json!({
        "query": {
            "match": {
                "value": "customer"
            }
        }
    });
    
    let resp = client.post(format!("{}/test-messages/_search", base_url))
        .json(&query)
        .send()
        .await?
        .json::<serde_json::Value>()
        .await?;
    
    print_search_results(&resp);

    // Term query
    println!("\nc) Find documents from 'orders' topic:");
    let query = json!({
        "query": {
            "term": {
                "topic": "orders"
            }
        }
    });
    
    let resp = client.post(format!("{}/test-messages/_search", base_url))
        .json(&query)
        .send()
        .await?
        .json::<serde_json::Value>()
        .await?;
    
    print_search_results(&resp);

    // Bool query
    println!("\nd) Complex query - orders topic with 'shipped' in value:");
    let query = json!({
        "query": {
            "bool": {
                "must": [
                    {
                        "term": {
                            "topic": "orders"
                        }
                    },
                    {
                        "match": {
                            "value": "shipped"
                        }
                    }
                ]
            }
        }
    });
    
    let resp = client.post(format!("{}/test-messages/_search", base_url))
        .json(&query)
        .send()
        .await?
        .json::<serde_json::Value>()
        .await?;
    
    print_search_results(&resp);

    // 5. Get specific document
    println!("\n5. Get specific document by ID:");
    let resp = client.get(format!("{}/test-messages/_doc/msg1", base_url))
        .send()
        .await?
        .json::<serde_json::Value>()
        .await?;
    
    println!("{}\n", serde_json::to_string_pretty(&resp)?);

    // 6. List indices
    println!("6. List all indices:");
    let resp = client.get(format!("{}/_cat/indices", base_url))
        .send()
        .await?
        .json::<serde_json::Value>()
        .await?;
    
    println!("{}\n", serde_json::to_string_pretty(&resp)?);

    // 7. Get mapping
    println!("7. Get index mapping:");
    let resp = client.get(format!("{}/test-messages/_mapping", base_url))
        .send()
        .await?
        .json::<serde_json::Value>()
        .await?;
    
    println!("{}\n", serde_json::to_string_pretty(&resp)?);

    // 8. Delete a document
    println!("8. Delete document msg3:");
    let resp = client.delete(format!("{}/test-messages/_doc/msg3", base_url))
        .send()
        .await?;
    
    if resp.status().is_success() {
        println!("Document deleted successfully\n");
    } else {
        println!("Failed to delete document: {}\n", resp.text().await?);
    }

    // 9. Search across all indices
    println!("9. Search across all indices:");
    let query = json!({
        "query": {
            "match": {
                "value": "order"
            }
        }
    });
    
    let resp = client.post(format!("{}/_search", base_url))
        .json(&query)
        .send()
        .await?
        .json::<serde_json::Value>()
        .await?;
    
    print_search_results(&resp);

    Ok(())
}

fn print_search_results(response: &serde_json::Value) {
    if let Some(hits) = response.get("hits") {
        if let Some(total) = hits.get("total").and_then(|t| t.get("value")) {
            println!("Total hits: {}", total);
        }
        
        if let Some(hits_array) = hits.get("hits").and_then(|h| h.as_array()) {
            for hit in hits_array {
                if let Some(source) = hit.get("_source") {
                    println!("  - {} (score: {})",
                        source.get("value").and_then(|v| v.as_str()).unwrap_or("N/A"),
                        hit.get("_score").and_then(|s| s.as_f64()).unwrap_or(0.0)
                    );
                }
            }
        }
    }
}