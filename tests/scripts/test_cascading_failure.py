#!/usr/bin/env python3
"""
Cascading Failure Test for Chronik Raft Cluster
Tests cluster behavior when multiple nodes fail in sequence (cascading failure)
"""

import time
import subprocess
import signal
import os
from kafka import KafkaProducer, KafkaConsumer, KafkaAdminClient
from kafka.admin import NewTopic
from kafka.errors import KafkaError
import json
import sys

# ANSI colors
GREEN = '\033[0;32m'
YELLOW = '\033[1;33m'
RED = '\033[0;31m'
BLUE = '\033[0;34m'
CYAN = '\033[0;36m'
NC = '\033[0m'

def print_header(text):
    print(f"\n{BLUE}{'='*70}{NC}")
    print(f"{BLUE}{text:^70}{NC}")
    print(f"{BLUE}{'='*70}{NC}\n")

def print_success(text):
    print(f"{GREEN}✓ {text}{NC}")

def print_warning(text):
    print(f"{YELLOW}⚠ {text}{NC}")

def print_error(text):
    print(f"{RED}✗ {text}{NC}")

def print_info(text):
    print(f"{BLUE}ℹ {text}{NC}")

def print_phase(text):
    print(f"\n{CYAN}{'─'*70}{NC}")
    print(f"{CYAN}{text}{NC}")
    print(f"{CYAN}{'─'*70}{NC}")


class ClusterManager:
    """Manage Chronik cluster nodes for testing"""

    def __init__(self, data_dir="./test-cascade-data"):
        self.data_dir = data_dir
        self.nodes = {}
        self.binary = "./target/release/chronik-server"

    def get_node_pid(self, node_id):
        """Get PID for a running node"""
        if node_id in self.nodes and self.nodes[node_id]:
            return self.nodes[node_id].pid if self.nodes[node_id].poll() is None else None
        return None

    def is_node_running(self, node_id):
        """Check if a node is running"""
        pid = self.get_node_pid(node_id)
        if pid:
            try:
                os.kill(pid, 0)  # Signal 0 just checks if process exists
                return True
            except OSError:
                return False
        return False

    def kill_node(self, node_id, graceful=True):
        """Kill a specific node"""
        if node_id not in self.nodes or not self.nodes[node_id]:
            print_warning(f"Node {node_id} not tracked")
            return False

        process = self.nodes[node_id]
        if process.poll() is not None:
            print_warning(f"Node {node_id} already stopped")
            return False

        try:
            if graceful:
                print_info(f"Gracefully stopping node {node_id} (SIGTERM)...")
                process.terminate()
                time.sleep(2)
                if process.poll() is None:
                    process.kill()
            else:
                print_info(f"Force killing node {node_id} (SIGKILL)...")
                process.kill()

            process.wait(timeout=5)
            print_success(f"Node {node_id} stopped (PID was {process.pid})")
            self.nodes[node_id] = None
            return True
        except Exception as e:
            print_error(f"Failed to stop node {node_id}: {e}")
            return False

    def get_cluster_status(self):
        """Get status of all nodes"""
        status = {}
        for node_id in [1, 2, 3]:
            status[node_id] = "RUNNING" if self.is_node_running(node_id) else "STOPPED"
        return status

    def print_cluster_status(self):
        """Print cluster status"""
        status = self.get_cluster_status()
        running = sum(1 for s in status.values() if s == "RUNNING")
        print_info(f"Cluster Status ({running}/3 nodes running):")
        for node_id, state in status.items():
            color = GREEN if state == "RUNNING" else RED
            pid_info = f"PID {self.get_node_pid(node_id)}" if state == "RUNNING" else "---"
            print(f"  Node {node_id}: {color}{state:8}{NC} ({pid_info})")


