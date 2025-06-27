//! Integration tests for handler with both local and controller-based group coordination

use chronik_ingest::{
    handler::{RequestHandler, HandlerConfig},
    ServerConfig, ConnectionPoolConfig,
};
use chronik_protocol::{
    Request, Response, RequestHeader, RequestBody, ResponseBody,
    JoinGroupRequest, SyncGroupRequest, HeartbeatRequest, LeaveGroupRequest,
    OffsetCommitRequest, OffsetFetchRequest, Protocol, Assignment,
    OffsetCommitRequestTopic, OffsetCommitRequestPartition,
    parser::ApiKey,
};
use chronik_storage::{SegmentReader, object_store::storage::ObjectStore};
use chronik_common::metadata::memory::InMemoryMetadataStore;
use std::sync::Arc;
use std::path::PathBuf;

/// Create a test handler with optional controller coordination
async fn create_test_handler(use_controller: bool) -> RequestHandler {
    let metadata_store = Arc::new(InMemoryMetadataStore::new());
    let storage = Arc::new(chronik_storage::object_store::LocalFileStore::new(
        PathBuf::from("/tmp/test-storage")
    ));
    let segment_reader = Arc::new(SegmentReader::new(storage.clone()));
    
    let config = HandlerConfig {
        node_id: 1,
        host: "127.0.0.1".to_string(),
        port: 9092,
        controller_addrs: if use_controller {
            vec!["127.0.0.1:9090".to_string()]
        } else {
            vec![]
        },
        use_controller_groups: use_controller,
    };
    
    RequestHandler::new(
        config,
        storage,
        metadata_store,
        segment_reader,
    ).await.unwrap()
}

/// Helper to create a request
fn create_request(api_key: ApiKey, body: RequestBody) -> Request {
    Request {
        header: RequestHeader {
            api_key: api_key as i16,
            api_version: 0,
            correlation_id: 1,
            client_id: Some("test-client".to_string()),
        },
        body,
    }
}

#[tokio::test]
async fn test_local_group_coordination() {
    let handler = create_test_handler(false).await;
    
    // Test join group
    let join_request = create_request(
        ApiKey::JoinGroup,
        RequestBody::JoinGroup(JoinGroupRequest {
            group_id: "local-test-group".to_string(),
            session_timeout: 30000,
            rebalance_timeout: 60000,
            member_id: String::new(),
            protocol_type: "consumer".to_string(),
            protocols: vec![Protocol {
                name: "range".to_string(),
                metadata: vec![1, 2, 3],
            }],
        }),
    );
    
    let response = handler.handle_request(join_request).await.unwrap();
    
    if let Response::JoinGroup(join_resp) = response {
        assert_eq!(join_resp.error_code, 0);
        assert!(!join_resp.member_id.is_empty());
        assert_eq!(join_resp.generation_id, 1);
        
        let member_id = join_resp.member_id;
        
        // Test sync group
        let sync_request = create_request(
            ApiKey::SyncGroup,
            RequestBody::SyncGroup(SyncGroupRequest {
                group_id: "local-test-group".to_string(),
                generation_id: 1,
                member_id: member_id.clone(),
                assignments: vec![Assignment {
                    member_id: member_id.clone(),
                    assignment: vec![4, 5, 6],
                }],
            }),
        );
        
        let response = handler.handle_request(sync_request).await.unwrap();
        
        if let Response::SyncGroup(sync_resp) = response {
            assert_eq!(sync_resp.error_code, 0);
            assert_eq!(sync_resp.assignment, vec![4, 5, 6]);
        } else {
            panic!("Expected SyncGroup response");
        }
        
        // Test heartbeat
        let heartbeat_request = create_request(
            ApiKey::Heartbeat,
            RequestBody::Heartbeat(HeartbeatRequest {
                group_id: "local-test-group".to_string(),
                generation_id: 1,
                member_id: member_id.clone(),
            }),
        );
        
        let response = handler.handle_request(heartbeat_request).await.unwrap();
        
        if let Response::Heartbeat(heartbeat_resp) = response {
            assert_eq!(heartbeat_resp.error_code, 0);
        } else {
            panic!("Expected Heartbeat response");
        }
    } else {
        panic!("Expected JoinGroup response");
    }
}

