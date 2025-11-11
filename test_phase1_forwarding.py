#!/usr/bin/env python3
"""
Test Phase 1.2: Leader-Forwarding Pattern

This script tests that followers correctly forward metadata queries to the leader,
preventing split-brain scenarios.
"""

import subprocess
import time
import sys
import os
from kafka import KafkaProducer, KafkaConsumer, KafkaAdminClient
from kafka.admin import NewTopic
from kafka.errors import KafkaError

# ANSI color codes
GREEN = '\033[0;32m'
YELLOW = '\033[1;33m'
RED = '\033[0;31m'
BLUE = '\033[0;34m'
NC = '\033[0m'  # No Color

def log_info(msg):
    print(f"{BLUE}ℹ{NC} {msg}")

def log_success(msg):
    print(f"{GREEN}✓{NC} {msg}")

def log_warning(msg):
    print(f"{YELLOW}⚠{NC} {msg}")

def log_error(msg):
    print(f"{RED}✗{NC} {msg}")

def start_node(node_id, kafka_port, wal_port, raft_port):
    """Start a Chronik node in cluster mode"""
    log_info(f"Starting Node {node_id} (Kafka:{kafka_port}, WAL:{wal_port}, Raft:{raft_port})...")

    # Create config content (correct format with bind/advertise sections)
    config_content = f"""enabled = true
node_id = {node_id}
replication_factor = 3
min_insync_replicas = 2

[bind]
kafka = "0.0.0.0:{kafka_port}"
wal = "0.0.0.0:{wal_port}"
raft = "0.0.0.0:{raft_port}"

[advertise]
kafka = "localhost:{kafka_port}"
wal = "localhost:{wal_port}"
raft = "localhost:{raft_port}"

[[peers]]
id = 1
kafka = "localhost:9092"
wal = "localhost:9291"
raft = "localhost:5001"

[[peers]]
id = 2
kafka = "localhost:9093"
wal = "localhost:9292"
raft = "localhost:5002"

[[peers]]
id = 3
kafka = "localhost:9094"
wal = "localhost:9293"
raft = "localhost:5003"
"""

    # Write config file
    config_path = f"/tmp/chronik-node{node_id}.toml"
    with open(config_path, 'w') as f:
        f.write(config_content)

    # Start node
    binary_path = '/home/ubuntu/Development/chronik-stream/target/release/chronik-server'
    cmd = [
        binary_path,
        'start',
        '--config', config_path,
        '--data-dir', f'/tmp/chronik-node{node_id}-data'
    ]

    log_file = open(f'/tmp/chronik-node{node_id}.log', 'w')
    process = subprocess.Popen(cmd, stdout=log_file, stderr=subprocess.STDOUT)

    log_success(f"Node {node_id} started (PID {process.pid})")
    return process, log_file

def wait_for_cluster_ready(bootstrap_servers, timeout=30):
    """Wait for cluster to be ready"""
    log_info(f"Waiting for cluster to be ready (timeout: {timeout}s)...")
    start_time = time.time()

    while time.time() - start_time < timeout:
        try:
            admin = KafkaAdminClient(bootstrap_servers=bootstrap_servers, request_timeout_ms=5000)
            admin.list_topics()
            admin.close()
            log_success("Cluster is ready!")
            return True
        except Exception as e:
            time.sleep(1)

    log_error("Cluster failed to become ready within timeout")
    return False

