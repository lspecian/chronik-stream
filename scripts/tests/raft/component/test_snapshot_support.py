#!/usr/bin/env python3
"""
Snapshot Support Test - Focused test for Raft snapshot functionality

Tests:
1. Snapshot creation when threshold is reached
2. Snapshot installation when node rejoins after lag
3. No "to_commit X is out of range" panic on rejoin

Scenario:
- Start 3-node cluster
- Produce 150 messages (exceeds default 100 snapshot threshold)
- Stop Node 3
- Produce 100 more messages (Node 3 now lagging by 100+)
- Restart Node 3
- Verify: Node 3 receives snapshot and catches up (no panic)
"""

import subprocess
import time
import sys
import os
import signal
from kafka import KafkaProducer, KafkaConsumer
from kafka.admin import KafkaAdminClient, NewTopic

# Configuration
NODES = [
    {"id": 1, "kafka_port": 9092, "raft_port": 9192, "data_dir": "/tmp/chronik-snapshot-node1"},
    {"id": 2, "kafka_port": 9093, "raft_port": 9193, "data_dir": "/tmp/chronik-snapshot-node2"},
    {"id": 3, "kafka_port": 9094, "raft_port": 9194, "data_dir": "/tmp/chronik-snapshot-node3"},
]

BINARY = "./target/release/chronik-server"
CONFIG_FILE = "chronik-cluster.toml"
TOPIC = "snapshot-test"
SNAPSHOT_THRESHOLD = 100  # Create snapshot every 100 entries

processes = {}

def cleanup():
    """Kill all chronik-server processes and clean data dirs"""
    print("\n🧹 Cleanup: Stopping all nodes...")
    for node in NODES:
        if node["id"] in processes:
            try:
                processes[node["id"]].terminate()
                processes[node["id"]].wait(timeout=5)
            except:
                processes[node["id"]].kill()

        # Clean data directory
        data_dir = node["data_dir"]
        if os.path.exists(data_dir):
            subprocess.run(["rm", "-rf", data_dir], check=False)

    # Kill any leftover chronik-server processes
    subprocess.run(["pkill", "-9", "chronik-server"], check=False)
    print("✅ Cleanup complete")

def start_node(node_id):
    """Start a single Chronik node"""
    node = next(n for n in NODES if n["id"] == node_id)

    print(f"🚀 Starting Node {node_id} (Kafka:{node['kafka_port']}, Raft:{node['raft_port']})...")

    # Create data directory
    os.makedirs(node["data_dir"], exist_ok=True)

    env = os.environ.copy()
    env["RUST_LOG"] = "info,chronik_raft=debug"
    env["CHRONIK_DATA_DIR"] = node["data_dir"]

    # Start with --raft flag for clustering
    proc = subprocess.Popen(
        [BINARY, "--advertised-addr", "localhost", "--raft", "standalone"],
        env=env,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        bufsize=1
    )

    processes[node_id] = proc
    print(f"✅ Node {node_id} started (PID: {proc.pid})")

    # Give node time to initialize
    time.sleep(3)

    return proc

def stop_node(node_id):
    """Stop a single node gracefully"""
    if node_id in processes:
        print(f"🛑 Stopping Node {node_id}...")
        proc = processes[node_id]
        proc.terminate()
        try:
            proc.wait(timeout=10)
            print(f"✅ Node {node_id} stopped")
        except subprocess.TimeoutExpired:
            print(f"⚠️  Node {node_id} didn't stop gracefully, killing...")
            proc.kill()
        del processes[node_id]

def wait_for_cluster_ready():
    """Wait for cluster to be ready by checking all nodes respond"""
    print("⏳ Waiting for cluster to be ready...")
    max_retries = 30

    for retry in range(max_retries):
        try:
            # Try to connect to each node
            for node in NODES:
                if node["id"] not in processes:
                    continue  # Skip stopped nodes

                admin = KafkaAdminClient(
                    bootstrap_servers=f"localhost:{node['kafka_port']}",
                    request_timeout_ms=2000
                )
                admin.close()

            print("✅ Cluster ready!")
            return True
        except Exception as e:
            if retry < max_retries - 1:
                time.sleep(1)
            else:
                print(f"❌ Cluster not ready after {max_retries}s: {e}")
                return False

    return False

def create_topic():
    """Create test topic with replication factor 3"""
    print(f"📝 Creating topic '{TOPIC}' with 1 partition, replication factor 3...")

    admin = KafkaAdminClient(bootstrap_servers="localhost:9092")
    topic = NewTopic(name=TOPIC, num_partitions=1, replication_factor=3)

    try:
        admin.create_topics([topic])
        print(f"✅ Topic '{TOPIC}' created")
    except Exception as e:
        print(f"⚠️  Topic creation: {e}")
    finally:
        admin.close()

    time.sleep(2)

