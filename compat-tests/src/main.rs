use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use tokio::process::Command;
use tracing::{error, info, warn};
use chrono::{DateTime, Utc};
use uuid::Uuid;
use bollard::Docker;
use colored::*;
use indicatif::{ProgressBar, ProgressStyle};
use tabled::{Table, Tabled};

mod client_tests;
mod compatibility_matrix;
mod docker_utils;
mod kafka_protocol;
mod regression_tests;

#[derive(Parser)]
#[clap(name = "chronik-compat-test")]
#[clap(about = "Kafka compatibility testing for Chronik Stream")]
struct Cli {
    #[clap(subcommand)]
    command: Commands,
    
    #[clap(short, long, default_value = "./configs/test-suite.yaml")]
    config: PathBuf,
    
    #[clap(short, long)]
    verbose: bool,
}

#[derive(Subcommand)]
enum Commands {
    /// Run all compatibility tests
    Test {
        #[clap(long)]
        all: bool,
        
        #[clap(long)]
        client: Option<String>,
        
        #[clap(long)]
        category: Option<String>,
        
        #[clap(long)]
        capture: bool,
    },
    
    /// Generate compatibility matrix
    Matrix {
        #[clap(long, default_value = "./results")]
        input: PathBuf,
        
        #[clap(long, default_value = "./docs/compatibility-matrix.md")]
        output: PathBuf,
    },
    
    /// Run specific regression test
    Regression {
        test_name: String,
    },
    
    /// List available tests and clients
    List,
    
    /// Clean up test environment
    Clean,
}

#[derive(Debug, Deserialize)]
struct TestConfig {
    suite: SuiteConfig,
    chronik: ChronikConfig,
    execution: ExecutionConfig,
    test_categories: Vec<TestCategory>,
    client_configs: HashMap<String, ClientConfig>,
}

#[derive(Debug, Deserialize)]
struct SuiteConfig {
    name: String,
    version: String,
    description: String,
}

#[derive(Debug, Deserialize)]
struct ChronikConfig {
    image: String,
    build_from_source: bool,
    startup_timeout: String,
    ports: PortConfig,
}

#[derive(Debug, Deserialize)]
struct PortConfig {
    primary: u16,
    ssl: u16,
    internal: u16,
}

#[derive(Debug, Deserialize)]
struct ExecutionConfig {
    parallel: bool,
    max_parallel: usize,
    timeout: String,
    retry_on_failure: u32,
    capture_packets: bool,
    verbose: bool,
}

#[derive(Debug, Deserialize)]
struct TestCategory {
    name: String,
    description: String,
    required: bool,
    tests: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct ClientConfig {
    versions: Vec<String>,
    special_handling: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct TestResult {
    test_id: Uuid,
    timestamp: DateTime<Utc>,
    client: String,
    client_version: String,
    test_name: String,
    category: String,
    status: TestStatus,
    duration_ms: u64,
    error_message: Option<String>,
    details: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Serialize, Deserialize, PartialEq)]
enum TestStatus {
    Passed,
    Failed,
    Skipped,
    Timeout,
}

#[derive(Debug, Serialize)]
struct CompatibilityReport {
    generated_at: DateTime<Utc>,
    chronik_version: String,
    total_tests: usize,
    passed: usize,
    failed: usize,
    skipped: usize,
    clients: HashMap<String, ClientReport>,
    regression_tests: Vec<RegressionTestResult>,
}

#[derive(Debug, Serialize)]
struct ClientReport {
    name: String,
    versions_tested: Vec<String>,
    compatibility_percentage: f32,
    api_support: HashMap<String, ApiCompatibility>,
    known_issues: Vec<String>,
}

#[derive(Debug, Serialize)]
struct ApiCompatibility {
    api_name: String,
    versions_supported: Vec<i16>,
    test_results: HashMap<String, TestStatus>,
}

#[derive(Debug, Serialize)]
struct RegressionTestResult {
    test_id: String,
    description: String,
    status: TestStatus,
    affected_clients: Vec<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    
    // Initialize logging
    let log_level = if cli.verbose { "debug" } else { "info" };
    tracing_subscriber::fmt()
        .with_env_filter(log_level)
        .init();
    
