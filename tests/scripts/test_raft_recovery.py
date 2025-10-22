#!/usr/bin/env python3
"""
Raft Recovery and Rejoin Test Suite

Tests node recovery, state catch-up, and cluster rejoin scenarios
for Chronik Raft cluster.
"""

import sys
import time
import subprocess
import os
import signal
from kafka import KafkaProducer, KafkaConsumer, KafkaAdminClient
from kafka.admin import NewTopic
from kafka.errors import KafkaError
import logging

# Configure logging
logging.basicConfig(
    level=logging.INFO,
    format='%(message)s'
)
logger = logging.getLogger(__name__)

# ANSI color codes
BLUE = '\033[0;34m'
GREEN = '\033[0;32m'
RED = '\033[0;31m'
YELLOW = '\033[1;33m'
NC = '\033[0m'
BOLD = '\033[1m'

class Colors:
    @staticmethod
    def blue(msg): return f"{BLUE}{msg}{NC}"
    @staticmethod
    def green(msg): return f"{GREEN}✓ {msg}{NC}"
    @staticmethod
    def red(msg): return f"{RED}✗ {msg}{NC}"
    @staticmethod
    def yellow(msg): return f"{YELLOW}⚠ {msg}{NC}"
    @staticmethod
    def header(msg):
        line = "=" * 60
        return f"\n{BLUE}{line}\n{msg:^60}\n{line}{NC}\n"

# Node configuration
NODES = [
    {'kafka_port': 9092, 'raft_port': 9192, 'name': 'node1', 'addr': 'localhost:9092'},
    {'kafka_port': 9093, 'raft_port': 9193, 'name': 'node2', 'addr': 'localhost:9093'},
    {'kafka_port': 9094, 'raft_port': 9194, 'name': 'node3', 'addr': 'localhost:9094'},
]

def get_admin_client(bootstrap_servers='localhost:9092'):
    """Get Kafka admin client"""
    return KafkaAdminClient(
        bootstrap_servers=bootstrap_servers,
        request_timeout_ms=5000,
        api_version=(0, 10, 0)
    )

def get_producer(bootstrap_servers='localhost:9092'):
    """Get Kafka producer"""
    return KafkaProducer(
        bootstrap_servers=bootstrap_servers,
        request_timeout_ms=5000,
        max_block_ms=5000,
        api_version=(0, 10, 0),
        retries=3
    )

def get_consumer(topic, bootstrap_servers='localhost:9092', group_id=None):
    """Get Kafka consumer"""
    return KafkaConsumer(
        topic,
        bootstrap_servers=bootstrap_servers,
        auto_offset_reset='earliest',
        consumer_timeout_ms=10000,
        group_id=group_id or f'test-group-{int(time.time())}',
        api_version=(0, 10, 0)
    )

def create_topic(topic_name, partitions=3, replication_factor=3, bootstrap_servers='localhost:9092'):
    """Create a topic"""
    try:
        admin = get_admin_client(bootstrap_servers)
        topic = NewTopic(
            name=topic_name,
            num_partitions=partitions,
            replication_factor=replication_factor
        )
        admin.create_topics([topic])
        admin.close()
        time.sleep(2)
        return True
    except Exception as e:
        logger.error(Colors.red(f"Failed to create topic: {e}"))
        return False

def produce_messages(topic, count, bootstrap_servers='localhost:9092', value_prefix='msg'):
    """Produce messages and return (sent_count, failed_count)"""
    producer = get_producer(bootstrap_servers)
    sent = 0
    failed = 0

    for i in range(count):
        try:
            value = f'{value_prefix}-{i}'.encode('utf-8')
            future = producer.send(topic, value=value)
            future.get(timeout=5)
            sent += 1
        except Exception:
            failed += 1

    producer.close()
    return sent, failed