class CascadeTestRunner:
    """Run cascading failure tests"""

    def __init__(self):
        self.cluster = ClusterManager()
        self.bootstrap_servers = "localhost:9092,localhost:9093,localhost:9094"

    def wait_for_cluster(self, min_nodes=1, timeout=30):
        """Wait for cluster to have at least min_nodes running"""
        print_info(f"Waiting for at least {min_nodes} node(s) to be accessible...")
        start = time.time()
        while time.time() - start < timeout:
            try:
                admin = KafkaAdminClient(
                    bootstrap_servers=self.bootstrap_servers,
                    request_timeout_ms=5000
                )
                # If we can connect, at least one node is up
                admin.close()
                print_success(f"Cluster accessible")
                return True
            except Exception as e:
                time.sleep(1)
        print_error(f"Cluster not accessible after {timeout}s")
        return False

    def create_topic(self, topic_name, partitions=3, replication_factor=3):
        """Create a test topic"""
        try:
            admin = KafkaAdminClient(
                bootstrap_servers=self.bootstrap_servers,
                request_timeout_ms=10000
            )
            topic = NewTopic(
                name=topic_name,
                num_partitions=partitions,
                replication_factor=replication_factor
            )
            admin.create_topics([topic], validate_only=False)
            admin.close()
            print_success(f"Created topic: {topic_name} (partitions={partitions}, RF={replication_factor})")
            time.sleep(3)  # Wait for replicas
            return True
        except Exception as e:
            if "TopicAlreadyExistsException" in str(e):
                print_warning(f"Topic already exists: {topic_name}")
                return True
            print_error(f"Failed to create topic: {e}")
            return False

    def produce_messages(self, topic, count=50, phase="unknown", timeout=30):
        """Produce messages to a topic"""
        try:
            producer = KafkaProducer(
                bootstrap_servers=self.bootstrap_servers,
                value_serializer=lambda v: json.dumps(v).encode('utf-8'),
                acks='all',
                retries=3,
                request_timeout_ms=10000
            )

            sent = 0
            failed = 0
            start_time = time.time()

            for i in range(count):
                if time.time() - start_time > timeout:
                    print_warning(f"Timeout after {timeout}s")
                    break

                try:
                    msg = {
                        "id": i,
                        "phase": phase,
                        "timestamp": time.time()
                    }
                    future = producer.send(topic, value=msg)
                    future.get(timeout=5)
                    sent += 1
                except Exception as e:
                    failed += 1

            producer.flush()
            producer.close()

            if sent > 0:
                print_success(f"Produced {sent}/{count} messages (phase: {phase})")
            else:
                print_warning(f"Produced {sent}/{count} messages (failed: {failed}, phase: {phase})")

            return sent, failed

        except Exception as e:
            print_error(f"Producer error: {e}")
            return 0, count

    def consume_messages(self, topic, timeout=15):
        """Consume all messages from a topic"""
        try:
            consumer = KafkaConsumer(
                topic,
                bootstrap_servers=self.bootstrap_servers,
                value_deserializer=lambda m: json.loads(m.decode('utf-8')),
                auto_offset_reset='earliest',
                consumer_timeout_ms=timeout * 1000
            )

            messages = []
            for msg in consumer:
                messages.append(msg.value)

            consumer.close()
            print_success(f"Consumed {len(messages)} messages")
            return messages

        except Exception as e:
            print_error(f"Consumer error: {e}")
            return []


