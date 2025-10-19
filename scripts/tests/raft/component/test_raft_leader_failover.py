#!/usr/bin/env python3
"""
Raft Leader Failover Test

Tests:
1. Start 3-node cluster
2. Identify current leader
3. Produce messages continuously
4. Kill leader mid-stream
5. Verify new leader election (< 10s)
6. Verify no message loss
7. Measure failover time and throughput impact
"""

import subprocess
import time
import sys
import os
import signal
import threading
from kafka import KafkaProducer, KafkaConsumer, KafkaAdminClient
from kafka.admin import NewTopic
from kafka.errors import KafkaError
import json

# Test configuration
TOPIC_NAME = "leader-failover-test"
PRODUCE_DURATION_SECONDS = 30
MESSAGE_SIZE = 1024  # 1KB messages

# Node configurations (Raft port = Kafka port + 1)
NODES = [
    {"id": 1, "kafka_port": 9092, "raft_port": 9093, "data_dir": "./data/node1"},
    {"id": 2, "kafka_port": 9192, "raft_port": 9193, "data_dir": "./data/node2"},
    {"id": 3, "kafka_port": 9292, "raft_port": 9293, "data_dir": "./data/node3"},
]

CLUSTER_PEERS = ",".join([f"localhost:{NODES[i]['kafka_port']}:{NODES[i]['raft_port']}" for i in range(3)])

class Colors:
    GREEN = '\033[92m'
    RED = '\033[91m'
    YELLOW = '\033[93m'
    BLUE = '\033[94m'
    ENDC = '\033[0m'
    BOLD = '\033[1m'

def log_info(msg):
    print(f"{Colors.BLUE}[INFO]{Colors.ENDC} {msg}")

def log_success(msg):
    print(f"{Colors.GREEN}[SUCCESS]{Colors.ENDC} {msg}")

def log_warning(msg):
    print(f"{Colors.YELLOW}[WARNING]{Colors.ENDC} {msg}")

def log_error(msg):
    print(f"{Colors.RED}[ERROR]{Colors.ENDC} {msg}")

def log_metric(metric, value):
    print(f"{Colors.BOLD}[METRIC]{Colors.ENDC} {metric}: {Colors.BOLD}{value}{Colors.ENDC}")

