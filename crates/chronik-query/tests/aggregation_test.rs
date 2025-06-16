//! Aggregation engine tests.

use chronik_query::aggregation::{
    AggregationEngine, AggregationQuery, AggregationFunction, WindowType
};
use std::sync::Arc;
use std::time::Duration;

#[tokio::test]
async fn test_count_aggregation() {
    let engine = Arc::new(AggregationEngine::new());
    engine.clone().start().await.unwrap();
    
    // Register a count aggregation
    let query = AggregationQuery {
        topic: "test-topic".to_string(),
        partition: None,
        group_by: vec![],
        window: WindowType::Fixed(Duration::from_secs(10)),
        function: AggregationFunction::Count,
        field: None,
        filter: None,
    };
    
    engine.register_aggregation("count-test".to_string(), query).await.unwrap();
    
    // Process some messages
    let base_time = 1234567890000i64; // Fixed timestamp for deterministic testing
    
    for i in 0..5 {
        engine.process_message(
            "test-topic",
            0,
            base_time + i * 1000,
            None,
            format!("message {}", i).as_bytes(),
        ).await.unwrap();
    }
    
    // Get results
    let results = engine.get_results("count-test").await.unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].value, 5.0);
    assert_eq!(results[0].count, 5);
}

#[tokio::test]
async fn test_sum_aggregation() {
    let engine = Arc::new(AggregationEngine::new());
    engine.clone().start().await.unwrap();
    
    // Register a sum aggregation
    let query = AggregationQuery {
        topic: "metrics".to_string(),
        partition: Some(0),
        group_by: vec![],
        window: WindowType::Fixed(Duration::from_secs(60)),
        function: AggregationFunction::Sum,
        field: Some("value".to_string()),
        filter: None,
    };
    
    engine.register_aggregation("sum-test".to_string(), query).await.unwrap();
    
    // Process numeric messages
    let base_time = 1234567890000i64;
    
    for i in 1..=10 {
        let json = format!(r#"{{"value": {}, "type": "metric"}}"#, i);
        engine.process_message(
            "metrics",
            0,
            base_time + i * 1000,
            None,
            json.as_bytes(),
        ).await.unwrap();
    }
    
    // Get results
    let results = engine.get_results("sum-test").await.unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].value, 55.0); // 1+2+3+...+10 = 55
}

#[tokio::test]
async fn test_grouped_aggregation() {
    let engine = Arc::new(AggregationEngine::new());
    engine.clone().start().await.unwrap();
    
    // Register a grouped count aggregation
    let query = AggregationQuery {
        topic: "events".to_string(),
        partition: None,
        group_by: vec!["type".to_string()],
        window: WindowType::Fixed(Duration::from_secs(30)),
        function: AggregationFunction::Count,
        field: None,
        filter: None,
    };
    
    engine.register_aggregation("grouped-test".to_string(), query).await.unwrap();
    
    // Process messages with different types
    let base_time = 1234567890000i64;
    
    for i in 0..6 {
        let event_type = if i % 2 == 0 { "click" } else { "view" };
        let json = format!(r#"{{"type": "{}", "user": "user{}"}}"#, event_type, i);
        engine.process_message(
            "events",
            0,
            base_time + i * 1000,
            None,
            json.as_bytes(),
        ).await.unwrap();
    }
    
    // Get results
    let results = engine.get_results("grouped-test").await.unwrap();
    assert_eq!(results.len(), 2);
    
    // Check grouped counts
    for result in results {
        if result.group_key == Some("click".to_string()) {
            assert_eq!(result.value, 3.0); // 0, 2, 4
        } else if result.group_key == Some("view".to_string()) {
            assert_eq!(result.value, 3.0); // 1, 3, 5
        }
    }
}

#[tokio::test]
async fn test_average_aggregation() {
    let engine = Arc::new(AggregationEngine::new());
    engine.clone().start().await.unwrap();
    
    // Register an average aggregation
    let query = AggregationQuery {
        topic: "temperatures".to_string(),
        partition: None,
        group_by: vec![],
        window: WindowType::Fixed(Duration::from_secs(60)),
        function: AggregationFunction::Average,
        field: Some("temp".to_string()),
        filter: None,
    };
    
    engine.register_aggregation("avg-test".to_string(), query).await.unwrap();
    
    // Process temperature readings
    let base_time = 1234567890000i64;
    let temps = [20.0, 22.0, 21.0, 23.0, 21.0];
    
    for (i, temp) in temps.iter().enumerate() {
        let json = format!(r#"{{"temp": {}, "sensor": "s1"}}"#, temp);
        engine.process_message(
            "temperatures",
            0,
            base_time + i as i64 * 1000,
            None,
            json.as_bytes(),
        ).await.unwrap();
    }
    
    // Get results
    let results = engine.get_results("avg-test").await.unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].value, 21.4); // (20+22+21+23+21)/5 = 21.4
}