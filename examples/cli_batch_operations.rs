//! Example demonstrating batch operations with the Chronik CLI

use std::fs;
use std::process::Command;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("Chronik CLI Batch Operations Demo\n");
    
    // First, generate an example batch file
    println!("1. Generating example batch file...");
    let output = Command::new("chronik-ctl")
        .args(&["batch", "example", "--output", "batch-demo.yaml"])
        .output()?;
    
    if output.status.success() {
        println!("   ✓ Example batch file created: batch-demo.yaml");
    } else {
        eprintln!("   ✗ Failed to generate example: {}", String::from_utf8_lossy(&output.stderr));
    }
    
    // Show the generated batch file
    println!("\n2. Generated batch file contents:");
    let content = fs::read_to_string("batch-demo.yaml")?;
    println!("{}", content);
    
    // Create a custom batch file for our demo
    println!("\n3. Creating custom batch operations file...");
    let custom_batch = r#"version: "1.0"
description: "Demo batch operations for Chronik Stream"
operations:
  # Create topics
  - type: create_topic
    name: events-stream
    partitions: 10
    replication_factor: 3
    config:
      retention_ms: 604800000  # 7 days
      segment_bytes: 1073741824  # 1GB
      min_insync_replicas: 2
      compression_type: snappy
  
  - type: create_topic
    name: logs-stream
    partitions: 5
    replication_factor: 2
    config:
      retention_ms: 259200000  # 3 days
      segment_bytes: 536870912  # 512MB
      min_insync_replicas: 1
      compression_type: lz4
  
  - type: create_topic
    name: metrics-stream
    partitions: 20
    replication_factor: 3
  
  # Create users
  - type: create_user
    username: event-producer
    password: secure-pass-123
    roles:
      - producer
  
  - type: create_user
    username: analytics-consumer
    password: secure-pass-456
    roles:
      - consumer
      - viewer
  
  # Create consumer groups
  - type: create_consumer_group
    group_id: real-time-analytics
    topics:
      - events-stream
      - metrics-stream
  
  - type: create_consumer_group
    group_id: log-processors
    topics:
      - logs-stream
  
  # Update topic configurations
  - type: update_topic_config
    name: events-stream
    config:
      min_insync_replicas: 3
      retention_ms: 1209600000  # 14 days
"#;
    
    fs::write("custom-batch.yaml", custom_batch)?;
    println!("   ✓ Created custom-batch.yaml");
    
    // Demonstrate dry run
    println!("\n4. Running batch operations in DRY RUN mode...");
    let output = Command::new("chronik-ctl")
        .args(&["batch", "execute", "custom-batch.yaml", "--dry-run"])
        .output()?;
    
    println!("   Output:\n{}", String::from_utf8_lossy(&output.stdout));
    
    // Demonstrate actual execution with continue-on-error
    println!("\n5. Execute batch operations (with --continue-on-error):");
    println!("   Command: chronik-ctl batch execute custom-batch.yaml --continue-on-error");
    println!("   This would execute all operations, continuing even if some fail.");
    
    // Demonstrate different output formats
    println!("\n6. Batch operations support different output formats:");
    println!("   - JSON: chronik-ctl batch execute custom-batch.yaml --output json");
    println!("   - YAML: chronik-ctl batch execute custom-batch.yaml --output yaml");
    println!("   - Table: chronik-ctl batch execute custom-batch.yaml --output table (default)");
    
    // Show how to list topics with YAML output
    println!("\n7. List topics in YAML format:");
    println!("   Command: chronik-ctl topic list --output yaml");
    
    // Clean up
    fs::remove_file("batch-demo.yaml").ok();
    fs::remove_file("custom-batch.yaml").ok();
    
    println!("\n✅ Demo completed successfully!");
    
    Ok(())
}