#!/usr/bin/env python3
"""
Test Chronik Raft Cluster - Phase 1 & 2 Verification

This test verifies:
1. 3-node Raft cluster starts successfully
2. Metadata replication works
3. Single-partition data replication
4. Basic Kafka produce/consume
"""

import subprocess
import time
import sys
import os
import signal
from kafka import KafkaProducer, KafkaConsumer, KafkaAdminClient
from kafka.admin import NewTopic

# Configuration
NODE1_KAFKA_PORT = 9091
NODE2_KAFKA_PORT = 9092
NODE3_KAFKA_PORT = 9093

NODE1_RAFT_PORT = 5001
NODE2_RAFT_PORT = 5002
NODE3_RAFT_PORT = 5003

BINARY = "./target/release/chronik-server"
TOPIC = "test-raft-replication"

processes = []

def cleanup():
    """Kill all chronik processes and clean data"""
    print("\n🧹 Cleanup...")
    for proc in processes:
        try:
            proc.terminate()
            proc.wait(timeout=5)
        except:
            proc.kill()

    subprocess.run(["pkill", "-9", "chronik-server"], check=False, stderr=subprocess.DEVNULL)
    time.sleep(1)

    # Clean data dirs
    for i in [1, 2, 3]:
        data_dir = f"/tmp/chronik-raft-node{i}"
        if os.path.exists(data_dir):
            subprocess.run(["rm", "-rf", data_dir], check=False)

    print("✅ Cleanup done\n")

def start_node(node_id, kafka_port, raft_port):
    """Start a Chronik Raft node"""
    data_dir = f"/tmp/chronik-raft-node{node_id}"
    os.makedirs(data_dir, exist_ok=True)

    # Build peers list (exclude self)
    peers = []
    all_peers = [
        (1, NODE1_RAFT_PORT),
        (2, NODE2_RAFT_PORT),
        (3, NODE3_RAFT_PORT)
    ]
    for peer_id, peer_port in all_peers:
        if peer_id != node_id:
            peers.append(f"{peer_id}@127.0.0.1:{peer_port}")

    peers_arg = ",".join(peers)

    cmd = [
        BINARY,
        "--kafka-port", str(kafka_port),
        "--data-dir", data_dir,
        "--advertised-addr", "localhost",
        "--advertised-port", str(kafka_port),
        "--node-id", str(node_id),
        "raft-cluster",
        "--raft-addr", f"0.0.0.0:{raft_port}",
        "--peers", peers_arg,
    ]

    # Add bootstrap flag only for node 1
    if node_id == 1:
        cmd.append("--bootstrap")

    print(f"🚀 Starting Node {node_id}:")
    print(f"   Kafka port: {kafka_port}")
    print(f"   Raft port: {raft_port}")
    print(f"   Peers: {peers_arg}")

    env = os.environ.copy()
    env["RUST_LOG"] = "info,chronik_raft=debug"

    log_file = open(f"/tmp/raft-node{node_id}.log", "w")
    proc = subprocess.Popen(
        cmd,
        stdout=log_file,
        stderr=subprocess.STDOUT,
        env=env
    )

    processes.append(proc)
    print(f"✅ Node {node_id} started (PID {proc.pid})")
    return proc

def wait_for_kafka(port, max_wait=30):
    """Wait for Kafka API to be ready"""
    print(f"⏳ Waiting for Kafka on port {port}...")
    for i in range(max_wait):
        try:
            admin = KafkaAdminClient(
                bootstrap_servers=f"localhost:{port}",
                request_timeout_ms=2000,
                api_version=(0, 10, 0)
            )
            admin.close()
            print(f"✅ Kafka ready on port {port}")
            return True
        except Exception as e:
            if i == max_wait - 1:
                print(f"❌ Kafka not ready after {max_wait}s: {e}")
                return False
            time.sleep(1)
    return False

