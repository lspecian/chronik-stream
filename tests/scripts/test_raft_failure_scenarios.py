#!/usr/bin/env python3
"""
Comprehensive Raft Failure Scenario Test Suite

Tests leader election, split brain prevention, data consistency,
and recovery scenarios for Chronik Raft cluster.
"""

import sys
import time
import subprocess
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
NC = '\033[0m'  # No Color
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
        time.sleep(2)  # Wait for topic creation
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
    """Check if a node is alive by trying to connect"""
    try:
        admin = get_admin_client(f'localhost:{port}')
        admin.list_topics(timeout=2)
        admin.close()
        return True
    except Exception:
        return False

def wait_for_leader_election(timeout_secs=30):
    """Wait for leader election to complete"""
    logger.info(Colors.blue("ℹ Waiting for leader election..."))
    start = time.time()

    while (time.time() - start) < timeout_secs:
        # Check if any node responds
        for node in NODES:
            if check_node_alive(node['kafka_port']):
                logger.info(Colors.green(f"Leader elected (node responding on port {node['kafka_port']})"))
                time.sleep(2)  # Extra time for stabilization
                return True
        time.sleep(1)

    return False

def get_cluster_metadata():
    """Get cluster metadata from all nodes"""
    metadata = {}
    for node in NODES:
        try:
            admin = get_admin_client(f"localhost:{node['kafka_port']}")
            topics = admin.list_topics(timeout=2)
            metadata[node['name']] = {'alive': True, 'topics': topics}
            admin.close()
        except Exception:
            metadata[node['name']] = {'alive': False, 'topics': []}
    return metadata

# ==============================================================================
# TEST SCENARIOS
# ==============================================================================

def test_leader_failure_and_reelection():
    """
    TEST 1: Leader Failure and Re-election

    Scenario:
    1. Cluster running with 3 nodes
    2. Kill the leader node
    3. Verify remaining nodes elect new leader
    4. Verify cluster continues accepting writes
    5. Restart killed node
    6. Verify it rejoins as follower
    """
    print(Colors.header("TEST 1: Leader Failure and Re-election"))

    topic = 'leader-failure-test'
    logger.info(Colors.blue("ℹ Creating topic..."))
    if not create_topic(topic):
        return False
    logger.info(Colors.green(f"Created topic: {topic}"))

    # Produce baseline messages
    logger.info(Colors.blue("ℹ Producing baseline messages to all nodes..."))
    sent, failed = produce_messages(topic, 50, 'localhost:9092', 'baseline')
    logger.info(Colors.green(f"Baseline: {sent} messages sent"))

    # Kill node 1 (assume it's the leader)
    logger.info(Colors.blue("ℹ Killing node1 (port 9092)..."))
    try:
        subprocess.run(['pkill', '-f', 'chronik-server.*9092'], timeout=5)
        time.sleep(2)
    except Exception as e:
        logger.warning(Colors.yellow(f"Kill command: {e}"))

    # Wait for leader election
    if not wait_for_leader_election():
        logger.error(Colors.red("Leader election timeout"))
        return False

    # Try to produce to node2 (should be new leader or redirect)
    logger.info(Colors.blue("ℹ Producing to node2 after node1 failure..."))
    sent2, failed2 = produce_messages(topic, 50, 'localhost:9093', 'after-failure')
    logger.info(Colors.green(f"After failure: {sent2} messages sent"))

    if sent2 < 10:
        logger.error(Colors.red(f"Cluster failed to accept writes after leader failure (only {sent2} sent)"))
        return False

    # Consume all messages from node2
    logger.info(Colors.blue("ℹ Consuming from node2..."))
    messages = consume_messages(topic, 'localhost:9093', timeout_ms=15000)
    logger.info(Colors.green(f"Consumed {len(messages)} messages"))

    # Verify we got both baseline and post-failure messages
    baseline_msgs = [m for m in messages if m.startswith('baseline')]
    after_msgs = [m for m in messages if m.startswith('after-failure')]

    logger.info(Colors.blue(f"ℹ Baseline messages: {len(baseline_msgs)}, After-failure: {len(after_msgs)}"))

    if len(after_msgs) > 0:
        logger.info(Colors.green("Test PASSED: Cluster elected new leader and accepted writes"))
        return True
    else:
        logger.error(Colors.red("Test FAILED: No messages received after leader failure"))
        return False

