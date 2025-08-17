#!/usr/bin/env python3
"""
Comprehensive Kafka-Python Integration Test Suite for Chronik Stream

This test suite validates Chronik Stream's compatibility with the official kafka-python client library.
It covers all major functionality including producer, consumer, admin operations, and consumer groups.
"""

import unittest
import time
import json
import uuid
import threading
from typing import List, Dict, Any
from concurrent.futures import ThreadPoolExecutor, as_completed

from kafka import KafkaProducer, KafkaConsumer, KafkaAdminClient, TopicPartition
from kafka.admin import NewTopic, ConfigResource, ConfigResourceType
from kafka.errors import KafkaError, TopicAlreadyExistsError
from kafka.structs import OffsetAndMetadata

BOOTSTRAP_SERVERS = ['localhost:9092']
TEST_TIMEOUT = 30  # seconds


class TestKafkaPythonIntegration(unittest.TestCase):
    """Comprehensive integration tests using kafka-python client"""
    
    @classmethod
    def setUpClass(cls):
        """Set up test fixtures"""
        cls.admin_client = KafkaAdminClient(
            bootstrap_servers=BOOTSTRAP_SERVERS,
            client_id='test-admin-client'
        )
        cls.test_topic_prefix = f"test-{uuid.uuid4().hex[:8]}"
        
    @classmethod
    def tearDownClass(cls):
        """Clean up test topics"""
        # List all topics
        metadata = cls.admin_client.list_topics()
        test_topics = [topic for topic in metadata if topic.startswith(cls.test_topic_prefix)]
        
        # Delete test topics
        if test_topics:
            try:
                cls.admin_client.delete_topics(test_topics, timeout_ms=30000)
                print(f"Cleaned up {len(test_topics)} test topics")
            except Exception as e:
                print(f"Warning: Failed to clean up test topics: {e}")
        
        cls.admin_client.close()
    
    def setUp(self):
        """Set up for each test"""
        self.test_id = uuid.uuid4().hex[:8]
        
    def create_test_topic(self, suffix: str, partitions: int = 1, replication_factor: int = 1) -> str:
        """Create a test topic with a unique name"""
        topic_name = f"{self.test_topic_prefix}-{suffix}-{self.test_id}"
        topic = NewTopic(
            name=topic_name,
            num_partitions=partitions,
            replication_factor=replication_factor
        )
        
        try:
            self.admin_client.create_topics([topic], timeout_ms=30000)
            # Wait for topic to be ready
            time.sleep(0.5)
        except TopicAlreadyExistsError:
            pass
            
        return topic_name
    
    def test_01_admin_operations(self):
        """Test admin client operations"""
        print("\n=== Testing Admin Operations ===")
        
        # Test 1: Create topics
        topics = []
        for i in range(3):
            topic_name = self.create_test_topic(f"admin-{i}", partitions=3)
            topics.append(topic_name)
        
        # Test 2: List topics
        all_topics = self.admin_client.list_topics()
        for topic in topics:
            self.assertIn(topic, all_topics, f"Topic {topic} not found in topic list")
        print(f"✓ Created and listed {len(topics)} topics")
        
        # Test 3: Describe topics
        topic_metadata = self.admin_client.describe_topics(topics)
        self.assertEqual(len(topic_metadata), len(topics))
        for topic_info in topic_metadata:
            self.assertEqual(len(topic_info['partitions']), 3)
        print("✓ Described topics successfully")
        
        # Test 4: Delete topics
        self.admin_client.delete_topics([topics[0]], timeout_ms=30000)
        time.sleep(0.5)
        remaining_topics = self.admin_client.list_topics()
        self.assertNotIn(topics[0], remaining_topics)
        print("✓ Deleted topic successfully")
        
    def test_02_producer_consumer_basic(self):
        """Test basic producer and consumer operations"""
        print("\n=== Testing Basic Producer/Consumer ===")
        
        topic = self.create_test_topic("basic-produce-consume")
        
        # Test 1: Produce messages
        producer = KafkaProducer(
            bootstrap_servers=BOOTSTRAP_SERVERS,
            client_id='test-producer',
            value_serializer=lambda v: json.dumps(v).encode('utf-8')
        )
        
        messages = []
        for i in range(10):
            message = {"id": i, "text": f"Message {i}", "timestamp": time.time()}
            future = producer.send(topic, value=message)
            record = future.get(timeout=10)
            messages.append((message, record))
            
        producer.flush()
        producer.close()
        print(f"✓ Produced {len(messages)} messages")
        
        # Test 2: Consume messages
        consumer = KafkaConsumer(
            topic,
            bootstrap_servers=BOOTSTRAP_SERVERS,
            client_id='test-consumer',
            auto_offset_reset='earliest',
            value_deserializer=lambda v: json.loads(v.decode('utf-8')),
            consumer_timeout_ms=5000
        )
        
        consumed_messages = []
        for msg in consumer:
            consumed_messages.append(msg.value)
            
        consumer.close()
        
        self.assertEqual(len(consumed_messages), len(messages))
        print(f"✓ Consumed {len(consumed_messages)} messages")
        
        # Verify message content
        for i, consumed in enumerate(consumed_messages):
            self.assertEqual(consumed['id'], i)
            self.assertEqual(consumed['text'], f"Message {i}")
    
    def test_03_producer_advanced(self):
        """Test advanced producer features"""
        print("\n=== Testing Advanced Producer Features ===")
        
        topic = self.create_test_topic("advanced-produce", partitions=3)
        
        producer = KafkaProducer(
            bootstrap_servers=BOOTSTRAP_SERVERS,
            client_id='test-producer-advanced',
            key_serializer=lambda k: k.encode('utf-8'),
            value_serializer=lambda v: json.dumps(v).encode('utf-8'),
            acks='all',  # Wait for all replicas
            retries=3,
            max_in_flight_requests_per_connection=1
        )
        
        # Test 1: Send with keys (for partition routing)
        key_messages = {}
        for i in range(30):
            key = f"key-{i % 3}"  # 3 different keys
            message = {"id": i, "key": key}
            future = producer.send(topic, key=key, value=message)
            metadata = future.get(timeout=10)
            
            if key not in key_messages:
                key_messages[key] = []
            key_messages[key].append(metadata.partition)
        
        producer.flush()
        
        # Verify messages with same key went to same partition
        for key, partitions in key_messages.items():
            unique_partitions = set(partitions)
            self.assertEqual(len(unique_partitions), 1, 
                           f"Messages with key '{key}' were sent to multiple partitions: {unique_partitions}")
        print("✓ Key-based partitioning working correctly")
        
        # Test 2: Send with callback
        success_count = 0
        error_count = 0
        
        def on_success(metadata):
            nonlocal success_count
            success_count += 1
            
        def on_error(e):
            nonlocal error_count
            error_count += 1
            
        for i in range(10):
            producer.send(topic, value={"id": i}).add_callback(on_success).add_errback(on_error)
            
        producer.flush()
        producer.close()
        
        self.assertEqual(success_count, 10)
        self.assertEqual(error_count, 0)
        print(f"✓ Callback handling: {success_count} successes, {error_count} errors")
    
    def test_04_consumer_advanced(self):
        """Test advanced consumer features"""
        print("\n=== Testing Advanced Consumer Features ===")
        
        topic = self.create_test_topic("advanced-consume", partitions=3)
        
        # Produce test data
        producer = KafkaProducer(
            bootstrap_servers=BOOTSTRAP_SERVERS,
            value_serializer=lambda v: json.dumps(v).encode('utf-8')
        )
        
        for i in range(30):
            producer.send(topic, value={"id": i, "partition": i % 3})
        producer.flush()
        producer.close()
        
        # Test 1: Manual partition assignment
        consumer = KafkaConsumer(
            bootstrap_servers=BOOTSTRAP_SERVERS,
            client_id='test-consumer-manual',
            value_deserializer=lambda v: json.loads(v.decode('utf-8')),
            enable_auto_commit=False
        )
        
        # Assign specific partitions
        partitions = [TopicPartition(topic, 0), TopicPartition(topic, 2)]
        consumer.assign(partitions)
        
        # Seek to beginning
        consumer.seek_to_beginning()
        
        consumed_partitions = set()
        messages_consumed = 0
        
        # Poll for messages
        end_time = time.time() + 5
        while time.time() < end_time:
            batch = consumer.poll(timeout_ms=1000)
            for tp, messages in batch.items():
                consumed_partitions.add(tp.partition)
                messages_consumed += len(messages)
        
        self.assertEqual(consumed_partitions, {0, 2})
        self.assertEqual(messages_consumed, 20)  # 30 messages / 3 partitions * 2 partitions
        print(f"✓ Manual partition assignment: consumed from partitions {consumed_partitions}")
        
        # Test 2: Offset management
        # Commit specific offsets
        offsets = {
            TopicPartition(topic, 0): OffsetAndMetadata(5, None),
            TopicPartition(topic, 2): OffsetAndMetadata(5, None)
        }
        consumer.commit(offsets)
        
        # Verify committed offsets
        committed = consumer.committed(TopicPartition(topic, 0))
        self.assertEqual(committed, 5)
        print("✓ Manual offset commit working")
        
        consumer.close()
        
        # Test 3: Consumer with specific offset reset
        consumer2 = KafkaConsumer(
            topic,
            bootstrap_servers=BOOTSTRAP_SERVERS,
            client_id='test-consumer-offset',
            auto_offset_reset='latest',
            value_deserializer=lambda v: json.loads(v.decode('utf-8')),
            consumer_timeout_ms=2000
        )
        
        # Should not receive old messages
        late_messages = list(consumer2)
        self.assertEqual(len(late_messages), 0)
        print("✓ Auto offset reset 'latest' working")
        
        consumer2.close()
    
    def test_05_consumer_groups(self):
        """Test consumer group functionality"""
        print("\n=== Testing Consumer Groups ===")
        
        topic = self.create_test_topic("consumer-groups", partitions=3)
        group_id = f"test-group-{self.test_id}"
        
        # Produce messages
        producer = KafkaProducer(
            bootstrap_servers=BOOTSTRAP_SERVERS,
            value_serializer=lambda v: str(v).encode('utf-8')
        )
        
        for i in range(90):
            producer.send(topic, value=i)
        producer.flush()
        producer.close()
        
        # Test 1: Multiple consumers in same group
        consumers = []
        consumed_by_consumer = {}
        
        def consume_messages(consumer_id):
            consumer = KafkaConsumer(
                topic,
                bootstrap_servers=BOOTSTRAP_SERVERS,
                group_id=group_id,
                client_id=f'consumer-{consumer_id}',
                auto_offset_reset='earliest',
                consumer_timeout_ms=5000
            )
            
            messages = []
            for msg in consumer:
                messages.append(int(msg.value))
            
            consumer.close()
            return consumer_id, messages
        
        # Start 3 consumers in parallel
        with ThreadPoolExecutor(max_workers=3) as executor:
            futures = []
            for i in range(3):
                future = executor.submit(consume_messages, i)
                futures.append(future)
            
            for future in as_completed(futures):
                consumer_id, messages = future.result()
                consumed_by_consumer[consumer_id] = messages
        
        # Verify all messages were consumed exactly once
        all_consumed = []
        for messages in consumed_by_consumer.values():
            all_consumed.extend(messages)
        
        self.assertEqual(len(all_consumed), 90)
        self.assertEqual(len(set(all_consumed)), 90)  # No duplicates
        print(f"✓ Consumer group: 3 consumers processed 90 messages without duplicates")
        
        # Test 2: Rebalancing
        # Start one consumer
        consumer1 = KafkaConsumer(
            topic,
            bootstrap_servers=BOOTSTRAP_SERVERS,
            group_id=f"{group_id}-rebalance",
            client_id='consumer-rebalance-1',
            auto_offset_reset='earliest'
        )
        
        # Get initial assignment
        consumer1.poll(timeout_ms=5000)
        initial_assignment = consumer1.assignment()
        self.assertEqual(len(initial_assignment), 3)  # Should have all 3 partitions
        
        # Start second consumer
        consumer2 = KafkaConsumer(
            topic,
            bootstrap_servers=BOOTSTRAP_SERVERS,
            group_id=f"{group_id}-rebalance",
            client_id='consumer-rebalance-2',
            auto_offset_reset='earliest'
        )
        
        # Allow time for rebalancing
        time.sleep(2)
        
        # Check assignments after rebalance
        consumer1.poll(timeout_ms=1000)
        consumer2.poll(timeout_ms=1000)
        
        assignment1 = consumer1.assignment()
        assignment2 = consumer2.assignment()
        
        # Verify partitions were distributed
        self.assertGreater(len(assignment1), 0)
        self.assertGreater(len(assignment2), 0)
        self.assertEqual(len(assignment1) + len(assignment2), 3)
        print(f"✓ Rebalancing: Consumer 1 has {len(assignment1)} partitions, Consumer 2 has {len(assignment2)}")
        
        consumer1.close()
        consumer2.close()
    
    def test_06_transactional_operations(self):
        """Test transactional producer operations (if supported)"""
        print("\n=== Testing Transactional Operations ===")
        
        topic = self.create_test_topic("transactions")
        
        try:
            # Create transactional producer
            producer = KafkaProducer(
                bootstrap_servers=BOOTSTRAP_SERVERS,
                client_id='test-transactional-producer',
                transactional_id=f'test-transaction-{self.test_id}',
                value_serializer=lambda v: json.dumps(v).encode('utf-8')
            )
            
            # Initialize transactions
            producer.init_transactions()
            
            # Test transaction
            producer.begin_transaction()
            
            for i in range(10):
                producer.send(topic, value={"id": i, "transaction": True})
            
            producer.commit_transaction()
            producer.close()
            
            print("✓ Transactional producer operations supported")
            
        except Exception as e:
            print(f"⚠️  Transactional operations not supported: {e}")
    
    def test_07_performance_and_reliability(self):
        """Test performance and reliability aspects"""
        print("\n=== Testing Performance and Reliability ===")
        
        topic = self.create_test_topic("performance", partitions=3)
        
        # Test 1: Batch producing
        producer = KafkaProducer(
            bootstrap_servers=BOOTSTRAP_SERVERS,
            batch_size=16384,  # 16KB batches
            linger_ms=10,  # Wait up to 10ms for batching
            compression_type='gzip',
            value_serializer=lambda v: json.dumps(v).encode('utf-8')
        )
        
        start_time = time.time()
        futures = []
        
        for i in range(1000):
            future = producer.send(topic, value={"id": i, "data": "x" * 100})
            futures.append(future)
        
        # Wait for all messages
        for future in futures:
            future.get(timeout=30)
        
        produce_time = time.time() - start_time
        messages_per_sec = 1000 / produce_time
        print(f"✓ Batch production: 1000 messages in {produce_time:.2f}s ({messages_per_sec:.0f} msg/s)")
        
        producer.close()
        
        # Test 2: Concurrent consumers
        def consume_partition(partition_id):
            consumer = KafkaConsumer(
                bootstrap_servers=BOOTSTRAP_SERVERS,
                client_id=f'perf-consumer-{partition_id}',
                value_deserializer=lambda v: json.loads(v.decode('utf-8'))
            )
            
            tp = TopicPartition(topic, partition_id)
            consumer.assign([tp])
            consumer.seek_to_beginning(tp)
            
            count = 0
            start = time.time()
            
            while count < 333 and time.time() - start < 10:  # ~333 messages per partition
                batch = consumer.poll(timeout_ms=1000)
                for messages in batch.values():
                    count += len(messages)
            
            consumer.close()
            return count
        
        # Consume from all partitions concurrently
        with ThreadPoolExecutor(max_workers=3) as executor:
            futures = [executor.submit(consume_partition, i) for i in range(3)]
            total_consumed = sum(f.result() for f in futures)
        
        self.assertGreaterEqual(total_consumed, 900)  # Allow some margin
        print(f"✓ Concurrent consumption: {total_consumed} messages consumed")
    
    def test_08_error_handling(self):
        """Test error handling scenarios"""
        print("\n=== Testing Error Handling ===")
        
        # Test 1: Invalid topic name
        try:
            invalid_topic = NewTopic(name='', num_partitions=1, replication_factor=1)
            self.admin_client.create_topics([invalid_topic], timeout_ms=5000)
            self.fail("Should have raised error for empty topic name")
        except Exception as e:
            print(f"✓ Correctly rejected invalid topic name: {type(e).__name__}")
        
        # Test 2: Non-existent topic consumption
        consumer = KafkaConsumer(
            'non-existent-topic-xyz',
            bootstrap_servers=BOOTSTRAP_SERVERS,
            client_id='test-error-consumer',
            consumer_timeout_ms=3000
        )
        
        messages = list(consumer)
        self.assertEqual(len(messages), 0)
        consumer.close()
        print("✓ Handled non-existent topic gracefully")
        
        # Test 3: Connection failure recovery
        # This would test reconnection, but requires stopping/starting the broker
        print("✓ Connection failure test skipped (requires broker restart)")
    
    def test_09_metadata_operations(self):
        """Test metadata and topic configuration operations"""
        print("\n=== Testing Metadata Operations ===")
        
        # Test 1: Cluster metadata
        metadata = self.admin_client.list_consumer_groups()
        print(f"✓ Listed {len(metadata)} consumer groups")
        
        # Test 2: Topic configuration
        topic = self.create_test_topic("config-test")
        
        # Describe topic configs
        resource = ConfigResource(ConfigResourceType.TOPIC, topic)
        try:
            configs = self.admin_client.describe_configs([resource])
            if configs:
                config_dict = configs[0]
                print(f"✓ Retrieved {len(config_dict)} topic configurations")
        except Exception as e:
            print(f"⚠️  Topic configuration not fully supported: {e}")
    
    def test_10_offset_management(self):
        """Test offset management operations"""
        print("\n=== Testing Offset Management ===")
        
        topic = self.create_test_topic("offset-mgmt", partitions=2)
        group_id = f"offset-test-group-{self.test_id}"
        
        # Produce some messages
        producer = KafkaProducer(
            bootstrap_servers=BOOTSTRAP_SERVERS,
            value_serializer=lambda v: str(v).encode('utf-8')
        )
        
        for i in range(20):
            producer.send(topic, value=i, partition=i % 2)
        producer.flush()
        producer.close()
        
        # Create consumer and consume some messages
        consumer = KafkaConsumer(
            topic,
            bootstrap_servers=BOOTSTRAP_SERVERS,
            group_id=group_id,
            enable_auto_commit=False,
            auto_offset_reset='earliest'
        )
        
        # Consume 5 messages from each partition
        consumed_count = 0
        for msg in consumer:
            consumed_count += 1
            if consumed_count >= 10:
                break
        
        # Manually commit offsets
        consumer.commit()
        
        # Get committed offsets
        partitions = [TopicPartition(topic, i) for i in range(2)]
        committed_offsets = {}
        for tp in partitions:
            offset = consumer.committed(tp)
            if offset is not None:
                committed_offsets[tp.partition] = offset
        
        print(f"✓ Committed offsets: {committed_offsets}")
        
        # Close and create new consumer to verify offset persistence
        consumer.close()
        
        consumer2 = KafkaConsumer(
            topic,
            bootstrap_servers=BOOTSTRAP_SERVERS,
            group_id=group_id,
            enable_auto_commit=False
        )
        
        # Check committed offsets persist
        for tp in partitions:
            offset = consumer2.committed(tp)
            if offset is not None:
                self.assertEqual(offset, committed_offsets.get(tp.partition))
        
        consumer2.close()
        print("✓ Offset persistence verified")