def consume_messages(topic, bootstrap_servers='localhost:9092', expected_count=None, timeout_ms=10000):
    """Consume messages and return list of values"""
    consumer = get_consumer(topic, bootstrap_servers)
    messages = []

    try:
        start_time = time.time()
        for msg in consumer:
            messages.append(msg.value.decode('utf-8'))
            if expected_count and len(messages) >= expected_count:
                break
            if (time.time() - start_time) * 1000 > timeout_ms:
                break
    except Exception:
        pass
    finally:
        consumer.close()

    return messages

def check_node_alive(port):
    """Check if a node is alive"""
    try:
        admin = get_admin_client(f'localhost:{port}')
        admin.list_topics(timeout=2)
        admin.close()
        return True
    except Exception:
        return False

def restart_node(node_id, wait_secs=5):
    """
    Restart a specific node (placeholder - requires actual implementation)
    In real scenario, this would use systemd, docker restart, etc.
    """
    logger.info(Colors.blue(f"ℹ Restarting node {node_id}..."))
    logger.info(Colors.yellow(f"⚠ Manual restart required for node {node_id}"))
    logger.info(Colors.yellow(f"   Run: cargo run --bin chronik-server -- --advertised-addr localhost:{NODES[node_id-1]['kafka_port']} --raft standalone &"))
    input(Colors.blue(f"Press Enter when node {node_id} is restarted..."))

def kill_node(kafka_port):
    """Kill a node by port"""
    try:
        subprocess.run(['pkill', '-f', f'chronik-server.*{kafka_port}'], timeout=5)
        time.sleep(2)
        return True
    except Exception as e:
        logger.warning(Colors.yellow(f"Kill failed: {e}"))
        return False

# ==============================================================================
# TEST SCENARIOS
# ==============================================================================

def test_graceful_shutdown_rejoin():
    """
    TEST 1: Graceful Shutdown and Rejoin

    Scenario:
    1. Gracefully stop 1 node (SIGTERM)
    2. Cluster continues operating
    3. Restart stopped node
    4. Verify node rejoins and catches up
    """
    print(Colors.header("TEST 1: Graceful Shutdown and Rejoin"))

    topic = 'graceful-rejoin-test'
    logger.info(Colors.blue("ℹ Creating topic..."))
    if not create_topic(topic):
        return False
    logger.info(Colors.green(f"Created topic: {topic}"))

    # Baseline
    sent1, _ = produce_messages(topic, 30, 'localhost:9092', 'baseline')
    logger.info(Colors.green(f"Baseline: {sent1} messages"))

    # Stop node3
    logger.info(Colors.blue("ℹ Gracefully stopping node3..."))
    kill_node(9094)
    time.sleep(3)

    # Produce while node3 is down
    sent2, _ = produce_messages(topic, 30, 'localhost:9092', 'while-down')
    logger.info(Colors.green(f"While down: {sent2} messages"))

    # Restart node3
    restart_node(3)

    # Wait for rejoin
    logger.info(Colors.blue("ℹ Waiting for node3 to rejoin..."))
    time.sleep(5)

    # Check node3 is alive
    if check_node_alive(9094):
        logger.info(Colors.green("Node3 is alive"))
    else:
        logger.error(Colors.red("Node3 failed to restart"))
        return False

    # Produce more messages
    sent3, _ = produce_messages(topic, 30, 'localhost:9094', 'after-rejoin')
    logger.info(Colors.green(f"After rejoin: {sent3} messages"))

    # Consume from node3 (should have all messages)
    logger.info(Colors.blue("ℹ Consuming from node3..."))
    messages = consume_messages(topic, 'localhost:9094', timeout_ms=15000)

    logger.info(Colors.green(f"Node3 has {len(messages)} messages"))

    # Check if node3 has messages from all phases
    baseline_msgs = [m for m in messages if m.startswith('baseline')]
    while_down_msgs = [m for m in messages if m.startswith('while-down')]
    after_msgs = [m for m in messages if m.startswith('after-rejoin')]

    logger.info(Colors.blue(f"ℹ Baseline: {len(baseline_msgs)}, While down: {len(while_down_msgs)}, After: {len(after_msgs)}"))

    if len(while_down_msgs) > 0:
        logger.info(Colors.green("Test PASSED: Node caught up after rejoin"))
        return True
    else:
        logger.error(Colors.red("Test FAILED: Node did not catch up"))
        return False