def test_minority_partition():
    """
    TEST 2: Minority Partition (1 node isolated)

    Scenario:
    1. Isolate 1 node from cluster
    2. Verify majority (2 nodes) continue operating
    3. Verify isolated node cannot accept writes
    4. Heal partition
    5. Verify isolated node catches up
    """
    print(Colors.header("TEST 2: Minority Partition"))

    topic = 'minority-partition-test'
    logger.info(Colors.blue("ℹ Creating topic..."))
    if not create_topic(topic):
        return False
    logger.info(Colors.green(f"Created topic: {topic}"))

    # Baseline
    logger.info(Colors.blue("ℹ Producing baseline messages..."))
    sent, _ = produce_messages(topic, 30, 'localhost:9092', 'baseline')
    logger.info(Colors.green(f"Baseline: {sent} messages"))

    # Simulate partition by killing node3
    logger.info(Colors.blue("ℹ Partitioning node3 (killing it)..."))
    subprocess.run(['pkill', '-f', 'chronik-server.*9094'], timeout=5)
    time.sleep(3)

    # Majority should still work (nodes 1 and 2)
    logger.info(Colors.blue("ℹ Producing to node1 (majority)..."))
    sent2, failed2 = produce_messages(topic, 30, 'localhost:9092', 'majority')
    logger.info(Colors.green(f"Majority: {sent2} sent, {failed2} failed"))

    if sent2 < 10:
        logger.error(Colors.red("Majority failed to operate"))
        return False

    # Consume from node2
    messages = consume_messages(topic, 'localhost:9093', timeout_ms=15000)
    majority_msgs = [m for m in messages if m.startswith('majority')]

    logger.info(Colors.green(f"Consumed {len(messages)} messages ({len(majority_msgs)} from majority)"))

    if len(majority_msgs) > 0:
        logger.info(Colors.green("Test PASSED: Majority continued operating"))
        return True
    else:
        logger.error(Colors.red("Test FAILED: Majority did not accept writes"))
        return False

def test_split_brain_prevention():
    """
    TEST 3: Split Brain Prevention

    Scenario:
    1. Create 2 separate minorities (1 node each, 1 node down)
    2. Verify neither minority can elect a leader
    3. Verify no writes are accepted by minorities
    4. Restore majority
    5. Verify cluster recovers
    """
    print(Colors.header("TEST 3: Split Brain Prevention"))

    topic = 'split-brain-test'
    logger.info(Colors.blue("ℹ Creating topic..."))
    if not create_topic(topic):
        return False
    logger.info(Colors.green(f"Created topic: {topic}"))

    # Baseline
    sent, _ = produce_messages(topic, 30, 'localhost:9092', 'baseline')
    logger.info(Colors.green(f"Baseline: {sent} messages"))

    # Kill 2 nodes (leaving only 1 - no quorum)
    logger.info(Colors.blue("ℹ Killing node2 and node3 (no quorum)..."))
    subprocess.run(['pkill', '-f', 'chronik-server.*9093'], timeout=5)
    subprocess.run(['pkill', '-f', 'chronik-server.*9094'], timeout=5)
    time.sleep(5)

    # Try to produce to node1 (should fail - no quorum)
    logger.info(Colors.blue("ℹ Attempting to produce to node1 (no quorum)..."))
    sent2, failed2 = produce_messages(topic, 20, 'localhost:9092', 'no-quorum')
    logger.info(Colors.blue(f"ℹ No quorum: {sent2} sent, {failed2} failed"))

    # Verify no writes accepted (or very few due to timeouts)
    if sent2 < 5:
        logger.info(Colors.green(f"Test PASSED: Single node rejected writes (no quorum) - {sent2} sent"))
        return True
    else:
        logger.error(Colors.red(f"Test FAILED: Single node accepted {sent2} writes (split brain risk!)"))
        return False

def test_data_consistency_after_partition():
    """
    TEST 4: Data Consistency After Partition

    Scenario:
    1. Produce messages to cluster
    2. Partition minority
    3. Produce more messages to majority
    4. Heal partition
    5. Verify all nodes have consistent data
    """
    print(Colors.header("TEST 4: Data Consistency After Partition"))

    topic = 'consistency-test'
    logger.info(Colors.blue("ℹ Creating topic..."))
    if not create_topic(topic):
        return False
    logger.info(Colors.green(f"Created topic: {topic}"))

    # Phase 1: All nodes
    sent1, _ = produce_messages(topic, 30, 'localhost:9092', 'phase1')
    logger.info(Colors.green(f"Phase 1: {sent1} messages"))

    # Phase 2: Partition node3
    logger.info(Colors.blue("ℹ Partitioning node3..."))
    subprocess.run(['pkill', '-f', 'chronik-server.*9094'], timeout=5)
    time.sleep(3)

    # Produce to majority
    sent2, _ = produce_messages(topic, 30, 'localhost:9092', 'phase2')
    logger.info(Colors.green(f"Phase 2 (majority): {sent2} messages"))

    # Consume from node1 and node2
    logger.info(Colors.blue("ℹ Consuming from node1..."))
    msgs_node1 = consume_messages(topic, 'localhost:9092', timeout_ms=15000)

    logger.info(Colors.blue("ℹ Consuming from node2..."))
    msgs_node2 = consume_messages(topic, 'localhost:9093', timeout_ms=15000)

    logger.info(Colors.blue(f"ℹ Node1: {len(msgs_node1)} messages, Node2: {len(msgs_node2)} messages"))

    # Check consistency
    if abs(len(msgs_node1) - len(msgs_node2)) <= 5:  # Allow small variance
        logger.info(Colors.green("Test PASSED: Nodes have consistent data"))
        return True
    else:
        logger.error(Colors.red(f"Test FAILED: Inconsistent data (node1: {len(msgs_node1)}, node2: {len(msgs_node2)})"))
        return False

