#!/usr/bin/env python3
"""
Targeted Raft Network Partition Test
Tests Raft consensus behavior during network partitions without proxying Kafka traffic
"""

import requests
import time
import subprocess
from kafka import KafkaProducer, KafkaConsumer, KafkaAdminClient
from kafka.admin import NewTopic
import json

TOXIPROXY_API = "http://localhost:8474"

# ANSI colors
GREEN = '\033[0;32m'
YELLOW = '\033[1;33m'
RED = '\033[0;31m'
BLUE = '\033[0;34m'
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

def partition_node_raft(node_id):
    """Partition a node's Raft communication (zero bandwidth)"""
    proxy_name = f"chronik-node{node_id}-raft"
    data = {
        "name": f"partition_{node_id}",
        "type": "bandwidth",
        "attributes": {"rate": 0}
    }
    resp = requests.post(f"{TOXIPROXY_API}/proxies/{proxy_name}/toxics", json=data)
    if resp.status_code == 200:
        print_success(f"Partitioned node {node_id} from Raft cluster")
    else:
        print_error(f"Failed to partition node {node_id}: {resp.text}")

def heal_partition(node_id):
    """Remove partition from a node"""
    proxy_name = f"chronik-node{node_id}-raft"
    toxic_name = f"partition_{node_id}"
    resp = requests.delete(f"{TOXIPROXY_API}/proxies/{proxy_name}/toxics/{toxic_name}")
    if resp.status_code == 204:
        print_success(f"Healed partition for node {node_id}")
    else:
        print_warning(f"Could not remove partition (may not exist)")

def get_raft_metrics(port):
    """Get Raft metrics from Prometheus endpoint"""
    try:
        resp = requests.get(f"http://localhost:{port}/metrics", timeout=2)
        metrics = {}
        for line in resp.text.split('\n'):
            if line.startswith('chronik_raft_'):
                parts = line.split()
                if len(parts) >= 2:
                    metric_name = parts[0].split('{')[0]
                    value = parts[-1]
                    metrics[metric_name] = float(value)
        return metrics
    except Exception as e:
        return {}

def check_cluster_health():
    """Check which nodes are leaders"""
    print_info("Checking cluster health...")

    for node_id, port in [(1, 9101), (2, 9102), (3, 9103)]:
        metrics = get_raft_metrics(port)
        if 'chronik_raft_is_leader' in metrics:
            is_leader = metrics['chronik_raft_is_leader'] == 1.0
            status = "LEADER" if is_leader else "FOLLOWER"
            print(f"  Node {node_id} (port {port-9}): {status}")
        else:
            print(f"  Node {node_id}: Unable to get metrics")

