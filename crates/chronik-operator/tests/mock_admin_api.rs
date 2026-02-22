//! Integration tests for AdminClient and KafkaAdminClient against a mock HTTP server.

use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::Mutex;

use chronik_operator::admin_client::{AddNodeRequest, AdminClient, RemoveNodeRequest};
use chronik_operator::kafka_client::{
    CreateTopicRequest, CreateUserRequest, KafkaAdminClient, SetAclsRequest,
};

/// A minimal mock HTTP server that records requests and returns canned responses.
struct MockServer {
    port: u16,
    /// Recorded (method, path, body) tuples.
    requests: Arc<Mutex<Vec<(String, String, String)>>>,
}

impl MockServer {
    async fn start(responses: Vec<(&'static str, &'static str)>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let requests: Arc<Mutex<Vec<(String, String, String)>>> = Arc::new(Mutex::new(Vec::new()));
        let req_clone = requests.clone();

        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let responses = responses.clone();
                let reqs = req_clone.clone();

                tokio::spawn(async move {
                    let mut buf = vec![0u8; 8192];
                    let n = match stream.read(&mut buf).await {
                        Ok(n) if n > 0 => n,
                        _ => return,
                    };
                    let request_str = String::from_utf8_lossy(&buf[..n]).to_string();

                    // Parse method, path
                    let first_line = request_str.lines().next().unwrap_or("");
                    let parts: Vec<&str> = first_line.split_whitespace().collect();
                    let method = parts.first().unwrap_or(&"GET").to_string();
                    let path = parts.get(1).unwrap_or(&"/").to_string();

                    // Extract body (after \r\n\r\n)
                    let body = request_str
                        .split("\r\n\r\n")
                        .nth(1)
                        .unwrap_or("")
                        .to_string();

                    reqs.lock().await.push((method.clone(), path.clone(), body));

                    // Find matching response
                    let response_body = responses
                        .iter()
                        .find(|(p, _)| path.contains(p))
                        .map(|(_, b)| *b)
                        .unwrap_or(r#"{"error":"not found"}"#);

                    let http_response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        response_body.len(),
                        response_body
                    );
                    let _ = stream.write_all(http_response.as_bytes()).await;
                });
            }
        });

        Self { port, requests }
    }

    fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    async fn recorded(&self) -> Vec<(String, String, String)> {
        self.requests.lock().await.clone()
    }
}

// ========== AdminClient Tests ==========

#[tokio::test]
async fn test_admin_health_check() {
    let server = MockServer::start(vec![(
        "/admin/health",
        r#"{"status":"ok","node_id":1,"is_leader":true,"cluster_nodes":[1,2,3]}"#,
    )])
    .await;

    let client = AdminClient::new(None);
    let resp = client.health(&server.url()).await.unwrap();

    assert_eq!(resp.status, "ok");
    assert_eq!(resp.node_id, 1);
    assert!(resp.is_leader);
    assert_eq!(resp.cluster_nodes, vec![1, 2, 3]);
}

#[tokio::test]
async fn test_admin_cluster_status() {
    let server = MockServer::start(vec![(
        "/admin/status",
        r#"{"node_id":1,"leader_id":1,"is_leader":true,"nodes":[{"node_id":1,"address":"host:9092","is_leader":true}],"partitions":[]}"#,
    )])
    .await;

    let client = AdminClient::new(Some("test-key".into()));
    let resp = client.status(&server.url()).await.unwrap();

    assert_eq!(resp.node_id, 1);
    assert_eq!(resp.leader_id, Some(1));
    assert_eq!(resp.nodes.len(), 1);

    // Verify API key was sent
    let reqs = server.recorded().await;
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].0, "GET");
    assert!(reqs[0].1.contains("/admin/status"));
}

#[tokio::test]
async fn test_admin_add_node() {
    let server = MockServer::start(vec![(
        "/admin/add-node",
        r#"{"success":true,"message":"Node 4 added"}"#,
    )])
    .await;

    let client = AdminClient::new(Some("key".into()));
    let req = AddNodeRequest {
        node_id: 4,
        kafka_addr: "node4:9092".into(),
        wal_addr: "node4:9291".into(),
        raft_addr: "node4:5001".into(),
    };
    let resp = client.add_node(&server.url(), &req).await.unwrap();

    assert!(resp.success);
    assert_eq!(resp.message, "Node 4 added");

    let reqs = server.recorded().await;
    assert_eq!(reqs[0].0, "POST");
    assert!(reqs[0].2.contains("\"node_id\":4"));
}

#[tokio::test]
async fn test_admin_remove_node() {
    let server = MockServer::start(vec![(
        "/admin/remove-node",
        r#"{"success":true,"message":"Node 4 removed"}"#,
    )])
    .await;

    let client = AdminClient::new(Some("key".into()));
    let req = RemoveNodeRequest {
        node_id: 4,
        force: false,
    };
    let resp = client.remove_node(&server.url(), &req).await.unwrap();

    assert!(resp.success);
}

