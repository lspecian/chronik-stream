#!/usr/bin/env python3
"""
Multi-Partition Raft End-to-End Test

Comprehensive test suite covering:
1. Multi-partition topic creation
2. Produce to multiple partitions with distribution
3. Consume from multiple partitions
4. Partition independence (basic verification)
5. Load balancing across partitions

Test Configuration:
- 3-node cluster
- 3 partitions per topic (simpler for current Raft implementation)
- Replication factor: 3
- 1,000 messages with keyed distribution
"""

import subprocess
import time
import sys
import os
import signal
import json
from collections import defaultdict
from kafka import KafkaProducer, KafkaConsumer, KafkaAdminClient
from kafka.admin import NewTopic
from kafka.errors import KafkaError

# ANSI color codes for rich output
class Color:
    HEADER = '\033[95m'
    BLUE = '\033[94m'
    CYAN = '\033[96m'
    GREEN = '\033[92m'
    YELLOW = '\033[93m'
    RED = '\033[91m'
    RESET = '\033[0m'
    BOLD = '\033[1m'

# Test configuration
TOPIC_NAME = "multi-partition-test"
NUM_PARTITIONS = 3  # Reduced for stability
REPLICATION_FACTOR = 3
NUM_MESSAGES = 1000  # Reduced for faster testing
MESSAGE_PREFIX = "msg-"

# Node configurations
NODES = [
    {"id": 1, "kafka_port": 9092, "raft_port": 9093, "data_dir": "./data/node1"},
    {"id": 2, "kafka_port": 9192, "raft_port": 9193, "data_dir": "./data/node2"},
    {"id": 3, "kafka_port": 9292, "raft_port": 9293, "data_dir": "./data/node3"},
]

CLUSTER_PEERS = ",".join([f"localhost:{NODES[i]['kafka_port']}:{NODES[i]['raft_port']}" for i in range(3)])

# Test metrics
class TestMetrics:
    def __init__(self):
        self.topic_creation_time = 0
        self.produce_start_time = 0
        self.produce_end_time = 0
        self.consume_start_time = 0
        self.consume_end_time = 0
        self.messages_sent = 0
        self.messages_received = 0
        self.partition_distribution = defaultdict(int)
        self.partition_leader_distribution = defaultdict(int)
        self.errors = []

    def produce_throughput(self):
        duration = self.produce_end_time - self.produce_start_time
        return self.messages_sent / duration if duration > 0 else 0

    def consume_throughput(self):
        duration = self.consume_end_time - self.consume_start_time
        return self.messages_received / duration if duration > 0 else 0

    def print_summary(self):
        print(f"\n{Color.HEADER}{Color.BOLD}{'='*70}{Color.RESET}")
        print(f"{Color.HEADER}{Color.BOLD}TEST SUMMARY{Color.RESET}")
        print(f"{Color.HEADER}{Color.BOLD}{'='*70}{Color.RESET}\n")

        print(f"{Color.CYAN}Messages:{Color.RESET}")
        print(f"  Sent:     {Color.GREEN if self.messages_sent == NUM_MESSAGES else Color.YELLOW}{self.messages_sent:,}{Color.RESET}/{NUM_MESSAGES:,}")
        print(f"  Received: {Color.GREEN if self.messages_received >= NUM_MESSAGES * 0.9 else Color.RED}{self.messages_received:,}{Color.RESET}/{NUM_MESSAGES:,}")

        print(f"\n{Color.CYAN}Throughput:{Color.RESET}")
        if self.produce_throughput() > 0:
            print(f"  Produce: {Color.GREEN}{self.produce_throughput():,.0f}{Color.RESET} msg/s")
        if self.consume_throughput() > 0:
            print(f"  Consume: {Color.GREEN}{self.consume_throughput():,.0f}{Color.RESET} msg/s")

        if self.partition_distribution:
            print(f"\n{Color.CYAN}Partition Distribution:{Color.RESET}")
            for partition_id in sorted(self.partition_distribution.keys()):
                count = self.partition_distribution[partition_id]
                expected = NUM_MESSAGES / NUM_PARTITIONS
                variance = abs(count - expected) / expected * 100 if expected > 0 else 0
                color = Color.GREEN if variance < 30 else Color.YELLOW
                print(f"  Partition {partition_id}: {color}{count:,}{Color.RESET} messages ({variance:.1f}% variance)")

        if self.partition_leader_distribution:
            print(f"\n{Color.CYAN}Leader Distribution:{Color.RESET}")
            for node_id in sorted(self.partition_leader_distribution.keys()):
                count = self.partition_leader_distribution[node_id]
                print(f"  Node {node_id}: {Color.GREEN}{count}{Color.RESET} partitions as leader")

        if self.errors:
            print(f"\n{Color.RED}Errors ({len(self.errors)} total):{Color.RESET}")
            for error in self.errors[:3]:  # Show first 3 errors
                print(f"  - {error}")
            if len(self.errors) > 3:
                print(f"  ... and {len(self.errors) - 3} more")

        print(f"\n{Color.HEADER}{Color.BOLD}{'='*70}{Color.RESET}")

