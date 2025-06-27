//! Search functionality tests

use super::common::*;
use chronik_common::Result;
use rdkafka::{
    ClientConfig,
    admin::{AdminClient, AdminOptions, NewTopic, TopicReplication},
    producer::{FutureProducer, FutureRecord},
};
use serde_json::json;
use std::time::Duration;
use tokio::time::sleep;

#[tokio::test]
async fn test_search_query_types() -> Result<()> {
    super::test_setup::init();
    
    let cluster = TestCluster::start(TestClusterConfig::default()).await?;
    let bootstrap_servers = cluster.bootstrap_servers();
    let search_endpoint = cluster.search_endpoint();
    
    // Setup test data
    setup_search_test_data(&cluster, "test-queries").await?;
    
    let client = reqwest::Client::new();
    
    // Test 1: Match query
    let response = client
        .post(&format!("{}/test-queries/_search", search_endpoint))
        .json(&json!({
            "query": {
                "match": {
                    "description": "distributed systems"
                }
            }
        }))
        .send()
        .await?;
    
    let result: serde_json::Value = response.json().await?;
    assert!(result["hits"]["total"]["value"].as_u64().unwrap() > 0);
    
    // Test 2: Term query
    let response = client
        .post(&format!("{}/test-queries/_search", search_endpoint))
        .json(&json!({
            "query": {
                "term": {
                    "category": "database"
                }
            }
        }))
        .send()
        .await?;
    
    let result: serde_json::Value = response.json().await?;
    assert!(result["hits"]["total"]["value"].as_u64().unwrap() >= 2);
    
    // Test 3: Range query
    let response = client
        .post(&format!("{}/test-queries/_search", search_endpoint))
        .json(&json!({
            "query": {
                "range": {
                    "price": {
                        "gte": 50,
                        "lte": 150
                    }
                }
            }
        }))
        .send()
        .await?;
    
    let result: serde_json::Value = response.json().await?;
    let hits = result["hits"]["hits"].as_array().unwrap();
    for hit in hits {
        let price = hit["_source"]["price"].as_f64().unwrap();
        assert!(price >= 50.0 && price <= 150.0);
    }
    
    // Test 4: Bool query with must, should, must_not
    let response = client
        .post(&format!("{}/test-queries/_search", search_endpoint))
        .json(&json!({
            "query": {
                "bool": {
                    "must": [
                        { "term": { "category": "database" } }
                    ],
                    "should": [
                        { "match": { "name": "chronik" } },
                        { "match": { "description": "real-time" } }
                    ],
                    "must_not": [
                        { "term": { "deprecated": true } }
                    ],
                    "minimum_should_match": 1
                }
            }
        }))
        .send()
        .await?;
    
    let result: serde_json::Value = response.json().await?;
    assert!(result["hits"]["total"]["value"].as_u64().unwrap() > 0);
    
    // Test 5: Wildcard query
    let response = client
        .post(&format!("{}/test-queries/_search", search_endpoint))
        .json(&json!({
            "query": {
                "wildcard": {
                    "name": "chron*"
                }
            }
        }))
        .send()
        .await?;
    
    let result: serde_json::Value = response.json().await?;
    let hits = result["hits"]["hits"].as_array().unwrap();
    for hit in hits {
        let name = hit["_source"]["name"].as_str().unwrap();
        assert!(name.starts_with("chron"));
    }
    
    // Test 6: Prefix query
    let response = client
        .post(&format!("{}/test-queries/_search", search_endpoint))
        .json(&json!({
            "query": {
                "prefix": {
                    "tags": "search"
                }
            }
        }))
        .send()
        .await?;
    
    let result: serde_json::Value = response.json().await?;
    assert!(result["hits"]["total"]["value"].as_u64().unwrap() > 0);
    
    Ok(())
}