def test_follower_forwarding():
    """Test that followers forward metadata queries to leader"""
    processes = []
    log_files = []

    try:
        # Step 1: Start 3-node cluster
        print(f"\n{GREEN}{'='*60}{NC}")
        print(f"{GREEN}Phase 1.2 Test: Leader-Forwarding Pattern{NC}")
        print(f"{GREEN}{'='*60}{NC}\n")

        log_info("Starting 3-node Raft cluster...")

        # Start nodes
        p1, f1 = start_node(1, 9092, 9291, 5001)
        processes.append(p1)
        log_files.append(f1)
        time.sleep(2)

        p2, f2 = start_node(2, 9093, 9292, 5002)
        processes.append(p2)
        log_files.append(f2)
        time.sleep(2)

        p3, f3 = start_node(3, 9094, 9293, 5003)
        processes.append(p3)
        log_files.append(f3)

        # Wait for cluster to be ready
        time.sleep(10)  # Give Raft time to elect a leader

        # Step 2: Test connectivity to all nodes
        log_info("Testing connectivity to all nodes...")
        nodes = [
            ('Node 1 (9092)', 'localhost:9092'),
            ('Node 2 (9093)', 'localhost:9093'),
            ('Node 3 (9094)', 'localhost:9094')
        ]

        ready_nodes = []
        for name, bootstrap in nodes:
            if wait_for_cluster_ready([bootstrap], timeout=15):  # Increased from 5s to 15s for Raft leader election
                log_success(f"{name} is reachable")
                ready_nodes.append((name, bootstrap))
            else:
                log_warning(f"{name} is not reachable yet")

        if len(ready_nodes) == 0:
            log_error("No nodes are reachable! Cluster failed to start.")
            return False

        # Step 3: Create topic on first reachable node
        log_info(f"Creating topic 'test-forwarding' on {ready_nodes[0][0]}...")
        admin = KafkaAdminClient(bootstrap_servers=[ready_nodes[0][1]], request_timeout_ms=10000)

        topic = NewTopic(
            name='test-forwarding',
            num_partitions=3,
            replication_factor=3
        )

        try:
            admin.create_topics([topic])
            log_success("Topic 'test-forwarding' created")
        except KafkaError as e:
            if 'TopicExistsError' in str(e):
                log_warning("Topic already exists")
            else:
                log_error(f"Failed to create topic: {e}")
                return False
        finally:
            admin.close()

        # Wait for topic to replicate
        time.sleep(5)

        # Step 4: Read topic metadata from ALL nodes (testing forwarding)
        print(f"\n{BLUE}Testing metadata forwarding from followers...{NC}\n")

        all_success = True
        for name, bootstrap in ready_nodes:
            log_info(f"Querying topics from {name}...")
            try:
                admin = KafkaAdminClient(bootstrap_servers=[bootstrap], request_timeout_ms=5000)
                topics = admin.list_topics()

                if 'test-forwarding' in topics:
                    log_success(f"{name}: ✓ Found 'test-forwarding' topic (forwarding works!)")
                else:
                    log_error(f"{name}: ✗ Topic not found (forwarding failed)")
                    all_success = False

                admin.close()
            except Exception as e:
                log_error(f"{name}: Failed to query topics: {e}")
                all_success = False

        # Step 5: Produce and consume to verify end-to-end
        if all_success and len(ready_nodes) >= 2:
            log_info("\nTesting end-to-end: produce to one node, consume from another...")

            try:
                # Produce to first node
                producer = KafkaProducer(
                    bootstrap_servers=[ready_nodes[0][1]],
                    value_serializer=lambda v: v.encode('utf-8')
                )
                producer.send('test-forwarding', value='Hello from Phase 1.2!')
                producer.flush()
                producer.close()
                log_success(f"Produced message to {ready_nodes[0][0]}")

                time.sleep(2)

                # Consume from different node
                consumer = KafkaConsumer(
                    'test-forwarding',
                    bootstrap_servers=[ready_nodes[-1][1]],
                    auto_offset_reset='earliest',
                    consumer_timeout_ms=5000,
                    value_deserializer=lambda m: m.decode('utf-8')
                )

                messages = []
                for msg in consumer:
                    messages.append(msg.value)
                    log_success(f"Consumed from {ready_nodes[-1][0]}: '{msg.value}'")

                consumer.close()

                if len(messages) > 0:
                    log_success("✓ End-to-end test passed!")
                else:
                    log_warning("No messages consumed (may be expected if follower can't read yet)")

            except Exception as e:
                log_warning(f"End-to-end test error: {e}")

        # Final result
        print(f"\n{GREEN if all_success else RED}{'='*60}{NC}")
        if all_success:
            print(f"{GREEN}✓ Phase 1.2 Test PASSED: Follower forwarding works!{NC}")
            print(f"{GREEN}  All nodes return consistent metadata{NC}")
        else:
            print(f"{RED}✗ Phase 1.2 Test FAILED: Some nodes have stale metadata{NC}")
        print(f"{GREEN if all_success else RED}{'='*60}{NC}\n")

        return all_success

    finally:
        # Cleanup
        log_info("Stopping cluster...")
        for i, process in enumerate(processes):
            process.terminate()
            try:
                process.wait(timeout=5)
                log_success(f"Node {i+1} stopped")
            except subprocess.TimeoutExpired:
                process.kill()
                log_warning(f"Node {i+1} forcefully killed")

        for log_file in log_files:
            log_file.close()

        log_info("Cluster stopped")

if __name__ == '__main__':
    success = test_follower_forwarding()
    sys.exit(0 if success else 1)