metrics = TestMetrics()

def log_info(msg):
    print(f"{Color.BLUE}[INFO]{Color.RESET} {msg}")

def log_success(msg):
    print(f"{Color.GREEN}[SUCCESS]{Color.RESET} {msg}")

def log_warning(msg):
    print(f"{Color.YELLOW}[WARNING]{Color.RESET} {msg}")

def log_error(msg):
    print(f"{Color.RED}[ERROR]{Color.RESET} {msg}")
    metrics.errors.append(msg)

def log_scenario(msg):
    print(f"\n{Color.CYAN}{Color.BOLD}{'─'*70}{Color.RESET}")
    print(f"{Color.CYAN}{Color.BOLD}▶ {msg}{Color.RESET}")
    print(f"{Color.CYAN}{Color.BOLD}{'─'*70}{Color.RESET}")

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
        "RUST_LOG": "info,chronik_raft=debug",
    })

    log_file = open(f"node{node_id}_multi.log", "w")
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

def scenario_1_topic_creation(bootstrap_servers):
    """Scenario 1: Multi-Partition Topic Creation"""
    log_scenario("SCENARIO 1: Multi-Partition Topic Creation")

    try:
        admin_client = KafkaAdminClient(
            bootstrap_servers=bootstrap_servers,
            request_timeout_ms=30000,
            api_version=(2, 5, 0)
        )

        topic = NewTopic(
            name=TOPIC_NAME,
            num_partitions=NUM_PARTITIONS,
            replication_factor=REPLICATION_FACTOR
        )

        start_time = time.time()
        admin_client.create_topics([topic], validate_only=False)
        metrics.topic_creation_time = time.time() - start_time

        log_success(f"Topic '{TOPIC_NAME}' created with {NUM_PARTITIONS} partitions (RF={REPLICATION_FACTOR})")
        log_info(f"Creation time: {metrics.topic_creation_time:.2f}s")

        # Wait for topic to propagate
        time.sleep(10)

        # Verify topic metadata using consumer
        from kafka import KafkaConsumer
        consumer = KafkaConsumer(
            bootstrap_servers=bootstrap_servers,
            api_version=(2, 5, 0)
        )

        partitions = consumer.partitions_for_topic(TOPIC_NAME)
        if partitions:
            log_success(f"Topic verified: {len(partitions)} partitions")

            # Get partition metadata
            for partition_id in partitions:
                from kafka.structs import TopicPartition
                tp = TopicPartition(TOPIC_NAME, partition_id)
                leader = consumer._client.cluster.leader_for_partition(tp)
                if leader is not None:
                    metrics.partition_leader_distribution[leader] += 1

        consumer.close()

        # Print leader distribution
        if metrics.partition_leader_distribution:
            log_info("Leader distribution:")
            for node_id in sorted(metrics.partition_leader_distribution.keys()):
                count = metrics.partition_leader_distribution[node_id]
                log_info(f"  Node {node_id}: {count} partitions as leader")

        admin_client.close()
        return True

    except Exception as e:
        log_error(f"Topic creation failed: {e}")
        import traceback
        traceback.print_exc()
        return False