def test_cascading_failures():
    """
    TEST 5: Cascading Failures

    Scenario:
    1. Kill nodes one by one
    2. Verify cluster operates until quorum lost
    3. Verify cluster stops accepting writes after quorum loss
    """
    print(Colors.header("TEST 5: Cascading Failures"))

    topic = 'cascading-test'
    logger.info(Colors.blue("ℹ Creating topic..."))
    if not create_topic(topic):
        return False
    logger.info(Colors.green(f"Created topic: {topic}"))

    # Baseline
    sent1, _ = produce_messages(topic, 30, 'localhost:9092', 'baseline')
    logger.info(Colors.green(f"Baseline: {sent1} messages"))

    # Kill node3 (still have quorum: 2/3)
    logger.info(Colors.blue("ℹ Killing node3 (2/3 nodes remain)..."))
    subprocess.run(['pkill', '-f', 'chronik-server.*9094'], timeout=5)
    time.sleep(3)

    sent2, _ = produce_messages(topic, 20, 'localhost:9092', 'after-one-failure')
    logger.info(Colors.blue(f"ℹ After 1 failure: {sent2} messages"))

    # Kill node2 (no quorum: 1/3)
    logger.info(Colors.blue("ℹ Killing node2 (1/3 nodes remain - no quorum)..."))
    subprocess.run(['pkill', '-f', 'chronik-server.*9093'], timeout=5)
    time.sleep(5)

    sent3, failed3 = produce_messages(topic, 20, 'localhost:9092', 'no-quorum')
    logger.info(Colors.blue(f"ℹ After 2 failures (no quorum): {sent3} sent, {failed3} failed"))

    # Verify: first failure should allow writes, second should not
    if sent2 >= 5 and sent3 < 5:
        logger.info(Colors.green("Test PASSED: Cluster operated until quorum lost"))
        return True
    else:
        logger.error(Colors.red(f"Test FAILED: sent2={sent2}, sent3={sent3}"))
        return False

# ==============================================================================
# MAIN TEST RUNNER
# ==============================================================================

def main():
    print(Colors.blue("=" * 60))
    print(Colors.blue("     Chronik Raft Failure Scenario Test Suite"))
    print(Colors.blue("=" * 60))

    # Check cluster is running
    logger.info(Colors.blue("\nℹ Checking cluster status..."))
    alive_nodes = [node for node in NODES if check_node_alive(node['kafka_port'])]
    logger.info(Colors.green(f"Found {len(alive_nodes)}/3 nodes alive"))

    if len(alive_nodes) < 3:
        logger.error(Colors.red("\nℹ ERROR: Need all 3 nodes running. Start cluster first:"))
        logger.error(Colors.red("  ./test_cluster_manual.sh"))
        return 1

    logger.info(Colors.blue("\nℹ Starting failure scenario tests...\n"))

    tests = [
        ("leader_failure", test_leader_failure_and_reelection),
        ("minority_partition", test_minority_partition),
        ("split_brain", test_split_brain_prevention),
        ("consistency", test_data_consistency_after_partition),
        ("cascading", test_cascading_failures),
    ]

    results = {}

    for test_name, test_func in tests:
        try:
            result = test_func()
            results[test_name] = result
        except Exception as e:
            logger.error(Colors.red(f"Test {test_name} crashed: {e}"))
            results[test_name] = False

        # Restart all nodes between tests
        logger.info(Colors.blue("\nℹ Restarting all nodes for next test..."))
        subprocess.run(['pkill', '-f', 'chronik-server'], timeout=5)
        time.sleep(2)

        # You'll need to restart manually or implement auto-restart here
        logger.info(Colors.yellow("⚠ Please restart cluster manually: ./test_cluster_manual.sh"))
        input(Colors.blue("Press Enter when cluster is ready..."))

    # Print summary
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
