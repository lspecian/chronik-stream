//! Demonstrates partition assignment functionality for Raft clustering.

use chronik_common::partition_assignment::PartitionAssignment;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("=== Chronik Raft Partition Assignment Demo ===\n");

    // Example 1: 3 nodes, 9 partitions, RF=3
    println!("Example 1: 3 nodes, 9 partitions, RF=3");
    println!("==========================================");
    let mut assignment = PartitionAssignment::new();
    let nodes = vec![1, 2, 3];
    assignment.add_topic("orders", 9, 3, &nodes)?;

    for partition in 0..9 {
        let replicas = assignment.get_replicas("orders", partition).unwrap();
        let leader = assignment.get_leader("orders", partition).unwrap();
        println!(
            "  Partition {}: replicas={:?}, leader={}",
            partition, replicas, leader
        );
    }

    // Verify leadership balance
    let mut leader_counts = std::collections::HashMap::new();
    for partition in 0..9 {
        let leader = assignment.get_leader("orders", partition).unwrap();
        *leader_counts.entry(leader).or_insert(0) += 1;
    }
    println!("\n  Leadership distribution:");
    for (node, count) in &leader_counts {
        println!("    Node {}: {} partitions", node, count);
    }

    // Example 2: 5 nodes, 10 partitions, RF=3
    println!("\nExample 2: 5 nodes, 10 partitions, RF=3");
    println!("==========================================");
    let mut assignment2 = PartitionAssignment::new();
    let nodes2 = vec![1, 2, 3, 4, 5];
    assignment2.add_topic("events", 10, 3, &nodes2)?;

    for partition in 0..10 {
        let replicas = assignment2.get_replicas("events", partition).unwrap();
        let leader = assignment2.get_leader("events", partition).unwrap();
        println!(
            "  Partition {}: replicas={:?}, leader={}",
            partition, replicas, leader
        );
    }

    let mut leader_counts2 = std::collections::HashMap::new();
    for partition in 0..10 {
        let leader = assignment2.get_leader("events", partition).unwrap();
        *leader_counts2.entry(leader).or_insert(0) += 1;
    }
    println!("\n  Leadership distribution:");
    for (node, count) in &leader_counts2 {
        println!("    Node {}: {} partitions", node, count);
    }

    // Example 3: JSON serialization
    println!("\nExample 3: JSON Serialization");
    println!("==============================");
    let mut assignment3 = PartitionAssignment::new();
    assignment3.add_topic("test", 3, 2, &vec![1, 2, 3])?;

    let json = assignment3.to_json()?;
    println!("JSON representation:\n{}", json);

    // Round-trip test
    let restored = PartitionAssignment::from_json(&json)?;
    println!(
        "\nRestored partition 0: replicas={:?}, leader={}",
        restored.get_replicas("test", 0).unwrap(),
        restored.get_leader("test", 0).unwrap()
    );

    // Example 4: Leader failover
    println!("\nExample 4: Leader Failover");
    println!("==========================");
    let mut assignment4 = PartitionAssignment::new();
    assignment4.add_topic("users", 3, 3, &vec![1, 2, 3])?;

    println!("Initial state:");
    for partition in 0..3 {
        let replicas = assignment4.get_replicas("users", partition).unwrap();
        let leader = assignment4.get_leader("users", partition).unwrap();
        println!(
            "  Partition {}: replicas={:?}, leader={}",
            partition, replicas, leader
        );
    }

    // Simulate leader failure on partition 0 (leader=1 fails, promote to 2)
    println!("\nSimulate leader failure on partition 0 (1 → 2):");
    assignment4.set_leader("users", 0, 2)?;
    let replicas = assignment4.get_replicas("users", 0).unwrap();
    let leader = assignment4.get_leader("users", 0).unwrap();
    println!(
        "  Partition 0: replicas={:?}, leader={} (changed)",
        replicas, leader
    );

    // Validate assignment
    println!("\nValidating assignment...");
    assignment4.validate()?;
    println!("  ✓ Assignment is valid");

    println!("\n=== Demo Complete ===");
    Ok(())
}