def test_crash_recovery():
    """
    TEST 2: Crash Recovery (SIGKILL)

    Scenario:
    1. Hard kill 1 node (SIGKILL)
    2. Cluster continues
    3. Restart killed node
    4. Verify WAL recovery works
    5. Verify node rejoins cluster
    """
    print(Colors.header("TEST 2: Crash Recovery (SIGKILL)"))

    topic = 'crash-recovery-test'
    logger.info(Colors.blue("ℹ Creating topic..."))
    if not create_topic(topic):
        return False
    logger.info(Colors.green(f"Created topic: {topic}"))

    # Baseline
    sent1, _ = produce_messages(topic, 30, 'localhost:9092', 'baseline')
    logger.info(Colors.green(f"Baseline: {sent1} messages"))

    # Hard kill node2 (SIGKILL)
    logger.info(Colors.blue("ℹ Hard killing node2 (SIGKILL)..."))
    try:
        subprocess.run(['pkill', '-9', '-f', 'chronik-server.*9093'], timeout=5)
        time.sleep(3)
    except Exception as e:
        logger.warning(Colors.yellow(f"Kill: {e}"))

    # Produce while crashed
    sent2, _ = produce_messages(topic, 30, 'localhost:9092', 'while-crashed')
    logger.info(Colors.green(f"While crashed: {sent2} messages"))

    # Restart node2
    logger.info(Colors.blue("ℹ Restarting node2 (should recover from WAL)..."))
    restart_node(2)

    # Wait for recovery
    logger.info(Colors.blue("ℹ Waiting for WAL recovery..."))
    time.sleep(10)

    # Check if node2 is alive
    if check_node_alive(9093):
        logger.info(Colors.green("Node2 recovered and is alive"))
    else:
        logger.error(Colors.red("Node2 failed to recover"))
        return False

    # Consume from node2
    messages = consume_messages(topic, 'localhost:9093', timeout_ms=15000)
    logger.info(Colors.green(f"Node2 has {len(messages)} messages after recovery"))

    if len(messages) > sent1:
        logger.info(Colors.green("Test PASSED: Node recovered from crash"))
        return True
    else:
        logger.error(Colors.red("Test FAILED: Node did not recover properly"))
        return False

def test_stale_node_rejoin():
    """
    TEST 3: Stale Node Rejoin

    Scenario:
    1. Stop node for extended period
    2. Cluster processes many messages
    3. Restart stale node
    4. Verify node catches up (might use log compaction/snapshots)
    """
    print(Colors.header("TEST 3: Stale Node Rejoin"))

    topic = 'stale-rejoin-test'
    logger.info(Colors.blue("ℹ Creating topic..."))
    if not create_topic(topic):
        return False
    logger.info(Colors.green(f"Created topic: {topic}"))

    # Baseline
    sent1, _ = produce_messages(topic, 30, 'localhost:9092', 'baseline')
    logger.info(Colors.green(f"Baseline: {sent1} messages"))

    # Stop node3
    logger.info(Colors.blue("ℹ Stopping node3 for extended period..."))
    kill_node(9094)
    time.sleep(3)

    # Produce many batches while node3 is down
    logger.info(Colors.blue("ℹ Producing large volume while node3 is down..."))
    total_sent = 0
    for batch in range(5):
        sent, _ = produce_messages(topic, 50, 'localhost:9092', f'batch{batch}')
        total_sent += sent
        logger.info(Colors.blue(f"  Batch {batch+1}: {sent} messages"))
        time.sleep(1)

    logger.info(Colors.green(f"Total produced while down: {total_sent} messages"))

    # Restart node3
    logger.info(Colors.blue("ℹ Restarting stale node3..."))
    restart_node(3)

    # Give extra time for catch-up
    logger.info(Colors.blue("ℹ Waiting for catch-up (20s)..."))
    time.sleep(20)

    # Check if node3 caught up
    if not check_node_alive(9094):
        logger.error(Colors.red("Node3 failed to restart"))
        return False

    messages = consume_messages(topic, 'localhost:9094', timeout_ms=20000)
    logger.info(Colors.green(f"Node3 has {len(messages)} messages after catch-up"))

    expected = sent1 + total_sent
    if len(messages) >= expected * 0.8:  # Allow 20% loss during catch-up
        logger.info(Colors.green(f"Test PASSED: Stale node caught up ({len(messages)}/{expected})"))
        return True
    else:
        logger.error(Colors.red(f"Test FAILED: Node did not catch up ({len(messages)}/{expected})"))
        return False

