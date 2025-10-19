//! Test module for Raft RPC protocol validation.
//!
//! This module contains test cases that validate the Raft RPC protocol implementation,
//! including message serialization, service trait implementations, and client-server communication.

#[cfg(test)]
mod tests {
    use crate::rpc::*;
    use async_trait::async_trait;
    use std::net::SocketAddr;
    use tokio::time::{timeout, Duration};
    use tonic::{Request, Response, Status};

    /// Mock Raft node for testing RPC handlers
    struct MockRaftNode {
        node_id: String,
        current_term: u64,
    }

    impl MockRaftNode {
        fn new(node_id: impl Into<String>) -> Self {
            Self {
                node_id: node_id.into(),
                current_term: 1,
            }
        }
    }

    #[async_trait]
    impl RaftRpcService for MockRaftNode {
        async fn handle_append_entries(
            &self,
            request: Request<AppendEntriesRequest>,
        ) -> Result<Response<AppendEntriesResponse>, Status> {
            let req = request.into_inner();

            // Simple logic: accept if term is >= current term
            let success = req.term >= self.current_term;

            Ok(Response::new(AppendEntriesResponse {
                term: self.current_term,
                success,
                conflict_index: if success { None } else { Some(0) },
                conflict_term: if success { None } else { Some(0) },
                node_id: self.node_id.clone(),
            }))
        }

        async fn handle_request_vote(
            &self,
            request: Request<RequestVoteRequest>,
        ) -> Result<Response<RequestVoteResponse>, Status> {
            let req = request.into_inner();

            // Simple logic: grant vote if term is >= current term
            let vote_granted = req.term >= self.current_term;

            Ok(Response::new(RequestVoteResponse {
                term: self.current_term,
                vote_granted,
                node_id: self.node_id.clone(),
            }))
        }

        async fn handle_install_snapshot(
            &self,
            request: Request<InstallSnapshotRequest>,
        ) -> Result<Response<InstallSnapshotResponse>, Status> {
            let req = request.into_inner();

            Ok(Response::new(InstallSnapshotResponse {
                term: self.current_term,
                node_id: self.node_id.clone(),
                bytes_written: req.data.len() as u64,
                success: true,
                error_message: None,
            }))
        }
    }

    #[test]
    fn test_append_entries_message_creation() {
        // Test that we can create AppendEntries messages
        let req = AppendEntriesRequest {
            term: 1,
            leader_id: "leader-1".to_string(),
            prev_log_index: 0,
            prev_log_term: 0,
            entries: vec![],
            leader_commit: 0,
        };

        assert_eq!(req.term, 1);
        assert_eq!(req.leader_id, "leader-1");
        assert_eq!(req.entries.len(), 0);
    }

    #[test]
    fn test_append_entries_with_log_entries() {
        // Test creating AppendEntries with log entries
        let entry = raft_rpc::LogEntry {
            index: 1,
            term: 1,
            command_type: "metadata_update".to_string(),
            data: vec![1, 2, 3, 4],
            checksum: Some(12345),
        };

        let req = AppendEntriesRequest {
            term: 1,
            leader_id: "leader-1".to_string(),
            prev_log_index: 0,
            prev_log_term: 0,
            entries: vec![entry.clone()],
            leader_commit: 0,
        };

        assert_eq!(req.entries.len(), 1);
        assert_eq!(req.entries[0].index, 1);
        assert_eq!(req.entries[0].command_type, "metadata_update");
        assert_eq!(req.entries[0].data, vec![1, 2, 3, 4]);
        assert_eq!(req.entries[0].checksum, Some(12345));
    }

    #[test]
    fn test_request_vote_message_creation() {
        // Test RequestVote message creation
        let req = RequestVoteRequest {
            term: 2,
            candidate_id: "candidate-1".to_string(),
            last_log_index: 10,
            last_log_term: 1,
        };

        assert_eq!(req.term, 2);
        assert_eq!(req.candidate_id, "candidate-1");
        assert_eq!(req.last_log_index, 10);
        assert_eq!(req.last_log_term, 1);
    }

    #[test]
    fn test_install_snapshot_message_creation() {
        // Test InstallSnapshot message creation
        let snapshot_data = vec![1, 2, 3, 4, 5];
        let req = InstallSnapshotRequest {
            term: 3,
            leader_id: "leader-1".to_string(),
            last_included_index: 100,
            last_included_term: 2,
            offset: 0,
            data: snapshot_data.clone(),
            done: true,
            checksum: Some(98765),
        };

        assert_eq!(req.term, 3);
        assert_eq!(req.leader_id, "leader-1");
        assert_eq!(req.last_included_index, 100);
        assert_eq!(req.data, snapshot_data);
        assert_eq!(req.done, true);
        assert_eq!(req.checksum, Some(98765));
    }

    #[tokio::test]
    async fn test_mock_node_append_entries() {
        // Test mock node handles AppendEntries correctly
        let node = MockRaftNode::new("test-node");

        let req = Request::new(AppendEntriesRequest {
            term: 1,
            leader_id: "leader".to_string(),
            prev_log_index: 0,
            prev_log_term: 0,
            entries: vec![],
            leader_commit: 0,
        });

        let response = node.handle_append_entries(req).await.unwrap();
        let resp = response.into_inner();

        assert_eq!(resp.term, 1);
        assert_eq!(resp.success, true);
        assert_eq!(resp.node_id, "test-node");
    }