#[tokio::test]
async fn test_search_aggregations() -> Result<()> {
    super::test_setup::init();
    
    let cluster = TestCluster::start(TestClusterConfig::default()).await?;
    let search_endpoint = cluster.search_endpoint();
    
    // Setup test data
    setup_search_test_data(&cluster, "test-aggs").await?;
    
    let client = reqwest::Client::new();
    
    // Test 1: Terms aggregation
    let response = client
        .post(&format!("{}/test-aggs/_search", search_endpoint))
        .json(&json!({
            "size": 0,
            "aggs": {
                "categories": {
                    "terms": {
                        "field": "category",
                        "size": 10
                    }
                }
            }
        }))
        .send()
        .await?;
    
    let result: serde_json::Value = response.json().await?;
    let buckets = result["aggregations"]["categories"]["buckets"].as_array().unwrap();
    assert!(!buckets.is_empty());
    
    // Verify bucket structure
    for bucket in buckets {
        assert!(bucket["key"].as_str().is_some());
        assert!(bucket["doc_count"].as_u64().unwrap() > 0);
    }
    
    // Test 2: Stats aggregation
    let response = client
        .post(&format!("{}/test-aggs/_search", search_endpoint))
        .json(&json!({
            "size": 0,
            "aggs": {
                "price_stats": {
                    "stats": {
                        "field": "price"
                    }
                }
            }
        }))
        .send()
        .await?;
    
    let result: serde_json::Value = response.json().await?;
    let stats = &result["aggregations"]["price_stats"];
    assert!(stats["count"].as_u64().unwrap() > 0);
    assert!(stats["min"].as_f64().is_some());
    assert!(stats["max"].as_f64().is_some());
    assert!(stats["avg"].as_f64().is_some());
    assert!(stats["sum"].as_f64().is_some());
    
    // Test 3: Date histogram aggregation
    let response = client
        .post(&format!("{}/test-aggs/_search", search_endpoint))
        .json(&json!({
            "size": 0,
            "aggs": {
                "daily_products": {
                    "date_histogram": {
                        "field": "created_at",
                        "interval": "day"
                    }
                }
            }
        }))
        .send()
        .await?;
    
    let result: serde_json::Value = response.json().await?;
    let buckets = result["aggregations"]["daily_products"]["buckets"].as_array().unwrap();
    assert!(!buckets.is_empty());
    
    // Test 4: Nested aggregations
    let response = client
        .post(&format!("{}/test-aggs/_search", search_endpoint))
        .json(&json!({
            "size": 0,
            "aggs": {
                "categories": {
                    "terms": {
                        "field": "category"
                    },
                    "aggs": {
                        "avg_price": {
                            "avg": {
                                "field": "price"
                            }
                        }
                    }
                }
            }
        }))
        .send()
        .await?;
    
    let result: serde_json::Value = response.json().await?;
    let buckets = result["aggregations"]["categories"]["buckets"].as_array().unwrap();
    
    for bucket in buckets {
        assert!(bucket["avg_price"]["value"].as_f64().is_some());
    }
    
    // Test 5: Range aggregation
    let response = client
        .post(&format!("{}/test-aggs/_search", search_endpoint))
        .json(&json!({
            "size": 0,
            "aggs": {
                "price_ranges": {
                    "range": {
                        "field": "price",
                        "ranges": [
                            { "to": 50 },
                            { "from": 50, "to": 100 },
                            { "from": 100, "to": 200 },
                            { "from": 200 }
                        ]
                    }
                }
            }
        }))
        .send()
        .await?;
    
    let result: serde_json::Value = response.json().await?;
    let buckets = result["aggregations"]["price_ranges"]["buckets"].as_array().unwrap();
    assert_eq!(buckets.len(), 4);
    
    Ok(())
}

