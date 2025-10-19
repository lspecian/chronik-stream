#!/usr/bin/env python3
"""
Phase 1 E2E Test: Single-Partition Raft Replication with Real Kafka Clients

This test verifies:
1. 3-node Raft cluster starts successfully
2. Kafka clients can produce/consume
3. Messages replicate across all nodes
4. Leader failover works
5. Zero message loss during failover

Uses actual Kafka protocol (kafka-python) to test end-to-end integration.
"""

import subprocess
import time
import sys
import os
import signal
from kafka import KafkaProducer, KafkaConsumer, KafkaAdminClient
from kafka.admin import NewTopic
from kafka.errors import KafkaError

# Test configuration
TOPIC = "test-replication"
MESSAGE_COUNT = 10

def start_node(node_id, kafka_port, raft_port, data_dir):
    """Start a Chronik node with Raft enabled"""
    cmd = [
        "./target/release/chronik-server",
        "--node-id", str(node_id),
        "--kafka-port", str(kafka_port),
        "--data-dir", data_dir,
        "--advertised-addr", "localhost",
        "raft-cluster",
        "--raft-addr", f"127.0.0.1:{raft_port}",
        "--peers", get_peers_arg(node_id)
    ]

    log_file = open(f"/tmp/raft-phase1-node{node_id}.log", "w")
    proc = subprocess.Popen(cmd, stdout=log_file, stderr=subprocess.STDOUT)
    return proc, log_file

def get_peers_arg(node_id):
    """Get peers argument for a node"""
    peers = []
    for i in [1, 2, 3]:
        if i != node_id:
            peers.append(f"{i}@127.0.0.1:{5000 + i}")
    return ",".join(peers)

def cleanup():
    """Cleanup old data and processes"""
    print("Cleaning up old processes and data...")
    subprocess.run(["pkill", "-9", "chronik-server"], stderr=subprocess.DEVNULL)
    time.sleep(1)
    subprocess.run(["rm", "-rf", "/tmp/chronik-raft-phase1-*"], stderr=subprocess.DEVNULL)

def wait_for_cluster(timeout=30):
    """Wait for cluster to form and become ready"""
    print(f"Waiting {timeout}s for cluster to stabilize...")
    time.sleep(timeout)

def test_produce_consume(bootstrap_servers):
    """Test basic produce and consume"""
    print("\n=== Testing Produce & Consume ===")

    # Create topic
    admin = KafkaAdminClient(bootstrap_servers=bootstrap_servers)
    topic = NewTopic(name=TOPIC, num_partitions=1, replication_factor=3)
    try:
        admin.create_topics([topic])
        print(f"✅ Created topic '{TOPIC}' with 1 partition, RF=3")
    except Exception as e:
        print(f"Topic creation note: {e}")

    time.sleep(2)  # Let topic propagate

    # Produce messages
    producer = KafkaProducer(
        bootstrap_servers=bootstrap_servers,
        api_version=(0, 10, 0)
    )

    print(f"Producing {MESSAGE_COUNT} messages...")
    for i in range(MESSAGE_COUNT):
        msg = f"test message {i}".encode()
        producer.send(TOPIC, msg)
        print(f"  Sent message {i}")

    producer.flush()
    print(f"✅ Produced {MESSAGE_COUNT} messages")

    time.sleep(2)  # Let messages replicate

    # Consume messages
    consumer = KafkaConsumer(
        TOPIC,
        bootstrap_servers=bootstrap_servers,
        auto_offset_reset='earliest',
        consumer_timeout_ms=5000,
        api_version=(0, 10, 0)
    )

    consumed = []
    for msg in consumer:
        consumed.append(msg.value)
        print(f"  Consumed: {msg.value.decode()}")

    print(f"✅ Consumed {len(consumed)} messages")

    if len(consumed) != MESSAGE_COUNT:
        print(f"❌ FAIL: Expected {MESSAGE_COUNT} messages, got {len(consumed)}")
        return False

    print("✅ SUCCESS: Produce/consume working")
    return True