    #[tokio::test]
    async fn test_mock_node_request_vote() {
        // Test mock node handles RequestVote correctly
        let node = MockRaftNode::new("test-node");

        let req = Request::new(RequestVoteRequest {
            term: 2,
            candidate_id: "candidate".to_string(),
            last_log_index: 0,
            last_log_term: 0,
        });

        let response = node.handle_request_vote(req).await.unwrap();
        let resp = response.into_inner();

        assert_eq!(resp.term, 1);
        assert_eq!(resp.vote_granted, true);
        assert_eq!(resp.node_id, "test-node");
    }

    #[tokio::test]
    async fn test_mock_node_install_snapshot() {
        // Test mock node handles InstallSnapshot correctly
        let node = MockRaftNode::new("test-node");

        let snapshot_data = vec![1, 2, 3, 4, 5];
        let req = Request::new(InstallSnapshotRequest {
            term: 1,
            leader_id: "leader".to_string(),
            last_included_index: 10,
            last_included_term: 1,
            offset: 0,
            data: snapshot_data.clone(),
            done: true,
            checksum: Some(12345),
        });

        let response = node.handle_install_snapshot(req).await.unwrap();
        let resp = response.into_inner();

        assert_eq!(resp.term, 1);
        assert_eq!(resp.success, true);
        assert_eq!(resp.bytes_written, snapshot_data.len() as u64);
        assert_eq!(resp.node_id, "test-node");
    }

    #[tokio::test]
    async fn test_append_entries_response_fields() {
        // Test all AppendEntriesResponse fields
        let resp = AppendEntriesResponse {
            term: 5,
            success: false,
            conflict_index: Some(42),
            conflict_term: Some(3),
            node_id: "node-1".to_string(),
        };

        assert_eq!(resp.term, 5);
        assert_eq!(resp.success, false);
        assert_eq!(resp.conflict_index, Some(42));
        assert_eq!(resp.conflict_term, Some(3));
        assert_eq!(resp.node_id, "node-1");
    }

    #[tokio::test]
    async fn test_request_vote_response_fields() {
        // Test all RequestVoteResponse fields
        let resp = RequestVoteResponse {
            term: 7,
            vote_granted: true,
            node_id: "voter-1".to_string(),
        };

        assert_eq!(resp.term, 7);
        assert_eq!(resp.vote_granted, true);
        assert_eq!(resp.node_id, "voter-1");
    }

    #[tokio::test]
    async fn test_install_snapshot_response_fields() {
        // Test all InstallSnapshotResponse fields
        let resp = InstallSnapshotResponse {
            term: 9,
            node_id: "follower-1".to_string(),
            bytes_written: 1024,
            success: true,
            error_message: None,
        };

        assert_eq!(resp.term, 9);
        assert_eq!(resp.node_id, "follower-1");
        assert_eq!(resp.bytes_written, 1024);
        assert_eq!(resp.success, true);
        assert_eq!(resp.error_message, None);
    }

    #[tokio::test]
    async fn test_install_snapshot_response_with_error() {
        // Test InstallSnapshotResponse with error message
        let resp = InstallSnapshotResponse {
            term: 9,
            node_id: "follower-1".to_string(),
            bytes_written: 0,
            success: false,
            error_message: Some("Disk full".to_string()),
        };

        assert_eq!(resp.success, false);
        assert_eq!(resp.error_message, Some("Disk full".to_string()));
    }

    // Integration test demonstrating full server-client interaction
    // This is commented out because it requires actual network setup
    // To run manually: cargo test test_server_client_integration -- --ignored --nocapture
    #[tokio::test]
    #[ignore]
    async fn test_server_client_integration() {
        use tokio::task::JoinHandle;

        // Start server in background
        let node = MockRaftNode::new("server-node");
        let addr: SocketAddr = "127.0.0.1:50051".parse().unwrap();

        let server_handle: JoinHandle<Result<(), crate::error::RaftError>> = tokio::spawn(async move {
            RaftRpcServer::new(node).serve(addr).await
        });

        // Give server time to start
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Connect client
        let mut client = RaftRpcClient::connect("http://127.0.0.1:50051")
            .await
            .expect("Failed to connect");

        // Test AppendEntries
        let append_req = AppendEntriesRequest {
            term: 1,
            leader_id: "client-leader".to_string(),
            prev_log_index: 0,
            prev_log_term: 0,
            entries: vec![],
            leader_commit: 0,
        };

        let append_resp = timeout(Duration::from_secs(5), client.append_entries(append_req))
            .await
            .expect("Timeout")
            .expect("RPC failed");

        assert_eq!(append_resp.success, true);

        // Test RequestVote
        let vote_req = RequestVoteRequest {
            term: 2,
            candidate_id: "client-candidate".to_string(),
            last_log_index: 0,
            last_log_term: 0,
        };

        let vote_resp = timeout(Duration::from_secs(5), client.request_vote(vote_req))
            .await
            .expect("Timeout")
            .expect("RPC failed");

        assert_eq!(vote_resp.vote_granted, true);

        // Cleanup: abort server (in real scenario, use graceful shutdown)
        server_handle.abort();
    }
}
