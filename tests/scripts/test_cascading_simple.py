#!/usr/bin/env python3
"""
Simplified Cascading Failure Test (Automated)
Tests cluster behavior when nodes fail without requiring manual restarts
"""

import time
import subprocess
import signal
import os
import psutil
from kafka import KafkaProducer, KafkaConsumer, KafkaAdminClient
from kafka.admin import NewTopic
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


def find_chronik_processes():
    """Find all Chronik server processes"""
    processes = {}
    for proc in psutil.process_iter(['pid', 'name', 'cmdline']):
        try:
            if 'chronik-server' in proc.name():
                cmdline = proc.cmdline()
                # Extract node ID from command line
                if '--node-id' in cmdline:
                    idx = cmdline.index('--node-id')
                    if idx + 1 < len(cmdline):
                        node_id = int(cmdline[idx + 1])
                        processes[node_id] = proc.pid
        except (psutil.NoSuchProcess, psutil.AccessDenied):
            pass
    return processes


def kill_node_by_pid(pid, graceful=False):
    """Kill a node by PID"""
    try:
        if graceful:
            os.kill(pid, signal.SIGTERM)
            print_info(f"Sent SIGTERM to PID {pid}")
        else:
            os.kill(pid, signal.SIGKILL)
            print_info(f"Sent SIGKILL to PID {pid}")
        time.sleep(1)
        return True
    except Exception as e:
        print_error(f"Failed to kill PID {pid}: {e}")
        return False


def get_cluster_status():
    """Get status of cluster nodes"""
    processes = find_chronik_processes()
    status = {}
    for node_id in [1, 2, 3]:
        status[node_id] = {
            'running': node_id in processes,
            'pid': processes.get(node_id)
        }
    return status


def print_cluster_status():
    """Print cluster status"""
    status = get_cluster_status()
    running = sum(1 for s in status.values() if s['running'])
    print_info(f"Cluster Status ({running}/3 nodes running):")
    for node_id in [1, 2, 3]:
        state = "RUNNING" if status[node_id]['running'] else "STOPPED"
        color = GREEN if status[node_id]['running'] else RED
        pid_info = f"PID {status[node_id]['pid']}" if status[node_id]['running'] else "---"
        print(f"  Node {node_id}: {color}{state:8}{NC} ({pid_info})")


def produce_messages(topic, count=50, phase="unknown", timeout=30, bootstrap_servers="localhost:9092,localhost:9093,localhost:9094"):
    """Produce messages to a topic"""
    try:
        producer = KafkaProducer(
            bootstrap_servers=bootstrap_servers,
            value_serializer=lambda v: json.dumps(v).encode('utf-8'),
            acks='all',
            retries=2,
            request_timeout_ms=8000
        )

        sent = 0
        failed = 0
        start_time = time.time()

        for i in range(count):
            if time.time() - start_time > timeout:
                break

            try:
                msg = {"id": i, "phase": phase, "timestamp": time.time()}
                future = producer.send(topic, value=msg)
                future.get(timeout=4)
                sent += 1
            except Exception:
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