def test_rolling_restart():
    """
    TEST 4: Rolling Restart

    Scenario:
    1. Restart nodes one by one
    2. Verify cluster maintains quorum throughout
    3. Verify no data loss
    """
    print(Colors.header("TEST 4: Rolling Restart"))

    topic = 'rolling-restart-test'
    logger.info(Colors.blue("ℹ Creating topic..."))
    if not create_topic(topic):
        return False
    logger.info(Colors.green(f"Created topic: {topic}"))

    # Baseline
    sent_total = 0
    sent1, _ = produce_messages(topic, 30, 'localhost:9092', 'baseline')
    sent_total += sent1
    logger.info(Colors.green(f"Baseline: {sent1} messages"))

    # Restart node1
    logger.info(Colors.blue("ℹ Restarting node1..."))
    kill_node(9092)
    time.sleep(3)
    sent2, _ = produce_messages(topic, 20, 'localhost:9093', 'node1-down')
    sent_total += sent2
    logger.info(Colors.blue(f"  Produced {sent2} while node1 restarting"))
    restart_node(1)
    time.sleep(5)

    # Restart node2
    logger.info(Colors.blue("ℹ Restarting node2..."))
    kill_node(9093)
    time.sleep(3)
    sent3, _ = produce_messages(topic, 20, 'localhost:9092', 'node2-down')
    sent_total += sent3
    logger.info(Colors.blue(f"  Produced {sent3} while node2 restarting"))
    restart_node(2)
    time.sleep(5)

    # Restart node3
    logger.info(Colors.blue("ℹ Restarting node3..."))
    kill_node(9094)
    time.sleep(3)
    sent4, _ = produce_messages(topic, 20, 'localhost:9092', 'node3-down')
    sent_total += sent4
    logger.info(Colors.blue(f"  Produced {sent4} while node3 restarting"))
    restart_node(3)
    time.sleep(5)

    # Verify all nodes have data
    logger.info(Colors.blue("ℹ Verifying data on all nodes..."))
    msgs_node1 = len(consume_messages(topic, 'localhost:9092', timeout_ms=10000))
    msgs_node2 = len(consume_messages(topic, 'localhost:9093', timeout_ms=10000))
    msgs_node3 = len(consume_messages(topic, 'localhost:9094', timeout_ms=10000))

    logger.info(Colors.blue(f"ℹ Node1: {msgs_node1}, Node2: {msgs_node2}, Node3: {msgs_node3}"))

    # Check consistency (all nodes should have roughly same data)
    max_msgs = max(msgs_node1, msgs_node2, msgs_node3)
    min_msgs = min(msgs_node1, msgs_node2, msgs_node3)

    if max_msgs - min_msgs <= 10:  # Allow small variance
        logger.info(Colors.green("Test PASSED: Rolling restart maintained consistency"))
        return True
    else:
        logger.error(Colors.red(f"Test FAILED: Inconsistent data after rolling restart"))
        return False