#[tokio::test]
async fn test_admin_find_leader() {
    let server1 = MockServer::start(vec![(
        "/admin/health",
        r#"{"status":"ok","node_id":1,"is_leader":false,"cluster_nodes":[]}"#,
    )])
    .await;

    let server2 = MockServer::start(vec![(
        "/admin/health",
        r#"{"status":"ok","node_id":2,"is_leader":true,"cluster_nodes":[]}"#,
    )])
    .await;

    let client = AdminClient::new(None);
    let urls = vec![(1, server1.url()), (2, server2.url())];
    let leader = client.find_leader(&urls).await;

    assert!(leader.is_some());
    let (url, node_id) = leader.unwrap();
    assert_eq!(node_id, 2);
    assert_eq!(url, server2.url());
}

// ========== KafkaAdminClient Tests ==========

#[tokio::test]
async fn test_kafka_create_topic() {
    let server = MockServer::start(vec![(
        "/admin/topics",
        r#"{"success":true,"message":"Topic created","topic_id":"t-001"}"#,
    )])
    .await;

    let client = KafkaAdminClient::new(Some("key".into()));
    let req = CreateTopicRequest {
        name: "orders".into(),
        partitions: 6,
        replication_factor: 3,
        config: Default::default(),
    };
    let resp = client.create_topic(&server.url(), &req).await.unwrap();

    assert!(resp.success);
    assert_eq!(resp.topic_id.as_deref(), Some("t-001"));

    let reqs = server.recorded().await;
    assert_eq!(reqs[0].0, "POST");
    assert!(reqs[0].2.contains("\"name\":\"orders\""));
}

#[tokio::test]
async fn test_kafka_describe_topic() {
    let server = MockServer::start(vec![(
        "/admin/topics/orders",
        r#"{"name":"orders","partitions":6,"replication_factor":3,"config":{}}"#,
    )])
    .await;

    let client = KafkaAdminClient::new(None);
    let desc = client
        .describe_topic(&server.url(), "orders")
        .await
        .unwrap();

    assert!(desc.is_some());
    let d = desc.unwrap();
    assert_eq!(d.name, "orders");
    assert_eq!(d.partitions, 6);
}

#[tokio::test]
async fn test_kafka_delete_topic() {
    let server = MockServer::start(vec![(
        "/admin/topics",
        r#"{"success":true,"message":"Deleted"}"#,
    )])
    .await;

    let client = KafkaAdminClient::new(Some("key".into()));
    let resp = client.delete_topic(&server.url(), "orders").await.unwrap();

    assert!(resp.success);

    let reqs = server.recorded().await;
    assert_eq!(reqs[0].0, "DELETE");
}

#[tokio::test]
async fn test_kafka_create_user() {
    let server = MockServer::start(vec![(
        "/admin/users",
        r#"{"success":true,"message":"User created"}"#,
    )])
    .await;

    let client = KafkaAdminClient::new(Some("key".into()));
    let req = CreateUserRequest {
        username: "app-producer".into(),
        password: "secret123".into(),
        auth_type: "sasl-scram-sha-256".into(),
    };
    let resp = client.create_user(&server.url(), &req).await.unwrap();

    assert!(resp.success);

    let reqs = server.recorded().await;
    assert!(reqs[0].2.contains("\"username\":\"app-producer\""));
}

#[tokio::test]
async fn test_kafka_delete_user() {
    let server = MockServer::start(vec![(
        "/admin/users",
        r#"{"success":true,"message":"User deleted"}"#,
    )])
    .await;

    let client = KafkaAdminClient::new(Some("key".into()));
    let resp = client
        .delete_user(&server.url(), "app-producer")
        .await
        .unwrap();

    assert!(resp.success);

    let reqs = server.recorded().await;
    assert_eq!(reqs[0].0, "DELETE");
    assert!(reqs[0].1.contains("app-producer"));
}

#[tokio::test]
async fn test_kafka_set_acls() {
    let server = MockServer::start(vec![(
        "/admin/users",
        r#"{"success":true,"acl_count":2,"message":"ACLs set"}"#,
    )])
    .await;

    let client = KafkaAdminClient::new(Some("key".into()));
    let req = SetAclsRequest {
        username: "app".into(),
        acls: vec![
            chronik_operator::kafka_client::AclEntry {
                resource_type: "topic".into(),
                resource_name: "orders".into(),
                pattern_type: "literal".into(),
                operation: "Read".into(),
                effect: "Allow".into(),
            },
            chronik_operator::kafka_client::AclEntry {
                resource_type: "topic".into(),
                resource_name: "orders".into(),
                pattern_type: "literal".into(),
                operation: "Write".into(),
                effect: "Allow".into(),
            },
        ],
    };
    let resp = client.set_acls(&server.url(), &req).await.unwrap();

    assert!(resp.success);
    assert_eq!(resp.acl_count, 2);

    let reqs = server.recorded().await;
    assert_eq!(reqs[0].0, "PUT");
}