def test_leader_failover(nodes):
    """Test leader failover"""
    print("\n=== Testing Leader Failover ===")

    # Kill node 1 (likely leader)
    print("Killing node 1...")
    nodes[0][0].terminate()
    nodes[0][0].wait(timeout=5)
    print("✅ Node 1 stopped")

    # Wait for re-election
    print("Waiting 10s for re-election...")
    time.sleep(10)

    # Try to produce to surviving nodes (9093 or 9094)
    producer = KafkaProducer(
        bootstrap_servers=["localhost:9093", "localhost:9094"],
        api_version=(0, 10, 0)
    )

    print("Producing message after failover...")
    try:
        future = producer.send(TOPIC, b"message after failover")
        future.get(timeout=10)
        print("✅ Produced message after failover")
    except Exception as e:
        print(f"❌ FAIL: Could not produce after failover: {e}")
        return False

    producer.flush()

    # Consume from surviving nodes
    consumer = KafkaConsumer(
        TOPIC,
        bootstrap_servers=["localhost:9093", "localhost:9094"],
        auto_offset_reset='earliest',
        consumer_timeout_ms=5000,
        api_version=(0, 10, 0)
    )

    consumed = list(consumer)
    print(f"✅ Consumed {len(consumed)} messages from surviving nodes")

    expected_count = MESSAGE_COUNT + 1  # Original + 1 new
    if len(consumed) != expected_count:
        print(f"❌ FAIL: Expected {expected_count} messages, got {len(consumed)}")
        return False

    print("✅ SUCCESS: Zero message loss during failover")
    return True

def main():
    print("=" * 60)
    print("Phase 1 E2E Test: Single-Partition Raft Replication")
    print("=" * 60)

    cleanup()

    # Start 3-node cluster
    print("\n=== Starting 3-Node Cluster ===")
    nodes = []
    for node_id in [1, 2, 3]:
        kafka_port = 9091 + node_id
        raft_port = 5000 + node_id
        data_dir = f"/tmp/chronik-raft-phase1-{node_id}"

        proc, log_file = start_node(node_id, kafka_port, raft_port, data_dir)
        nodes.append((proc, log_file))
        print(f"✅ Started node {node_id} (Kafka: {kafka_port}, Raft: {raft_port})")

    wait_for_cluster()

    # Check all nodes are running
    for i, (proc, _) in enumerate(nodes, 1):
        if proc.poll() is not None:
            print(f"❌ FAIL: Node {i} died during startup")
            cleanup()
            sys.exit(1)

    print("✅ All nodes running")

    # Run tests
    bootstrap_servers = ["localhost:9092", "localhost:9093", "localhost:9094"]

    if not test_produce_consume(bootstrap_servers):
        print("\n❌ TEST FAILED: Produce/Consume")
        cleanup()
        sys.exit(1)

    if not test_leader_failover(nodes):
        print("\n❌ TEST FAILED: Leader Failover")
        cleanup()
        sys.exit(1)

    # Cleanup
    print("\n=== Cleanup ===")
    for proc, log_file in nodes:
        if proc.poll() is None:
            proc.terminate()
            proc.wait(timeout=5)
        log_file.close()

    print("\n" + "=" * 60)
    print("🎉 ALL TESTS PASSED")
    print("=" * 60)
    print("\nTest Summary:")
    print("  ✅ 3-node cluster started successfully")
    print("  ✅ Message replication working")
    print("  ✅ Leader failover working")
    print("  ✅ Zero message loss during failover")

    cleanup()
    sys.exit(0)

if __name__ == "__main__":
    try:
        main()
    except KeyboardInterrupt:
        print("\nTest interrupted")
        cleanup()
        sys.exit(1)
    except Exception as e:
        print(f"\n❌ TEST ERROR: {e}")
        import traceback
        traceback.print_exc()
        cleanup()
        sys.exit(1)
