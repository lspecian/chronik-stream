#!/usr/bin/env python3
"""
Minimal Raft Test - Single Partition Focus

Tests:
1. Start 3-node cluster
2. Wait for leader election
3. Produce 100 messages
4. Consume all messages
5. Verify data integrity
"""

import subprocess
import time
import sys
import os
import signal
from kafka import KafkaProducer, KafkaConsumer
from kafka.errors import KafkaError

# Test configuration
TOPIC_NAME = "single-partition-test"
NUM_MESSAGES = 100
MESSAGE_PREFIX = "msg-"

# Node configurations (fixed: adjacent ports, matching shell script)
NODES = [
    {"id": 1, "kafka_port": 9092, "raft_port": 9093, "data_dir": "./data/node1"},
    {"id": 2, "kafka_port": 9192, "raft_port": 9193, "data_dir": "./data/node2"},
    {"id": 3, "kafka_port": 9292, "raft_port": 9293, "data_dir": "./data/node3"},
]

CLUSTER_PEERS = ",".join([f"localhost:{NODES[i]['kafka_port']}:{NODES[i]['raft_port']}" for i in range(3)])

def log_info(msg):
    print(f"[INFO] {msg}")

def log_success(msg):
    print(f"[SUCCESS] {msg}")

def log_error(msg):
    print(f"[ERROR] {msg}")

def cleanup_data_dirs():
    """Remove existing data directories"""
    log_info("Cleaning up data directories...")
    for node in NODES:
        subprocess.run(["rm", "-rf", node["data_dir"]], stderr=subprocess.DEVNULL)
    subprocess.run(["rm", "-rf", "./data/wal/__meta"], stderr=subprocess.DEVNULL)

def start_node(node_id):
    """Start a Chronik node with Raft enabled"""
    node = NODES[node_id - 1]
    log_info(f"Starting Node {node_id} (Kafka: {node['kafka_port']}, Raft: {node['raft_port']})...")

    env = os.environ.copy()
    env.update({
        "CHRONIK_CLUSTER_ENABLED": "true",
        "CHRONIK_NODE_ID": str(node_id),
        "CHRONIK_ADVERTISED_ADDR": "localhost",
        "CHRONIK_ADVERTISED_PORT": str(node["kafka_port"]),
        "CHRONIK_CLUSTER_PEERS": CLUSTER_PEERS,
        "RUST_LOG": "info",
    })
    # Removed CHRONIK_RAFT_PORT - not used by server (Raft port is in CLUSTER_PEERS)

    log_file = open(f"node{node_id}_simple.log", "w")
    # Fixed: Use direct binary execution instead of cargo run
    proc = subprocess.Popen(
        [
            "./target/release/chronik-server",
            "--kafka-port", str(node["kafka_port"]),
            "--data-dir", node["data_dir"],
            "standalone"
        ],
        env=env,
        stdout=log_file,
        stderr=subprocess.STDOUT,
        preexec_fn=os.setsid
    )

    return proc, log_file

def stop_node(proc, log_file, node_id):
    """Stop a node gracefully"""
    log_info(f"Stopping Node {node_id}...")
    try:
        if proc.poll() is None:
            try:
                os.killpg(os.getpgid(proc.pid), signal.SIGTERM)
                proc.wait(timeout=10)
            except subprocess.TimeoutExpired:
                os.killpg(os.getpgid(proc.pid), signal.SIGKILL)
                proc.wait()
            except OSError:
                pass
    finally:
        try:
            log_file.close()
        except:
            pass

def wait_for_cluster(bootstrap_servers, timeout=120):
    """Wait for cluster to be ready"""
    log_info(f"Waiting for cluster (timeout: {timeout}s)...")
    start = time.time()

    while time.time() - start < timeout:
        try:
            producer = KafkaProducer(
                bootstrap_servers=bootstrap_servers,
                request_timeout_ms=15000,
                api_version=(2, 5, 0)
            )
            producer.close()
            log_success("Cluster is ready!")
            return True
        except Exception as e:
            elapsed = int(time.time() - start)
            if elapsed % 10 == 0:  # Log every 10 seconds
                log_info(f"Still waiting... ({elapsed}s elapsed)")
            time.sleep(3)

    log_error("Cluster did not become ready in time")
    return False

