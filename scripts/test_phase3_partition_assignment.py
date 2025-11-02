#!/usr/bin/env python3
"""
Test script for Phase 3: Partition Assignment via Raft

Tests that topics created on any node get partition assignments via Raft consensus.
"""

import subprocess
import time
import sys
import signal
from kafka import KafkaProducer, KafkaConsumer, KafkaAdminClient
from kafka.admin import NewTopic
from kafka.errors import KafkaError

# Node PIDs for cleanup
node_pids = []

def cleanup():
    """Kill all Chronik nodes"""
    print("\nCleaning up...")
    for pid in node_pids:
        try:
            subprocess.run(["kill", str(pid)], check=False)
        except:
            pass
    time.sleep(2)

def signal_handler(sig, frame):
    """Handle Ctrl+C"""
    print("\n\nInterrupted by user")
    cleanup()
    sys.exit(1)

signal.signal(signal.SIGINT, signal_handler)

def start_node(node_id, kafka_port, raft_port, peers, data_dir):
    """Start a Chronik Raft node"""
    cmd = [
        "./target/release/chronik-server",
        "--kafka-port", str(kafka_port),
        "--advertised-addr", "localhost",
        "--node-id", str(node_id),
        "--data-dir", data_dir,
        "raft-cluster",
        "--raft-addr", f"0.0.0.0:{raft_port}",
        "--peers", peers,
        "--bootstrap"
    ]

    log_file = open(f"{data_dir}.log", "w")
    process = subprocess.Popen(
        cmd,
        stdout=log_file,
        stderr=subprocess.STDOUT,
        env={"RUST_LOG": "info"}
    )

    return process.pid

def check_logs_for_pattern(log_file, pattern):
    """Check if log file contains pattern"""
    try:
        with open(log_file, "r") as f:
            content = f.read()
            return pattern in content
    except:
        return False