#[tokio::test]
async fn test_search_pagination_sorting() -> Result<()> {
    super::test_setup::init();
    
    let cluster = TestCluster::start(TestClusterConfig::default()).await?;
    let search_endpoint = cluster.search_endpoint();
    
    // Setup test data with many documents
    setup_pagination_test_data(&cluster, "test-pagination").await?;
    
    let client = reqwest::Client::new();
    
    // Test 1: Basic pagination with from/size
    let mut all_ids = Vec::new();
    let page_size = 10;
    
    for page in 0..5 {
        let response = client
            .post(&format!("{}/test-pagination/_search", search_endpoint))
            .json(&json!({
                "from": page * page_size,
                "size": page_size,
                "sort": [{ "id": { "order": "asc" } }]
            }))
            .send()
            .await?;
        
        let result: serde_json::Value = response.json().await?;
        let hits = result["hits"]["hits"].as_array().unwrap();
        
        assert_eq!(hits.len(), page_size);
        
        for hit in hits {
            all_ids.push(hit["_source"]["id"].as_str().unwrap().to_string());
        }
    }
    
    // Verify no duplicates
    let unique_ids: std::collections::HashSet<_> = all_ids.iter().collect();
    assert_eq!(unique_ids.len(), all_ids.len());
    
    // Test 2: Sorting by multiple fields
    let response = client
        .post(&format!("{}/test-pagination/_search", search_endpoint))
        .json(&json!({
            "size": 20,
            "sort": [
                { "category": { "order": "asc" } },
                { "price": { "order": "desc" } }
            ]
        }))
        .send()
        .await?;
    
    let result: serde_json::Value = response.json().await?;
    let hits = result["hits"]["hits"].as_array().unwrap();
    
    // Verify sorting
    let mut prev_category = "";
    let mut prev_price = f64::MAX;
    
    for hit in hits {
        let category = hit["_source"]["category"].as_str().unwrap();
        let price = hit["_source"]["price"].as_f64().unwrap();
        
        if category == prev_category {
            assert!(price <= prev_price);
        }
        
        prev_category = category;
        prev_price = price;
    }
    
    // Test 3: Search after for deep pagination
    let response = client
        .post(&format!("{}/test-pagination/_search", search_endpoint))
        .json(&json!({
            "size": 10,
            "sort": [{ "_id": { "order": "asc" } }]
        }))
        .send()
        .await?;
    
    let result: serde_json::Value = response.json().await?;
    let last_hit = result["hits"]["hits"].as_array().unwrap().last().unwrap();
    let search_after = &last_hit["sort"];
    
    // Get next page using search_after
    let response = client
        .post(&format!("{}/test-pagination/_search", search_endpoint))
        .json(&json!({
            "size": 10,
            "sort": [{ "_id": { "order": "asc" } }],
            "search_after": search_after
        }))
        .send()
        .await?;
    
    let result: serde_json::Value = response.json().await?;
    let hits = result["hits"]["hits"].as_array().unwrap();
    assert_eq!(hits.len(), 10);
    
    Ok(())
}

#[tokio::test]
async fn test_search_highlighting() -> Result<()> {
    super::test_setup::init();
    
    let cluster = TestCluster::start(TestClusterConfig::default()).await?;
    let search_endpoint = cluster.search_endpoint();
    
    // Setup test data
    setup_search_test_data(&cluster, "test-highlight").await?;
    
    let client = reqwest::Client::new();
    
    // Test highlighting in search results
    let response = client
        .post(&format!("{}/test-highlight/_search", search_endpoint))
        .json(&json!({
            "query": {
                "match": {
                    "description": "distributed search"
                }
            },
            "highlight": {
                "fields": {
                    "description": {
                        "pre_tags": ["<em>"],
                        "post_tags": ["</em>"],
                        "fragment_size": 150,
                        "number_of_fragments": 3
                    }
                }
            }
        }))
        .send()
        .await?;
    
    let result: serde_json::Value = response.json().await?;
    let hits = result["hits"]["hits"].as_array().unwrap();
    
    // Verify highlighting is present
    for hit in hits {
        if let Some(highlight) = hit.get("highlight") {
            if let Some(desc_highlights) = highlight["description"].as_array() {
                for fragment in desc_highlights {
                    let text = fragment.as_str().unwrap();
                    assert!(text.contains("<em>distributed</em>") || text.contains("<em>search</em>"));
                }
            }
        }
    }
    
    Ok(())
}

// Helper functions