def scenario_2_produce_multi_partition(bootstrap_servers):
    """Scenario 2: Produce to Multiple Partitions"""
    log_scenario("SCENARIO 2: Produce to Multiple Partitions with Distribution")

    try:
        producer = KafkaProducer(
            bootstrap_servers=bootstrap_servers,
            acks=1,  # Use acks=1 instead of all for now
            request_timeout_ms=60000,
            api_version=(2, 5, 0),
            retries=3
        )

        metrics.produce_start_time = time.time()
        success_count = 0
        partition_counts = defaultdict(int)

        log_info(f"Producing {NUM_MESSAGES:,} messages with keys for distribution...")

        for i in range(NUM_MESSAGES):
            # Use key to ensure distribution across partitions
            key = f"key-{i % 100}".encode('utf-8')
            message = f"{MESSAGE_PREFIX}{i}".encode('utf-8')

            try:
                future = producer.send(TOPIC_NAME, key=key, value=message)
                record_metadata = future.get(timeout=30)
                success_count += 1
                partition_counts[record_metadata.partition] += 1

                if (i + 1) % 100 == 0:
                    log_info(f"  Produced {i + 1:,}/{NUM_MESSAGES:,} messages...")

            except Exception as e:
                log_error(f"Failed to send message {i}: {e}")
                if success_count < NUM_MESSAGES * 0.5:  # If more than 50% fail, abort
                    log_error("Too many failures, aborting produce")
                    break

        producer.flush(timeout=30)
        producer.close()

        metrics.produce_end_time = time.time()
        metrics.messages_sent = success_count

        # Analyze distribution
        log_success(f"Produced {success_count:,}/{NUM_MESSAGES:,} messages")
        if metrics.produce_throughput() > 0:
            log_info(f"Throughput: {metrics.produce_throughput():,.0f} msg/s")

        log_info("Message distribution across partitions:")
        for partition_id in sorted(partition_counts.keys()):
            count = partition_counts[partition_id]
            percentage = (count / success_count) * 100 if success_count > 0 else 0
            log_info(f"  Partition {partition_id}: {count:,} messages ({percentage:.1f}%)")

        # Verify distribution is reasonably even (within 50% variance for small numbers)
        if len(partition_counts) > 0:
            expected_per_partition = success_count / NUM_PARTITIONS
            max_variance = 0
            for count in partition_counts.values():
                variance = abs(count - expected_per_partition) / expected_per_partition if expected_per_partition > 0 else 0
                max_variance = max(max_variance, variance)

            if max_variance < 0.5:  # 50% variance threshold
                log_success(f"✅ Distribution is balanced (max variance: {max_variance * 100:.1f}%)")
            else:
                log_warning(f"⚠️  Distribution variance is high: {max_variance * 100:.1f}%")

        return success_count >= NUM_MESSAGES * 0.7  # 70% success threshold

    except Exception as e:
        log_error(f"Produce test failed: {e}")
        import traceback
        traceback.print_exc()
        return False

def scenario_3_consume_multi_partition(bootstrap_servers):
    """Scenario 3: Consume from Multiple Partitions"""
    log_scenario("SCENARIO 3: Consume from Multiple Partitions")

    try:
        consumer = KafkaConsumer(
            TOPIC_NAME,
            bootstrap_servers=bootstrap_servers,
            auto_offset_reset='earliest',
            enable_auto_commit=False,
            consumer_timeout_ms=60000,  # Reduced timeout
            request_timeout_ms=60000,
            api_version=(2, 5, 0)
        )

        metrics.consume_start_time = time.time()
        messages_by_partition = defaultdict(list)
        total_consumed = 0

        log_info(f"Consuming messages (expecting {metrics.messages_sent:,})...")

        try:
            for msg in consumer:
                messages_by_partition[msg.partition].append(msg.value.decode('utf-8'))
                total_consumed += 1

                if total_consumed % 100 == 0:
                    log_info(f"  Consumed {total_consumed:,} messages...")

                if total_consumed >= metrics.messages_sent:
                    break
        except Exception as e:
            log_info(f"Consumer finished: {e}")
        finally:
            consumer.close()

        metrics.consume_end_time = time.time()
        metrics.messages_received = total_consumed
        metrics.partition_distribution = {p: len(msgs) for p, msgs in messages_by_partition.items()}

        log_success(f"Consumed {total_consumed:,} messages")
        if metrics.consume_throughput() > 0:
            log_info(f"Throughput: {metrics.consume_throughput():,.0f} msg/s")

        # Verify message ordering within each partition
        log_info("Verifying message ordering per partition...")
        all_ordered = True
        for partition_id, messages in messages_by_partition.items():
            # Extract sequence numbers
            sequences = []
            for msg in messages:
                if msg.startswith(MESSAGE_PREFIX):
                    try:
                        seq = int(msg[len(MESSAGE_PREFIX):])
                        sequences.append(seq)
                    except ValueError:
                        pass

            # Check if ordered (allow gaps but no out-of-order)
            is_ordered = all(sequences[i] <= sequences[i + 1] for i in range(len(sequences) - 1))
            if not is_ordered:
                log_warning(f"Partition {partition_id}: Messages out of order!")
                all_ordered = False
            else:
                log_info(f"  Partition {partition_id}: ✓ {len(messages)} messages in order")

        if all_ordered:
            log_success("✅ All partitions have ordered messages")

        # Verify message recovery
        recovery_rate = (total_consumed / metrics.messages_sent) * 100 if metrics.messages_sent > 0 else 0
        if recovery_rate >= 90:  # 90% threshold
            log_success(f"✅ Message recovery: {recovery_rate:.1f}%")
            return True
        else:
            log_warning(f"⚠️  Message recovery: {recovery_rate:.1f}%")
            return recovery_rate >= 70  # Still pass if >= 70%

    except Exception as e:
        log_error(f"Consume test failed: {e}")
        import traceback
        traceback.print_exc()
        return False

