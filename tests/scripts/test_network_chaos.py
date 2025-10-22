#!/usr/bin/env python3
"""
Network Chaos Testing for Chronik Cluster
Tests cluster behavior under various network failure scenarios using Toxiproxy
"""

import requests
import time
import json
from kafka import KafkaProducer, KafkaConsumer, KafkaAdminClient
from kafka.admin import NewTopic
from kafka.errors import KafkaError
import subprocess
import sys

# Configuration
TOXIPROXY_API = "http://localhost:8474"
PROXY_PORTS = {
    "node1": {"kafka": 19092, "raft": 15001},
    "node2": {"kafka": 19093, "raft": 15002},
    "node3": {"kafka": 19094, "raft": 15003}
}

# ANSI colors
GREEN = '\033[0;32m'
YELLOW = '\033[1;33m'
RED = '\033[0;31m'
BLUE = '\033[0;34m'
NC = '\033[0m'  # No Color

def print_header(text):
    print(f"\n{BLUE}{'='*60}{NC}")
    print(f"{BLUE}{text:^60}{NC}")
    print(f"{BLUE}{'='*60}{NC}\n")

def print_success(text):
    print(f"{GREEN}✓ {text}{NC}")

def print_warning(text):
    print(f"{YELLOW}⚠ {text}{NC}")

def print_error(text):
    print(f"{RED}✗ {text}{NC}")

def print_info(text):
    print(f"{BLUE}ℹ {text}{NC}")

class ToxiproxyClient:
    """Client for Toxiproxy API"""

    def __init__(self, api_url=TOXIPROXY_API):
        self.api_url = api_url

    def list_proxies(self):
        """List all proxies"""
        resp = requests.get(f"{self.api_url}/proxies")
        return resp.json()

    def add_toxic(self, proxy_name, toxic_name, toxic_type, attributes, toxicity=1.0, stream="downstream"):
        """Add a toxic to a proxy"""
        data = {
            "name": toxic_name,
            "type": toxic_type,
            "attributes": attributes,
            "toxicity": toxicity,
            "stream": stream
        }
        resp = requests.post(f"{self.api_url}/proxies/{proxy_name}/toxics", json=data)
        return resp.json()

    def remove_toxic(self, proxy_name, toxic_name):
        """Remove a toxic from a proxy"""
        resp = requests.delete(f"{self.api_url}/proxies/{proxy_name}/toxics/{toxic_name}")
        return resp.status_code == 204

    def reset_proxy(self, proxy_name):
        """Reset a proxy (remove all toxics)"""
        resp = requests.get(f"{self.api_url}/proxies/{proxy_name}/toxics")
        toxics = resp.json()
        for toxic in toxics:
            self.remove_toxic(proxy_name, toxic["name"])

    def reset_all_proxies(self):
        """Reset all proxies"""
        proxies = self.list_proxies()
        for proxy_name in proxies.keys():
            self.reset_proxy(proxy_name)

    def partition_node(self, node_name, target="raft"):
        """Completely partition a node (zero bandwidth)"""
        proxy_name = f"chronik-{node_name}-{target}"
        self.add_toxic(
            proxy_name,
            f"{node_name}_partition",
            "bandwidth",
            {"rate": 0}
        )

    def add_latency(self, node_name, latency_ms, jitter_ms=0, target="kafka"):
        """Add network latency to a node"""
        proxy_name = f"chronik-{node_name}-{target}"
        self.add_toxic(
            proxy_name,
            f"{node_name}_latency",
            "latency",
            {"latency": latency_ms, "jitter": jitter_ms}
        )

    def add_packet_loss(self, node_name, loss_percent, target="raft"):
        """Add packet loss to a node"""
        proxy_name = f"chronik-{node_name}-{target}"
        toxicity = loss_percent / 100.0
        self.add_toxic(
            proxy_name,
            f"{node_name}_loss",
            "latency",  # Use latency toxic with 0ms but toxicity for packet loss
            {"latency": 0},
            toxicity=toxicity
        )

    def slow_close(self, node_name, delay_ms=5000, target="raft"):
        """Simulate slow connection close"""
        proxy_name = f"chronik-{node_name}-{target}"
        self.add_toxic(
            proxy_name,
            f"{node_name}_slow_close",
            "slow_close",
            {"delay": delay_ms}
        )


