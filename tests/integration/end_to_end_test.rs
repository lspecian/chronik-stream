//! End-to-end integration tests.

use chronik_admin::AdminConfig;
use chronik_cli::client::AdminClient;
use chronik_controller::{ControllerConfig, ControllerNode};
use chronik_ingest::{IngestServer, ServerConfig};
use sqlx::PgPool;
use tempfile::TempDir;
use testcontainers::{clients::Cli, images::postgres::Postgres, Container};

struct TestCluster {
    _pg_container: Container<'static, Postgres>,
    _temp_dir: TempDir,
    controller_addr: String,
    ingest_addr: String,
    admin_addr: String,
    db_url: String,
}

async fn setup_test_cluster() -> TestCluster {
    // Start PostgreSQL
    let docker = Cli::default();
    let pg_container = docker.run(Postgres::default());
    let pg_port = pg_container.get_host_port_ipv4(5432);
    let db_url = format!("postgres://postgres:postgres@localhost:{}/postgres", pg_port);
    
    // Create database schema
    let pool = PgPool::connect(&db_url).await.unwrap();
    sqlx::migrate!("./migrations").run(&pool).await.unwrap();
    
    // Start controller
    let temp_dir = TempDir::new().unwrap();
    let controller_config = ControllerConfig {
        node_id: 1,
        listen_addr: "127.0.0.1:0".to_string(),
        db_url: db_url.clone(),
        storage_path: temp_dir.path().to_path_buf(),
    };
    
    let controller = ControllerNode::new(controller_config).await.unwrap();
    let controller_addr = controller.local_addr().await.unwrap();
    
    tokio::spawn(async move {
        controller.run().await.unwrap();
    });
    
    // Start ingest server
    let ingest_config = ServerConfig {
        listen_addr: "127.0.0.1:0".to_string(),
        segment_size: 1024 * 1024,
        flush_interval_ms: 100,
    };
    
    let storage = chronik_storage::segment::SegmentManager::new(temp_dir.path())
        .await
        .unwrap();
    let ingest_server = IngestServer::new(ingest_config, storage);
    let ingest_addr = ingest_server.local_addr().await.unwrap();
    
    tokio::spawn(async move {
        ingest_server.run().await.unwrap();
    });
    
    // Start admin API
    let admin_config = AdminConfig {
        server: chronik_admin::config::ServerConfig {
            address: "127.0.0.1".to_string(),
            port: 0,
            tls: None,
        },
        database: chronik_admin::config::DatabaseConfig {
            url: db_url.clone(),
            max_connections: 5,
        },
        controller: chronik_admin::config::ControllerConfig {
            endpoints: vec![controller_addr.clone()],
            timeout_secs: 30,
        },
        auth: chronik_admin::config::AuthConfig {
            enabled: false,
            jwt_secret: "test-secret".to_string(),
            token_expiration_secs: 3600,
        },
    };
    
    let admin_state = chronik_admin::state::AppState::new(admin_config.clone())
        .await
        .unwrap();
    
    let admin_app = chronik_admin::api::create_router(admin_state);
    let admin_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap();
    let admin_addr = format!("http://{}", admin_listener.local_addr().unwrap());
    
    tokio::spawn(async move {
        axum::serve(admin_listener, admin_app).await.unwrap();
    });
    
    // Give services time to start
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
    
    TestCluster {
        _pg_container: pg_container,
        _temp_dir: temp_dir,
        controller_addr,
        ingest_addr,
        admin_addr,
        db_url,
    }
}

