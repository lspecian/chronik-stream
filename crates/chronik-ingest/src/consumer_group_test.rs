//! Unit tests for consumer group functionality

#[cfg(test)]
mod tests {
    use crate::consumer_group::{
        GroupManager, GroupState, ConsumerGroup, AssignmentStrategy,
        JoinGroupResponse, SyncGroupResponse, HeartbeatResponse,
        CommitOffsetsResponse, TopicPartitionOffset,
    };
    use chronik_common::metadata::memory::InMemoryMetadataStore;
    use std::sync::Arc;
    use std::time::Duration;
    use std::collections::HashMap;

    fn create_test_manager() -> GroupManager {
        let metadata_store = Arc::new(InMemoryMetadataStore::new());
        GroupManager::new(metadata_store)
    }

    #[tokio::test]
    async fn test_create_group() {
        let manager = create_test_manager();
        
        let group_id = manager.get_or_create_group(
            "test-group".to_string(),
            "consumer".to_string(),
        ).await.unwrap();
        
        assert_eq!(group_id, "test-group");
        
        // Should return same group ID if called again
        let group_id2 = manager.get_or_create_group(
            "test-group".to_string(),
            "consumer".to_string(),
        ).await.unwrap();
        
        assert_eq!(group_id2, "test-group");
    }

    #[tokio::test]
    async fn test_join_group_single_member() {
        let manager = create_test_manager();
        
        let response = manager.join_group(
            "test-group".to_string(),
            None,
            "client1".to_string(),
            "127.0.0.1".to_string(),
            Duration::from_secs(30),
            Duration::from_secs(60),
            "consumer".to_string(),
            vec![("range".to_string(), vec![1, 2, 3])],
            None,
        ).await.unwrap();
        
        assert_eq!(response.error_code, 0);
        assert!(!response.member_id.is_empty());
        assert_eq!(response.generation_id, 1);
        assert_eq!(response.leader_id, response.member_id);
        assert_eq!(response.protocol, "range");
        assert_eq!(response.members.len(), 1);
    }

    #[tokio::test]
    async fn test_join_group_multiple_members() {
        let manager = create_test_manager();
        
        // First member joins
        let response1 = manager.join_group(
            "multi-group".to_string(),
            None,
            "client1".to_string(),
            "127.0.0.1".to_string(),
            Duration::from_secs(30),
            Duration::from_secs(60),
            "consumer".to_string(),
            vec![("range".to_string(), vec![])],
            None,
        ).await.unwrap();
        
        let member1_id = response1.member_id.clone();
        
        // Second member joins - should trigger rebalance
        let response2 = manager.join_group(
            "multi-group".to_string(),
            None,
            "client2".to_string(),
            "127.0.0.2".to_string(),
            Duration::from_secs(30),
            Duration::from_secs(60),
            "consumer".to_string(),
            vec![("range".to_string(), vec![])],
            None,
        ).await.unwrap();
        
        // Both should have same generation (incremented due to rebalance)
        assert_eq!(response2.generation_id, response1.generation_id + 1);
        // First member remains leader
        assert_eq!(response2.leader_id, member1_id);
    }

    #[tokio::test]
    async fn test_sync_group() {
        let manager = create_test_manager();
        
        // Join first
        let join_response = manager.join_group(
            "sync-group".to_string(),
            None,
            "client1".to_string(),
            "127.0.0.1".to_string(),
            Duration::from_secs(30),
            Duration::from_secs(60),
            "consumer".to_string(),
            vec![("range".to_string(), vec![])],
            None,
        ).await.unwrap();
        
        let member_id = join_response.member_id.clone();
        
        // Sync as leader with assignments
        let mut assignments = HashMap::new();
        assignments.insert(member_id.clone(), vec![1, 2, 3, 4]);
        
        let sync_response = manager.sync_group(
            "sync-group".to_string(),
            join_response.generation_id,
            member_id.clone(),
            0,
            Some(assignments.into_iter().collect()),
        ).await.unwrap();
        
        assert_eq!(sync_response.error_code, 0);
        assert_eq!(sync_response.assignment, vec![1, 2, 3, 4]);
    }

    #[tokio::test]
    async fn test_heartbeat() {
        let manager = create_test_manager();
        
        // Join first
        let join_response = manager.join_group(
            "heartbeat-group".to_string(),
            None,
            "client1".to_string(),
            "127.0.0.1".to_string(),
            Duration::from_secs(30),
            Duration::from_secs(60),
            "consumer".to_string(),
            vec![("range".to_string(), vec![])],
            None,
        ).await.unwrap();
        
        // Send heartbeat
        let heartbeat_response = manager.heartbeat(
            "heartbeat-group".to_string(),
            join_response.member_id.clone(),
            join_response.generation_id,
            None,
        ).await.unwrap();
        
        assert_eq!(heartbeat_response.error_code, 0);
        
        // Heartbeat with wrong generation should fail
        let bad_heartbeat = manager.heartbeat(
            "heartbeat-group".to_string(),
            join_response.member_id.clone(),
            join_response.generation_id + 1,
            None,
        ).await.unwrap();
        
        assert_ne!(bad_heartbeat.error_code, 0);
    }