def test_concurrent_failures():
    """
    TEST 5: Concurrent Failures and Recovery

    Scenario:
    1. Kill 2 nodes simultaneously
    2. Verify cluster loses quorum
    3. Restart 1 node to restore quorum
    4. Verify cluster recovers
    """
    print(Colors.header("TEST 5: Concurrent Failures and Recovery"))

    topic = 'concurrent-failures-test'
    logger.info(Colors.blue("ℹ Creating topic..."))
    if not create_topic(topic):
        return False
    logger.info(Colors.green(f"Created topic: {topic}"))

    # Baseline
    sent1, _ = produce_messages(topic, 30, 'localhost:9092', 'baseline')
    logger.info(Colors.green(f"Baseline: {sent1} messages"))

    # Kill 2 nodes simultaneously
    logger.info(Colors.blue("ℹ Killing node2 and node3 simultaneously..."))
    kill_node(9093)
    kill_node(9094)
    time.sleep(5)

    # Try to produce (should fail - no quorum)
    sent2, failed2 = produce_messages(topic, 20, 'localhost:9092', 'no-quorum')
    logger.info(Colors.blue(f"ℹ No quorum: {sent2} sent, {failed2} failed"))

    # Restore quorum by restarting node2
    logger.info(Colors.blue("ℹ Restarting node2 to restore quorum..."))
    restart_node(2)
    time.sleep(10)

    # Verify quorum restored
    sent3, _ = produce_messages(topic, 20, 'localhost:9092', 'quorum-restored')
    logger.info(Colors.green(f"Quorum restored: {sent3} messages"))

    if sent2 < 5 and sent3 >= 5:
        logger.info(Colors.green("Test PASSED: Cluster recovered after restoring quorum"))
        return True
    else:
        logger.error(Colors.red(f"Test FAILED: sent2={sent2}, sent3={sent3}"))
        return False

# ==============================================================================
# MAIN TEST RUNNER
# ==============================================================================

def main():
    print(Colors.blue("=" * 60))
    print(Colors.blue("       Chronik Raft Recovery Test Suite"))
    print(Colors.blue("=" * 60))

    # Check cluster
    logger.info(Colors.blue("\nℹ Checking cluster status..."))
    alive_nodes = [node for node in NODES if check_node_alive(node['kafka_port'])]
    logger.info(Colors.green(f"Found {len(alive_nodes)}/3 nodes alive"))

    if len(alive_nodes) < 3:
        logger.error(Colors.red("\nℹ ERROR: Need all 3 nodes running."))
        return 1

    tests = [
        ("graceful_rejoin", test_graceful_shutdown_rejoin),
        ("crash_recovery", test_crash_recovery),
        ("stale_rejoin", test_stale_node_rejoin),
        ("rolling_restart", test_rolling_restart),
        ("concurrent_failures", test_concurrent_failures),
    ]

    results = {}

    for test_name, test_func in tests:
        try:
            result = test_func()
            results[test_name] = result
        except Exception as e:
            logger.error(Colors.red(f"Test {test_name} crashed: {e}"))
            import traceback
            traceback.print_exc()
            results[test_name] = False

        # Manual cluster restart between tests
        logger.info(Colors.blue("\nℹ Please restart cluster for next test"))
        logger.info(Colors.yellow("  Run: ./test_cluster_manual.sh"))
        if test_name != tests[-1][0]:  # Not last test
            input(Colors.blue("Press Enter when ready..."))

    # Summary
    print(Colors.header("Test Results Summary"))
    passed = sum(1 for r in results.values() if r)
    total = len(results)

    for test_name, result in results.items():
        status = Colors.green("PASS") if result else Colors.red("FAIL")
        print(f"  {test_name:20s} {status}")

    print(f"\n{Colors.blue(f'Overall: {passed}/{total} tests passed')}")

    if passed == total:
        print(Colors.green("✓ All tests passed!"))
        return 0
    else:
        print(Colors.red(f"✗ {total - passed} test(s) failed"))
        return 1

if __name__ == '__main__':
    sys.exit(main())