def test_sequential_failure():
    """Test 1: Sequential node failures (cascading one by one)"""
    print_header("TEST 1: Sequential Cascading Failure")
    print_info("Scenario: Kill nodes 1, 2, 3 in sequence, then recover in reverse")

    runner = CascadeTestRunner()
    topic = "cascade-sequential"

    # Verify cluster is running
    print_phase("Phase 0: Verify Cluster Running")
    if not runner.wait_for_cluster(min_nodes=3, timeout=10):
        print_error("Cluster not fully running - start with ./test_cluster_manual.sh start")
        return False

    runner.cluster.print_cluster_status()

    # Create topic
    print_phase("Phase 1: Create Topic & Produce Baseline")
    if not runner.create_topic(topic):
        return False

    sent_baseline, _ = runner.produce_messages(topic, count=50, phase="baseline")

    # Kill node 1
    print_phase("Phase 2: Kill Node 1 (2/3 nodes remain, quorum OK)")
    runner.cluster.kill_node(1, graceful=False)
    time.sleep(3)
    runner.cluster.print_cluster_status()

    sent_after_node1, _ = runner.produce_messages(topic, count=50, phase="after_node1_death")

    # Kill node 2 (quorum lost!)
    print_phase("Phase 3: Kill Node 2 (1/3 nodes remain, QUORUM LOST)")
    runner.cluster.kill_node(2, graceful=False)
    time.sleep(3)
    runner.cluster.print_cluster_status()

    print_warning("Attempting to produce with only 1/3 nodes (should mostly fail)...")
    sent_no_quorum, failed_no_quorum = runner.produce_messages(
        topic, count=50, phase="no_quorum", timeout=20
    )

    # Kill node 3 (complete outage)
    print_phase("Phase 4: Kill Node 3 (0/3 nodes, COMPLETE OUTAGE)")
    runner.cluster.kill_node(3, graceful=False)
    time.sleep(2)
    runner.cluster.print_cluster_status()

    print_warning("Cluster completely down - cannot produce")

    # Recovery: Restart node 3
    print_phase("Phase 5: Restart Node 3 (1/3 nodes, still no quorum)")
    print_info("Node 3 would need to be restarted manually: ./test_cluster_manual.sh start")
    print_warning("Skipping automatic restart (requires manual cluster management)")

    # For now, consume what we have
    print_phase("Phase 6: Consume Messages (after manual cluster restart)")
    print_info("Please restart cluster manually and press Enter to continue...")
    input()

    messages = runner.consume_messages(topic, timeout=15)

    # Analysis
    print_phase("Phase 7: Analysis")
    phases = {}
    for msg in messages:
        phase = msg.get('phase', 'unknown')
        phases[phase] = phases.get(phase, 0) + 1

    print_info("Message breakdown by phase:")
    for phase, count in phases.items():
        print(f"  {phase}: {count} messages")

    total_sent = sent_baseline + sent_after_node1 + sent_no_quorum
    total_consumed = len(messages)

    print_info(f"Total sent: {total_sent}, consumed: {total_consumed}")

    success = (
        sent_baseline > 0 and
        sent_after_node1 >= sent_baseline * 0.8 and  # Should work with 2/3 nodes
        sent_no_quorum < 10 and  # Should mostly fail with 1/3 nodes
        total_consumed >= (sent_baseline + sent_after_node1) * 0.9  # Should recover committed messages
    )

    if success:
        print_success("TEST PASSED: Sequential failures handled correctly")
    else:
        print_error("TEST FAILED: Unexpected behavior during sequential failures")

    return success


def test_simultaneous_failure():
    """Test 2: Simultaneous failure of 2/3 nodes"""
    print_header("TEST 2: Simultaneous Cascading Failure")
    print_info("Scenario: Kill nodes 2 and 3 simultaneously (immediate quorum loss)")

    runner = CascadeTestRunner()
    topic = "cascade-simultaneous"

    # Verify cluster
    print_phase("Phase 0: Verify Cluster Running")
    if not runner.wait_for_cluster(min_nodes=3, timeout=10):
        print_error("Cluster not running")
        return False

    runner.cluster.print_cluster_status()

    # Create topic
    print_phase("Phase 1: Create Topic & Produce Baseline")
    if not runner.create_topic(topic):
        return False

    sent_baseline, _ = runner.produce_messages(topic, count=50, phase="baseline")

    # Kill nodes 2 and 3 simultaneously
    print_phase("Phase 2: Kill Nodes 2 and 3 Simultaneously (IMMEDIATE QUORUM LOSS)")
    print_info("Killing nodes 2 and 3...")
    runner.cluster.kill_node(2, graceful=False)
    runner.cluster.kill_node(3, graceful=False)
    time.sleep(3)
    runner.cluster.print_cluster_status()

    print_warning("Attempting to produce with only 1/3 nodes (should fail)...")
    sent_no_quorum, failed_no_quorum = runner.produce_messages(
        topic, count=50, phase="no_quorum", timeout=20
    )

    # Consume
    print_phase("Phase 3: Consume Messages (after manual cluster restart)")
    print_info("Please restart cluster and press Enter...")
    input()

    messages = runner.consume_messages(topic, timeout=15)

    # Analysis
    total_sent = sent_baseline + sent_no_quorum
    total_consumed = len(messages)

    print_info(f"Baseline: {sent_baseline}, No quorum: {sent_no_quorum}, Consumed: {total_consumed}")

    success = (
        sent_baseline > 0 and
        sent_no_quorum < 10 and
        total_consumed >= sent_baseline * 0.9
    )

    if success:
        print_success("TEST PASSED: Simultaneous failures handled correctly")
    else:
        print_error("TEST FAILED: Unexpected behavior")

    return success