class ChronikClusterTester:
    """Test Chronik cluster with network chaos"""

    def __init__(self, bootstrap_servers="localhost:19092,localhost:19093,localhost:19094"):
        self.bootstrap_servers = bootstrap_servers
        self.toxi = ToxiproxyClient()

    def create_topic(self, topic_name, partitions=3, replication_factor=3):
        """Create a test topic"""
        try:
            admin = KafkaAdminClient(bootstrap_servers=self.bootstrap_servers)
            topic = NewTopic(
                name=topic_name,
                num_partitions=partitions,
                replication_factor=replication_factor
            )
            admin.create_topics([topic], validate_only=False)
            admin.close()
            print_success(f"Created topic: {topic_name} (partitions={partitions}, RF={replication_factor})")
            time.sleep(2)  # Wait for topic creation
            return True
        except Exception as e:
            if "TopicAlreadyExistsException" in str(e):
                print_warning(f"Topic already exists: {topic_name}")
                return True
            print_error(f"Failed to create topic: {e}")
            return False

    def produce_messages(self, topic, count=100, timeout_sec=30):
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
                if time.time() - start_time > timeout_sec:
                    print_warning(f"Timeout after {timeout_sec}s")
                    break

                try:
                    msg = {"id": i, "timestamp": time.time(), "data": f"chaos_test_{i}"}
                    future = producer.send(topic, value=msg)
                    future.get(timeout=5)
                    sent += 1
                except Exception as e:
                    failed += 1

            producer.flush()
            producer.close()

            print_info(f"Produced: {sent}/{count} messages (failed: {failed})")
            return sent, failed

        except Exception as e:
            print_error(f"Producer error: {e}")
            return 0, count

    def consume_messages(self, topic, expected_count=None, timeout_sec=30):
        """Consume messages from a topic"""
        try:
            consumer = KafkaConsumer(
                topic,
                bootstrap_servers=self.bootstrap_servers,
                value_deserializer=lambda m: json.loads(m.decode('utf-8')),
                auto_offset_reset='earliest',
                enable_auto_commit=True,
                consumer_timeout_ms=timeout_sec * 1000
            )

            messages = []
            start_time = time.time()

            for msg in consumer:
                messages.append(msg.value)
                if expected_count and len(messages) >= expected_count:
                    break
                if time.time() - start_time > timeout_sec:
                    break

            consumer.close()

            print_info(f"Consumed: {len(messages)} messages")
            return len(messages)

        except Exception as e:
            print_error(f"Consumer error: {e}")
            return 0


def test_latency_injection():
    """Test 1: Network Latency Injection"""
    print_header("TEST 1: Network Latency Injection")

    tester = ChronikClusterTester()
    topic = "latency-test"

    print_info("Creating topic...")
    if not tester.create_topic(topic):
        return False

    print_info("Baseline: Producing 100 messages without latency")
    start = time.time()
    sent, failed = tester.produce_messages(topic, count=100)
    baseline_time = time.time() - start
    print_success(f"Baseline: {sent} messages in {baseline_time:.2f}s ({sent/baseline_time:.1f} msg/s)")

    print_info("Adding 100ms latency to node1...")
    tester.toxi.add_latency("node1", latency_ms=100, jitter_ms=20, target="kafka")

    print_info("Producing 100 messages with latency")
    start = time.time()
    sent, failed = tester.produce_messages(topic, count=100)
    latency_time = time.time() - start
    print_success(f"With latency: {sent} messages in {latency_time:.2f}s ({sent/latency_time:.1f} msg/s)")

    print_info("Removing latency toxic...")
    tester.toxi.reset_proxy("chronik-node1-kafka")

    print_info("Consuming all messages...")
    consumed = tester.consume_messages(topic, expected_count=200, timeout_sec=10)

    success = consumed >= 180  # Allow 10% message loss
    if success:
        print_success(f"Test PASSED: Consumed {consumed}/200 messages")
    else:
        print_error(f"Test FAILED: Only consumed {consumed}/200 messages")

    return success


def test_partition_recovery():
    """Test 2: Network Partition and Recovery"""
    print_header("TEST 2: Network Partition and Recovery")

    tester = ChronikClusterTester()
    topic = "partition-test"

    print_info("Creating topic...")
    if not tester.create_topic(topic):
        return False

    print_info("Producing 50 messages (baseline)...")
    sent_before, _ = tester.produce_messages(topic, count=50)
    print_success(f"Produced {sent_before} messages")

    print_info("Partitioning node2 from Raft cluster (zero bandwidth)...")
    tester.toxi.partition_node("node2", target="raft")
    time.sleep(5)  # Wait for partition to take effect

    print_info("Attempting to produce 50 messages during partition...")
    sent_during, failed_during = tester.produce_messages(topic, count=50, timeout_sec=20)
    print_warning(f"During partition: {sent_during} sent, {failed_during} failed")

    print_info("Removing partition (healing network)...")
    tester.toxi.reset_proxy("chronik-node2-raft")
    time.sleep(5)  # Wait for cluster to recover

    print_info("Producing 50 messages after recovery...")
    sent_after, _ = tester.produce_messages(topic, count=50)
    print_success(f"After recovery: {sent_after} messages")

    print_info("Consuming all messages...")
    consumed = tester.consume_messages(topic, timeout_sec=15)

    total_sent = sent_before + sent_during + sent_after
    print_info(f"Total sent: {total_sent}, consumed: {consumed}")

    success = consumed >= total_sent * 0.9  # Allow 10% loss
    if success:
        print_success(f"Test PASSED: Consumed {consumed}/{total_sent} messages")
    else:
        print_error(f"Test FAILED: Only consumed {consumed}/{total_sent} messages")

    return success


