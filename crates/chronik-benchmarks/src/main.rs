//! Benchmark runner main executable

use anyhow::Result;
use chronik_benchmarks::{BenchmarkHarness, BenchmarkSuite, BenchmarkScenario, EnvironmentInfo};
use clap::{Arg, Command};
use tracing::{info, Level};
use tracing_subscriber;

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_max_level(Level::INFO)
        .init();
    
    // Parse command line arguments
    let matches = Command::new("chronik-benchmarks")
        .version(env!("CARGO_PKG_VERSION"))
        .author("Chronik Stream Team")
        .about("Comprehensive performance benchmarking suite for Chronik Stream")
        .arg(
            Arg::new("scenario")
                .short('s')
                .long("scenario")
                .value_name("SCENARIO")
                .help("Benchmark scenario to run")
                .value_parser(["light", "medium", "heavy", "all"])
                .default_value("all")
        )
        .arg(
            Arg::new("output")
                .short('o')
                .long("output")
                .value_name("DIR")
                .help("Output directory for results")
                .default_value("./benchmark-results")
        )
        .arg(
            Arg::new("threads")
                .short('t')
                .long("threads")
                .value_name("COUNT")
                .help("Number of threads to use")
                .value_parser(clap::value_parser!(usize))
        )
        .arg(
            Arg::new("duration")
                .short('d')
                .long("duration")
                .value_name("SECONDS")
                .help("Duration to run each benchmark in seconds")
                .value_parser(clap::value_parser!(u64))
        )
        .arg(
            Arg::new("skip-compatibility")
                .long("skip-compatibility")
                .help("Skip compatibility tests")
                .action(clap::ArgAction::SetTrue)
        )
        .arg(
            Arg::new("skip-mixed")
                .long("skip-mixed")
                .help("Skip mixed workload tests")
                .action(clap::ArgAction::SetTrue)
        )
        .arg(
            Arg::new("enable-scalability")
                .long("enable-scalability")
                .help("Enable scalability tests (resource intensive)")
                .action(clap::ArgAction::SetTrue)
        )
        .arg(
            Arg::new("enable-reliability")
                .long("enable-reliability")
                .help("Enable reliability tests (requires special setup)")
                .action(clap::ArgAction::SetTrue)
        )
        .get_matches();
    
    // Collect environment information
    let env_info = EnvironmentInfo::collect();
    info!("Environment: {} cores, {}MB RAM, {}", 
          env_info.cpu_cores, env_info.total_memory_mb, env_info.os);
    
    // Create benchmark suite configuration
    let scenario_arg = matches.get_one::<String>("scenario").unwrap();
    let scenarios = match scenario_arg.as_str() {
        "light" => vec![BenchmarkScenario::Light],
        "medium" => vec![BenchmarkScenario::Medium],
        "heavy" => vec![BenchmarkScenario::Heavy],
        "all" => vec![
            BenchmarkScenario::Light,
            BenchmarkScenario::Medium,
            BenchmarkScenario::Heavy,
        ],
        _ => {
            eprintln!("Invalid scenario: {}", scenario_arg);
            std::process::exit(1);
        }
    };
    
    let mut suite = BenchmarkSuite {
        name: "Chronik Stream Performance Benchmark".to_string(),
        scenarios,
        run_kafka_compatibility: !matches.get_flag("skip-compatibility"),
        run_elasticsearch_compatibility: !matches.get_flag("skip-compatibility"),
        run_mixed_workload: !matches.get_flag("skip-mixed"),
        run_scalability: matches.get_flag("enable-scalability"),
        run_reliability: matches.get_flag("enable-reliability"),
        output_dir: matches.get_one::<String>("output").unwrap().clone(),
    };
    
    // Apply custom parameters if provided
    if let Some(threads) = matches.get_one::<usize>("threads") {
        for scenario in &mut suite.scenarios {
            if let BenchmarkScenario::Custom(ref mut config) = scenario {
                config.threads = *threads;
            }
        }
    }
    
    if let Some(duration_secs) = matches.get_one::<u64>("duration") {
        for scenario in &mut suite.scenarios {
            if let BenchmarkScenario::Custom(ref mut config) = scenario {
                config.duration = std::time::Duration::from_secs(*duration_secs);
            }
        }
    }
    
    info!("Starting benchmark suite with {} scenarios", suite.scenarios.len());
    
    // Create and run benchmark harness
    let mut harness = BenchmarkHarness::new(suite);
    match harness.run_all().await {
        Ok(()) => {
            info!("Benchmark suite completed successfully");
            
            // Print final summary
            println!("\n{}", "=".repeat(80));
            println!("  BENCHMARK SUITE COMPLETED");
            println!("{}", "=".repeat(80));
            println!("Total benchmarks run: {}", harness.results().len());
            
            // Calculate overall statistics
            let results = harness.results();
            if !results.is_empty() {
                let avg_throughput: f64 = results.iter()
                    .map(|r| r.throughput_msg_per_sec)
                    .sum::<f64>() / results.len() as f64;
                
                let avg_latency: f64 = results.iter()
                    .map(|r| r.avg_latency_ms)
                    .sum::<f64>() / results.len() as f64;
                
                println!("Average throughput: {:.0} msg/sec", avg_throughput);
                println!("Average latency: {:.2} ms", avg_latency);
                
                let total_errors: u64 = results.iter().map(|r| r.error_count).sum();
                if total_errors > 0 {
                    println!("Total errors: {}", total_errors);
                }
            }
            
            println!("Results saved to: {}", matches.get_one::<String>("output").unwrap());
            println!("{}", "=".repeat(80));
        }
        Err(e) => {
            eprintln!("Benchmark suite failed: {}", e);
            std::process::exit(1);
        }
    }
    
    Ok(())
}