def test_rolling_failure_recovery():
    """Test 3: Rolling failure and recovery"""
    print_header("TEST 3: Rolling Failure and Recovery")
    print_info("Scenario: Kill and recover nodes in a rolling fashion")

    runner = CascadeTestRunner()
    topic = "cascade-rolling"

    print_phase("Phase 0: Verify Cluster")
    if not runner.wait_for_cluster(min_nodes=3, timeout=10):
        print_error("Cluster not running")
        return False

    runner.cluster.print_cluster_status()

    print_phase("Phase 1: Baseline")
    if not runner.create_topic(topic):
        return False

    sent_baseline, _ = runner.produce_messages(topic, count=30, phase="baseline")

    # Rolling failures
    print_phase("Phase 2: Rolling Failures")

    print_info("Kill node 1, produce, recover node 1")
    runner.cluster.kill_node(1, graceful=False)
    time.sleep(2)
    sent_no1, _ = runner.produce_messages(topic, count=30, phase="no_node1")

    print_info("Manual recovery needed for node 1 - simulating by keeping nodes 2,3 up")
    time.sleep(2)

    print_info("Kill node 2 (while node 1 still down = quorum lost)")
    runner.cluster.kill_node(2, graceful=False)
    time.sleep(2)
    runner.cluster.print_cluster_status()

    sent_no12, _ = runner.produce_messages(topic, count=30, phase="no_nodes_12", timeout=15)

    # Consume
    print_phase("Phase 3: Consume after manual restart")
    print_info("Restart cluster and press Enter...")
    input()

    messages = runner.consume_messages(topic, timeout=15)

    total_sent = sent_baseline + sent_no1 + sent_no12
    total_consumed = len(messages)

    print_info(f"Total sent: {total_sent}, consumed: {total_consumed}")

    success = total_consumed >= (sent_baseline + sent_no1) * 0.8

    if success:
        print_success("TEST PASSED: Rolling failures handled")
    else:
        print_error("TEST FAILED")

    return success


def main():
    print_header("Chronik Cascading Failure Tests")

    print(f"{YELLOW}IMPORTANT: This test requires a running cluster{NC}")
    print(f"{YELLOW}Start cluster with: ./test_cluster_manual.sh start{NC}")
    print()

    # Check if cluster is accessible
    try:
        admin = KafkaAdminClient(
            bootstrap_servers="localhost:9092,localhost:9093,localhost:9094",
            request_timeout_ms=5000
        )
        admin.close()
        print_success("Cluster is accessible")
    except Exception as e:
        print_error(f"Cluster not accessible: {e}")
        print_info("Please start cluster first: ./test_cluster_manual.sh start")
        return 1

    results = {}

    print_info("\n" + "="*70)
    print_info("Starting cascading failure tests...")
    print_info("="*70 + "\n")

    results["sequential"] = test_sequential_failure()
    time.sleep(3)

    print_info("\nRestart cluster before next test: ./test_cluster_manual.sh restart")
    input("Press Enter when cluster is ready...")

    results["simultaneous"] = test_simultaneous_failure()
    time.sleep(3)

    print_info("\nRestart cluster before next test: ./test_cluster_manual.sh restart")
    input("Press Enter when cluster is ready...")

    results["rolling"] = test_rolling_failure_recovery()

    # Summary
    print_header("Test Results Summary")
    passed = sum(1 for v in results.values() if v)
    total = len(results)

    for test_name, result in results.items():
        status = f"{GREEN}PASS{NC}" if result else f"{RED}FAIL{NC}"
        print(f"  {test_name:20} {status}")

    print(f"\n{BLUE}Overall: {passed}/{total} tests passed{NC}")

    if passed == total:
        print_success("\n🎉 All cascading failure tests passed!")
        return 0
    else:
        print_error(f"\n⚠️  {total - passed} test(s) failed")
        return 1


if __name__ == "__main__":
    sys.exit(main())
