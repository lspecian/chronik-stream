//! Benchmark harness and test orchestration

use crate::{
    BenchmarkConfig, BenchmarkResults, BenchmarkScenario,
    IngestThroughputBenchmark, SearchLatencyBenchmark, EndToEndBenchmark,
    KafkaCompatibilityScenario, ElasticsearchCompatibilityScenario,
    MixedWorkloadScenario, ScalabilityScenario, ReliabilityScenario,
};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::{Duration, Instant};
use tracing::{info, warn, error};

/// Benchmark suite configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchmarkSuite {
    /// Name of the benchmark suite
    pub name: String,
    /// Scenarios to run
    pub scenarios: Vec<BenchmarkScenario>,
    /// Whether to run compatibility tests
    pub run_kafka_compatibility: bool,
    pub run_elasticsearch_compatibility: bool,
    /// Whether to run mixed workload tests
    pub run_mixed_workload: bool,
    /// Whether to run scalability tests
    pub run_scalability: bool,
    /// Whether to run reliability tests
    pub run_reliability: bool,
    /// Output directory for reports
    pub output_dir: String,
}

impl Default for BenchmarkSuite {
    fn default() -> Self {
        Self {
            name: "Chronik Stream Benchmark Suite".to_string(),
            scenarios: vec![
                BenchmarkScenario::Light,
                BenchmarkScenario::Medium,
                BenchmarkScenario::Heavy,
            ],
            run_kafka_compatibility: true,
            run_elasticsearch_compatibility: true,
            run_mixed_workload: true,
            run_scalability: false, // Disabled by default as it's resource intensive
            run_reliability: false, // Disabled by default as it requires special setup
            output_dir: "./benchmark-results".to_string(),
        }
    }
}

/// Benchmark harness for orchestrating and running benchmarks
pub struct BenchmarkHarness {
    suite: BenchmarkSuite,
    results: Vec<BenchmarkResults>,
}

impl BenchmarkHarness {
    /// Create a new benchmark harness
    pub fn new(suite: BenchmarkSuite) -> Self {
        Self {
            suite,
            results: Vec::new(),
        }
    }
    
    /// Run all benchmarks in the suite
    pub async fn run_all(&mut self) -> Result<()> {
        info!("Starting benchmark suite: {}", self.suite.name);
        let suite_start = Instant::now();
        
        // Create output directory
        std::fs::create_dir_all(&self.suite.output_dir)?;
        
        // Run core benchmarks for each scenario
        for scenario in &self.suite.scenarios {
            info!("Running scenario: {}", scenario.name());
            
            // Run ingest throughput benchmark
            if let Ok(result) = self.run_ingest_throughput(scenario.clone()).await {
                self.results.push(result);
            }
            
            // Run search latency benchmark
            if let Ok(result) = self.run_search_latency(scenario.clone()).await {
                self.results.push(result);
            }
            
            // Run end-to-end benchmark
            if let Ok(result) = self.run_end_to_end(scenario.clone()).await {
                self.results.push(result);
            }
        }
        
        // Run compatibility tests
        if self.suite.run_kafka_compatibility {
            info!("Running Kafka compatibility tests");
            for scenario in &self.suite.scenarios {
                if let Ok(result) = self.run_kafka_compatibility(scenario.clone()).await {
                    self.results.push(result);
                }
            }
        }
        
        if self.suite.run_elasticsearch_compatibility {
            info!("Running Elasticsearch compatibility tests");
            for scenario in &self.suite.scenarios {
                if let Ok(result) = self.run_elasticsearch_compatibility(scenario.clone()).await {
                    self.results.push(result);
                }
            }
        }
        
        // Run mixed workload tests
        if self.suite.run_mixed_workload {
            info!("Running mixed workload tests");
            for scenario in &self.suite.scenarios {
                if let Ok(result) = self.run_mixed_workload(scenario.clone()).await {
                    self.results.push(result);
                }
            }
        }
        
        // Run scalability tests
        if self.suite.run_scalability {
            info!("Running scalability tests");
            if let Ok(results) = self.run_scalability().await {
                self.results.extend(results);
            }
        }
        
        // Run reliability tests
        if self.suite.run_reliability {
            info!("Running reliability tests");
            if let Ok(results) = self.run_reliability().await {
                self.results.extend(results);
            }
        }
        
        let suite_duration = suite_start.elapsed();
        info!("Benchmark suite completed in {:?}", suite_duration);
        
        // Generate reports
        self.generate_reports().await?;
        
        Ok(())
    }
    