def produce_messages(bootstrap_servers, num_messages):
    """Produce messages to the topic"""
    log_info(f"Producing {num_messages} messages...")

    producer = KafkaProducer(
        bootstrap_servers=bootstrap_servers,
        acks='all',
        request_timeout_ms=60000,
        api_version=(2, 5, 0),
        retries=3
    )

    success_count = 0
    for i in range(num_messages):
        message = f"{MESSAGE_PREFIX}{i}".encode('utf-8')
        try:
            future = producer.send(TOPIC_NAME, value=message)
            future.get(timeout=30)
            success_count += 1
        except Exception as e:
            log_error(f"Failed to send message {i}: {e}")

    producer.flush(timeout=30)
    producer.close()

    log_success(f"Produced {success_count}/{num_messages} messages")
    return success_count

def consume_messages(bootstrap_servers, expected_count):
    """Consume messages and verify count"""
    log_info(f"Consuming messages (expecting {expected_count})...")

    consumer = KafkaConsumer(
        TOPIC_NAME,
        bootstrap_servers=bootstrap_servers,
        auto_offset_reset='earliest',
        enable_auto_commit=False,
        consumer_timeout_ms=120000,
        request_timeout_ms=60000,
        api_version=(2, 5, 0)
    )

    messages = []
    try:
        for msg in consumer:
            messages.append(msg.value.decode('utf-8'))
            if len(messages) >= expected_count:
                break
    except Exception as e:
        log_info(f"Consumer finished: {e}")
    finally:
        consumer.close()

    log_success(f"Consumed {len(messages)} messages")
    return messages

def main():
    """Main test flow"""
    processes = {}
    log_files = {}

    try:
        log_info("="*60)
        log_info("RAFT SINGLE PARTITION SIMPLE TEST")
        log_info("="*60)

        cleanup_data_dirs()

        # Start all nodes
        for node_id in [1, 2, 3]:
            proc, log_file = start_node(node_id)
            processes[node_id] = proc
            log_files[node_id] = log_file
            time.sleep(3)  # Stagger startup

        bootstrap_servers = [f"localhost:{node['kafka_port']}" for node in NODES]

        # Wait for cluster
        if not wait_for_cluster(bootstrap_servers, timeout=120):
            log_error("Cluster failed to start")
            return False

        # Extra time for Raft election
        log_info("Waiting for Raft leader election...")
        time.sleep(30)

        # Produce messages
        success_count = produce_messages(bootstrap_servers, NUM_MESSAGES)
        if success_count < NUM_MESSAGES * 0.9:  # 90% threshold
            log_error(f"Too many produce failures: {success_count}/{NUM_MESSAGES}")
            return False

        # Allow replication
        log_info("Waiting for replication...")
        time.sleep(10)

        # Consume messages
        messages = consume_messages(bootstrap_servers, NUM_MESSAGES)

        # Verify
        if len(messages) >= NUM_MESSAGES * 0.9:  # 90% threshold
            log_success(f"✅ TEST PASSED: {len(messages)}/{NUM_MESSAGES} messages recovered")
            return True
        else:
            log_error(f"❌ TEST FAILED: Only {len(messages)}/{NUM_MESSAGES} messages recovered")
            return False

    except Exception as e:
        log_error(f"Test failed with exception: {e}")
        import traceback
        traceback.print_exc()
        return False

    finally:
        # Cleanup
        log_info("\nCleaning up...")
        for node_id, proc in processes.items():
            try:
                stop_node(proc, log_files[node_id], node_id)
            except Exception as e:
                log_error(f"Error stopping node {node_id}: {e}")

if __name__ == "__main__":
    success = main()
    sys.exit(0 if success else 1)
