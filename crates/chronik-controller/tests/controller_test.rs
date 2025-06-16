use chronik_controller::{ControllerNode, ControllerConfig, Proposal, TopicConfig};
use std::collections::HashMap;
use tokio::time::{sleep, Duration};

#[tokio::test]
async fn test_single_node_controller() {
    let config = ControllerConfig {
        node_id: 1,
        peers: vec![],
        tick_interval: Duration::from_millis(50),
        election_timeout: (150, 300),
    };
    
    let controller = ControllerNode::new(config).unwrap();
    controller.start().await.unwrap();
    
    // Wait for election
    sleep(Duration::from_millis(500)).await;
    
    // Should become leader since it's the only node
    controller.campaign().unwrap();
    sleep(Duration::from_millis(200)).await;
    
    assert!(controller.is_leader());
    
    // Test creating a topic
    let topic_config = TopicConfig {
        name: "test-topic".to_string(),
        partition_count: 3,
        replication_factor: 1,
        configs: HashMap::new(),
    };
    
    controller.propose(Proposal::CreateTopic(topic_config.clone())).unwrap();
    
    // Wait for proposal to be committed
    sleep(Duration::from_millis(200)).await;
    
    // Check state
    let state = controller.get_state();
    assert!(state.topics.contains_key("test-topic"));
    assert_eq!(state.topics["test-topic"].partition_count, 3);
    
    controller.stop().await.unwrap();
}

#[tokio::test]
async fn test_state_persistence() {
    let config = ControllerConfig::default();
    let controller = ControllerNode::new(config).unwrap();
    controller.start().await.unwrap();
    
    // Campaign to become leader
    controller.campaign().unwrap();
    sleep(Duration::from_millis(300)).await;
    
    // Create multiple topics
    for i in 0..3 {
        let topic_config = TopicConfig {
            name: format!("topic-{}", i),
            partition_count: i + 1,
            replication_factor: 1,
            configs: HashMap::new(),
        };
        controller.propose(Proposal::CreateTopic(topic_config)).unwrap();
    }
    
    // Wait for proposals
    sleep(Duration::from_millis(300)).await;
    
    // Verify state
    let state = controller.get_state();
    assert_eq!(state.topics.len(), 3);
    assert_eq!(state.topics["topic-0"].partition_count, 1);
    assert_eq!(state.topics["topic-1"].partition_count, 2);
    assert_eq!(state.topics["topic-2"].partition_count, 3);
    
    controller.stop().await.unwrap();
}