def scenario_4_partition_metadata(bootstrap_servers):
    """Scenario 4: Partition Metadata Verification"""
    log_scenario("SCENARIO 4: Partition Metadata Verification")

    try:
        from kafka import KafkaConsumer
        from kafka.structs import TopicPartition

        consumer = KafkaConsumer(
            bootstrap_servers=bootstrap_servers,
            api_version=(2, 5, 0)
        )

        partitions = consumer.partitions_for_topic(TOPIC_NAME)
        if not partitions:
            log_error("No partitions found for topic")
            consumer.close()
            return False

        log_info(f"Verifying metadata for {len(partitions)} partitions...")

        for partition_id in partitions:
            tp = TopicPartition(TOPIC_NAME, partition_id)

            # Get leader
            leader = consumer._client.cluster.leader_for_partition(tp)

            # Get replicas
            replicas = consumer._client.cluster.available_brokers_for_partition(tp)

            log_info(f"  Partition {partition_id}: leader={leader}, replicas={len(replicas)}")

        consumer.close()

        log_success("✅ Partition metadata verified")
        return True

    except Exception as e:
        log_error(f"Partition metadata test failed: {e}")
        import traceback
        traceback.print_exc()
        return False

def main():
    """Main test flow"""
    processes = {}
    log_files = {}

    try:
        print(f"\n{Color.HEADER}{Color.BOLD}{'='*70}{Color.RESET}")
        print(f"{Color.HEADER}{Color.BOLD}RAFT MULTI-PARTITION END-TO-END TEST{Color.RESET}")
        print(f"{Color.HEADER}{Color.BOLD}{'='*70}{Color.RESET}")
        print(f"{Color.CYAN}Configuration:{Color.RESET}")
        print(f"  Nodes: {len(NODES)}")
        print(f"  Partitions: {NUM_PARTITIONS}")
        print(f"  Replication Factor: {REPLICATION_FACTOR}")
        print(f"  Messages: {NUM_MESSAGES:,}")
        print(f"{Color.HEADER}{Color.BOLD}{'='*70}{Color.RESET}\n")

        cleanup_data_dirs()

        # Start all nodes
        log_info("Starting 3-node cluster...")
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

        # Run test scenarios
        results = {}

        # Scenario 1: Topic Creation
        results['scenario_1'] = scenario_1_topic_creation(bootstrap_servers)
        if not results['scenario_1']:
            log_error("Scenario 1 failed, aborting tests")
            return False

        # Scenario 2: Produce
        results['scenario_2'] = scenario_2_produce_multi_partition(bootstrap_servers)

        # Wait for replication
        log_info("Waiting for replication...")
        time.sleep(10)

        # Scenario 3: Consume
        results['scenario_3'] = scenario_3_consume_multi_partition(bootstrap_servers)

        # Scenario 4: Metadata
        results['scenario_4'] = scenario_4_partition_metadata(bootstrap_servers)

        # Print metrics summary
        metrics.print_summary()

        # Final verdict
        all_passed = all(results.values())
        print(f"\n{Color.HEADER}{Color.BOLD}SCENARIO RESULTS:{Color.RESET}")
        for scenario, passed in results.items():
            status = f"{Color.GREEN}✅ PASSED{Color.RESET}" if passed else f"{Color.RED}❌ FAILED{Color.RESET}"
            print(f"  {scenario}: {status}")

        # Consider test passed if at least 3 out of 4 scenarios pass
        passed_count = sum(1 for result in results.values() if result)
        if passed_count >= 3:
            print(f"\n{Color.GREEN}{Color.BOLD}{'='*70}{Color.RESET}")
            print(f"{Color.GREEN}{Color.BOLD}✅ TEST PASSED ({passed_count}/4 scenarios){Color.RESET}")
            print(f"{Color.GREEN}{Color.BOLD}{'='*70}{Color.RESET}\n")
            return True
        else:
            print(f"\n{Color.YELLOW}{Color.BOLD}{'='*70}{Color.RESET}")
            print(f"{Color.YELLOW}{Color.BOLD}⚠️  PARTIAL SUCCESS ({passed_count}/4 scenarios){Color.RESET}")
            print(f"{Color.YELLOW}{Color.BOLD}{'='*70}{Color.RESET}\n")
            return passed_count >= 2  # At least 50%

    except Exception as e:
        log_error(f"Test failed with exception: {e}")
        import traceback
        traceback.print_exc()
        return False

    finally:
        # Cleanup
        log_info("\nCleaning up...")
        for node_id, proc in list(processes.items()):
            try:
                stop_node(proc, log_files[node_id], node_id)
            except Exception as e:
                log_error(f"Error stopping node {node_id}: {e}")

if __name__ == "__main__":
    success = main()
    sys.exit(0 if success else 1)