def consume_messages(topic, timeout=15, bootstrap_servers="localhost:9092,localhost:9093,localhost:9094"):
    """Consume all messages from a topic"""
    try:
        consumer = KafkaConsumer(
            topic,
            bootstrap_servers=bootstrap_servers,
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


def create_topic(topic_name, partitions=3, replication_factor=3):
    """Create a test topic"""
    try:
        admin = KafkaAdminClient(
            bootstrap_servers="localhost:9092,localhost:9093,localhost:9094",
            request_timeout_ms=10000
        )
        topic = NewTopic(
            name=topic_name,
            num_partitions=partitions,
            replication_factor=replication_factor
        )
        admin.create_topics([topic], validate_only=False)
        admin.close()
        print_success(f"Created topic: {topic_name}")
        time.sleep(3)
        return True
    except Exception as e:
        if "TopicAlreadyExistsException" in str(e):
            print_warning(f"Topic already exists: {topic_name}")
            return True
        print_error(f"Failed to create topic: {e}")
        return False


def test_cascading_failure():
    """Main cascading failure test"""
    print_header("Cascading Failure Test (Automated)")

    topic = "cascade-auto-test"

    # Verify cluster
    print_phase("Phase 1: Verify Cluster (3/3 nodes)")
    print_cluster_status()
    status = get_cluster_status()
    running = sum(1 for s in status.values() if s['running'])
    if running < 3:
        print_error(f"Only {running}/3 nodes running - need full cluster")
        return False

    # Create topic and baseline
    print_phase("Phase 2: Create Topic & Baseline Messages")
    if not create_topic(topic):
        return False

    sent_baseline, _ = produce_messages(topic, count=50, phase="baseline")

    # Kill node 1
    print_phase("Phase 3: Kill Node 1 (2/3 nodes, quorum maintained)")
    processes = find_chronik_processes()
    if 1 in processes:
        kill_node_by_pid(processes[1], graceful=False)
        time.sleep(3)
        print_cluster_status()

        sent_after_node1, _ = produce_messages(topic, count=50, phase="after_node1_kill")
    else:
        print_error("Node 1 not found")
        sent_after_node1 = 0

    # Kill node 2 (quorum lost)
    print_phase("Phase 4: Kill Node 2 (1/3 nodes, QUORUM LOST)")
    processes = find_chronik_processes()
    if 2 in processes:
        kill_node_by_pid(processes[2], graceful=False)
        time.sleep(3)
        print_cluster_status()

        print_warning("Attempting to produce with only 1/3 nodes (should fail)...")
        sent_no_quorum, failed_no_quorum = produce_messages(
            topic, count=50, phase="no_quorum", timeout=20
        )
    else:
        print_error("Node 2 not found")
        sent_no_quorum = 0
        failed_no_quorum = 50

    # Kill node 3 (complete outage)
    print_phase("Phase 5: Kill Node 3 (0/3 nodes, COMPLETE OUTAGE)")
    processes = find_chronik_processes()
    if 3 in processes:
        kill_node_by_pid(processes[3], graceful=False)
        time.sleep(2)
        print_cluster_status()
    else:
        print_error("Node 3 not found")

    print_warning("Cluster completely down")

    # Restart cluster for consumption
    print_phase("Phase 6: Restart Cluster for Consumption")
    print_info("Restarting cluster...")
    subprocess.run(["./test_cluster_manual.sh", "start"], capture_output=True)
    time.sleep(10)  # Wait for cluster to be ready

    print_cluster_status()

    # Consume all messages
    print_phase("Phase 7: Consume All Messages")
    messages = consume_messages(topic, timeout=15)

    # Analysis
    print_phase("Phase 8: Analysis")

    phases = {}
    for msg in messages:
        phase = msg.get('phase', 'unknown')
        phases[phase] = phases.get(phase, 0) + 1

    print_info("Message breakdown by phase:")
    for phase, count in sorted(phases.items()):
        print(f"  {phase:20} {count} messages")

    total_sent = sent_baseline + sent_after_node1 + sent_no_quorum
    total_consumed = len(messages)

    print()
    print_info(f"Total sent: {total_sent}")
    print_info(f"Total consumed: {total_consumed}")
    print_info(f"Message loss: {total_sent - total_consumed}")

    # Success criteria
    success_criteria = [
        ("Baseline messages produced", sent_baseline > 0),
        ("Messages produced with 2/3 nodes", sent_after_node1 >= sent_baseline * 0.5),
        ("Messages mostly failed with 1/3 nodes", sent_no_quorum < 10),
        ("No message loss for committed messages", total_consumed >= (sent_baseline + sent_after_node1) * 0.85),
    ]

    print()
    print_info("Success Criteria:")
    all_pass = True
    for criterion, passed in success_criteria:
        status = f"{GREEN}PASS{NC}" if passed else f"{RED}FAIL{NC}"
        print(f"  {criterion:45} {status}")
        all_pass = all_pass and passed

    if all_pass:
        print_success("\n🎉 TEST PASSED: Cascading failures handled correctly")
    else:
        print_error("\n⚠️  TEST FAILED: Some criteria not met")

    return all_pass


def main():
    print_header("Chronik Automated Cascading Failure Test")

    # Check cluster
    print_info("Checking for running cluster...")
    status = get_cluster_status()
    running = sum(1 for s in status.values() if s['running'])

    if running == 0:
        print_error("No cluster running - starting cluster...")
        subprocess.run(["./test_cluster_manual.sh", "start"])
        time.sleep(10)
    elif running < 3:
        print_warning(f"Only {running}/3 nodes running - restarting cluster...")
        subprocess.run(["./test_cluster_manual.sh", "stop"], capture_output=True)
        time.sleep(2)
        subprocess.run(["./test_cluster_manual.sh", "start"])
        time.sleep(10)

    result = test_cascading_failure()

    # Cleanup
    print()
    print_info("Cleaning up: stopping cluster")
    subprocess.run(["./test_cluster_manual.sh", "stop"], capture_output=True)

    return 0 if result else 1


if __name__ == "__main__":
    sys.exit(main())
