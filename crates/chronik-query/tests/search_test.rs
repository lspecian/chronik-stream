//! Search engine tests.

use chronik_query::search::{SearchEngine, SearchQuery};
use tempfile::TempDir;

#[tokio::test]
async fn test_search_engine() {
    let temp_dir = TempDir::new().unwrap();
    let search_engine = SearchEngine::new(temp_dir.path()).unwrap();
    
    // Index some test messages
    search_engine.index_message(
        "test-topic",
        0,
        1,
        1234567890,
        Some(b"key1"),
        b"Hello world",
    ).await.unwrap();
    
    search_engine.index_message(
        "test-topic",
        0,
        2,
        1234567891,
        Some(b"key2"),
        b"Hello search engine",
    ).await.unwrap();
    
    search_engine.index_message(
        "another-topic",
        1,
        1,
        1234567892,
        None,
        b"Different topic message",
    ).await.unwrap();
    
    // Commit changes
    search_engine.commit().await.unwrap();
    
    // Search for "hello"
    let query = SearchQuery {
        query: "hello".to_string(),
        topic: None,
        partition: None,
        start_time: None,
        end_time: None,
        limit: 10,
        offset: 0,
    };
    
    let results = search_engine.search(query).await.unwrap();
    assert_eq!(results.total_hits, 2);
    assert_eq!(results.hits.len(), 2);
    
    // Search with topic filter
    let query = SearchQuery {
        query: "".to_string(),
        topic: Some("test-topic".to_string()),
        partition: None,
        start_time: None,
        end_time: None,
        limit: 10,
        offset: 0,
    };
    
    let results = search_engine.search(query).await.unwrap();
    assert_eq!(results.total_hits, 2);
    assert_eq!(results.hits[0].topic, "test-topic");
    
    // Search with partition filter
    let query = SearchQuery {
        query: "".to_string(),
        topic: None,
        partition: Some(1),
        start_time: None,
        end_time: None,
        limit: 10,
        offset: 0,
    };
    
    let results = search_engine.search(query).await.unwrap();
    assert_eq!(results.total_hits, 1);
    assert_eq!(results.hits[0].partition, 1);
}

#[tokio::test]
async fn test_pagination() {
    let temp_dir = TempDir::new().unwrap();
    let search_engine = SearchEngine::new(temp_dir.path()).unwrap();
    
    // Index 20 messages
    for i in 0..20 {
        search_engine.index_message(
            "test-topic",
            0,
            i,
            1234567890 + i,
            None,
            format!("Message number {}", i).as_bytes(),
        ).await.unwrap();
    }
    
    search_engine.commit().await.unwrap();
    
    // First page
    let query = SearchQuery {
        query: "message".to_string(),
        topic: None,
        partition: None,
        start_time: None,
        end_time: None,
        limit: 10,
        offset: 0,
    };
    
    let results = search_engine.search(query).await.unwrap();
    assert_eq!(results.total_hits, 20);
    assert_eq!(results.hits.len(), 10);
    
    // Second page
    let query = SearchQuery {
        query: "message".to_string(),
        topic: None,
        partition: None,
        start_time: None,
        end_time: None,
        limit: 10,
        offset: 10,
    };
    
    let results = search_engine.search(query).await.unwrap();
    assert_eq!(results.total_hits, 20);
    assert_eq!(results.hits.len(), 10);
}