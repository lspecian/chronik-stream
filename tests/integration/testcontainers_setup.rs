//! Test container setup for integration tests

use anyhow::Result;
use testcontainers::{clients::Cli, core::WaitFor, Container, GenericImage};
use std::collections::HashMap;

pub struct TestEnvironment {
    pub docker: Cli,
    pub minio: Container<'static, GenericImage>,
    pub minio_endpoint: String,
    pub minio_access_key: String,
    pub minio_secret_key: String,
}

impl TestEnvironment {
    pub async fn new() -> Result<Self> {
        let docker = Cli::default();
        
        // MinIO container for object storage
        let minio_image = GenericImage::new("minio/minio", "latest")
            .with_env_var("MINIO_ROOT_USER", "minioadmin")
            .with_env_var("MINIO_ROOT_PASSWORD", "minioadmin")
            .with_exposed_port(9000)
            .with_wait_for(WaitFor::message_on_stderr("MinIO Object Storage Server"))
            .with_cmd(vec!["server", "/data"]);
        
        let minio = docker.run(minio_image);
        let minio_port = minio.get_host_port_ipv4(9000);
        let minio_endpoint = format!("http://127.0.0.1:{}", minio_port);
        
        Ok(Self {
            docker,
            minio,
            minio_endpoint,
            minio_access_key: "minioadmin".to_string(),
            minio_secret_key: "minioadmin".to_string(),
        })
    }
    
    pub async fn create_bucket(&self, bucket_name: &str) -> Result<()> {
        // Create S3 client and bucket
        use opendal::{Operator, services};
        
        let mut builder = services::S3::default();
        builder
            .endpoint(&self.minio_endpoint)
            .region("us-east-1")
            .bucket(bucket_name)
            .access_key_id(&self.minio_access_key)
            .secret_access_key(&self.minio_secret_key);
        
        let op = Operator::new(builder)?.finish();
        
        // MinIO doesn't auto-create buckets, we need to use AWS SDK
        use aws_sdk_s3::{Client, Config};
        use aws_credential_types::Credentials;
        
        let creds = Credentials::new(
            &self.minio_access_key,
            &self.minio_secret_key,
            None,
            None,
            "minio",
        );
        
        let config = Config::builder()
            .endpoint_url(&self.minio_endpoint)
            .region(aws_sdk_s3::config::Region::new("us-east-1"))
            .credentials_provider(creds)
            .force_path_style(true)
            .build();
        
        let client = Client::from_conf(config);
        
        client
            .create_bucket()
            .bucket(bucket_name)
            .send()
            .await?;
        
        Ok(())
    }
}

pub struct ChronikCluster {
    pub nodes: Vec<ChronikNode>,
    pub client_port_base: u16,
}

pub struct ChronikNode {
    pub node_id: u32,
    pub kafka_port: u16,
    pub admin_port: u16,
    pub internal_port: u16,
    pub data_dir: tempfile::TempDir,
    pub process: Option<tokio::process::Child>,
}

impl ChronikCluster {
    pub async fn new(num_nodes: usize, test_env: &TestEnvironment) -> Result<Self> {
        let client_port_base = 19092;
        let mut nodes = Vec::with_capacity(num_nodes);
        
        for i in 0..num_nodes {
            let node_id = i as u32;
            let kafka_port = client_port_base + (i as u16);
            let admin_port = 18080 + (i as u16);
            let internal_port = 17000 + (i as u16);
            let data_dir = tempfile::tempdir()?;
            
            nodes.push(ChronikNode {
                node_id,
                kafka_port,
                admin_port,
                internal_port,
                data_dir,
                process: None,
            });
        }
        
        Ok(Self {
            nodes,
            client_port_base,
        })
    }
    
    pub async fn start(&mut self, test_env: &TestEnvironment) -> Result<()> {
        use std::process::Command;
        use tokio::process::Command as TokioCommand;
        
        // Build the project first if needed
        let output = Command::new("cargo")
            .args(&["build", "--bin", "chronik-ingest"])
            .output()?;
        
        if !output.status.success() {
            anyhow::bail!("Failed to build chronik-ingest: {}", String::from_utf8_lossy(&output.stderr));
        }
        
        // Start each node
        for node in &mut self.nodes {
            let mut cmd = TokioCommand::new("cargo");
            cmd.args(&["run", "--bin", "chronik-ingest", "--"])
                .env("CHRONIK_NODE_ID", node.node_id.to_string())
                .env("CHRONIK_KAFKA_PORT", node.kafka_port.to_string())
                .env("CHRONIK_ADMIN_PORT", node.admin_port.to_string())
                .env("CHRONIK_INTERNAL_PORT", node.internal_port.to_string())
                .env("CHRONIK_DATA_DIR", node.data_dir.path().to_str().unwrap())
                .env("CHRONIK_STORAGE_TYPE", "s3")
                .env("CHRONIK_S3_ENDPOINT", &test_env.minio_endpoint)
                .env("CHRONIK_S3_BUCKET", "chronik-test")
                .env("CHRONIK_S3_ACCESS_KEY", &test_env.minio_access_key)
                .env("CHRONIK_S3_SECRET_KEY", &test_env.minio_secret_key)
                .env("CHRONIK_S3_REGION", "us-east-1")
                .env("RUST_LOG", "chronik=debug,warn");
            
            // Add peer addresses for multi-node setup
            if self.nodes.len() > 1 {
                let peers: Vec<String> = self.nodes.iter()
                    .filter(|n| n.node_id != node.node_id)
                    .map(|n| format!("127.0.0.1:{}", n.internal_port))
                    .collect();
                cmd.env("CHRONIK_PEERS", peers.join(","));
            }
            
            let child = cmd.spawn()?;
            node.process = Some(child);
        }
        
        // Wait for nodes to be ready
        tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
        
        // Health check
        for node in &self.nodes {
            let health_url = format!("http://127.0.0.1:{}/health", node.admin_port);
            let client = reqwest::Client::new();
            
            for _ in 0..30 {
                if let Ok(resp) = client.get(&health_url).send().await {
                    if resp.status().is_success() {
                        break;
                    }
                }
                tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
            }
        }
        
        Ok(())
    }
    
    pub async fn stop(&mut self) -> Result<()> {
        for node in &mut self.nodes {
            if let Some(mut process) = node.process.take() {
                process.kill().await?;
            }
        }
        Ok(())
    }
    
    pub fn bootstrap_servers(&self) -> String {
        self.nodes.iter()
            .map(|n| format!("127.0.0.1:{}", n.kafka_port))
            .collect::<Vec<_>>()
            .join(",")
    }
}