#[tokio::test]
async fn test_offset_commit_and_fetch() {
    let handler = create_test_handler(false).await;
    
    // Join group first
    let join_request = create_request(
        ApiKey::JoinGroup,
        RequestBody::JoinGroup(JoinGroupRequest {
            group_id: "offset-test-group".to_string(),
            session_timeout: 30000,
            rebalance_timeout: 60000,
            member_id: String::new(),
            protocol_type: "consumer".to_string(),
            protocols: vec![Protocol {
                name: "range".to_string(),
                metadata: vec![],
            }],
        }),
    );
    
    let response = handler.handle_request(join_request).await.unwrap();
    let member_id = if let Response::JoinGroup(resp) = response {
        resp.member_id
    } else {
        panic!("Expected JoinGroup response");
    };
    
    // Sync group
    let sync_request = create_request(
        ApiKey::SyncGroup,
        RequestBody::SyncGroup(SyncGroupRequest {
            group_id: "offset-test-group".to_string(),
            generation_id: 1,
            member_id: member_id.clone(),
            assignments: vec![],
        }),
    );
    handler.handle_request(sync_request).await.unwrap();
    
    // Commit offsets
    let commit_request = create_request(
        ApiKey::OffsetCommit,
        RequestBody::OffsetCommit(OffsetCommitRequest {
            group_id: "offset-test-group".to_string(),
            generation_id: 1,
            member_id: member_id.clone(),
            topics: vec![
                OffsetCommitRequestTopic {
                    name: "test-topic".to_string(),
                    partitions: vec![
                        OffsetCommitRequestPartition {
                            partition_index: 0,
                            committed_offset: 100,
                            committed_metadata: "checkpoint-0".to_string(),
                        },
                        OffsetCommitRequestPartition {
                            partition_index: 1,
                            committed_offset: 200,
                            committed_metadata: "checkpoint-1".to_string(),
                        },
                    ],
                },
            ],
        }),
    );
    
    let response = handler.handle_request(commit_request).await.unwrap();
    
    if let Response::OffsetCommit(commit_resp) = response {
        assert_eq!(commit_resp.topics.len(), 1);
        assert_eq!(commit_resp.topics[0].partitions.len(), 2);
        for partition in &commit_resp.topics[0].partitions {
            assert_eq!(partition.error_code, 0);
        }
    } else {
        panic!("Expected OffsetCommit response");
    }
    
    // Fetch offsets
    let fetch_request = create_request(
        ApiKey::OffsetFetch,
        RequestBody::OffsetFetch(OffsetFetchRequest {
            group_id: "offset-test-group".to_string(),
            topics: Some(vec!["test-topic".to_string()]),
        }),
    );
    
    let response = handler.handle_request(fetch_request).await.unwrap();
    
    if let Response::OffsetFetch(fetch_resp) = response {
        assert_eq!(fetch_resp.topics.len(), 1);
        assert_eq!(fetch_resp.topics[0].name, "test-topic");
        assert_eq!(fetch_resp.topics[0].partitions.len(), 2);
        
        let partition_map: std::collections::HashMap<i32, i64> = fetch_resp.topics[0].partitions.iter()
            .map(|p| (p.partition_index, p.committed_offset))
            .collect();
        
        assert_eq!(partition_map.get(&0), Some(&100));
        assert_eq!(partition_map.get(&1), Some(&200));
    } else {
        panic!("Expected OffsetFetch response");
    }
}