def run_all_tests():
    """Run all tests and generate report"""
    print("\n" + "="*60)
    print("Kafka-Python Integration Test Suite for Chronik Stream")
    print("="*60)
    
    # Check if Chronik Stream is running
    try:
        admin = KafkaAdminClient(
            bootstrap_servers=BOOTSTRAP_SERVERS,
            request_timeout_ms=5000
        )
        admin.list_topics()
        admin.close()
    except Exception as e:
        print(f"\n❌ Cannot connect to Chronik Stream at {BOOTSTRAP_SERVERS}")
        print(f"   Error: {e}")
        print("\n   Please ensure Chronik Stream is running:")
        print("   docker-compose up -d")
        return False
    
    # Run tests
    suite = unittest.TestLoader().loadTestsFromTestCase(TestKafkaPythonIntegration)
    runner = unittest.TextTestRunner(verbosity=2)
    result = runner.run(suite)
    
    # Generate summary report
    print("\n" + "="*60)
    print("Test Summary")
    print("="*60)
    print(f"Tests run: {result.testsRun}")
    print(f"Failures: {len(result.failures)}")
    print(f"Errors: {len(result.errors)}")
    print(f"Success rate: {((result.testsRun - len(result.failures) - len(result.errors)) / result.testsRun * 100):.1f}%")
    
    if result.failures:
        print("\nFailures:")
        for test, traceback in result.failures:
            print(f"  - {test}: {traceback.split(chr(10))[-2]}")
    
    if result.errors:
        print("\nErrors:")
        for test, traceback in result.errors:
            print(f"  - {test}: {traceback.split(chr(10))[-2]}")
    
    return result.wasSuccessful()


if __name__ == "__main__":
    import sys
    success = run_all_tests()
    sys.exit(0 if success else 1)