#!/usr/bin/env python3
"""
Final Phase 3 test: Create topics on different nodes to test Raft partition assignment
"""

import subprocess
import time
import sys
import signal
from kafka import KafkaAdminClient
from kafka.admin import NewTopic

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

def main():
    print("=" * 70)
    print("Phase 3 FINAL TEST: Raft Partition Assignment")
    print("=" * 70)
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

    pid1 = start_node(1, 9092, 9192, "2@localhost:9193,3@localhost:9194", "/tmp/chronik-test-cluster/node1")
    node_pids.append(pid1)
    print(f"   ✓ Node 1 started (PID: {pid1})")

    pid2 = start_node(2, 9093, 9193, "1@localhost:9192,3@localhost:9194", "/tmp/chronik-test-cluster/node2")
    node_pids.append(pid2)
    print(f"   ✓ Node 2 started (PID: {pid2})")

    pid3 = start_node(3, 9094, 9194, "1@localhost:9192,2@localhost:9193", "/tmp/chronik-test-cluster/node3")
    node_pids.append(pid3)
    print(f"   ✓ Node 3 started (PID: {pid3})")

    # Wait for cluster to form
    print("\n3. Waiting for cluster to form (25 seconds)...")
    time.sleep(25)

    # Check if nodes are still running
    for i, pid in enumerate(node_pids, 1):
        try:
            subprocess.run(["kill", "-0", str(pid)], check=True)
            print(f"   ✓ Node {i} is running")
        except subprocess.CalledProcessError:
            print(f"   ✗ Node {i} crashed!")
            cleanup()
            sys.exit(1)

    # Step 4: Create topics on ALL 3 nodes to ensure we hit the leader
    print("\n4. Creating topics on all 3 nodes...")

    for port, node_id in [(9092, 1), (9093, 2), (9094, 3)]:
        try:
            admin_client = KafkaAdminClient(
                bootstrap_servers=f"localhost:{port}",
                request_timeout_ms=10000,
                api_version=(2, 5, 0)
            )

            topic_name = f"topic-from-node-{node_id}"
            topic = NewTopic(
                name=topic_name,
                num_partitions=3,
                replication_factor=1
            )

            admin_client.create_topics([topic], timeout_ms=10000)
            print(f"   ✓ Created '{topic_name}' on Node {node_id} (port {port})")

        except Exception as e:
            print(f"   ✗ Failed to create topic on port {port}: {e}")

    # Wait for topics to be created
    time.sleep(5)

    # Step 5: Check Raft logs for partition assignments
    print("\n5. Checking for Raft partition metadata initialization...")

    for node_id in [1, 2, 3]:
        log_file = f"/tmp/chronik-test-cluster/node{node_id}.log"

        # Check if this node is the leader
        is_leader = False
        try:
            result = subprocess.run(
                ["grep", "-i", "became leader\\|Raft leader", log_file],
                capture_output=True,
                text=True
            )
            if result.returncode == 0:
                is_leader = True
        except:
            pass

        # Check for partition metadata initialization
        found_init = False
        try:
            result = subprocess.run(
                ["grep", "-i", "Completed Raft partition.*initialization.*leader node", log_file],
                capture_output=True,
                text=True
            )
            if result.returncode == 0:
                found_init = True
                print(f"\n   Node {node_id} {'(LEADER)' if is_leader else ''}:")
                for line in result.stdout.strip().split("\n"):
                    print(f"      ✓ {line[:120]}")
        except:
            pass

        if not found_init and is_leader:
            print(f"\n   Node {node_id} (LEADER):")
            print(f"      ✗ No partition metadata initialization found (expected on leader!)")

    # Step 6: Verify topics exist on all nodes
    print("\n6. Verifying topic visibility across all nodes...")

    expected_topics = {"topic-from-node-1", "topic-from-node-2", "topic-from-node-3"}

    for port, node_id in [(9092, 1), (9093, 2), (9094, 3)]:
        try:
            admin_client = KafkaAdminClient(
                bootstrap_servers=f"localhost:{port}",
                request_timeout_ms=10000,
                api_version=(2, 5, 0)
            )
            metadata = admin_client.list_topics()
            visible_topics = set(metadata) & expected_topics

            print(f"   Node {node_id} (port {port}): {len(visible_topics)}/3 topics visible")
            if len(visible_topics) == 3:
                print(f"      ✓ All topics replicated!")
            else:
                missing = expected_topics - visible_topics
                print(f"      ✗ Missing: {missing}")

        except Exception as e:
            print(f"   ✗ Failed to list topics on port {port}: {e}")

    # Cleanup
    cleanup()

    print("\n" + "=" * 70)
    print("Phase 3 Test Complete")
    print("=" * 70)
    print("\nLogs available at: /tmp/chronik-test-cluster/node*.log")
    print("\nTo check Raft partition assignments, run:")
    print("  grep -i 'Initialized Raft partition metadata' /tmp/chronik-test-cluster/node*.log")

if __name__ == "__main__":
    try:
        main()
    except Exception as e:
        print(f"\n✗ Test failed with exception: {e}")
        import traceback
        traceback.print_exc()
        cleanup()
        sys.exit(1)
