//! Tests for controller-based group manager

use chronik_ingest::{
    controller_group_manager::ControllerGroupManager,
    consumer_group::{JoinGroupResponse, SyncGroupResponse, HeartbeatResponse, 
                     CommitOffsetsResponse, TopicPartitionOffset},
};
use chronik_common::metadata::memory::InMemoryMetadataStore;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::timeout;

/// Mock controller server for testing
mod mock_controller {
    use chronik_controller::proto::{
        controller_service_server::{ControllerService, ControllerServiceServer},
        JoinGroupRequest, JoinGroupResponse, JoinGroupResponseMember,
        SyncGroupRequest, SyncGroupResponse,
        HeartbeatRequest, HeartbeatResponse,
        LeaveGroupRequest, LeaveGroupResponse,
        CommitOffsetsRequest, CommitOffsetsResponse, PartitionOffsetCommitResult,
        FetchOffsetsRequest, FetchOffsetsResponse, TopicPartitionOffset,
        DescribeGroupRequest, DescribeGroupResponse,
        ListGroupsRequest, ListGroupsResponse, GroupListing,
    };
    use tonic::{Request, Response, Status};
    use std::sync::{Arc, Mutex};
    use std::collections::HashMap;
    
    #[derive(Default)]
    pub struct MockState {
        pub groups: HashMap<String, GroupInfo>,
        pub offsets: HashMap<(String, String, i32), i64>, // (group, topic, partition) -> offset
    }
    
    pub struct GroupInfo {
        pub generation_id: i32,
        pub leader_id: String,
        pub members: HashMap<String, MemberInfo>,
        pub protocol: String,
    }
    
    pub struct MemberInfo {
        pub client_id: String,
        pub assignment: Vec<u8>,
    }
    
    pub struct MockController {
        state: Arc<Mutex<MockState>>,
    }
    
    impl MockController {
        pub fn new() -> Self {
            Self {
                state: Arc::new(Mutex::new(MockState::default())),
            }
        }
    }
    
    #[tonic::async_trait]
    impl ControllerService for MockController {
        async fn join_group(
            &self,
            request: Request<JoinGroupRequest>,
        ) -> Result<Response<JoinGroupResponse>, Status> {
            let req = request.into_inner();
            let mut state = self.state.lock().unwrap();
            
            let group = state.groups.entry(req.group_id.clone())
                .or_insert_with(|| GroupInfo {
                    generation_id: 1,
                    leader_id: String::new(),
                    members: HashMap::new(),
                    protocol: "range".to_string(),
                });
            
            let member_id = if req.member_id.is_empty() {
                format!("member-{}", uuid::Uuid::new_v4())
            } else {
                req.member_id
            };
            
            if group.leader_id.is_empty() {
                group.leader_id = member_id.clone();
            }
            
            group.members.insert(member_id.clone(), MemberInfo {
                client_id: req.client_id,
                assignment: vec![],
            });
            
            let members = if member_id == group.leader_id {
                group.members.iter()
                    .map(|(id, _)| JoinGroupResponseMember {
                        member_id: id.clone(),
                        metadata: vec![],
                    })
                    .collect()
            } else {
                vec![]
            };
            
            Ok(Response::new(JoinGroupResponse {
                error_code: 0,
                generation_id: group.generation_id,
                protocol: group.protocol.clone(),
                leader_id: group.leader_id.clone(),
                member_id,
                members,
            }))
        }
        
        async fn sync_group(
            &self,
            request: Request<SyncGroupRequest>,
        ) -> Result<Response<SyncGroupResponse>, Status> {
            let req = request.into_inner();
            let mut state = self.state.lock().unwrap();
            
            let assignment = if let Some(group) = state.groups.get_mut(&req.group_id) {
                if let Some(member) = group.members.get_mut(&req.member_id) {
                    if !req.assignments.is_empty() {
                        // Leader is providing assignments
                        for assignment in req.assignments {
                            if let Some(m) = group.members.get_mut(&assignment.member_id) {
                                m.assignment = assignment.assignment;
                            }
                        }
                    }
                    member.assignment.clone()
                } else {
                    vec![]
                }
            } else {
                vec![]
            };
            
            Ok(Response::new(SyncGroupResponse {
                error_code: 0,
                assignment,
            }))
        }
        