#[tokio::test]
#[ignore] // Requires Docker
async fn test_full_message_flow() {
    let cluster = setup_test_cluster().await;
    
    // Create admin client
    let admin_client = AdminClient::new(&cluster.admin_addr, None).unwrap();
    
    // 1. Create topic via admin API
    let create_topic_request = serde_json::json!({
        "name": "test-events",
        "partitions": 3,
        "replication_factor": 1,
    });
    
    admin_client
        .post::<_, serde_json::Value>("/api/v1/topics", &create_topic_request)
        .await
        .unwrap();
    
    // 2. Produce messages via Kafka protocol
    let mut producer_stream = tokio::net::TcpStream::connect(&cluster.ingest_addr)
        .await
        .unwrap();
    
    // Send produce request
    let produce_request = create_produce_request("test-events", vec![
        b"Event 1".to_vec(),
        b"Event 2".to_vec(),
        b"Event 3".to_vec(),
    ]);
    
    send_kafka_request(&mut producer_stream, produce_request).await;
    
    // 3. Consume messages via Kafka protocol
    let mut consumer_stream = tokio::net::TcpStream::connect(&cluster.ingest_addr)
        .await
        .unwrap();
    
    let fetch_request = create_fetch_request("test-events", 0, 0);
    let response = send_kafka_request(&mut consumer_stream, fetch_request).await;
    
    // 4. Verify via admin API
    let topic_info: serde_json::Value = admin_client
        .get("/api/v1/topics/test-events")
        .await
        .unwrap();
    
    assert_eq!(topic_info["name"], "test-events");
    assert_eq!(topic_info["partitions"], 3);
    
    // 5. Search for messages
    let search_response: serde_json::Value = admin_client
        .get("/api/v1/search?q=Event&index=test-events")
        .await
        .unwrap();
    
    assert!(search_response["results"].as_array().unwrap().len() >= 3);
}

#[tokio::test]
#[ignore] // Requires Docker
async fn test_consumer_group_coordination() {
    let cluster = setup_test_cluster().await;
    
    // Create topic
    let admin_client = AdminClient::new(&cluster.admin_addr, None).unwrap();
    let create_topic_request = serde_json::json!({
        "name": "group-test",
        "partitions": 6,
        "replication_factor": 1,
    });
    
    admin_client
        .post::<_, serde_json::Value>("/api/v1/topics", &create_topic_request)
        .await
        .unwrap();
    
    // Start multiple consumers in the same group
    let mut consumers = vec![];
    for i in 0..3 {
        let addr = cluster.ingest_addr.clone();
        let consumer = tokio::spawn(async move {
            let mut stream = tokio::net::TcpStream::connect(&addr).await.unwrap();
            
            // Join group
            let join_request = create_join_group_request(
                "test-group",
                &format!("consumer-{}", i),
            );
            send_kafka_request(&mut stream, join_request).await;
            
            // Keep connection alive
            tokio::time::sleep(tokio::time::Duration::from_secs(10)).await;
        });
        consumers.push(consumer);
    }
    
    // Give time for group to stabilize
    tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
    
    // Check group status via admin API
    let group_info: serde_json::Value = admin_client
        .get("/api/v1/consumer-groups/test-group")
        .await
        .unwrap();
    
    assert_eq!(group_info["group_id"], "test-group");
    assert_eq!(group_info["state"], "Stable");
    assert_eq!(group_info["members"], 3);
}

// Helper functions

fn create_produce_request(topic: &str, messages: Vec<Vec<u8>>) -> Vec<u8> {
    // Simplified - in real code, use protocol encoding
    vec![]
}

fn create_fetch_request(topic: &str, partition: i32, offset: i64) -> Vec<u8> {
    // Simplified - in real code, use protocol encoding
    vec![]
}

fn create_join_group_request(group_id: &str, member_id: &str) -> Vec<u8> {
    // Simplified - in real code, use protocol encoding
    vec![]
}

async fn send_kafka_request(stream: &mut tokio::net::TcpStream, request: Vec<u8>) -> Vec<u8> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    
    // Send size + request
    let size = request.len() as i32;
    stream.write_all(&size.to_be_bytes()).await.unwrap();
    stream.write_all(&request).await.unwrap();
    
    // Read response
    let mut size_buf = [0u8; 4];
    stream.read_exact(&mut size_buf).await.unwrap();
    let response_size = i32::from_be_bytes(size_buf);
    
    let mut response = vec![0u8; response_size as usize];
    stream.read_exact(&mut response).await.unwrap();
    
    response
}