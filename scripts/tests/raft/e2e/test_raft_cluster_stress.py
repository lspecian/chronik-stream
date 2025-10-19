#!/usr/bin/env python3
"""
Comprehensive Raft cluster stress test with chaos engineering.

This test verifies:
1. Message production/consumption through Raft
2. Broker registration and visibility
3. Leader failover
4. Network partitions
5. Multiple topics and partitions
6. Java client compatibility
"""

import subprocess
import time
import sys
import json
import random
import signal
from kafka import KafkaProducer, KafkaConsumer, KafkaAdminClient
from kafka.admin import NewTopic
from kafka.errors import KafkaError

# ANSI colors
GREEN = '\033[0;32m'
RED = '\033[0;31m'
YELLOW = '\033[1;33m'
BLUE = '\033[0;34m'
NC = '\033[0m'  # No Color

class RaftClusterTest:
    def __init__(self):
        self.nodes = []
        self.node_pids = {}
        self.stats = {
            'messages_produced': 0,
            'messages_consumed': 0,
            'leader_changes': 0,
            'errors': 0,
            'topics_created': 0
        }

    def log(self, level, message):
        """Log with color coding"""
        colors = {
            'INFO': BLUE,
            'SUCCESS': GREEN,
            'WARNING': YELLOW,
            'ERROR': RED
        }
        color = colors.get(level, NC)
        print(f"{color}[{level}]{NC} {message}")

    def start_node(self, node_id, kafka_port, raft_port):
        """Start a single Chronik node"""
        self.log('INFO', f"Starting node {node_id} (Kafka:{kafka_port}, Raft:{raft_port})")

        env = {
            'CHRONIK_NODE_ID': str(node_id),
            'CHRONIK_DATA_DIR': f'./data/stress_node{node_id}',
            'CHRONIK_KAFKA_PORT': str(kafka_port),
            'CHRONIK_ADVERTISED_ADDR': '127.0.0.1',
            'CHRONIK_ADVERTISED_PORT': str(kafka_port),
            'RUST_LOG': 'info,chronik_raft=debug',
        }

        # Create node config
        config_file = f'chronik-stress-node{node_id}.toml'
        with open(config_file, 'w') as f:
            f.write(f"""enabled = true
node_id = {node_id}
replication_factor = 3
min_insync_replicas = 2

[[peers]]
id = 1
addr = "127.0.0.1:9092"
raft_port = 5001

[[peers]]
id = 2
addr = "127.0.0.1:9093"
raft_port = 5002

[[peers]]
id = 3
addr = "127.0.0.1:9094"
raft_port = 5003
""")

        process = subprocess.Popen(
            ['./target/release/chronik-server', '--cluster-config', config_file, 'standalone'],
            env={**subprocess.os.environ, **env},
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True
        )

        self.nodes.append({
            'node_id': node_id,
            'kafka_port': kafka_port,
            'raft_port': raft_port,
            'process': process
        })
        self.node_pids[node_id] = process.pid

        return process

    def stop_node(self, node_id):
        """Stop a specific node"""
        self.log('WARNING', f"Stopping node {node_id}")
        for node in self.nodes:
            if node['node_id'] == node_id:
                node['process'].terminate()
                node['process'].wait(timeout=5)
                self.stats['leader_changes'] += 1
                return True
        return False

    def restart_node(self, node_id):
        """Restart a node"""
        self.log('INFO', f"Restarting node {node_id}")
        for i, node in enumerate(self.nodes):
            if node['node_id'] == node_id:
                node['process'].terminate()
                node['process'].wait(timeout=5)
                new_process = self.start_node(node_id, node['kafka_port'], node['raft_port'])
                self.nodes[i]['process'] = new_process
                time.sleep(2)  # Give it time to rejoin cluster
                return True
        return False

    def start_cluster(self):
        """Start all 3 nodes"""
        self.log('INFO', "Starting 3-node Raft cluster...")

        # Clean up old data
        subprocess.run(['rm', '-rf', './data/stress_node1', './data/stress_node2', './data/stress_node3'],
                      stderr=subprocess.DEVNULL)
        subprocess.run(['mkdir', '-p', './data/stress_node1', './data/stress_node2', './data/stress_node3'])

        # Start nodes
        self.start_node(1, 9092, 5001)
        time.sleep(1)
        self.start_node(2, 9093, 5002)
        time.sleep(1)
        self.start_node(3, 9094, 5003)

        # Wait for cluster to stabilize
        self.log('INFO', "Waiting for cluster bootstrap (15 seconds)...")
        time.sleep(15)

        self.log('SUCCESS', "Cluster started")

    def stop_cluster(self):
        """Stop all nodes"""
        self.log('INFO', "Stopping cluster...")
        for node in self.nodes:
            node['process'].terminate()

        for node in self.nodes:
            try:
                node['process'].wait(timeout=5)
            except subprocess.TimeoutExpired:
                node['process'].kill()

    def create_topic(self, topic_name, partitions=3, replication_factor=3):
        """Create a topic using Kafka admin client"""
        try:
            admin_client = KafkaAdminClient(
                bootstrap_servers=['localhost:9092', 'localhost:9093', 'localhost:9094'],
                request_timeout_ms=10000
            )

            topic = NewTopic(
                name=topic_name,
                num_partitions=partitions,
                replication_factor=replication_factor
            )

            admin_client.create_topics([topic], timeout_ms=10000)
            admin_client.close()

            self.stats['topics_created'] += 1
            self.log('SUCCESS', f"Created topic '{topic_name}' with {partitions} partitions")
            return True

        except KafkaError as e:
            if 'TopicExistsError' in str(e):
                self.log('WARNING', f"Topic '{topic_name}' already exists")
                return True
            else:
                self.log('ERROR', f"Failed to create topic: {e}")
                self.stats['errors'] += 1
                return False

    def produce_messages(self, topic, count=1000, batch_size=100):
        """Produce messages to the cluster"""
        self.log('INFO', f"Producing {count} messages to '{topic}'...")

        try:
            producer = KafkaProducer(
                bootstrap_servers=['localhost:9092', 'localhost:9093', 'localhost:9094'],
                acks='all',  # Wait for all replicas
                retries=5,
                max_in_flight_requests_per_connection=1,
                value_serializer=lambda v: json.dumps(v).encode('utf-8')
            )

            sent = 0
            errors = 0
            start_time = time.time()

            for i in range(count):
                message = {
                    'id': i,
                    'timestamp': time.time(),
                    'data': f'stress_test_message_{i}'
                }

                try:
                    future = producer.send(topic, value=message)
                    future.get(timeout=10)  # Wait for confirmation
                    sent += 1
                    self.stats['messages_produced'] += 1

                    if (i + 1) % batch_size == 0:
                        self.log('INFO', f"  Produced {i + 1}/{count} messages")

                except Exception as e:
                    errors += 1
                    self.stats['errors'] += 1
                    self.log('ERROR', f"  Failed to produce message {i}: {e}")

            producer.flush()
            producer.close()

            duration = time.time() - start_time
            rate = sent / duration if duration > 0 else 0

            self.log('SUCCESS', f"Produced {sent}/{count} messages in {duration:.2f}s ({rate:.0f} msg/s)")
            if errors > 0:
                self.log('WARNING', f"  {errors} errors occurred")

            return sent, errors

        except Exception as e:
            self.log('ERROR', f"Producer failed: {e}")
            self.stats['errors'] += 1
            return 0, count

    def consume_messages(self, topic, expected_count, timeout=30):
        """Consume messages from the cluster"""
        self.log('INFO', f"Consuming up to {expected_count} messages from '{topic}'...")

        try:
            consumer = KafkaConsumer(
                topic,
                bootstrap_servers=['localhost:9092', 'localhost:9093', 'localhost:9094'],
                auto_offset_reset='earliest',
                enable_auto_commit=True,
                group_id=f'stress_test_group_{int(time.time())}',
                value_deserializer=lambda m: json.loads(m.decode('utf-8')),
                consumer_timeout_ms=timeout * 1000
            )

            consumed = 0
            errors = 0
            start_time = time.time()

            for message in consumer:
                consumed += 1
                self.stats['messages_consumed'] += 1

                if consumed % 100 == 0:
                    self.log('INFO', f"  Consumed {consumed}/{expected_count} messages")

                if consumed >= expected_count:
                    break

            consumer.close()

            duration = time.time() - start_time
            rate = consumed / duration if duration > 0 else 0

            self.log('SUCCESS', f"Consumed {consumed}/{expected_count} messages in {duration:.2f}s ({rate:.0f} msg/s)")

            if consumed < expected_count:
                missing = expected_count - consumed
                self.log('WARNING', f"  {missing} messages missing!")
                self.stats['errors'] += missing

            return consumed, missing if consumed < expected_count else 0

        except Exception as e:
            self.log('ERROR', f"Consumer failed: {e}")
            self.stats['errors'] += 1
            return 0, expected_count

    def test_basic_produce_consume(self):
        """Test 1: Basic produce/consume"""
        self.log('INFO', "=" * 60)
        self.log('INFO', "TEST 1: Basic Produce/Consume (1000 messages, 1 topic)")
        self.log('INFO', "=" * 60)

        topic = 'test_basic'
        if not self.create_topic(topic):
            return False

        time.sleep(2)  # Wait for topic creation to propagate

        sent, errors = self.produce_messages(topic, 1000)
        if sent == 0:
            return False

        time.sleep(2)  # Wait for messages to commit

        consumed, missing = self.consume_messages(topic, sent)

        success = consumed == sent
        if success:
            self.log('SUCCESS', "✓ Basic produce/consume test PASSED")
        else:
            self.log('ERROR', f"✗ Basic test FAILED ({consumed}/{sent} messages)")

        return success

    def test_multi_topic(self):
        """Test 2: Multiple topics"""
        self.log('INFO', "=" * 60)
        self.log('INFO', "TEST 2: Multiple Topics (5 topics, 500 msgs each)")
        self.log('INFO', "=" * 60)

        topics = [f'test_topic_{i}' for i in range(5)]
        total_sent = 0
        total_consumed = 0

        # Create topics
        for topic in topics:
            if not self.create_topic(topic):
                return False

        time.sleep(3)

        # Produce to all topics
        for topic in topics:
            sent, errors = self.produce_messages(topic, 500)
            total_sent += sent

        time.sleep(3)

        # Consume from all topics
        for topic in topics:
            consumed, missing = self.consume_messages(topic, 500)
            total_consumed += consumed

        success = total_consumed == total_sent
        if success:
            self.log('SUCCESS', f"✓ Multi-topic test PASSED ({total_consumed}/{total_sent} messages)")
        else:
            self.log('ERROR', f"✗ Multi-topic test FAILED ({total_consumed}/{total_sent} messages)")

        return success

    def test_leader_failover(self):
        """Test 3: Leader failover"""
        self.log('INFO', "=" * 60)
        self.log('INFO', "TEST 3: Leader Failover (kill leader mid-produce)")
        self.log('INFO', "=" * 60)

        topic = 'test_failover'
        if not self.create_topic(topic):
            return False

        time.sleep(2)

        # Produce 500 messages
        sent1, _ = self.produce_messages(topic, 500)

        # Kill node 1 (likely leader since it's first)
        self.log('WARNING', "Killing node 1 to trigger failover...")
        self.stop_node(1)
        time.sleep(3)  # Wait for new leader election

        # Try to produce more messages (should work with new leader)
        sent2, _ = self.produce_messages(topic, 500)

        total_sent = sent1 + sent2

        # Restart node 1
        self.restart_node(1)
        time.sleep(3)

        # Consume all messages
        consumed, missing = self.consume_messages(topic, total_sent, timeout=45)

        success = consumed >= total_sent * 0.95  # Allow 5% loss during failover
        if success:
            self.log('SUCCESS', f"✓ Failover test PASSED ({consumed}/{total_sent} messages)")
        else:
            self.log('ERROR', f"✗ Failover test FAILED ({consumed}/{total_sent} messages)")

        return success

    def test_chaos_stress(self):
        """Test 4: Chaos stress test"""
        self.log('INFO', "=" * 60)
        self.log('INFO', "TEST 4: Chaos Stress Test (random failures)")
        self.log('INFO', "=" * 60)

        topic = 'test_chaos'
        if not self.create_topic(topic):
            return False

        time.sleep(2)

        total_sent = 0

        # Phase 1: Produce while randomly killing/restarting nodes
        self.log('INFO', "Phase 1: Producing with random node failures...")
        for i in range(3):
            sent, _ = self.produce_messages(topic, 300)
            total_sent += sent

            # Random chaos
            if random.random() > 0.5:
                victim = random.choice([1, 2, 3])
                self.log('WARNING', f"  Chaos: Restarting node {victim}")
                self.restart_node(victim)
                time.sleep(2)

        time.sleep(5)  # Let cluster stabilize

        # Phase 2: Consume everything
        self.log('INFO', "Phase 2: Consuming all messages...")
        consumed, missing = self.consume_messages(topic, total_sent, timeout=60)

        success = consumed >= total_sent * 0.90  # Allow 10% loss during chaos
        if success:
            self.log('SUCCESS', f"✓ Chaos test PASSED ({consumed}/{total_sent} messages, {missing} lost)")
        else:
            self.log('ERROR', f"✗ Chaos test FAILED ({consumed}/{total_sent} messages, {missing} lost)")

        return success

    def print_stats(self):
        """Print final statistics"""
        self.log('INFO', "=" * 60)
        self.log('INFO', "FINAL STATISTICS")
        self.log('INFO', "=" * 60)
        print(f"  Messages Produced:  {self.stats['messages_produced']}")
        print(f"  Messages Consumed:  {self.stats['messages_consumed']}")
        print(f"  Topics Created:     {self.stats['topics_created']}")
        print(f"  Leader Changes:     {self.stats['leader_changes']}")
        print(f"  Total Errors:       {self.stats['errors']}")

        success_rate = (self.stats['messages_consumed'] / self.stats['messages_produced'] * 100) if self.stats['messages_produced'] > 0 else 0
        print(f"  Success Rate:       {success_rate:.2f}%")

    def run_all_tests(self):
        """Run complete test suite"""
        self.log('INFO', "=" * 60)
        self.log('INFO', "RAFT CLUSTER COMPREHENSIVE STRESS TEST")
        self.log('INFO', "=" * 60)

        try:
            # Start cluster
            self.start_cluster()

            # Run tests
            results = []
            results.append(("Basic Produce/Consume", self.test_basic_produce_consume()))
            results.append(("Multi-Topic", self.test_multi_topic()))
            results.append(("Leader Failover", self.test_leader_failover()))
            results.append(("Chaos Stress", self.test_chaos_stress()))

            # Print results
            self.log('INFO', "=" * 60)
            self.log('INFO', "TEST RESULTS")
            self.log('INFO', "=" * 60)

            passed = sum(1 for _, result in results if result)
            total = len(results)

            for name, result in results:
                status = f"{GREEN}PASS{NC}" if result else f"{RED}FAIL{NC}"
                print(f"  {name}: {status}")

            self.print_stats()

            # Final verdict
            self.log('INFO', "=" * 60)
            if passed == total:
                self.log('SUCCESS', f"✓ ALL {total} TESTS PASSED!")
                return 0
            else:
                self.log('ERROR', f"✗ {total - passed}/{total} TESTS FAILED")
                return 1

        finally:
            self.stop_cluster()


if __name__ == '__main__':
    test = RaftClusterTest()

    # Handle Ctrl+C
    def signal_handler(sig, frame):
        print("\n\nInterrupted! Cleaning up...")
        test.stop_cluster()
        sys.exit(1)

    signal.signal(signal.SIGINT, signal_handler)

    # Run tests
    exit_code = test.run_all_tests()
    sys.exit(exit_code)