def test_metadata_replication():
    """Test that metadata replicates across nodes"""
    print("\n" + "="*70)
    print("📊 Test: Metadata Replication")
    print("="*70)

    # Create topic on node 1
    print("\n1. Creating topic on Node 1...")
    try:
        admin = KafkaAdminClient(
            bootstrap_servers=f"localhost:{NODE1_KAFKA_PORT}",
            api_version=(0, 10, 0)
        )
        topic = NewTopic(name=TOPIC, num_partitions=1, replication_factor=1)
        admin.create_topics([topic])
        admin.close()
        print(f"✅ Topic '{TOPIC}' created on Node 1")
    except Exception as e:
        print(f"⚠️  Topic creation: {e}")

    time.sleep(3)  # Wait for metadata replication

    # Verify topic exists on all nodes
    print("\n2. Verifying topic on all nodes...")
    for node_id, port in [(1, NODE1_KAFKA_PORT), (2, NODE2_KAFKA_PORT), (3, NODE3_KAFKA_PORT)]:
        try:
            admin = KafkaAdminClient(
                bootstrap_servers=f"localhost:{port}",
                api_version=(0, 10, 0)
            )
            topics = admin.list_topics()
            admin.close()

            if TOPIC in topics:
                print(f"✅ Node {node_id}: Topic '{TOPIC}' found")
            else:
                print(f"❌ Node {node_id}: Topic '{TOPIC}' NOT found")
                return False
        except Exception as e:
            print(f"❌ Node {node_id}: Error listing topics: {e}")
            return False

    print("\n✅ Metadata replication successful!")
    return True

def test_data_replication():
    """Test that data replicates across nodes"""
    print("\n" + "="*70)
    print("📊 Test: Data Replication")
    print("="*70)

    message_count = 10

    # Produce messages to node 1
    print(f"\n1. Producing {message_count} messages to Node 1...")
    try:
        producer = KafkaProducer(
            bootstrap_servers=f"localhost:{NODE1_KAFKA_PORT}",
            acks='all',
            api_version=(0, 10, 0)
        )

        for i in range(message_count):
            msg = f"Test message {i}".encode('utf-8')
            producer.send(TOPIC, value=msg).get(timeout=10)

        producer.flush()
        producer.close()
        print(f"✅ Produced {message_count} messages")
    except Exception as e:
        print(f"❌ Produce failed: {e}")
        return False

    time.sleep(2)  # Wait for replication

    # Consume from each node
    print(f"\n2. Consuming messages from all nodes...")
    for node_id, port in [(1, NODE1_KAFKA_PORT), (2, NODE2_KAFKA_PORT), (3, NODE3_KAFKA_PORT)]:
        try:
            consumer = KafkaConsumer(
                TOPIC,
                bootstrap_servers=f"localhost:{port}",
                auto_offset_reset='earliest',
                consumer_timeout_ms=5000,
                api_version=(0, 10, 0)
            )

            consumed = []
            for msg in consumer:
                consumed.append(msg.value)

            consumer.close()

            if len(consumed) == message_count:
                print(f"✅ Node {node_id}: Consumed {len(consumed)}/{message_count} messages")
            else:
                print(f"❌ Node {node_id}: Only consumed {len(consumed)}/{message_count} messages")
                return False
        except Exception as e:
            print(f"❌ Node {node_id}: Consume failed: {e}")
            return False

    print("\n✅ Data replication successful!")
    return True

def main():
    print("="*70)
    print("🧪 Chronik Raft Cluster Test - Phase 1 & 2")
    print("="*70)

    cleanup()

    try:
        # Start cluster
        print("\n📍 Starting 3-node Raft cluster...")
        start_node(1, NODE1_KAFKA_PORT, NODE1_RAFT_PORT)
        time.sleep(2)
        start_node(2, NODE2_KAFKA_PORT, NODE2_RAFT_PORT)
        time.sleep(2)
        start_node(3, NODE3_KAFKA_PORT, NODE3_RAFT_PORT)

        # Wait for cluster to stabilize
        print("\n⏳ Waiting for cluster to stabilize (30s)...")
        time.sleep(30)

        # Wait for Kafka APIs
        print("\n📍 Checking Kafka API availability...")
        if not wait_for_kafka(NODE1_KAFKA_PORT):
            print("❌ FAILED: Node 1 not ready")
            return 1
        if not wait_for_kafka(NODE2_KAFKA_PORT):
            print("❌ FAILED: Node 2 not ready")
            return 1
        if not wait_for_kafka(NODE3_KAFKA_PORT):
            print("❌ FAILED: Node 3 not ready")
            return 1

        print("\n✅ All nodes ready!")

        # Run tests
        if not test_metadata_replication():
            print("\n❌ FAILED: Metadata replication test")
            return 1

        if not test_data_replication():
            print("\n❌ FAILED: Data replication test")
            return 1

        print("\n" + "="*70)
        print("✅ ALL TESTS PASSED!")
        print("="*70)
        return 0

    except KeyboardInterrupt:
        print("\n\n⚠️  Test interrupted by user")
        return 130
    except Exception as e:
        print(f"\n❌ Test failed with exception: {e}")
        import traceback
        traceback.print_exc()
        return 1
    finally:
        cleanup()

if __name__ == "__main__":
    sys.exit(main())