    /// Run ingest throughput benchmark for a scenario
    async fn run_ingest_throughput(&self, scenario: BenchmarkScenario) -> Result<BenchmarkResults> {
        let config = scenario.config();
        let mut benchmark = IngestThroughputBenchmark::new(config);
        
        benchmark.setup().await?;
        let mut results = benchmark.run().await?;
        results.name = format!("Ingest Throughput - {}", scenario.name());
        
        Ok(results)
    }
    
    /// Run search latency benchmark for a scenario
    async fn run_search_latency(&self, scenario: BenchmarkScenario) -> Result<BenchmarkResults> {
        let config = scenario.config();
        let mut benchmark = SearchLatencyBenchmark::new(config);
        
        benchmark.setup().await?;
        let mut results = benchmark.run().await?;
        results.name = format!("Search Latency - {}", scenario.name());
        
        Ok(results)
    }
    
    /// Run end-to-end benchmark for a scenario
    async fn run_end_to_end(&self, scenario: BenchmarkScenario) -> Result<BenchmarkResults> {
        let config = scenario.config();
        let mut benchmark = EndToEndBenchmark::new(config);
        
        benchmark.setup().await?;
        let mut results = benchmark.run().await?;
        results.name = format!("End-to-End - {}", scenario.name());
        
        Ok(results)
    }
    
    /// Run Kafka compatibility test for a scenario
    async fn run_kafka_compatibility(&self, scenario: BenchmarkScenario) -> Result<BenchmarkResults> {
        let benchmark = KafkaCompatibilityScenario::new(scenario);
        benchmark.run().await
    }
    
    /// Run Elasticsearch compatibility test for a scenario
    async fn run_elasticsearch_compatibility(&self, scenario: BenchmarkScenario) -> Result<BenchmarkResults> {
        let benchmark = ElasticsearchCompatibilityScenario::new(scenario);
        benchmark.run().await
    }
    
    /// Run mixed workload test for a scenario
    async fn run_mixed_workload(&self, scenario: BenchmarkScenario) -> Result<BenchmarkResults> {
        let benchmark = MixedWorkloadScenario::new(scenario, 0.7); // 70% ingest, 30% search
        benchmark.run().await
    }
    
    /// Run scalability tests
    async fn run_scalability(&self) -> Result<Vec<BenchmarkResults>> {
        let base_config = BenchmarkScenario::Medium.config();
        let scale_factors = vec![1.0, 2.0, 4.0, 8.0];
        
        let benchmark = ScalabilityScenario::new(base_config, scale_factors);
        benchmark.run().await
    }
    
    /// Run reliability tests
    async fn run_reliability(&self) -> Result<Vec<BenchmarkResults>> {
        let config = BenchmarkScenario::Medium.config();
        let fault_types = vec![
            crate::scenarios::FaultType::NetworkPartition,
            crate::scenarios::FaultType::NodeFailure,
            crate::scenarios::FaultType::MemoryPressure,
        ];
        
        let benchmark = ReliabilityScenario::new(config, fault_types);
        benchmark.run().await
    }
    
    /// Generate benchmark reports
    async fn generate_reports(&self) -> Result<()> {
        info!("Generating benchmark reports");
        
        // Generate summary report
        self.generate_summary_report().await?;
        
        // Generate detailed JSON report
        self.generate_json_report().await?;
        
        // Generate CSV report for analysis
        self.generate_csv_report().await?;
        
        // Print summary to console
        self.print_summary();
        
        Ok(())
    }
    