    info!("🚀 Chronik Compatibility Test Framework v1.0.0");
    
    // Load test configuration
    let config = load_config(&cli.config).await?;
    
    match cli.command {
        Commands::Test { all, client, category, capture } => {
            run_tests(config, all, client, category, capture).await?;
        }
        Commands::Matrix { input, output } => {
            generate_matrix(input, output).await?;
        }
        Commands::Regression { test_name } => {
            run_regression_test(config, test_name).await?;
        }
        Commands::List => {
            list_available_tests(config).await?;
        }
        Commands::Clean => {
            clean_environment().await?;
        }
    }
    
    Ok(())
}

async fn load_config(path: &PathBuf) -> Result<TestConfig> {
    let content = tokio::fs::read_to_string(path)
        .await
        .context("Failed to read config file")?;
    
    let config: TestConfig = serde_yaml::from_str(&content)
        .context("Failed to parse config YAML")?;
    
    Ok(config)
}

async fn run_tests(
    config: TestConfig,
    all: bool,
    client: Option<String>,
    category: Option<String>,
    capture: bool,
) -> Result<()> {
    info!("🧪 Starting compatibility tests...");
    
    // Start Chronik if not already running
    ensure_chronik_running(&config.chronik).await?;
    
    // Start packet capture if requested
    if capture {
        start_packet_capture().await?;
    }
    
    let mut results = Vec::new();
    
    if all {
        // Run all tests for all clients
        for (client_name, client_config) in &config.client_configs {
            for version in &client_config.versions {
                info!("Testing {} v{}", client_name.cyan(), version);
                let client_results = run_client_tests(
                    &config,
                    client_name,
                    version,
                    None,
                ).await?;
                results.extend(client_results);
            }
        }
    } else if let Some(client_name) = client {
        // Run tests for specific client
        if let Some(client_config) = config.client_configs.get(&client_name) {
            for version in &client_config.versions {
                info!("Testing {} v{}", client_name.cyan(), version);
                let client_results = run_client_tests(
                    &config,
                    &client_name,
                    version,
                    category.as_deref(),
                ).await?;
                results.extend(client_results);
            }
        } else {
            error!("Client '{}' not found in configuration", client_name);
        }
    } else if let Some(cat) = category {
        // Run specific category for all clients
        for (client_name, client_config) in &config.client_configs {
            for version in &client_config.versions {
                info!("Testing {} v{} - Category: {}", client_name.cyan(), version, cat.yellow());
                let client_results = run_client_tests(
                    &config,
                    client_name,
                    version,
                    Some(&cat),
                ).await?;
                results.extend(client_results);
            }
        }
    }
    
    // Save results
    save_results(&results).await?;
    
    // Print summary
    print_summary(&results);
    
    Ok(())
}

async fn ensure_chronik_running(config: &ChronikConfig) -> Result<()> {
    let docker = Docker::connect_with_local_defaults()?;
    
    // Check if Chronik container is running
    let containers = docker.list_containers::<String>(None).await?;
    let chronik_running = containers.iter()
        .any(|c| c.names.as_ref()
            .map(|names| names.iter().any(|n| n.contains("chronik")))
            .unwrap_or(false));
    
    if !chronik_running {
        info!("Starting Chronik container...");
        
        if config.build_from_source {
            // Build from source
            Command::new("docker-compose")
                .args(&["build", "chronik"])
                .output()
                .await?;
        }
        
        // Start Chronik
        Command::new("docker-compose")
            .args(&["up", "-d", "chronik"])
            .output()
            .await?;
        
        // Wait for Chronik to be ready
        wait_for_chronik(config.ports.primary).await?;
    } else {
        info!("✅ Chronik is already running");
    }
    
    Ok(())
}

async fn wait_for_chronik(port: u16) -> Result<()> {
    let pb = ProgressBar::new_spinner();
    pb.set_style(
        ProgressStyle::default_spinner()
            .template("{spinner:.green} {msg}")
            .unwrap()
    );
    pb.set_message("Waiting for Chronik to be ready...");
    
    let mut attempts = 0;
    const MAX_ATTEMPTS: u32 = 30;
    
    while attempts < MAX_ATTEMPTS {
        if check_chronik_health(port).await {
            pb.finish_with_message("✅ Chronik is ready!");
            return Ok(());
        }
        
        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
        attempts += 1;
    }
    
    pb.finish_with_message("❌ Chronik failed to start");
    Err(anyhow::anyhow!("Chronik failed to become ready"))
}

async fn check_chronik_health(port: u16) -> bool {
    // Try to connect via TCP
    tokio::net::TcpStream::connect(format!("127.0.0.1:{}", port))
        .await
        .is_ok()
}

async fn run_client_tests(
    config: &TestConfig,
    client: &str,
    version: &str,
    category: Option<&str>,
) -> Result<Vec<TestResult>> {
    let mut results = Vec::new();
    
    // Filter test categories
    let categories: Vec<_> = if let Some(cat) = category {
        config.test_categories.iter()
            .filter(|c| c.name == cat)
            .collect()
    } else {
        config.test_categories.iter().collect()
    };
    
    for test_category in categories {
        if !test_category.required && category.is_none() {
            info!("Skipping optional category: {}", test_category.name);
            continue;
        }
        
        for test_name in &test_category.tests {
            let result = execute_single_test(
                client,
                version,
                &test_category.name,
                test_name,
            ).await?;
            results.push(result);
        }
    }
    
    Ok(results)
}

async fn execute_single_test(
    client: &str,
    version: &str,
    category: &str,
    test_name: &str,
) -> Result<TestResult> {
    let start = std::time::Instant::now();
    
    info!("  Running test: {}", test_name);
    
    // Execute test based on client type
    let (status, error_message) = match client {
        "librdkafka" | "confluent-kafka-python" | "confluent-kafka-go" => {
            client_tests::librdkafka::run_test(test_name).await
        }
        "kafka-python" => {
            client_tests::kafka_python::run_test(test_name).await
        }
        "java" => {
            client_tests::java::run_test(test_name).await
        }
        "sarama" => {
            client_tests::sarama::run_test(test_name).await
        }
        _ => (TestStatus::Skipped, Some(format!("Unknown client: {}", client)))
    };
    
    let duration_ms = start.elapsed().as_millis() as u64;
    
    let result = TestResult {
        test_id: Uuid::new_v4(),
        timestamp: Utc::now(),
        client: client.to_string(),
        client_version: version.to_string(),
        test_name: test_name.to_string(),
        category: category.to_string(),
        status,
        duration_ms,
        error_message,
        details: HashMap::new(),
    };
    
    // Print immediate feedback
    let status_str = match result.status {
        TestStatus::Passed => "✅ PASSED".green(),
        TestStatus::Failed => "❌ FAILED".red(),
        TestStatus::Skipped => "⏭️ SKIPPED".yellow(),
        TestStatus::Timeout => "⏱️ TIMEOUT".magenta(),
    };
    println!("    {} - {} ({}ms)", test_name, status_str, duration_ms);
    
    Ok(result)
}

async fn save_results(results: &[TestResult]) -> Result<()> {
    let timestamp = Utc::now().format("%Y%m%d_%H%M%S");
    let results_dir = format!("./results/{}", timestamp);
    tokio::fs::create_dir_all(&results_dir).await?;
    
    let results_file = format!("{}/results.json", results_dir);
    let json = serde_json::to_string_pretty(results)?;
    tokio::fs::write(results_file, json).await?;
    
    info!("💾 Results saved to {}", results_dir);
    
    Ok(())
}

fn print_summary(results: &[TestResult]) {
    println!("\n{}", "═".repeat(80).blue());
    println!("{}", "TEST SUMMARY".bold().blue());
    println!("{}", "═".repeat(80).blue());
    
    let total = results.len();
    let passed = results.iter().filter(|r| r.status == TestStatus::Passed).count();
    let failed = results.iter().filter(|r| r.status == TestStatus::Failed).count();
    let skipped = results.iter().filter(|r| r.status == TestStatus::Skipped).count();
    
    println!("Total Tests: {}", total);
    println!("  ✅ Passed:  {} ({:.1}%)", passed, (passed as f32 / total as f32) * 100.0);
    println!("  ❌ Failed:  {} ({:.1}%)", failed, (failed as f32 / total as f32) * 100.0);
    println!("  ⏭️ Skipped: {} ({:.1}%)", skipped, (skipped as f32 / total as f32) * 100.0);
    
    if failed > 0 {
        println!("\n{}", "Failed Tests:".red().bold());
        for result in results.iter().filter(|r| r.status == TestStatus::Failed) {
            println!("  - {} / {} / {}", 
                result.client.red(), 
                result.test_name.yellow(),
                result.error_message.as_ref().unwrap_or(&"Unknown error".to_string())
            );
        }
    }
}

async fn generate_matrix(input: PathBuf, output: PathBuf) -> Result<()> {
    info!("📊 Generating compatibility matrix...");
    compatibility_matrix::generate(input, output).await
}

async fn run_regression_test(config: TestConfig, test_name: String) -> Result<()> {
    info!("🔄 Running regression test: {}", test_name);
    regression_tests::run_test(&config, &test_name).await
}

async fn list_available_tests(config: TestConfig) -> Result<()> {
    println!("\n{}", "Available Test Categories:".bold().cyan());
    for category in &config.test_categories {
        println!("\n  {} ({})", category.name.yellow(), 
            if category.required { "required" } else { "optional" });
        println!("  {}", category.description);
        println!("  Tests:");
        for test in &category.tests {
            println!("    - {}", test);
        }
    }
    
    println!("\n{}", "Available Clients:".bold().cyan());
    for (client, config) in &config.client_configs {
        println!("  {} - versions: {:?}", client.green(), config.versions);
    }
    
    Ok(())
}

async fn clean_environment() -> Result<()> {
    info!("🧹 Cleaning up test environment...");
    
    Command::new("docker-compose")
        .args(&["down", "-v"])
        .output()
        .await?;
    
    info!("✅ Environment cleaned");
    
    Ok(())
}

async fn start_packet_capture() -> Result<()> {
    info!("📦 Starting packet capture...");
    
    Command::new("docker-compose")
        .args(&["up", "-d", "tcpdump"])
        .output()
        .await?;
    
    Ok(())
}

// Module stubs (would be implemented in separate files)
mod client_tests {
    pub mod librdkafka {
        use crate::TestStatus;
        pub async fn run_test(_test_name: &str) -> (TestStatus, Option<String>) {
            // Implementation would go here
            (TestStatus::Passed, None)
        }
    }
    
    pub mod kafka_python {
        use crate::TestStatus;
        pub async fn run_test(_test_name: &str) -> (TestStatus, Option<String>) {
            // Implementation would go here
            (TestStatus::Passed, None)
        }
    }
    
    pub mod java {
        use crate::TestStatus;
        pub async fn run_test(_test_name: &str) -> (TestStatus, Option<String>) {
            // Implementation would go here
            (TestStatus::Passed, None)
        }
    }
    
    pub mod sarama {
        use crate::TestStatus;
        pub async fn run_test(_test_name: &str) -> (TestStatus, Option<String>) {
            // Implementation would go here
            (TestStatus::Passed, None)
        }
    }
}

mod compatibility_matrix {
    use std::path::PathBuf;
    use anyhow::Result;
    
    pub async fn generate(_input: PathBuf, _output: PathBuf) -> Result<()> {
        // Implementation would go here
        Ok(())
    }
}

mod regression_tests {
    use crate::TestConfig;
    use anyhow::Result;
    
    pub async fn run_test(_config: &TestConfig, _test_name: &str) -> Result<()> {
        // Implementation would go here
        Ok(())
    }
}

mod docker_utils {
    // Docker utility functions
}

mod kafka_protocol {
    // Kafka protocol helpers
}