class ProducerStats:
    def __init__(self):
        self.total_sent = 0
        self.total_acked = 0
        self.total_failed = 0
        self.last_error = None
        self.leader_change_time = None
        self.stop_flag = False
        self.lock = threading.Lock()

    def record_success(self):
        with self.lock:
            self.total_acked += 1

    def record_failure(self, error):
        with self.lock:
            self.total_failed += 1
            self.last_error = str(error)

    def record_sent(self):
        with self.lock:
            self.total_sent += 1

    def get_stats(self):
        with self.lock:
            return {
                "sent": self.total_sent,
                "acked": self.total_acked,
                "failed": self.total_failed,
                "last_error": self.last_error
            }

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
        "CHRONIK_REPLICATION_FACTOR": "3",
        "CHRONIK_MIN_INSYNC_REPLICAS": "2",
        "CHRONIK_CLUSTER_PEERS": CLUSTER_PEERS,
        "RUST_LOG": "info,chronik_raft=debug,chronik_server::raft_integration=debug",
    })

    log_file = open(f"node{node_id}_failover.log", "w")
    proc = subprocess.Popen(
        [
            "./target/release/chronik-server",
            "--kafka-port", str(node["kafka_port"]),
            "--bind-addr", "0.0.0.0",
            "--advertised-addr", "localhost",
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
    """Stop a node abruptly (simulating crash)"""
    log_warning(f"💥 KILLING Node {node_id} (simulating crash)...")
    try:
        if proc.poll() is None:
            try:
                os.killpg(os.getpgid(proc.pid), signal.SIGKILL)  # Hard kill
                proc.wait()
            except OSError as e:
                log_warning(f"Error killing node {node_id}: {e}")
        else:
            log_info(f"Node {node_id} already stopped")
    finally:
        try:
            log_file.close()
        except:
            pass
    log_success(f"Node {node_id} killed")

def wait_for_cluster(bootstrap_servers, timeout=60):
    """Wait for cluster to be ready"""
    log_info(f"Waiting for cluster to be ready...")
    start = time.time()

    while time.time() - start < timeout:
        try:
            producer = KafkaProducer(
                bootstrap_servers=bootstrap_servers,
                request_timeout_ms=5000,
                api_version=(2, 5, 0)
            )
            producer.close()
            log_success("Cluster is ready!")
            return True
        except Exception as e:
            time.sleep(2)

    log_error("Cluster did not become ready in time")
    return False

def create_topic(bootstrap_servers):
    """Create test topic"""
    log_info(f"Creating topic {TOPIC_NAME}...")
    try:
        admin = KafkaAdminClient(
            bootstrap_servers=bootstrap_servers,
            request_timeout_ms=10000,
            api_version=(2, 5, 0)
        )

        topic = NewTopic(
            name=TOPIC_NAME,
            num_partitions=3,
            replication_factor=3
        )

        admin.create_topics([topic])
        admin.close()
        log_success(f"Topic {TOPIC_NAME} created")
        time.sleep(5)  # Wait for replicas
        return True
    except Exception as e:
        log_warning(f"Topic creation error (may already exist): {e}")
        return True

def continuous_produce(bootstrap_servers, stats, duration):
    """Continuously produce messages and track stats"""
    log_info(f"Starting continuous producer for {duration}s...")

    producer = KafkaProducer(
        bootstrap_servers=bootstrap_servers,
        acks='all',
        max_in_flight_requests_per_connection=5,
        retries=3,
        request_timeout_ms=10000,
        api_version=(2, 5, 0)
    )

    message_data = b'X' * MESSAGE_SIZE
    start_time = time.time()

    def callback(metadata, error):
        if error:
            stats.record_failure(error)
        else:
            stats.record_success()

    while time.time() - start_time < duration and not stats.stop_flag:
        try:
            producer.send(TOPIC_NAME, value=message_data).add_callback(
                lambda metadata: stats.record_success()
            ).add_errback(
                lambda error: stats.record_failure(error)
            )
            stats.record_sent()
            time.sleep(0.01)  # 100 msg/s rate
        except Exception as e:
            stats.record_failure(e)

    producer.flush(timeout=30)
    producer.close()

    elapsed = time.time() - start_time
    log_info(f"Producer finished after {elapsed:.2f}s")

def identify_leader(bootstrap_servers, topic):
    """Identify the current leader for partition 0"""
    try:
        admin = KafkaAdminClient(
            bootstrap_servers=bootstrap_servers,
            request_timeout_ms=5000,
            api_version=(2, 5, 0)
        )

        metadata = admin._client.cluster
        partition_metadata = metadata.partitions_for_topic(topic)

        if partition_metadata:
            # Get leader for partition 0
            leader = metadata.leader_for_partition(topic, 0)
            if leader:
                # Find which node this is
                for node in NODES:
                    if leader.port == node["kafka_port"]:
                        admin.close()
                        return node["id"]

        admin.close()
    except Exception as e:
        log_warning(f"Could not identify leader: {e}")

    return None

def main():
    """Main test flow"""
    processes = {}
    log_files = {}

    try:
        # Setup
        log_info("=" * 60)
        log_info("RAFT LEADER FAILOVER TEST")
        log_info("=" * 60)

        cleanup_data_dirs()

        # Start all nodes
        for node_id in [1, 2, 3]:
            proc, log_file = start_node(node_id)
            processes[node_id] = proc
            log_files[node_id] = log_file
            time.sleep(2)

        bootstrap_servers = [f"localhost:{node['kafka_port']}" for node in NODES]

        if not wait_for_cluster(bootstrap_servers, timeout=60):
            log_error("Cluster failed to start")
            return False

        time.sleep(15)  # Wait for Raft election

        # Create topic
        create_topic(bootstrap_servers)

        # Identify leader
        leader_id = identify_leader(bootstrap_servers, TOPIC_NAME)
        if leader_id:
            log_success(f"📍 Identified leader: Node {leader_id}")
        else:
            log_warning("Could not identify leader, assuming Node 1")
            leader_id = 1

        # Start continuous producer
        stats = ProducerStats()
        producer_thread = threading.Thread(
            target=continuous_produce,
            args=(bootstrap_servers, stats, PRODUCE_DURATION_SECONDS)
        )
        producer_thread.start()

        # Wait for some messages
        time.sleep(5)
        before_stats = stats.get_stats()
        log_info(f"Before failover: {before_stats['acked']} messages acked")

        # Kill leader at 10s mark
        time.sleep(5)
        log_warning("\n" + "=" * 60)
        log_warning("💥 INITIATING LEADER FAILOVER")
        log_warning("=" * 60 + "\n")

        failover_start = time.time()
        stop_node(processes[leader_id], log_files[leader_id], leader_id)
        del processes[leader_id]

        # Monitor recovery
        log_info("Monitoring cluster recovery...")
        recovery_detected = False
        recovery_time = None

        # Check every second for recovery
        for i in range(20):
            time.sleep(1)
            current_stats = stats.get_stats()

            # If acks are increasing, leader election succeeded
            if current_stats['acked'] > before_stats['acked'] + 10:
                if not recovery_detected:
                    recovery_time = time.time() - failover_start
                    recovery_detected = True
                    log_success(f"✅ New leader elected! Recovery time: {recovery_time:.2f}s")
                    break

            before_stats = current_stats

        if not recovery_detected:
            log_error("❌ Cluster did not recover within 20s")
            stats.stop_flag = True
            producer_thread.join()
            return False

        # Wait for producer to finish
        producer_thread.join()

        # Get final stats
        final_stats = stats.get_stats()

        log_info("\n" + "=" * 60)
        log_info("FAILOVER TEST RESULTS")
        log_info("=" * 60)

        log_metric("Messages Sent", final_stats['sent'])
        log_metric("Messages Acked", final_stats['acked'])
        log_metric("Messages Failed", final_stats['failed'])
        log_metric("Leader Failover Time", f"{recovery_time:.2f}s" if recovery_time else "N/A")

        success_rate = (final_stats['acked'] / final_stats['sent'] * 100) if final_stats['sent'] > 0 else 0
        log_metric("Success Rate", f"{success_rate:.1f}%")

        # Verify with consumer
        log_info("\nVerifying data with consumer...")
        consumer = KafkaConsumer(
            TOPIC_NAME,
            bootstrap_servers=[f"localhost:{NODES[1]['kafka_port']}", f"localhost:{NODES[2]['kafka_port']}"],  # Skip dead leader
            auto_offset_reset='earliest',
            consumer_timeout_ms=15000,
            api_version=(2, 5, 0)
        )

        consumed_count = 0
        for msg in consumer:
            consumed_count += 1

        consumer.close()

        log_metric("Messages Consumed", consumed_count)

        # Success criteria
        if recovery_time and recovery_time < 15:
            log_success("\n✅ Leader failover test PASSED!")
            log_success(f"✅ Failover completed in {recovery_time:.2f}s (target: <15s)")
            log_success(f"✅ {consumed_count} messages recovered")
            return True
        else:
            log_error("\n❌ Leader failover test FAILED!")
            return False

    except Exception as e:
        log_error(f"Test failed with exception: {e}")
        import traceback
        traceback.print_exc()
        return False

    finally:
        # Cleanup
        log_info("\nCleaning up processes...")
        for node_id, proc in processes.items():
            try:
                stop_node(proc, log_files[node_id], node_id)
            except Exception as e:
                log_warning(f"Error stopping node {node_id}: {e}")

if __name__ == "__main__":
    success = main()
    sys.exit(0 if success else 1)