def test_single_node_partition():
    """Test 1: Partition single node, verify cluster maintains quorum"""
    print_header("TEST 1: Single Node Partition (Quorum Maintained)")

    # Direct connection to cluster (no proxy for Kafka)
    bootstrap_servers = "localhost:9092,localhost:9093,localhost:9094"

    print_info("Initial cluster state:")
    check_cluster_health()

    # Create topic
    print_info("Creating test topic...")
    try:
        admin = KafkaAdminClient(bootstrap_servers=bootstrap_servers)
        topic = NewTopic(name="partition-test-1", num_partitions=3, replication_factor=3)
        admin.create_topics([topic], validate_only=False)
        admin.close()
        print_success("Topic created: partition-test-1")
        time.sleep(3)  # Wait for replicas
    except Exception as e:
        if "TopicAlreadyExistsException" in str(e):
            print_warning("Topic already exists")
        else:
            print_error(f"Failed to create topic: {e}")

    # Produce baseline messages
    print_info("Producing 50 baseline messages...")
    try:
        producer = KafkaProducer(
            bootstrap_servers=bootstrap_servers,
            value_serializer=lambda v: json.dumps(v).encode('utf-8'),
            acks='all',
            retries=3,
            request_timeout_ms=10000
        )

        sent = 0
        for i in range(50):
            try:
                msg = {"id": i, "phase": "baseline"}
                future = producer.send("partition-test-1", value=msg)
                future.get(timeout=5)
                sent += 1
            except Exception as e:
                pass

        producer.flush()
        producer.close()
        print_success(f"Produced {sent}/50 baseline messages")
    except Exception as e:
        print_error(f"Producer error: {e}")
        sent = 0

    # Partition node 2 (Raft only, not Kafka)
    print_info("Partitioning Node 2 from Raft cluster...")
    partition_node_raft(2)
    time.sleep(5)  # Wait for partition to take effect

    print_info("Cluster state after partition:")
    check_cluster_health()

    # Produce during partition (should work - quorum is 2/3)
    print_info("Producing 50 messages during partition...")
    try:
        producer = KafkaProducer(
            bootstrap_servers=bootstrap_servers,
            value_serializer=lambda v: json.dumps(v).encode('utf-8'),
            acks='all',
            retries=3,
            request_timeout_ms=10000
        )

        sent_partition = 0
        for i in range(50, 100):
            try:
                msg = {"id": i, "phase": "during_partition"}
                future = producer.send("partition-test-1", value=msg)
                future.get(timeout=5)
                sent_partition += 1
            except Exception as e:
                pass

        producer.flush()
        producer.close()
        print_success(f"Produced {sent_partition}/50 messages during partition")
    except Exception as e:
        print_error(f"Producer error: {e}")
        sent_partition = 0

    # Heal partition
    print_info("Healing partition...")
    heal_partition(2)
    time.sleep(5)  # Wait for recovery

    print_info("Cluster state after recovery:")
    check_cluster_health()

    # Produce after recovery
    print_info("Producing 50 messages after recovery...")
    try:
        producer = KafkaProducer(
            bootstrap_servers=bootstrap_servers,
            value_serializer=lambda v: json.dumps(v).encode('utf-8'),
            acks='all',
            retries=3,
            request_timeout_ms=10000
        )

        sent_recovery = 0
        for i in range(100, 150):
            try:
                msg = {"id": i, "phase": "after_recovery"}
                future = producer.send("partition-test-1", value=msg)
                future.get(timeout=5)
                sent_recovery += 1
            except Exception as e:
                pass

        producer.flush()
        producer.close()
        print_success(f"Produced {sent_recovery}/50 messages after recovery")
    except Exception as e:
        print_error(f"Producer error: {e}")
        sent_recovery = 0

    # Consume all messages
    print_info("Consuming all messages...")
    try:
        consumer = KafkaConsumer(
            "partition-test-1",
            bootstrap_servers=bootstrap_servers,
            value_deserializer=lambda m: json.loads(m.decode('utf-8')),
            auto_offset_reset='earliest',
            consumer_timeout_ms=10000
        )

        messages = []
        for msg in consumer:
            messages.append(msg.value)

        consumer.close()
        print_success(f"Consumed {len(messages)} messages")

        # Verify phases
        baseline = len([m for m in messages if m.get('phase') == 'baseline'])
        during = len([m for m in messages if m.get('phase') == 'during_partition'])
        after = len([m for m in messages if m.get('phase') == 'after_recovery'])

        print_info(f"Message breakdown:")
        print(f"  Baseline: {baseline}/{sent}")
        print(f"  During partition: {during}/{sent_partition}")
        print(f"  After recovery: {after}/{sent_recovery}")

    except Exception as e:
        print_error(f"Consumer error: {e}")
        messages = []

    # Results
    total_sent = sent + sent_partition + sent_recovery
    total_consumed = len(messages)

    success = (total_consumed >= total_sent * 0.95 and
               sent_partition >= 40)  # Should work during partition

    if success:
        print_success(f"TEST PASSED: {total_consumed}/{total_sent} messages, cluster maintained quorum")
    else:
        print_error(f"TEST FAILED: Only {total_consumed}/{total_sent} messages, or partition blocked writes")

    return success