        async fn heartbeat(
            &self,
            request: Request<HeartbeatRequest>,
        ) -> Result<Response<HeartbeatResponse>, Status> {
            let req = request.into_inner();
            let state = self.state.lock().unwrap();
            
            let error_code = if let Some(group) = state.groups.get(&req.group_id) {
                if group.members.contains_key(&req.member_id) {
                    0
                } else {
                    25 // UNKNOWN_MEMBER_ID
                }
            } else {
                15 // GROUP_COORDINATOR_NOT_AVAILABLE
            };
            
            Ok(Response::new(HeartbeatResponse { error_code }))
        }
        
        async fn leave_group(
            &self,
            request: Request<LeaveGroupRequest>,
        ) -> Result<Response<LeaveGroupResponse>, Status> {
            let req = request.into_inner();
            let mut state = self.state.lock().unwrap();
            
            if let Some(group) = state.groups.get_mut(&req.group_id) {
                group.members.remove(&req.member_id);
                if group.members.is_empty() {
                    state.groups.remove(&req.group_id);
                }
            }
            
            Ok(Response::new(LeaveGroupResponse { error_code: 0 }))
        }
        
        async fn commit_offsets(
            &self,
            request: Request<CommitOffsetsRequest>,
        ) -> Result<Response<CommitOffsetsResponse>, Status> {
            let req = request.into_inner();
            let mut state = self.state.lock().unwrap();
            
            let results = req.offsets.into_iter()
                .map(|offset| {
                    let key = (req.group_id.clone(), offset.topic.clone(), offset.partition);
                    state.offsets.insert(key, offset.offset);
                    
                    PartitionOffsetCommitResult {
                        topic: offset.topic,
                        partition: offset.partition,
                        error_code: 0,
                    }
                })
                .collect();
            
            Ok(Response::new(CommitOffsetsResponse { results }))
        }
        
        async fn fetch_offsets(
            &self,
            request: Request<FetchOffsetsRequest>,
        ) -> Result<Response<FetchOffsetsResponse>, Status> {
            let req = request.into_inner();
            let state = self.state.lock().unwrap();
            
            let mut offsets = vec![];
            
            for topic in req.topics {
                // For testing, assume 3 partitions per topic
                for partition in 0..3 {
                    let key = (req.group_id.clone(), topic.clone(), partition);
                    if let Some(&offset) = state.offsets.get(&key) {
                        offsets.push(TopicPartitionOffset {
                            topic: topic.clone(),
                            partition,
                            offset,
                            metadata: String::new(),
                        });
                    }
                }
            }
            
            Ok(Response::new(FetchOffsetsResponse { offsets }))
        }
        
        async fn describe_group(
            &self,
            request: Request<DescribeGroupRequest>,
        ) -> Result<Response<DescribeGroupResponse>, Status> {
            let req = request.into_inner();
            let state = self.state.lock().unwrap();
            
            let response = if let Some(group) = state.groups.get(&req.group_id) {
                DescribeGroupResponse {
                    exists: true,
                    group_id: req.group_id,
                    state: "Stable".to_string(),
                    protocol_type: "consumer".to_string(),
                    protocol: group.protocol.clone(),
                    members: group.members.len() as i32,
                }
            } else {
                DescribeGroupResponse {
                    exists: false,
                    ..Default::default()
                }
            };
            
            Ok(Response::new(response))
        }
        