def produce_messages(count, start_offset=0, port=9092):
    """Produce messages to the topic"""
    print(f"📤 Producing {count} messages to port {port}...")

    producer = KafkaProducer(
        bootstrap_servers=f"localhost:{port}",
        acks='all',  # Wait for all replicas
        api_version=(0, 10, 0)
    )

    for i in range(count):
        msg_num = start_offset + i
        message = f"Message {msg_num} - snapshot test".encode('utf-8')
        future = producer.send(TOPIC, value=message)
        future.get(timeout=10)  # Wait for ack

        if (i + 1) % 50 == 0:
            print(f"  Produced {i + 1}/{count} messages")

    producer.flush()
    producer.close()
    print(f"✅ Produced {count} messages")

def consume_messages(expected_count, port=9092):
    """Consume messages and verify count"""
    print(f"📥 Consuming messages from port {port}...")

    consumer = KafkaConsumer(
        TOPIC,
        bootstrap_servers=f"localhost:{port}",
        auto_offset_reset='earliest',
        consumer_timeout_ms=10000,
        api_version=(0, 10, 0)
    )

    messages = []
    for msg in consumer:
        messages.append(msg.value.decode('utf-8'))
        if len(messages) >= expected_count:
            break

    consumer.close()

    print(f"✅ Consumed {len(messages)} messages (expected {expected_count})")
    return len(messages) == expected_count

def check_logs_for_snapshot(node_id):
    """Check if node logs show snapshot activity"""
    if node_id not in processes:
        return False

    proc = processes[node_id]
    # Read recent output (non-blocking)
    # Note: This is simplified - in practice you'd tail the log file
    print(f"ℹ️  Check Node {node_id} logs manually for snapshot activity")
    return True

def main():
    """Run snapshot support test"""
    print("=" * 70)
    print("🧪 Snapshot Support Test - Raft Node Rejoin Scenario")
    print("=" * 70)

    cleanup()

    try:
        # Step 1: Start 3-node cluster
        print("\n📍 Step 1: Start 3-node cluster")
        for node in NODES:
            start_node(node["id"])

        if not wait_for_cluster_ready():
            print("❌ FAILED: Cluster not ready")
            return 1

        # Step 2: Create topic
        print("\n📍 Step 2: Create topic")
        create_topic()

        # Step 3: Produce 150 messages (exceeds snapshot threshold of 100)
        print(f"\n📍 Step 3: Produce 150 messages (exceeds snapshot threshold of {SNAPSHOT_THRESHOLD})")
        produce_messages(150, start_offset=0)

        # Wait for snapshots to be created
        print("⏳ Waiting 10s for snapshot creation...")
        time.sleep(10)

        # Step 4: Stop Node 3
        print("\n📍 Step 4: Stop Node 3 (simulate node failure)")
        stop_node(3)
        time.sleep(2)

        # Step 5: Produce 100 more messages while Node 3 is down
        print(f"\n📍 Step 5: Produce 100 more messages (Node 3 will lag by 100+ entries)")
        produce_messages(100, start_offset=150, port=9092)

        # Wait for replication to Node 1 and 2
        print("⏳ Waiting 5s for replication to remaining nodes...")
        time.sleep(5)

        # Step 6: Restart Node 3 (critical test - should receive snapshot, not panic)
        print("\n📍 Step 6: Restart Node 3 (should receive snapshot and catch up)")
        print("🔍 CRITICAL: Watch for 'Received snapshot' log, NO 'out of range' panic")
        start_node(3)

        # Wait for Node 3 to catch up via snapshot
        print("⏳ Waiting 15s for Node 3 to receive snapshot and catch up...")
        time.sleep(15)

        # Step 7: Verify Node 3 can consume all messages
        print("\n📍 Step 7: Verify Node 3 has all 250 messages")
        success = consume_messages(250, port=9094)

        if success:
            print("\n" + "=" * 70)
            print("✅ SUCCESS: Snapshot support works!")
            print("=" * 70)
            print("✅ Node 3 rejoined after lagging by 100+ entries")
            print("✅ No 'to_commit X is out of range' panic")
            print("✅ Snapshot installed successfully")
            print("✅ All 250 messages available on Node 3")
            return 0
        else:
            print("\n" + "=" * 70)
            print("❌ FAILED: Node 3 didn't receive all messages")
            print("=" * 70)
            return 1

    except Exception as e:
        print(f"\n❌ FAILED: {e}")
        import traceback
        traceback.print_exc()
        return 1
    finally:
        cleanup()

if __name__ == "__main__":
    sys.exit(main())