def test_majority_partition():
    """Test 2: Partition 2/3 nodes, verify cluster becomes unavailable"""
    print_header("TEST 2: Majority Partition (Quorum Lost)")

    bootstrap_servers = "localhost:9092,localhost:9093,localhost:9094"

    print_info("Initial cluster state:")
    check_cluster_health()

    # Create topic
    print_info("Creating test topic...")
    try:
        admin = KafkaAdminClient(bootstrap_servers=bootstrap_servers, request_timeout_ms=5000)
        topic = NewTopic(name="partition-test-2", num_partitions=3, replication_factor=3)
        admin.create_topics([topic], validate_only=False)
        admin.close()
        print_success("Topic created: partition-test-2")
        time.sleep(3)
    except Exception as e:
        if "TopicAlreadyExistsException" in str(e):
            print_warning("Topic already exists")
        else:
            print_error(f"Failed to create topic: {e}")

    # Produce baseline
    print_info("Producing 50 baseline messages...")
    try:
        producer = KafkaProducer(
            bootstrap_servers=bootstrap_servers,
            value_serializer=lambda v: json.dumps(v).encode('utf-8'),
            acks='all',
            retries=2,
            request_timeout_ms=10000
        )

        sent = 0
        for i in range(50):
            try:
                msg = {"id": i, "phase": "baseline"}
                future = producer.send("partition-test-2", value=msg)
                future.get(timeout=5)
                sent += 1
            except Exception as e:
                pass

        producer.flush()
        producer.close()
        print_success(f"Produced {sent}/50 baseline messages")
    except Exception as e:
        print_error(f"Producer error: {e}")
        sent = 0

    # Partition nodes 2 and 3 (quorum lost!)
    print_info("Partitioning Nodes 2 and 3 from Raft cluster (QUORUM LOST)...")
    partition_node_raft(2)
    partition_node_raft(3)
    time.sleep(5)

    print_info("Cluster state after partition (expecting no leader):")
    check_cluster_health()

    # Try to produce (should fail or have very low success rate)
    print_info("Attempting to produce 50 messages without quorum...")
    try:
        producer = KafkaProducer(
            bootstrap_servers=bootstrap_servers,
            value_serializer=lambda v: json.dumps(v).encode('utf-8'),
            acks='all',
            retries=1,
            request_timeout_ms=5000  # Lower timeout
        )

        sent_no_quorum = 0
        for i in range(50, 100):
            try:
                msg = {"id": i, "phase": "no_quorum"}
                future = producer.send("partition-test-2", value=msg)
                future.get(timeout=3)
                sent_no_quorum += 1
            except Exception as e:
                pass

        producer.flush()
        producer.close()
        print_warning(f"Produced {sent_no_quorum}/50 messages without quorum (expected: low)")
    except Exception as e:
        print_warning(f"Producer mostly failed (expected): {e}")
        sent_no_quorum = 0

    # Heal one node to restore quorum
    print_info("Healing Node 2 to restore quorum...")
    heal_partition(2)
    time.sleep(5)

    print_info("Cluster state after partial recovery (2/3 nodes):")
    check_cluster_health()

    # Produce after recovery
    print_info("Producing 50 messages after quorum restored...")
    try:
        producer = KafkaProducer(
            bootstrap_servers=bootstrap_servers,
            value_serializer=lambda v: json.dumps(v).encode('utf-8'),
            acks='all',
            retries=3,
            request_timeout_ms=10000
        )

        sent_recovery = 0
        for i in range(100, 150):
            try:
                msg = {"id": i, "phase": "after_recovery"}
                future = producer.send("partition-test-2", value=msg)
                future.get(timeout=5)
                sent_recovery += 1
            except Exception as e:
                pass

        producer.flush()
        producer.close()
        print_success(f"Produced {sent_recovery}/50 messages after recovery")
    except Exception as e:
        print_error(f"Producer error: {e}")
        sent_recovery = 0

    # Full heal
    print_info("Healing Node 3...")
    heal_partition(3)
    time.sleep(3)

    print_info("Final cluster state:")
    check_cluster_health()

    # Consume all
    print_info("Consuming all messages...")
    try:
        consumer = KafkaConsumer(
            "partition-test-2",
            bootstrap_servers=bootstrap_servers,
            value_deserializer=lambda m: json.loads(m.decode('utf-8')),
            auto_offset_reset='earliest',
            consumer_timeout_ms=10000
        )

        messages = []
        for msg in consumer:
            messages.append(msg.value)

        consumer.close()
        print_success(f"Consumed {len(messages)} messages")

    except Exception as e:
        print_error(f"Consumer error: {e}")
        messages = []

    # Results
    success = (sent_no_quorum < 10 and  # Should mostly fail without quorum
               sent_recovery >= 40 and  # Should work after quorum restored
               len(messages) >= sent * 0.9)  # Should get baseline messages

    if success:
        print_success(f"TEST PASSED: Cluster correctly rejected writes without quorum, recovered after heal")
    else:
        print_error(f"TEST FAILED: Cluster behavior unexpected during quorum loss")

    return success


def main():
    print_header("Chronik Raft Network Partition Tests")

    # Check Toxiproxy
    try:
        resp = requests.get(f"{TOXIPROXY_API}/proxies")
        print_success("Toxiproxy is running")
    except Exception as e:
        print_error(f"Toxiproxy is not running: {e}")
        return 1

    # Clean up any existing toxics
    print_info("Cleaning up existing toxics...")
    for node_id in [1, 2, 3]:
        heal_partition(node_id)

    print("\n" + "="*70)
    print("Starting Raft partition tests...")
    print("="*70 + "\n")

    results = {}

    results["single_partition"] = test_single_node_partition()
    time.sleep(3)

    results["majority_partition"] = test_majority_partition()

    # Summary
    print_header("Test Results Summary")
    passed = sum(1 for v in results.values() if v)
    total = len(results)

    for test_name, result in results.items():
        status = f"{GREEN}PASS{NC}" if result else f"{RED}FAIL{NC}"
        print(f"  {test_name:25} {status}")

    print(f"\n{BLUE}Overall: {passed}/{total} tests passed{NC}")

    if passed == total:
        print_success("\n🎉 All Raft partition tests passed!")
        return 0
    else:
        print_error(f"\n⚠️  {total - passed} test(s) failed")
        return 1


if __name__ == "__main__":
    import sys
    sys.exit(main())