        async fn list_groups(
            &self,
            _request: Request<ListGroupsRequest>,
        ) -> Result<Response<ListGroupsResponse>, Status> {
            let state = self.state.lock().unwrap();
            
            let groups = state.groups.keys()
                .map(|id| GroupListing {
                    group_id: id.clone(),
                    protocol_type: "consumer".to_string(),
                })
                .collect();
            
            Ok(Response::new(ListGroupsResponse { groups }))
        }
    }
}

/// Start a mock controller server
async fn start_mock_controller() -> (String, tokio::task::JoinHandle<()>) {
    use tonic::transport::Server;
    use chronik_controller::proto::controller_service_server::ControllerServiceServer;
    
    let mock = mock_controller::MockController::new();
    let addr = "127.0.0.1:0".parse().unwrap();
    
    let server = Server::builder()
        .add_service(ControllerServiceServer::new(mock))
        .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(
            tokio::net::TcpListener::bind(addr).await.unwrap()
        ));
    
    let local_addr = server.local_addr();
    let handle = tokio::spawn(async move {
        server.await.unwrap();
    });
    
    // Give server time to start
    tokio::time::sleep(Duration::from_millis(100)).await;
    
    (local_addr.to_string(), handle)
}

#[tokio::test]
async fn test_controller_group_manager_lifecycle() {
    let (controller_addr, _handle) = start_mock_controller().await;
    let metadata_store = Arc::new(InMemoryMetadataStore::new());
    
    let manager = ControllerGroupManager::new(
        format!("http://{}", controller_addr),
        metadata_store,
    ).await.unwrap();
    
    // Test join group
    let join_response = manager.join_group(
        "test-group".to_string(),
        None,
        "test-client".to_string(),
        "127.0.0.1".to_string(),
        Duration::from_secs(30),
        Duration::from_secs(60),
        "consumer".to_string(),
        vec![("range".to_string(), vec![1, 2, 3])],
        None,
    ).await.unwrap();
    
    assert_eq!(join_response.error_code, 0);
    assert!(!join_response.member_id.is_empty());
    assert_eq!(join_response.generation_id, 1);
    
    let member_id = join_response.member_id.clone();
    
    // Test sync group
    let sync_response = manager.sync_group(
        "test-group".to_string(),
        1,
        member_id.clone(),
        0,
        Some(vec![(member_id.clone(), vec![4, 5, 6])]),
    ).await.unwrap();
    
    assert_eq!(sync_response.error_code, 0);
    
    // Test heartbeat
    let heartbeat_response = manager.heartbeat(
        "test-group".to_string(),
        member_id.clone(),
        1,
        None,
    ).await.unwrap();
    
    assert_eq!(heartbeat_response.error_code, 0);
    
    // Test leave group
    manager.leave_group(
        "test-group".to_string(),
        member_id,
    ).await.unwrap();
}

#[tokio::test]
async fn test_controller_offset_management() {
    let (controller_addr, _handle) = start_mock_controller().await;
    let metadata_store = Arc::new(InMemoryMetadataStore::new());
    
    let manager = ControllerGroupManager::new(
        format!("http://{}", controller_addr),
        metadata_store,
    ).await.unwrap();
    
    // Join group first
    let join_response = manager.join_group(
        "offset-group".to_string(),
        None,
        "offset-client".to_string(),
        "127.0.0.1".to_string(),
        Duration::from_secs(30),
        Duration::from_secs(60),
        "consumer".to_string(),
        vec![("range".to_string(), vec![])],
        None,
    ).await.unwrap();
    
    let member_id = join_response.member_id.clone();
    
    // Test commit offsets
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
        1,
        member_id,
        None,
        offsets,
    ).await.unwrap();
    
    assert_eq!(commit_response.error_code, 0);
    assert_eq!(commit_response.partition_errors.len(), 2);
    
    // Test fetch offsets
    let fetched_offsets = manager.fetch_offsets(
        "offset-group".to_string(),
        vec!["test-topic".to_string()],
    ).await.unwrap();
    
    assert_eq!(fetched_offsets.len(), 2);
    
    let offset_map: std::collections::HashMap<i32, i64> = fetched_offsets.iter()
        .map(|o| (o.partition, o.offset))
        .collect();
    
    assert_eq!(offset_map.get(&0), Some(&100));
    assert_eq!(offset_map.get(&1), Some(&200));
}