    #[tokio::test]
    async fn test_leave_group() {
        let manager = create_test_manager();
        
        // Join first
        let join_response = manager.join_group(
            "leave-group".to_string(),
            None,
            "client1".to_string(),
            "127.0.0.1".to_string(),
            Duration::from_secs(30),
            Duration::from_secs(60),
            "consumer".to_string(),
            vec![("range".to_string(), vec![])],
            None,
        ).await.unwrap();
        
        let member_id = join_response.member_id.clone();
        
        // Leave group
        manager.leave_group(
            "leave-group".to_string(),
            member_id.clone(),
        ).await.unwrap();
        
        // Heartbeat should fail after leaving
        let heartbeat_response = manager.heartbeat(
            "leave-group".to_string(),
            member_id,
            join_response.generation_id,
            None,
        ).await.unwrap();
        
        assert_ne!(heartbeat_response.error_code, 0);
    }

    #[tokio::test]
    async fn test_offset_management() {
        let manager = create_test_manager();
        
        // Join and sync first
        let join_response = manager.join_group(
            "offset-group".to_string(),
            None,
            "client1".to_string(),
            "127.0.0.1".to_string(),
            Duration::from_secs(30),
            Duration::from_secs(60),
            "consumer".to_string(),
            vec![("range".to_string(), vec![])],
            None,
        ).await.unwrap();
        
        let member_id = join_response.member_id.clone();
        
        // Sync group
        manager.sync_group(
            "offset-group".to_string(),
            join_response.generation_id,
            member_id.clone(),
            0,
            None,
        ).await.unwrap();
        
        // Commit offsets
        let offsets = vec![
            TopicPartitionOffset {
                topic: "test-topic".to_string(),
                partition: 0,
                offset: 100,
                metadata: "checkpoint".to_string(),
            },
            TopicPartitionOffset {
                topic: "test-topic".to_string(),
                partition: 1,
                offset: 200,
                metadata: String::new(),
            },
        ];
        
        let commit_response = manager.commit_offsets(
            "offset-group".to_string(),
            join_response.generation_id,
            member_id,
            None,
            offsets,
        ).await.unwrap();
        
        assert_eq!(commit_response.error_code, 0);
        assert_eq!(commit_response.partition_errors.len(), 2);
        
        // Fetch offsets
        let fetched_offsets = manager.fetch_offsets(
            "offset-group".to_string(),
            vec!["test-topic".to_string()],
        ).await.unwrap();
        
        assert_eq!(fetched_offsets.len(), 2);
        
        let offset_map: HashMap<i32, i64> = fetched_offsets.iter()
            .map(|o| (o.partition, o.offset))
            .collect();
        
        assert_eq!(offset_map.get(&0), Some(&100));
        assert_eq!(offset_map.get(&1), Some(&200));
    }

    #[tokio::test]
    async fn test_group_state_transitions() {
        let manager = create_test_manager();
        
        // Create group - should be Empty
        manager.get_or_create_group(
            "state-group".to_string(),
            "consumer".to_string(),
        ).await.unwrap();
        
        let group = manager.describe_group("state-group".to_string()).await.unwrap().unwrap();
        assert_eq!(group.state, GroupState::Empty);
        
        // Join group - should transition to PreparingRebalance
        let join_response = manager.join_group(
            "state-group".to_string(),
            None,
            "client1".to_string(),
            "127.0.0.1".to_string(),
            Duration::from_secs(30),
            Duration::from_secs(60),
            "consumer".to_string(),
            vec![("range".to_string(), vec![])],
            None,
        ).await.unwrap();
        
        // After sync - should be Stable
        manager.sync_group(
            "state-group".to_string(),
            join_response.generation_id,
            join_response.member_id.clone(),
            0,
            None,
        ).await.unwrap();
        
        let group = manager.describe_group("state-group".to_string()).await.unwrap().unwrap();
        assert_eq!(group.state, GroupState::Stable);
    }

    #[tokio::test]
    async fn test_rejoin_with_member_id() {
        let manager = create_test_manager();
        
        // Initial join
        let response1 = manager.join_group(
            "rejoin-group".to_string(),
            None,
            "client1".to_string(),
            "127.0.0.1".to_string(),
            Duration::from_secs(30),
            Duration::from_secs(60),
            "consumer".to_string(),
            vec![("range".to_string(), vec![])],
            None,
        ).await.unwrap();
        
        let member_id = response1.member_id.clone();
        
        // Rejoin with same member ID
        let response2 = manager.join_group(
            "rejoin-group".to_string(),
            Some(member_id.clone()),
            "client1".to_string(),
            "127.0.0.1".to_string(),
            Duration::from_secs(30),
            Duration::from_secs(60),
            "consumer".to_string(),
            vec![("range".to_string(), vec![])],
            None,
        ).await.unwrap();
        
        assert_eq!(response2.member_id, member_id);
        // Generation should increment due to rejoin
        assert_eq!(response2.generation_id, response1.generation_id + 1);
    }

    #[tokio::test]
    async fn test_list_groups() {
        let manager = create_test_manager();
        
        // Create multiple groups
        for i in 0..3 {
            manager.get_or_create_group(
                format!("list-group-{}", i),
                "consumer".to_string(),
            ).await.unwrap();
        }
        
        let groups = manager.list_groups().await.unwrap();
        assert!(groups.len() >= 3);
        
        for i in 0..3 {
            assert!(groups.contains(&format!("list-group-{}", i)));
        }
    }
}