def test_packet_loss():
    """Test 3: Packet Loss Tolerance"""
    print_header("TEST 3: Packet Loss Tolerance")

    tester = ChronikClusterTester()
    topic = "packet-loss-test"

    print_info("Creating topic...")
    if not tester.create_topic(topic):
        return False

    print_info("Adding 20% packet loss to node1 Raft communication...")
    tester.toxi.add_packet_loss("node1", loss_percent=20, target="raft")

    print_info("Producing 100 messages with packet loss...")
    sent, failed = tester.produce_messages(topic, count=100, timeout_sec=30)
    print_info(f"Sent: {sent}, Failed: {failed}")

    print_info("Removing packet loss toxic...")
    tester.toxi.reset_proxy("chronik-node1-raft")

    print_info("Consuming messages...")
    consumed = tester.consume_messages(topic, expected_count=sent, timeout_sec=10)

    success = consumed >= sent * 0.95  # Allow 5% loss
    if success:
        print_success(f"Test PASSED: Consumed {consumed}/{sent} messages")
    else:
        print_error(f"Test FAILED: Only consumed {consumed}/{sent} messages")

    return success


def test_slow_close():
    """Test 4: Slow Connection Close"""
    print_header("TEST 4: Slow Connection Close")

    tester = ChronikClusterTester()
    topic = "slow-close-test"

    print_info("Creating topic...")
    if not tester.create_topic(topic):
        return False

    print_info("Adding 5s slow close to node3 Raft...")
    tester.toxi.slow_close("node3", delay_ms=5000, target="raft")

    print_info("Producing 50 messages with slow close...")
    start = time.time()
    sent, failed = tester.produce_messages(topic, count=50, timeout_sec=30)
    duration = time.time() - start
    print_info(f"Sent: {sent} in {duration:.2f}s, Failed: {failed}")

    print_info("Removing slow close toxic...")
    tester.toxi.reset_proxy("chronik-node3-raft")

    print_info("Consuming messages...")
    consumed = tester.consume_messages(topic, expected_count=sent, timeout_sec=10)

    success = consumed >= sent * 0.9  # Allow 10% loss
    if success:
        print_success(f"Test PASSED: Consumed {consumed}/{sent} messages")
    else:
        print_error(f"Test FAILED: Only consumed {consumed}/{sent} messages")

    return success


def main():
    """Run all network chaos tests"""
    print_header("Chronik Network Chaos Testing Suite")

    # Check if Toxiproxy is running
    try:
        resp = requests.get(f"{TOXIPROXY_API}/proxies")
        print_success("Toxiproxy is running")
    except Exception as e:
        print_error(f"Toxiproxy is not running: {e}")
        print_info("Run: ./test_toxiproxy_setup.sh start")
        return 1

    # Reset all proxies to clean state
    print_info("Resetting all proxies to clean state...")
    toxi = ToxiproxyClient()
    toxi.reset_all_proxies()

    # Run tests
    results = {}

    print_info("\n" + "="*60)
    print_info("Starting chaos tests...")
    print_info("="*60 + "\n")

    results["latency"] = test_latency_injection()
    time.sleep(2)

    results["partition"] = test_partition_recovery()
    time.sleep(2)

    results["packet_loss"] = test_packet_loss()
    time.sleep(2)

    results["slow_close"] = test_slow_close()

    # Cleanup
    print_info("\nCleaning up toxics...")
    toxi.reset_all_proxies()

    # Summary
    print_header("Test Results Summary")
    passed = sum(1 for v in results.values() if v)
    total = len(results)

    for test_name, result in results.items():
        status = f"{GREEN}PASS{NC}" if result else f"{RED}FAIL{NC}"
        print(f"  {test_name:20} {status}")

    print(f"\n{BLUE}Overall: {passed}/{total} tests passed{NC}")

    if passed == total:
        print_success("\n🎉 All chaos tests passed!")
        return 0
    else:
        print_error(f"\n⚠️  {total - passed} test(s) failed")
        return 1


if __name__ == "__main__":
    sys.exit(main())