    /// Generate summary report
    async fn generate_summary_report(&self) -> Result<()> {
        let path = format!("{}/summary.txt", self.suite.output_dir);
        let mut content = String::new();
        
        content.push_str(&format!("# {} - Summary Report\n\n", self.suite.name));
        content.push_str(&format!("Generated: {}\n", chrono::Utc::now().format("%Y-%m-%d %H:%M:%S UTC")));
        content.push_str(&format!("Total benchmarks: {}\n\n", self.results.len()));
        
        // Group results by category
        let mut categories: HashMap<String, Vec<&BenchmarkResults>> = HashMap::new();
        for result in &self.results {
            let category = result.name.split(" - ").next().unwrap_or("Other").to_string();
            categories.entry(category).or_default().push(result);
        }
        
        for (category, results) in categories {
            content.push_str(&format!("## {}\n\n", category));
            
            for result in results {
                content.push_str(&format!("### {}\n", result.name));
                content.push_str(&format!("- Throughput: {:.0} msg/sec ({:.1} MB/sec)\n", 
                                         result.throughput_msg_per_sec, result.throughput_mb_per_sec));
                content.push_str(&format!("- Latency: avg={:.1}ms, p95={:.1}ms, p99={:.1}ms\n",
                                         result.avg_latency_ms, result.p95_latency_ms, result.p99_latency_ms));
                content.push_str(&format!("- Resources: CPU={:.1}%, Memory={:.0}MB\n",
                                         result.cpu_usage_percent, result.memory_usage_mb));
                if result.error_count > 0 {
                    content.push_str(&format!("- Errors: {}\n", result.error_count));
                }
                content.push_str("\n");
            }
        }
        
        tokio::fs::write(path, content).await?;
        Ok(())
    }
    
    /// Generate detailed JSON report
    async fn generate_json_report(&self) -> Result<()> {
        let path = format!("{}/detailed_results.json", self.suite.output_dir);
        let json = serde_json::to_string_pretty(&self.results)?;
        tokio::fs::write(path, json).await?;
        Ok(())
    }
    
    /// Generate CSV report for analysis
    async fn generate_csv_report(&self) -> Result<()> {
        let path = format!("{}/results.csv", self.suite.output_dir);
        let mut content = String::new();
        
        // CSV header
        content.push_str("name,throughput_msg_per_sec,throughput_mb_per_sec,avg_latency_ms,p50_latency_ms,p95_latency_ms,p99_latency_ms,max_latency_ms,total_messages,total_bytes,duration_ms,error_count,cpu_usage_percent,memory_usage_mb\n");
        
        // CSV data
        for result in &self.results {
            content.push_str(&format!("{},{},{},{},{},{},{},{},{},{},{},{},{},{}\n",
                result.name,
                result.throughput_msg_per_sec,
                result.throughput_mb_per_sec,
                result.avg_latency_ms,
                result.p50_latency_ms,
                result.p95_latency_ms,
                result.p99_latency_ms,
                result.max_latency_ms,
                result.total_messages,
                result.total_bytes,
                result.duration.as_millis(),
                result.error_count,
                result.cpu_usage_percent,
                result.memory_usage_mb,
            ));
        }
        
        tokio::fs::write(path, content).await?;
        Ok(())
    }
    
    /// Print summary to console
    fn print_summary(&self) {
        println!("\n{}", "=".repeat(80));
        println!("  {} - RESULTS SUMMARY", self.suite.name.to_uppercase());
        println!("{}", "=".repeat(80));
        
        for result in &self.results {
            result.print();
        }
        
        println!("\n{}", "=".repeat(80));
        println!("Reports generated in: {}", self.suite.output_dir);
        println!("{}", "=".repeat(80));
    }
    
    /// Get all results
    pub fn results(&self) -> &[BenchmarkResults] {
        &self.results
    }
}