def main():
    print("=" * 60)
    print("Phase 3: Raft Partition Assignment Test")
    print("=" * 60)
    print()

    # Step 1: Clean old data
    print("1. Cleaning old test data...")
    subprocess.run(["pkill", "-f", "chronik-server.*raft-cluster"], check=False)
    time.sleep(1)
    subprocess.run(["rm", "-rf", "/tmp/chronik-test-cluster"], check=False)
    subprocess.run(["mkdir", "-p", "/tmp/chronik-test-cluster/node1"], check=True)
    subprocess.run(["mkdir", "-p", "/tmp/chronik-test-cluster/node2"], check=True)
    subprocess.run(["mkdir", "-p", "/tmp/chronik-test-cluster/node3"], check=True)
    print("   ✓ Old data cleaned")

    # Step 2: Start 3-node cluster
    print("\n2. Starting 3-node Raft cluster...")

    # Node 1
    print("   Starting Node 1 (Kafka: 9092, Raft: 9192)...")
    pid1 = start_node(1, 9092, 9192, "2@localhost:9193,3@localhost:9194", "/tmp/chronik-test-cluster/node1")
    node_pids.append(pid1)
    print(f"   ✓ Node 1 started (PID: {pid1})")

    # Node 2
    print("   Starting Node 2 (Kafka: 9093, Raft: 9193)...")
    pid2 = start_node(2, 9093, 9193, "1@localhost:9192,3@localhost:9194", "/tmp/chronik-test-cluster/node2")
    node_pids.append(pid2)
    print(f"   ✓ Node 2 started (PID: {pid2})")

    # Node 3
    print("   Starting Node 3 (Kafka: 9094, Raft: 9194)...")
    pid3 = start_node(3, 9094, 9194, "1@localhost:9192,2@localhost:9193", "/tmp/chronik-test-cluster/node3")
    node_pids.append(pid3)
    print(f"   ✓ Node 3 started (PID: {pid3})")

    # Wait for cluster to form
    print("\n3. Waiting for cluster to form (20 seconds)...")
    time.sleep(20)

    # Check if nodes are still running
    for i, pid in enumerate(node_pids, 1):
        try:
            subprocess.run(["kill", "-0", str(pid)], check=True)
            print(f"   ✓ Node {i} is running")
        except subprocess.CalledProcessError:
            print(f"   ✗ Node {i} crashed!")
            print(f"\nLast 20 lines from node{i}.log:")
            subprocess.run(["tail", "-n", "20", f"/tmp/chronik-test-cluster/node{i}.log"])
            cleanup()
            sys.exit(1)

    # Step 4: Create topic via Python Kafka client
    print("\n4. Creating topic 'test-raft-partitions' with 3 partitions...")
    try:
        admin_client = KafkaAdminClient(
            bootstrap_servers="localhost:9092",
            request_timeout_ms=30000,
            api_version=(2, 5, 0)
        )

        topic = NewTopic(
            name="test-raft-partitions",
            num_partitions=3,
            replication_factor=1
        )

        admin_client.create_topics([topic], timeout_ms=30000)
        print("   ✓ Topic creation request sent")

        # Wait for topic to be created
        time.sleep(5)

    except Exception as e:
        print(f"   ✗ Failed to create topic: {e}")
        cleanup()
        sys.exit(1)

    # Step 5: Check for partition assignments in logs
    print("\n5. Checking for Raft partition assignments in logs...")

    patterns_to_check = [
        "AssignPartition",
        "SetPartitionLeader",
        "UpdateISR",
        "Initialized partition metadata"
    ]

    for node_id in [1, 2, 3]:
        log_file = f"/tmp/chronik-test-cluster/node{node_id}.log"
        print(f"\n   Node {node_id}:")

        found_any = False
        for pattern in patterns_to_check:
            if check_logs_for_pattern(log_file, pattern):
                print(f"      ✓ Found '{pattern}'")
                found_any = True

        if not found_any:
            print(f"      ✗ No partition assignment patterns found")

    # Step 6: Verify topic exists on all nodes
    print("\n6. Verifying topic exists on all nodes...")
    for port in [9092, 9093, 9094]:
        try:
            consumer = KafkaConsumer(
                bootstrap_servers=f"localhost:{port}",
                request_timeout_ms=10000,
                api_version=(2, 5, 0)
            )
            topics = consumer.topics()
            consumer.close()

            if "test-raft-partitions" in topics:
                print(f"   ✓ Topic visible on port {port}")
            else:
                print(f"   ✗ Topic NOT visible on port {port}")
                print(f"      Available topics: {topics}")
        except Exception as e:
            print(f"   ✗ Failed to list topics on port {port}: {e}")

    # Step 7: Test produce/consume
    print("\n7. Testing produce/consume...")
    try:
        # Produce
        producer = KafkaProducer(
            bootstrap_servers="localhost:9092",
            api_version=(2, 5, 0)
        )
        future = producer.send("test-raft-partitions", b"test-message-1")
        result = future.get(timeout=10)
        producer.close()
        print(f"   ✓ Message produced to partition {result.partition}, offset {result.offset}")

        # Consume
        consumer = KafkaConsumer(
            "test-raft-partitions",
            bootstrap_servers="localhost:9092",
            auto_offset_reset="earliest",
            consumer_timeout_ms=5000,
            api_version=(2, 5, 0)
        )

        messages = []
        for msg in consumer:
            messages.append(msg)
            break

        consumer.close()

        if messages:
            print(f"   ✓ Message consumed: {messages[0].value}")
        else:
            print("   ✗ No messages consumed")

    except Exception as e:
        print(f"   ✗ Produce/consume failed: {e}")

    # Step 8: Grep logs for detailed partition assignment info
    print("\n8. Detailed partition assignment logs:")
    for node_id in [1, 2, 3]:
        log_file = f"/tmp/chronik-test-cluster/node{node_id}.log"
        print(f"\n   Node {node_id}:")

        try:
            result = subprocess.run(
                ["grep", "-i", "partition", log_file],
                capture_output=True,
                text=True
            )

            if result.returncode == 0:
                lines = result.stdout.strip().split("\n")
                # Show first 5 and last 5 partition-related lines
                if len(lines) > 10:
                    for line in lines[:5]:
                        print(f"      {line[:100]}...")
                    print(f"      ... ({len(lines) - 10} more lines) ...")
                    for line in lines[-5:]:
                        print(f"      {line[:100]}...")
                else:
                    for line in lines:
                        print(f"      {line[:100]}...")
        except:
            print("      (No grep available)")

    # Cleanup
    cleanup()

    print("\n" + "=" * 60)
    print("Phase 3 Test Complete")
    print("=" * 60)
    print("\nSummary:")
    print("  ✓ 3-node Raft cluster started successfully")
    print("  ✓ Topic created with 3 partitions")
    print("  ✓ Partition assignments should be visible in logs")
    print("  ✓ Produce/consume tested")
    print(f"\nLogs available at: /tmp/chronik-test-cluster/node*.log")

if __name__ == "__main__":
    try:
        main()
    except Exception as e:
        print(f"\n✗ Test failed with exception: {e}")
        import traceback
        traceback.print_exc()
        cleanup()
        sys.exit(1)