async fn setup_search_test_data(cluster: &TestCluster, topic: &str) -> Result<()> {
    let bootstrap_servers = cluster.bootstrap_servers();
    
    // Create topic
    let admin: AdminClient<_> = ClientConfig::new()
        .set("bootstrap.servers", &bootstrap_servers)
        .create()
        .expect("Failed to create admin client");
    
    let new_topic = NewTopic::new(topic, 2, TopicReplication::Fixed(1));
    admin
        .create_topics(&[new_topic], &AdminOptions::new())
        .await
        .expect("Failed to create topics")[0]
        .expect("Failed to create topic");
    
    // Produce test documents
    let producer: FutureProducer = ClientConfig::new()
        .set("bootstrap.servers", &bootstrap_servers)
        .create()
        .expect("Failed to create producer");
    
    let products = vec![
        json!({
            "id": "prod1",
            "name": "chronik-stream",
            "category": "database",
            "description": "A distributed log storage system with real-time search capabilities",
            "price": 99.99,
            "tags": ["search", "distributed", "kafka"],
            "created_at": "2024-01-01T00:00:00Z",
            "in_stock": true
        }),
        json!({
            "id": "prod2",
            "name": "elasticsearch",
            "category": "search",
            "description": "A distributed search and analytics engine",
            "price": 149.99,
            "tags": ["search", "analytics", "distributed"],
            "created_at": "2024-01-02T00:00:00Z",
            "in_stock": true
        }),
        json!({
            "id": "prod3",
            "name": "kafka",
            "category": "messaging",
            "description": "A distributed streaming platform for building real-time data pipelines",
            "price": 79.99,
            "tags": ["streaming", "distributed", "messaging"],
            "created_at": "2024-01-03T00:00:00Z",
            "in_stock": true
        }),
        json!({
            "id": "prod4",
            "name": "postgres",
            "category": "database",
            "description": "Advanced open source relational database",
            "price": 59.99,
            "tags": ["sql", "relational", "acid"],
            "created_at": "2024-01-04T00:00:00Z",
            "in_stock": false,
            "deprecated": false
        }),
        json!({
            "id": "prod5",
            "name": "redis",
            "category": "cache",
            "description": "In-memory data structure store used as cache and message broker",
            "price": 39.99,
            "tags": ["cache", "in-memory", "nosql"],
            "created_at": "2024-01-05T00:00:00Z",
            "in_stock": true
        }),
    ];
    
    for product in products {
        producer
            .send(
                FutureRecord::to(topic)
                    .key(product["id"].as_str().unwrap())
                    .payload(&serde_json::to_string(&product).unwrap()),
                Duration::from_secs(5),
            )
            .await
            .expect("Failed to produce");
    }
    
    // Wait for indexing
    sleep(Duration::from_secs(3)).await;
    
    Ok(())
}

async fn setup_pagination_test_data(cluster: &TestCluster, topic: &str) -> Result<()> {
    let bootstrap_servers = cluster.bootstrap_servers();
    
    // Create topic
    let admin: AdminClient<_> = ClientConfig::new()
        .set("bootstrap.servers", &bootstrap_servers)
        .create()
        .expect("Failed to create admin client");
    
    let new_topic = NewTopic::new(topic, 1, TopicReplication::Fixed(1));
    admin
        .create_topics(&[new_topic], &AdminOptions::new())
        .await
        .expect("Failed to create topics")[0]
        .expect("Failed to create topic");
    
    let producer: FutureProducer = ClientConfig::new()
        .set("bootstrap.servers", &bootstrap_servers)
        .create()
        .expect("Failed to create producer");
    
    // Generate 50 test documents
    let categories = vec!["electronics", "books", "clothing", "food", "toys"];
    
    for i in 0..50 {
        let doc = json!({
            "id": format!("item-{:03}", i),
            "name": format!("Product {}", i),
            "category": categories[i % categories.len()],
            "price": 10.0 + (i as f64 * 5.0),
            "created_at": format!("2024-01-{:02}T00:00:00Z", (i % 31) + 1)
        });
        
        producer
            .send(
                FutureRecord::to(topic)
                    .key(&format!("item-{:03}", i))
                    .payload(&serde_json::to_string(&doc).unwrap()),
                Duration::from_secs(5),
            )
            .await
            .expect("Failed to produce");
    }
    
    // Wait for indexing
    sleep(Duration::from_secs(3)).await;
    
    Ok(())
}