#[tokio::test]
async fn test_multiple_members_rebalance() {
    let handler = create_test_handler(false).await;
    
    // Member 1 joins
    let join_request1 = create_request(
        ApiKey::JoinGroup,
        RequestBody::JoinGroup(JoinGroupRequest {
            group_id: "rebalance-group".to_string(),
            session_timeout: 30000,
            rebalance_timeout: 60000,
            member_id: String::new(),
            protocol_type: "consumer".to_string(),
            protocols: vec![Protocol {
                name: "range".to_string(),
                metadata: vec![],
            }],
        }),
    );
    
    let response1 = handler.handle_request(join_request1).await.unwrap();
    let (member1_id, gen1) = if let Response::JoinGroup(resp) = response1 {
        assert_eq!(resp.leader_id, resp.member_id); // First member becomes leader
        (resp.member_id, resp.generation_id)
    } else {
        panic!("Expected JoinGroup response");
    };
    
    // Member 2 joins - should trigger rebalance
    let join_request2 = create_request(
        ApiKey::JoinGroup,
        RequestBody::JoinGroup(JoinGroupRequest {
            group_id: "rebalance-group".to_string(),
            session_timeout: 30000,
            rebalance_timeout: 60000,
            member_id: String::new(),
            protocol_type: "consumer".to_string(),
            protocols: vec![Protocol {
                name: "range".to_string(),
                metadata: vec![],
            }],
        }),
    );
    
    let response2 = handler.handle_request(join_request2).await.unwrap();
    let (member2_id, gen2) = if let Response::JoinGroup(resp) = response2 {
        assert_eq!(resp.leader_id, member1_id); // Member 1 remains leader
        (resp.member_id, resp.generation_id)
    } else {
        panic!("Expected JoinGroup response");
    };
    
    // Generation should increase due to rebalance
    assert!(gen2 > gen1);
}

#[tokio::test]
async fn test_leave_group_behavior() {
    let handler = create_test_handler(false).await;
    
    // Join group
    let join_request = create_request(
        ApiKey::JoinGroup,
        RequestBody::JoinGroup(JoinGroupRequest {
            group_id: "leave-test-group".to_string(),
            session_timeout: 30000,
            rebalance_timeout: 60000,
            member_id: String::new(),
            protocol_type: "consumer".to_string(),
            protocols: vec![Protocol {
                name: "range".to_string(),
                metadata: vec![],
            }],
        }),
    );
    
    let response = handler.handle_request(join_request).await.unwrap();
    let member_id = if let Response::JoinGroup(resp) = response {
        resp.member_id
    } else {
        panic!("Expected JoinGroup response");
    };
    
    // Leave group
    let leave_request = create_request(
        ApiKey::LeaveGroup,
        RequestBody::LeaveGroup(LeaveGroupRequest {
            group_id: "leave-test-group".to_string(),
            member_id: member_id.clone(),
        }),
    );
    
    let response = handler.handle_request(leave_request).await.unwrap();
    
    if let Response::LeaveGroup(leave_resp) = response {
        assert_eq!(leave_resp.error_code, 0);
    } else {
        panic!("Expected LeaveGroup response");
    }
    
    // Heartbeat should fail after leaving
    let heartbeat_request = create_request(
        ApiKey::Heartbeat,
        RequestBody::Heartbeat(HeartbeatRequest {
            group_id: "leave-test-group".to_string(),
            generation_id: 1,
            member_id,
        }),
    );
    
    let response = handler.handle_request(heartbeat_request).await.unwrap();
    
    if let Response::Heartbeat(heartbeat_resp) = response {
        assert_ne!(heartbeat_resp.error_code, 0); // Should fail
    } else {
        panic!("Expected Heartbeat response");
    }
}

#[tokio::test]
async fn test_api_versions() {
    let handler = create_test_handler(false).await;
    
    let api_versions_request = create_request(
        ApiKey::ApiVersions,
        RequestBody::ApiVersions(chronik_protocol::ApiVersionsRequest {}),
    );
    
    let response = handler.handle_request(api_versions_request).await.unwrap();
    
    if let Response::ApiVersions(versions_resp) = response {
        assert_eq!(versions_resp.error_code, 0);
        assert!(!versions_resp.api_keys.is_empty());
        
        // Check that group coordination APIs are supported
        let supported_apis: std::collections::HashSet<i16> = versions_resp.api_keys.iter()
            .map(|k| k.api_key)
            .collect();
        
        assert!(supported_apis.contains(&(ApiKey::JoinGroup as i16)));
        assert!(supported_apis.contains(&(ApiKey::SyncGroup as i16)));
        assert!(supported_apis.contains(&(ApiKey::Heartbeat as i16)));
        assert!(supported_apis.contains(&(ApiKey::LeaveGroup as i16)));
        assert!(supported_apis.contains(&(ApiKey::OffsetCommit as i16)));
        assert!(supported_apis.contains(&(ApiKey::OffsetFetch as i16)));
    } else {
        panic!("Expected ApiVersions response");
    }
}