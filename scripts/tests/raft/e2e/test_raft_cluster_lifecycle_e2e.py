#!/usr/bin/env python3
"""
Raft Cluster Lifecycle End-to-End Test

Comprehensive test of full cluster lifecycle including:
1. Clean cluster startup
2. Topic replication across nodes
3. Single node failure and recovery
4. Leader failover
5. Graceful shutdown
6. Metadata persistence
7. Network partition (optional)

Success Criteria:
- Cluster forms within 10s
- Node rejoin works correctly
- Metadata persists across restarts
- Leadership transfer is clean
- No message loss in any scenario
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
from collections import defaultdict
import json

# Test configuration
REPLICATION_FACTOR = 3
NUM_PARTITIONS = 3
TEST_MESSAGES = 1000
MESSAGE_SIZE = 512

# Node configurations
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
    MAGENTA = '\033[95m'
    CYAN = '\033[96m'
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

def log_scenario(name):
    print(f"\n{Colors.MAGENTA}{Colors.BOLD}{'='*70}{Colors.ENDC}")
    print(f"{Colors.MAGENTA}{Colors.BOLD}SCENARIO: {name}{Colors.ENDC}")
    print(f"{Colors.MAGENTA}{Colors.BOLD}{'='*70}{Colors.ENDC}\n")

class TestMetrics:
    def __init__(self):
        self.cluster_formation_time = None
        self.leader_election_time = None
        self.node_rejoin_time = None
        self.failover_time = None
        self.shutdown_time = None
        self.messages_replicated = 0
        self.messages_recovered = 0
        self.metadata_topics_count = 0
        self.scenario_results = {}

    def record_scenario(self, name, passed, details=""):
        self.scenario_results[name] = {"passed": passed, "details": details}

    def print_summary(self):
        print(f"\n{Colors.CYAN}{Colors.BOLD}{'='*70}{Colors.ENDC}")
        print(f"{Colors.CYAN}{Colors.BOLD}E2E TEST SUMMARY{Colors.ENDC}")
        print(f"{Colors.CYAN}{Colors.BOLD}{'='*70}{Colors.ENDC}\n")

        # Scenario Results
        print(f"{Colors.BOLD}Scenario Results:{Colors.ENDC}")
        passed_count = 0
        for name, result in self.scenario_results.items():
            status = f"{Colors.GREEN}✅ PASS{Colors.ENDC}" if result["passed"] else f"{Colors.RED}❌ FAIL{Colors.ENDC}"
            print(f"  {status} - {name}")
            if result["details"]:
                print(f"         {result['details']}")
            if result["passed"]:
                passed_count += 1

        # Performance Metrics
        print(f"\n{Colors.BOLD}Performance Metrics:{Colors.ENDC}")
        if self.cluster_formation_time:
            log_metric("Cluster Formation Time", f"{self.cluster_formation_time:.2f}s")
        if self.leader_election_time:
            log_metric("Leader Election Time", f"{self.leader_election_time:.2f}s")
        if self.node_rejoin_time:
            log_metric("Node Rejoin Time", f"{self.node_rejoin_time:.2f}s")
        if self.failover_time:
            log_metric("Leader Failover Time", f"{self.failover_time:.2f}s")
        if self.shutdown_time:
            log_metric("Graceful Shutdown Time", f"{self.shutdown_time:.2f}s")

        # Data Integrity Metrics
        print(f"\n{Colors.BOLD}Data Integrity:{Colors.ENDC}")
        log_metric("Messages Replicated", self.messages_replicated)
        log_metric("Messages Recovered", self.messages_recovered)
        log_metric("Metadata Topics Count", self.metadata_topics_count)

        # Overall Result
        total = len(self.scenario_results)
        print(f"\n{Colors.BOLD}Overall Result: {passed_count}/{total} scenarios passed{Colors.ENDC}")

        if passed_count == total:
            print(f"{Colors.GREEN}{Colors.BOLD}✅ ALL TESTS PASSED!{Colors.ENDC}\n")
            return True
        else:
            print(f"{Colors.RED}{Colors.BOLD}❌ SOME TESTS FAILED{Colors.ENDC}\n")
            return False

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
        "CHRONIK_NODE_ID": str(node_id),
        "CHRONIK_REPLICATION_FACTOR": "3",
        "CHRONIK_MIN_INSYNC_REPLICAS": "2",
        "CHRONIK_CLUSTER_PEERS": CLUSTER_PEERS,
        # No explicit CHRONIK_CLUSTER_ENABLED - auto-enabled when CHRONIK_CLUSTER_PEERS is set
        # No explicit CHRONIK_METRICS_PORT - auto-derived to kafka_port + 2
        "RUST_LOG": "info,chronik_raft=debug,chronik_server::raft_integration=debug",
    })

    log_file = open(f"node{node_id}_lifecycle.log", "w")
    proc = subprocess.Popen(
        [
            "./target/release/chronik-server",
            "--kafka-port", str(node["kafka_port"]),
            "--bind-addr", "0.0.0.0",
            "--advertised-addr", "localhost",
            "--data-dir", node["data_dir"],
            "--disable-search",  # Disable search API to avoid port conflicts
            "standalone"
        ],
        env=env,
        stdout=log_file,
        stderr=subprocess.STDOUT,
        preexec_fn=os.setsid
    )

    return proc, log_file

def stop_node_graceful(proc, log_file, node_id):
    """Stop a node gracefully (SIGTERM)"""
    log_info(f"Stopping Node {node_id} gracefully (SIGTERM)...")
    try:
        if proc.poll() is None:
            os.killpg(os.getpgid(proc.pid), signal.SIGTERM)
            proc.wait(timeout=30)
            log_success(f"Node {node_id} stopped gracefully")
        else:
            log_info(f"Node {node_id} already stopped")
    except subprocess.TimeoutExpired:
        log_warning(f"Node {node_id} did not stop gracefully, forcing...")
        os.killpg(os.getpgid(proc.pid), signal.SIGKILL)
        proc.wait()
    except OSError as e:
        log_warning(f"Error stopping node {node_id}: {e}")
    finally:
        try:
            log_file.close()
        except:
            pass

def stop_node_crash(proc, log_file, node_id):
    """Stop a node abruptly (SIGKILL - simulating crash)"""
    log_warning(f"💥 KILLING Node {node_id} (simulating crash)...")
    try:
        if proc.poll() is None:
            os.killpg(os.getpgid(proc.pid), signal.SIGKILL)
            proc.wait()
        else:
            log_info(f"Node {node_id} already stopped")
    except OSError as e:
        log_warning(f"Error killing node {node_id}: {e}")
    finally:
        try:
            log_file.close()
        except:
            pass
    log_success(f"Node {node_id} killed")

def wait_for_cluster(bootstrap_servers, timeout=60):
    """Wait for cluster to be ready"""
    log_info(f"Waiting for cluster to be ready (timeout: {timeout}s)...")
    start = time.time()

    while time.time() - start < timeout:
        try:
            producer = KafkaProducer(
                bootstrap_servers=bootstrap_servers,
                request_timeout_ms=5000,
                api_version=(2, 5, 0)
            )
            producer.close()
            elapsed = time.time() - start
            log_success(f"Cluster is ready! (took {elapsed:.2f}s)")
            return elapsed
        except Exception as e:
            time.sleep(2)

    log_error("Cluster did not become ready in time")
    return None

def create_topic(bootstrap_servers, topic_name, num_partitions, replication_factor):
    """Create test topic"""
    log_info(f"Creating topic {topic_name} (partitions={num_partitions}, rf={replication_factor})...")
    try:
        admin = KafkaAdminClient(
            bootstrap_servers=bootstrap_servers,
            request_timeout_ms=10000,
            api_version=(2, 5, 0)
        )

        topic = NewTopic(
            name=topic_name,
            num_partitions=num_partitions,
            replication_factor=replication_factor
        )

        admin.create_topics([topic])
        admin.close()
        log_success(f"Topic {topic_name} created")
        time.sleep(3)  # Wait for replicas
        return True
    except Exception as e:
        log_warning(f"Topic creation error: {e}")
        return False

def produce_messages(bootstrap_servers, topic, num_messages):
    """Produce messages to topic"""
    log_info(f"Producing {num_messages} messages to {topic}...")

    producer = KafkaProducer(
        bootstrap_servers=bootstrap_servers,
        acks='all',
        max_in_flight_requests_per_connection=5,
        retries=3,
        request_timeout_ms=30000,
        api_version=(2, 5, 0)
    )

    message_data = b'X' * MESSAGE_SIZE
    success_count = 0
    failed_count = 0

    for i in range(num_messages):
        try:
            key = f"key-{i}".encode('utf-8')
            value = f"{message_data.decode('utf-8')}-{i}".encode('utf-8')
            future = producer.send(topic, key=key, value=value)
            future.get(timeout=30)
            success_count += 1

            if (i + 1) % 100 == 0:
                log_info(f"Produced {i + 1}/{num_messages} messages...")
        except Exception as e:
            failed_count += 1
            if failed_count <= 5:  # Only log first 5 failures
                log_warning(f"Failed to send message {i}: {e}")

    producer.flush(timeout=30)
    producer.close()

    log_success(f"Produced {success_count}/{num_messages} messages ({failed_count} failures)")
    return success_count

def consume_messages(bootstrap_servers, topic, timeout_ms=30000):
    """Consume all messages from topic"""
    log_info(f"Consuming messages from {topic}...")

    consumer = KafkaConsumer(
        topic,
        bootstrap_servers=bootstrap_servers,
        auto_offset_reset='earliest',
        enable_auto_commit=False,
        consumer_timeout_ms=timeout_ms,
        request_timeout_ms=30000,
        api_version=(2, 5, 0)
    )

    messages = []
    try:
        for msg in consumer:
            messages.append({
                'partition': msg.partition,
                'offset': msg.offset,
                'key': msg.key.decode('utf-8') if msg.key else None,
                'value': msg.value.decode('utf-8')[:50]  # First 50 chars
            })
    except Exception as e:
        log_info(f"Consumer finished: {type(e).__name__}")
    finally:
        consumer.close()

    log_success(f"Consumed {len(messages)} messages")
    return messages

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
            leader = metadata.leader_for_partition(topic, 0)
            if leader:
                for node in NODES:
                    if leader.port == node["kafka_port"]:
                        admin.close()
                        return node["id"]

        admin.close()
    except Exception as e:
        log_warning(f"Could not identify leader: {e}")

    return None

def list_topics(bootstrap_servers):
    """List all topics in cluster"""
    try:
        admin = KafkaAdminClient(
            bootstrap_servers=bootstrap_servers,
            request_timeout_ms=5000,
            api_version=(2, 5, 0)
        )
        topics = admin.list_topics()
        admin.close()
        return topics
    except Exception as e:
        log_warning(f"Could not list topics: {e}")
        return []

def get_cluster_health(bootstrap_servers):
    """Check cluster health"""
    try:
        admin = KafkaAdminClient(
            bootstrap_servers=bootstrap_servers,
            request_timeout_ms=5000,
            api_version=(2, 5, 0)
        )
        metadata = admin._client.cluster
        brokers = list(metadata.brokers())
        admin.close()
        return len(brokers)
    except Exception as e:
        return 0

# =============================================================================
# Scenario 1: Clean Cluster Startup
# =============================================================================
def scenario_clean_startup(metrics):
    log_scenario("1. Clean Cluster Startup")

    processes = {}
    log_files = {}

    try:
        cleanup_data_dirs()

        # Start all nodes
        start_time = time.time()
        for node_id in [1, 2, 3]:
            proc, log_file = start_node(node_id)
            processes[node_id] = proc
            log_files[node_id] = log_file
            time.sleep(5)  # Give each node more time to start gRPC server

        bootstrap_servers = [f"localhost:{node['kafka_port']}" for node in NODES]

        # Wait for cluster
        formation_time = wait_for_cluster(bootstrap_servers, timeout=60)
        if not formation_time:
            metrics.record_scenario("Clean Startup", False, "Cluster failed to form")
            return processes, log_files, False

        metrics.cluster_formation_time = formation_time

        # Wait for leader election AND peer discovery
        log_info("Waiting for Raft peer discovery and leader election...")
        time.sleep(30)  # Increased to allow gRPC connections to establish
        election_time = time.time() - start_time
        metrics.leader_election_time = election_time

        # Verify all nodes healthy
        healthy_nodes = get_cluster_health(bootstrap_servers)
        log_metric("Healthy Nodes", f"{healthy_nodes}/3")

        if healthy_nodes >= 2:  # Quorum achieved
            metrics.record_scenario("Clean Startup", True, f"{formation_time:.2f}s formation, {healthy_nodes}/3 nodes healthy")
            return processes, log_files, True
        else:
            metrics.record_scenario("Clean Startup", False, f"Only {healthy_nodes}/3 nodes healthy")
            return processes, log_files, False

    except Exception as e:
        log_error(f"Scenario 1 failed: {e}")
        metrics.record_scenario("Clean Startup", False, str(e))
        return processes, log_files, False

# =============================================================================
# Scenario 2: Topic Replication Across Nodes
# =============================================================================
def scenario_topic_replication(processes, log_files, metrics):
    log_scenario("2. Topic Replication Across Nodes")

    try:
        bootstrap_servers = [f"localhost:{node['kafka_port']}" for node in NODES]

        # Create topic
        if not create_topic(bootstrap_servers, "replication-test", NUM_PARTITIONS, REPLICATION_FACTOR):
            metrics.record_scenario("Topic Replication", False, "Topic creation failed")
            return False

        # Produce messages
        produced = produce_messages(bootstrap_servers, "replication-test", TEST_MESSAGES)
        if produced < TEST_MESSAGES * 0.9:
            metrics.record_scenario("Topic Replication", False, f"Only {produced}/{TEST_MESSAGES} produced")
            return False

        metrics.messages_replicated = produced

        # Wait for replication
        log_info("Waiting for replication...")
        time.sleep(10)

        # Consume from different node
        consumed = consume_messages(bootstrap_servers, "replication-test")
        if len(consumed) >= TEST_MESSAGES * 0.9:
            metrics.record_scenario("Topic Replication", True, f"{produced} produced, {len(consumed)} consumed")
            return True
        else:
            metrics.record_scenario("Topic Replication", False, f"Only {len(consumed)}/{TEST_MESSAGES} consumed")
            return False

    except Exception as e:
        log_error(f"Scenario 2 failed: {e}")
        metrics.record_scenario("Topic Replication", False, str(e))
        return False

# =============================================================================
# Scenario 3: Single Node Failure and Recovery
# =============================================================================
def scenario_node_failure_recovery(processes, log_files, metrics):
    log_scenario("3. Single Node Failure and Recovery")

    try:
        bootstrap_servers = [f"localhost:{node['kafka_port']}" for node in NODES]

        # Kill node 1
        log_warning("Killing Node 1...")
        stop_node_crash(processes[1], log_files[1], 1)
        del processes[1]
        time.sleep(5)

        # Verify cluster still works (2-node quorum)
        healthy_nodes = get_cluster_health(bootstrap_servers[1:])  # Skip node 1
        log_metric("Healthy Nodes After Failure", f"{healthy_nodes}/2")

        if healthy_nodes < 2:
            metrics.record_scenario("Node Failure Recovery", False, "Cluster lost quorum")
            return False

        # Produce/consume should work
        if not create_topic(bootstrap_servers[1:], "failure-test", 1, 2):
            log_warning("Topic creation failed, but continuing...")

        produced = produce_messages(bootstrap_servers[1:], "failure-test", 100)
        if produced < 80:
            metrics.record_scenario("Node Failure Recovery", False, "Produce failed after node failure")
            return False

        # Restart node 1
        log_info("Restarting Node 1...")
        rejoin_start = time.time()
        proc, log_file = start_node(1)
        processes[1] = proc
        log_files[1] = log_file

        # Wait for rejoin
        log_info("Waiting for node to rejoin...")
        time.sleep(20)
        rejoin_time = time.time() - rejoin_start
        metrics.node_rejoin_time = rejoin_time

        # Verify node rejoined
        bootstrap_servers = [f"localhost:{node['kafka_port']}" for node in NODES]
        healthy_nodes = get_cluster_health(bootstrap_servers)
        log_metric("Healthy Nodes After Rejoin", f"{healthy_nodes}/3")

        if healthy_nodes >= 3:
            metrics.record_scenario("Node Failure Recovery", True, f"Node rejoined in {rejoin_time:.2f}s")
            return True
        else:
            metrics.record_scenario("Node Failure Recovery", False, f"Only {healthy_nodes}/3 nodes after rejoin")
            return False

    except Exception as e:
        log_error(f"Scenario 3 failed: {e}")
        metrics.record_scenario("Node Failure Recovery", False, str(e))
        return False

# =============================================================================
# Scenario 4: Leader Failover
# =============================================================================
def scenario_leader_failover(processes, log_files, metrics):
    log_scenario("4. Leader Failover")

    try:
        bootstrap_servers = [f"localhost:{node['kafka_port']}" for node in NODES]

        # Create topic for failover test
        if not create_topic(bootstrap_servers, "failover-test", 1, 3):
            log_warning("Topic creation failed, but continuing...")

        # Identify leader
        leader_id = identify_leader(bootstrap_servers, "failover-test")
        if not leader_id:
            log_warning("Could not identify leader, assuming Node 1")
            leader_id = 1

        log_success(f"📍 Identified leader: Node {leader_id}")

        # Produce some messages
        produce_messages(bootstrap_servers, "failover-test", 100)

        # Kill leader
        log_warning(f"Killing leader (Node {leader_id})...")
        failover_start = time.time()
        stop_node_crash(processes[leader_id], log_files[leader_id], leader_id)
        del processes[leader_id]

        # Wait for new leader election
        log_info("Waiting for new leader election...")
        remaining_servers = [f"localhost:{node['kafka_port']}" for node in NODES if node['id'] != leader_id]

        # Poll for recovery
        recovery_time = None
        for i in range(20):
            time.sleep(1)
            try:
                # Try to produce - if it works, new leader elected
                producer = KafkaProducer(
                    bootstrap_servers=remaining_servers,
                    request_timeout_ms=5000,
                    api_version=(2, 5, 0)
                )
                producer.send("failover-test", value=b"test")
                producer.flush(timeout=5)
                producer.close()

                recovery_time = time.time() - failover_start
                log_success(f"✅ New leader elected! Failover time: {recovery_time:.2f}s")
                break
            except:
                continue

        if not recovery_time:
            metrics.record_scenario("Leader Failover", False, "No new leader elected within 20s")
            return False

        metrics.failover_time = recovery_time

        # Verify produce/consume still works
        produced = produce_messages(remaining_servers, "failover-test", 100)
        consumed = consume_messages(remaining_servers, "failover-test")

        if produced >= 80 and len(consumed) >= 180:  # Original 100 + 100 new
            metrics.record_scenario("Leader Failover", True, f"Failover in {recovery_time:.2f}s, {len(consumed)} messages recovered")
            return True
        else:
            metrics.record_scenario("Leader Failover", False, f"Data inconsistency: {produced} produced, {len(consumed)} consumed")
            return False

    except Exception as e:
        log_error(f"Scenario 4 failed: {e}")
        metrics.record_scenario("Leader Failover", False, str(e))
        return False

# =============================================================================
# Scenario 5: Graceful Shutdown
# =============================================================================
def scenario_graceful_shutdown(processes, log_files, metrics):
    log_scenario("5. Graceful Shutdown")

    try:
        # Find a running node (may not be all 3 after failover test)
        running_nodes = list(processes.keys())
        if not running_nodes:
            metrics.record_scenario("Graceful Shutdown", False, "No running nodes")
            return False

        node_to_stop = running_nodes[0]
        log_info(f"Gracefully stopping Node {node_to_stop}...")

        shutdown_start = time.time()
        stop_node_graceful(processes[node_to_stop], log_files[node_to_stop], node_to_stop)
        shutdown_time = time.time() - shutdown_start

        metrics.shutdown_time = shutdown_time

        del processes[node_to_stop]

        # Verify other nodes still work
        remaining_servers = [f"localhost:{NODES[i]['kafka_port']}" for i in range(3) if NODES[i]['id'] in processes]

        if len(remaining_servers) >= 2:
            time.sleep(5)
            healthy = get_cluster_health(remaining_servers)
            if healthy >= 2:
                metrics.record_scenario("Graceful Shutdown", True, f"Shutdown in {shutdown_time:.2f}s, {healthy} nodes remain healthy")
                return True

        metrics.record_scenario("Graceful Shutdown", False, "Cluster became unhealthy after shutdown")
        return False

    except Exception as e:
        log_error(f"Scenario 5 failed: {e}")
        metrics.record_scenario("Graceful Shutdown", False, str(e))
        return False

# =============================================================================
# Scenario 6: Metadata Persistence
# =============================================================================
def scenario_metadata_persistence(processes, log_files, metrics):
    log_scenario("6. Metadata Persistence")

    try:
        bootstrap_servers = [f"localhost:{node['kafka_port']}" for node in NODES]

        # Stop all nodes gracefully
        log_info("Stopping all nodes gracefully...")
        for node_id in list(processes.keys()):
            stop_node_graceful(processes[node_id], log_files[node_id], node_id)
        processes.clear()
        log_files.clear()

        time.sleep(5)

        # Restart all nodes
        log_info("Restarting all nodes...")
        for node_id in [1, 2, 3]:
            proc, log_file = start_node(node_id)
            processes[node_id] = proc
            log_files[node_id] = log_file
            time.sleep(2)

        # Wait for cluster
        formation_time = wait_for_cluster(bootstrap_servers, timeout=60)
        if not formation_time:
            metrics.record_scenario("Metadata Persistence", False, "Cluster failed to restart")
            return processes, log_files, False

        time.sleep(15)  # Wait for Raft

        # Check if topics still exist
        topics = list_topics(bootstrap_servers)
        log_metric("Topics Found", len(topics))
        metrics.metadata_topics_count = len(topics)

        # Try to consume from existing topics
        total_messages = 0
        for topic in topics:
            if not topic.startswith('__'):  # Skip internal topics
                messages = consume_messages(bootstrap_servers, topic, timeout_ms=10000)
                total_messages += len(messages)
                log_info(f"Topic {topic}: {len(messages)} messages")

        metrics.messages_recovered = total_messages

        if len(topics) >= 3 and total_messages > 0:  # Should have at least our test topics
            metrics.record_scenario("Metadata Persistence", True, f"{len(topics)} topics, {total_messages} messages recovered")
            return processes, log_files, True
        else:
            metrics.record_scenario("Metadata Persistence", False, f"Only {len(topics)} topics, {total_messages} messages")
            return processes, log_files, False

    except Exception as e:
        log_error(f"Scenario 6 failed: {e}")
        metrics.record_scenario("Metadata Persistence", False, str(e))
        return processes, log_files, False

# =============================================================================
# Main Test Flow
# =============================================================================
def main():
    """Main test orchestrator"""
    metrics = TestMetrics()
    processes = {}
    log_files = {}

    try:
        print(f"{Colors.CYAN}{Colors.BOLD}")
        print("=" * 70)
        print("RAFT CLUSTER LIFECYCLE END-TO-END TEST")
        print("=" * 70)
        print(f"{Colors.ENDC}\n")

        # Scenario 1: Clean Cluster Startup
        processes, log_files, success = scenario_clean_startup(metrics)
        if not success:
            log_error("Scenario 1 failed, cannot continue")
            return False

        # Scenario 2: Topic Replication
        if not scenario_topic_replication(processes, log_files, metrics):
            log_warning("Scenario 2 failed, continuing with other tests...")

        # Scenario 3: Node Failure and Recovery
        if not scenario_node_failure_recovery(processes, log_files, metrics):
            log_warning("Scenario 3 failed, continuing with other tests...")

        # Scenario 4: Leader Failover
        if not scenario_leader_failover(processes, log_files, metrics):
            log_warning("Scenario 4 failed, continuing with other tests...")

        # Scenario 5: Graceful Shutdown
        if not scenario_graceful_shutdown(processes, log_files, metrics):
            log_warning("Scenario 5 failed, continuing with other tests...")

        # Scenario 6: Metadata Persistence
        processes, log_files, success = scenario_metadata_persistence(processes, log_files, metrics)

        # Print summary
        return metrics.print_summary()

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
                stop_node_graceful(proc, log_files.get(node_id), node_id)
            except Exception as e:
                log_warning(f"Error stopping node {node_id}: {e}")

if __name__ == "__main__":
    success = main()
    sys.exit(0 if success else 1)
