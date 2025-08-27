#!/usr/bin/env python3
"""
Comprehensive test suite for Chronik Stream fixes
Tests all the issues we've fixed including:
1. Auto-topic creation
2. Consumer group support
3. Offset management
4. Compression support
5. Basic produce/consume functionality
"""

import time
import json
import sys
from kafka import KafkaProducer, KafkaConsumer, KafkaAdminClient
from kafka.admin import NewTopic
from kafka.errors import KafkaError
import uuid

BOOTSTRAP_SERVERS = 'localhost:9092'
TEST_PREFIX = f"test-{int(time.time())}"

class ChronikStreamTester:
    def __init__(self):
        self.bootstrap_servers = BOOTSTRAP_SERVERS
        self.results = []
        self.passed = 0
        self.failed = 0
    
    def log_result(self, test_name, passed, details=""):
        status = "✅ PASS" if passed else "❌ FAIL"
        self.results.append(f"{status}: {test_name}")
        if details:
            self.results.append(f"   {details}")
        if passed:
            self.passed += 1
        else:
            self.failed += 1
        print(f"{status}: {test_name}")
        if details:
            print(f"   {details}")
    
    def test_auto_topic_creation(self):
        """Test that topics are auto-created when producing to non-existent topic"""
        print("\n=== Testing Auto-Topic Creation ===")
        topic = f"{TEST_PREFIX}-auto-create"
        
        try:
            producer = KafkaProducer(
                bootstrap_servers=self.bootstrap_servers,
                value_serializer=lambda v: json.dumps(v).encode('utf-8'),
                request_timeout_ms=10000,
                max_block_ms=10000
            )
            
            # Send message to non-existent topic
            future = producer.send(topic, value={'test': 'auto-create'})
            record_metadata = future.get(timeout=10)
            
            self.log_result(
                "Auto-topic creation", 
                True, 
                f"Topic '{topic}' created automatically, offset: {record_metadata.offset}"
            )
            
            producer.close()
            return True
            
        except Exception as e:
            self.log_result("Auto-topic creation", False, str(e))
            return False
    
    def test_consumer_group(self):
        """Test consumer group coordination"""
        print("\n=== Testing Consumer Group Support ===")
        topic = f"{TEST_PREFIX}-consumer-group"
        group_id = f"{TEST_PREFIX}-group"
        
        try:
            # Create topic first
            producer = KafkaProducer(
                bootstrap_servers=self.bootstrap_servers,
                value_serializer=lambda v: json.dumps(v).encode('utf-8')
            )
            
            # Send test messages
            for i in range(5):
                producer.send(topic, value={'msg': f'test-{i}'})
            producer.flush()
            producer.close()
            
            # Create consumer with group
            consumer = KafkaConsumer(
                topic,
                group_id=group_id,
                bootstrap_servers=self.bootstrap_servers,
                auto_offset_reset='earliest',
                value_deserializer=lambda m: json.loads(m.decode('utf-8')),
                consumer_timeout_ms=5000,
                enable_auto_commit=True
            )
            
            messages = []
            for message in consumer:
                messages.append(message.value)
                if len(messages) >= 5:
                    break
            
            consumer.close()
            
            self.log_result(
                "Consumer group creation", 
                len(messages) == 5,
                f"Consumed {len(messages)} messages with group '{group_id}'"
            )
            
            # Test that a new consumer in same group doesn't re-read messages
            consumer2 = KafkaConsumer(
                topic,
                group_id=group_id,
                bootstrap_servers=self.bootstrap_servers,
                auto_offset_reset='earliest',
                value_deserializer=lambda m: json.loads(m.decode('utf-8')),
                consumer_timeout_ms=2000
            )
            
            duplicate_messages = list(consumer2)
            consumer2.close()
            
            self.log_result(
                "Consumer group offset tracking",
                len(duplicate_messages) == 0,
                f"Second consumer read {len(duplicate_messages)} messages (should be 0)"
            )
            
            return True
            
        except Exception as e:
            self.log_result("Consumer group support", False, str(e))
            return False
    
    def test_offset_commit_fetch(self):
        """Test offset commit and fetch"""
        print("\n=== Testing Offset Management ===")
        topic = f"{TEST_PREFIX}-offsets"
        group_id = f"{TEST_PREFIX}-offset-group"
        
        try:
            # Produce messages
            producer = KafkaProducer(
                bootstrap_servers=self.bootstrap_servers,
                value_serializer=lambda v: json.dumps(v).encode('utf-8')
            )
            
            for i in range(10):
                producer.send(topic, value={'id': i})
            producer.flush()
            producer.close()
            
            # Consumer 1: Read first 5 messages and commit
            consumer1 = KafkaConsumer(
                topic,
                group_id=group_id,
                bootstrap_servers=self.bootstrap_servers,
                auto_offset_reset='earliest',
                enable_auto_commit=False,
                value_deserializer=lambda m: json.loads(m.decode('utf-8'))
            )
            
            count = 0
            for message in consumer1:
                count += 1
                if count == 5:
                    consumer1.commit()
                    break
            
            consumer1.close()
            
            # Consumer 2: Should start from offset 5
            consumer2 = KafkaConsumer(
                topic,
                group_id=group_id,
                bootstrap_servers=self.bootstrap_servers,
                auto_offset_reset='earliest',
                enable_auto_commit=False,
                value_deserializer=lambda m: json.loads(m.decode('utf-8')),
                consumer_timeout_ms=3000
            )
            
            messages = []
            for message in consumer2:
                messages.append(message.value['id'])
            
            consumer2.close()
            
            expected_ids = [5, 6, 7, 8, 9]
            actual_ids = messages[:5]
            
            self.log_result(
                "Offset commit/fetch",
                actual_ids == expected_ids,
                f"Expected IDs {expected_ids}, got {actual_ids}"
            )
            
            return actual_ids == expected_ids
            
        except Exception as e:
            self.log_result("Offset management", False, str(e))
            return False
    
    def test_compression(self):
        """Test different compression types"""
        print("\n=== Testing Compression Support ===")
        
        compression_types = ['gzip', 'snappy', 'lz4']
        
        for comp_type in compression_types:
            topic = f"{TEST_PREFIX}-compression-{comp_type}"
            
            try:
                # Producer with compression
                producer = KafkaProducer(
                    bootstrap_servers=self.bootstrap_servers,
                    compression_type=comp_type,
                    value_serializer=lambda v: json.dumps(v).encode('utf-8')
                )
                
                # Send a large message to test compression
                large_data = {'data': 'x' * 10000, 'compression': comp_type}
                future = producer.send(topic, value=large_data)
                metadata = future.get(timeout=10)
                producer.close()
                
                # Consumer to verify
                consumer = KafkaConsumer(
                    topic,
                    bootstrap_servers=self.bootstrap_servers,
                    auto_offset_reset='earliest',
                    value_deserializer=lambda m: json.loads(m.decode('utf-8')),
                    consumer_timeout_ms=5000
                )
                
                received = None
                for message in consumer:
                    received = message.value
                    break
                
                consumer.close()
                
                success = received and received['compression'] == comp_type
                self.log_result(
                    f"Compression: {comp_type}",
                    success,
                    f"Message sent and received with {comp_type} compression"
                )
                
            except Exception as e:
                self.log_result(f"Compression: {comp_type}", False, str(e))
    
    def test_basic_produce_consume(self):
        """Test basic produce and consume functionality"""
        print("\n=== Testing Basic Produce/Consume ===")
        topic = f"{TEST_PREFIX}-basic"
        
        try:
            # Produce
            producer = KafkaProducer(
                bootstrap_servers=self.bootstrap_servers,
                value_serializer=lambda v: json.dumps(v).encode('utf-8'),
                key_serializer=lambda k: k.encode('utf-8') if k else None
            )
            
            test_messages = [
                ('key1', {'id': 1, 'data': 'message1'}),
                ('key2', {'id': 2, 'data': 'message2'}),
                (None, {'id': 3, 'data': 'message3'})
            ]
            
            for key, value in test_messages:
                producer.send(topic, key=key, value=value)
            producer.flush()
            producer.close()
            
            # Consume
            consumer = KafkaConsumer(
                topic,
                bootstrap_servers=self.bootstrap_servers,
                auto_offset_reset='earliest',
                value_deserializer=lambda m: json.loads(m.decode('utf-8')),
                key_deserializer=lambda k: k.decode('utf-8') if k else None,
                consumer_timeout_ms=5000
            )
            
            received = []
            for message in consumer:
                received.append((message.key, message.value))
                if len(received) >= 3:
                    break
            
            consumer.close()
            
            self.log_result(
                "Basic produce/consume",
                len(received) == 3,
                f"Sent 3 messages, received {len(received)}"
            )
            
            return len(received) == 3
            
        except Exception as e:
            self.log_result("Basic produce/consume", False, str(e))
            return False
    
    def test_metadata_request(self):
        """Test metadata request functionality"""
        print("\n=== Testing Metadata Requests ===")
        
        try:
            admin = KafkaAdminClient(
                bootstrap_servers=self.bootstrap_servers,
                request_timeout_ms=10000
            )
            
            # Get cluster metadata
            metadata = admin.list_topics()
            
            self.log_result(
                "Metadata request",
                metadata is not None and len(metadata) > 0,
                f"Retrieved metadata for {len(metadata)} topics"
            )
            
            admin.close()
            return True
            
        except Exception as e:
            self.log_result("Metadata request", False, str(e))
            return False
    
    def run_all_tests(self):
        """Run all tests"""
        print("=" * 60)
        print("CHRONIK STREAM COMPREHENSIVE TEST SUITE")
        print("=" * 60)
        
        # Check if server is running - use a simple producer test instead of admin
        try:
            test_producer = KafkaProducer(
                bootstrap_servers=self.bootstrap_servers,
                request_timeout_ms=5000,
                max_block_ms=5000
            )
            test_producer.close()
        except Exception as e:
            print(f"\n❌ ERROR: Cannot connect to Chronik Stream at {self.bootstrap_servers}")
            print(f"   Make sure the server is running: cargo run --bin chronik")
            print(f"   Error: {e}")
            return False
        
        # Run tests
        self.test_auto_topic_creation()
        self.test_basic_produce_consume()
        self.test_consumer_group()
        self.test_offset_commit_fetch()
        self.test_compression()
        self.test_metadata_request()
        
        # Print summary
        print("\n" + "=" * 60)
        print("TEST SUMMARY")
        print("=" * 60)
        for result in self.results:
            print(result)
        
        print(f"\nTotal: {self.passed + self.failed} tests")
        print(f"Passed: {self.passed}")
        print(f"Failed: {self.failed}")
        
        if self.failed == 0:
            print("\n🎉 ALL TESTS PASSED!")
            return True
        else:
            print(f"\n⚠️  {self.failed} TESTS FAILED")
            return False

if __name__ == '__main__':
    tester = ChronikStreamTester()
    success = tester.run_all_tests()
    sys.exit(0 if success else 1)