#[tokio::test]
async fn test_controller_group_operations() {
    let (controller_addr, _handle) = start_mock_controller().await;
    let metadata_store = Arc::new(InMemoryMetadataStore::new());
    
    let manager = ControllerGroupManager::new(
        format!("http://{}", controller_addr),
        metadata_store,
    ).await.unwrap();
    
    // Create a group
    let group_id = manager.get_or_create_group(
        "ops-group".to_string(),
        "consumer".to_string(),
    ).await.unwrap();
    
    assert_eq!(group_id, "ops-group");
    
    // Join the group
    manager.join_group(
        group_id.clone(),
        None,
        "ops-client".to_string(),
        "127.0.0.1".to_string(),
        Duration::from_secs(30),
        Duration::from_secs(60),
        "consumer".to_string(),
        vec![("range".to_string(), vec![])],
        None,
    ).await.unwrap();
    
    // List groups
    let groups = manager.list_groups().await.unwrap();
    assert!(groups.contains(&group_id));
    
    // Describe group
    let group_desc = manager.describe_group(group_id).await.unwrap();
    assert!(group_desc.is_some());
}

#[tokio::test]
async fn test_controller_reconnection() {
    let (controller_addr, handle) = start_mock_controller().await;
    let metadata_store = Arc::new(InMemoryMetadataStore::new());
    
    let manager = ControllerGroupManager::new(
        format!("http://{}", controller_addr),
        metadata_store,
    ).await.unwrap();
    
    // Join group
    let join_response = manager.join_group(
        "reconnect-group".to_string(),
        None,
        "reconnect-client".to_string(),
        "127.0.0.1".to_string(),
        Duration::from_secs(30),
        Duration::from_secs(60),
        "consumer".to_string(),
        vec![("range".to_string(), vec![])],
        None,
    ).await.unwrap();
    
    let member_id = join_response.member_id.clone();
    
    // Simulate controller restart by aborting the server
    handle.abort();
    tokio::time::sleep(Duration::from_millis(100)).await;
    
    // Try heartbeat - should fail
    let heartbeat_result = timeout(
        Duration::from_secs(1),
        manager.heartbeat(
            "reconnect-group".to_string(),
            member_id,
            1,
            None,
        )
    ).await;
    
    assert!(heartbeat_result.is_err() || heartbeat_result.unwrap().is_err());
}

#[tokio::test]
async fn test_controller_concurrent_joins() {
    let (controller_addr, _handle) = start_mock_controller().await;
    let metadata_store = Arc::new(InMemoryMetadataStore::new());
    
    let manager = Arc::new(ControllerGroupManager::new(
        format!("http://{}", controller_addr),
        metadata_store,
    ).await.unwrap());
    
    let num_clients = 5;
    let mut handles = vec![];
    
    // Spawn concurrent join requests
    for i in 0..num_clients {
        let manager = manager.clone();
        let handle = tokio::spawn(async move {
            manager.join_group(
                "concurrent-group".to_string(),
                None,
                format!("client-{}", i),
                format!("127.0.0.{}", i + 1),
                Duration::from_secs(30),
                Duration::from_secs(60),
                "consumer".to_string(),
                vec![("range".to_string(), vec![])],
                None,
            ).await
        });
        handles.push(handle);
    }
    
    // Wait for all joins
    let mut results = vec![];
    for handle in handles {
        let result = handle.await.unwrap().unwrap();
        results.push(result);
    }
    
    // All should succeed
    for result in &results {
        assert_eq!(result.error_code, 0);
    }
    
    // All should have same generation
    let generation = results[0].generation_id;
    for result in &results {
        assert_eq!(result.generation_id, generation);
    }
}