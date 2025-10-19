#!/usr/bin/env python3
"""
Comprehensive Kafka Python Client Compatibility Tests for Chronik Stream
Tests both kafka-python and confluent-kafka-python (librdkafka)
"""

import json
import os
import sys
import time
import uuid
from dataclasses import dataclass, asdict
from datetime import datetime
from enum import Enum
from typing import Dict, List, Optional, Any
import traceback

# Support both kafka-python and confluent-kafka
try:
    from kafka import KafkaProducer, KafkaConsumer, KafkaAdminClient
    from kafka.admin import NewTopic
    from kafka.errors import KafkaError
    KAFKA_PYTHON_AVAILABLE = True
except ImportError:
    KAFKA_PYTHON_AVAILABLE = False

try:
    from confluent_kafka import Producer, Consumer, AdminClient
    from confluent_kafka.admin import NewTopic as ConfluentNewTopic
    CONFLUENT_KAFKA_AVAILABLE = True
except ImportError:
    CONFLUENT_KAFKA_AVAILABLE = False


class TestStatus(Enum):
    PASSED = "passed"
    FAILED = "failed"
    SKIPPED = "skipped"
    TIMEOUT = "timeout"


@dataclass
class TestResult:
    test_id: str
    timestamp: str
    client: str
    client_version: str
    test_name: str
    category: str
    status: TestStatus
    duration_ms: int
    error_message: Optional[str] = None
    details: Dict[str, Any] = None


class ChronikCompatibilityTests:
    def __init__(self, bootstrap_servers: str = None, client_type: str = "kafka-python"):
        self.bootstrap_servers = bootstrap_servers or os.getenv('BOOTSTRAP_SERVERS', 'localhost:9092')
        self.client_type = client_type
        self.results: List[TestResult] = []
        
    def run_all_tests(self) -> List[TestResult]:
        """Run all compatibility tests"""
        print(f"🧪 Running Chronik compatibility tests with {self.client_type}")
        print(f"📍 Bootstrap servers: {self.bootstrap_servers}")
        
        # Connectivity tests
        self.test_api_versions()
        self.test_metadata()
        
        # Produce/Consume tests
        self.test_produce_single()
        self.test_produce_batch()
        self.test_fetch_single()
        self.test_compression_types()
        
        # Consumer group tests
        self.test_consumer_groups()
        
        # Admin operations
        self.test_admin_operations()
        
        # Regression tests
        self.test_produce_v2_throttle_time()
        
        return self.results
    
    def _record_result(self, test_name: str, category: str, status: TestStatus, 
                      duration_ms: int, error_message: str = None, details: Dict = None):
        """Record a test result"""
        result = TestResult(
            test_id=str(uuid.uuid4()),
            timestamp=datetime.utcnow().isoformat(),
            client=self.client_type,
            client_version=self._get_client_version(),
            test_name=test_name,
            category=category,
            status=status,
            duration_ms=duration_ms,
            error_message=error_message,
            details=details or {}
        )
        self.results.append(result)
        
        # Print immediate feedback
        status_symbol = {
            TestStatus.PASSED: "✅",
            TestStatus.FAILED: "❌",
            TestStatus.SKIPPED: "⏭️",
            TestStatus.TIMEOUT: "⏱️"
        }[status]
        
        print(f"  {status_symbol} {test_name}: {status.value} ({duration_ms}ms)")
        if error_message:
            print(f"     Error: {error_message}")
    
    def _get_client_version(self) -> str:
        """Get the client library version"""
        if self.client_type == "kafka-python":
            import kafka
            return kafka.__version__
        elif self.client_type == "confluent-kafka":
            import confluent_kafka
            return confluent_kafka.version()[0]
        return "unknown"
    
    def test_api_versions(self):
        """Test ApiVersions request/response"""
        start_time = time.time()
        try:
            if self.client_type == "kafka-python" and KAFKA_PYTHON_AVAILABLE:
                from kafka import KafkaClient
                client = KafkaClient(bootstrap_servers=self.bootstrap_servers)
                
                # Check API versions
                api_versions = client.config.get('api_version')
                client.close()
                
                duration_ms = int((time.time() - start_time) * 1000)
                self._record_result(
                    "api_versions",
                    "connectivity",
                    TestStatus.PASSED,
                    duration_ms,
                    details={"api_versions": str(api_versions)}
                )
            
            elif self.client_type == "confluent-kafka" and CONFLUENT_KAFKA_AVAILABLE:
                admin = AdminClient({'bootstrap.servers': self.bootstrap_servers})
                metadata = admin.list_topics(timeout=5)
                
                duration_ms = int((time.time() - start_time) * 1000)
                self._record_result(
                    "api_versions",
                    "connectivity",
                    TestStatus.PASSED,
                    duration_ms,
                    details={"broker_count": len(metadata.brokers)}
                )
            else:
                self._record_result(
                    "api_versions",
                    "connectivity",
                    TestStatus.SKIPPED,
                    0,
                    error_message="Client library not available"
                )
                
        except Exception as e:
            duration_ms = int((time.time() - start_time) * 1000)
            self._record_result(
                "api_versions",
                "connectivity",
                TestStatus.FAILED,
                duration_ms,
                error_message=str(e)
            )
    
    def test_metadata(self):
        """Test Metadata request/response"""
        start_time = time.time()
        try:
            if self.client_type == "kafka-python" and KAFKA_PYTHON_AVAILABLE:
                consumer = KafkaConsumer(
                    bootstrap_servers=self.bootstrap_servers,
                    consumer_timeout_ms=1000
                )
                
                # Get cluster metadata
                metadata = consumer.bootstrap_connected()
                topics = consumer.topics()
                
                consumer.close()
                
                duration_ms = int((time.time() - start_time) * 1000)
                self._record_result(
                    "metadata",
                    "connectivity",
                    TestStatus.PASSED,
                    duration_ms,
                    details={
                        "connected": metadata,
                        "topic_count": len(topics) if topics else 0
                    }
                )
            
            elif self.client_type == "confluent-kafka" and CONFLUENT_KAFKA_AVAILABLE:
                admin = AdminClient({'bootstrap.servers': self.bootstrap_servers})
                metadata = admin.list_topics(timeout=5)
                
                duration_ms = int((time.time() - start_time) * 1000)
                self._record_result(
                    "metadata",
                    "connectivity",
                    TestStatus.PASSED,
                    duration_ms,
                    details={
                        "topic_count": len(metadata.topics),
                        "broker_count": len(metadata.brokers)
                    }
                )
            else:
                self._record_result(
                    "metadata",
                    "connectivity",
                    TestStatus.SKIPPED,
                    0,
                    error_message="Client library not available"
                )
                
        except Exception as e:
            duration_ms = int((time.time() - start_time) * 1000)
            self._record_result(
                "metadata",
                "connectivity",
                TestStatus.FAILED,
                duration_ms,
                error_message=str(e)
            )
    
    def test_produce_single(self):
        """Test producing a single message"""
        start_time = time.time()
        topic = f"compat-test-{uuid.uuid4().hex[:8]}"
        
        try:
            if self.client_type == "kafka-python" and KAFKA_PYTHON_AVAILABLE:
                producer = KafkaProducer(
                    bootstrap_servers=self.bootstrap_servers,
                    value_serializer=lambda v: json.dumps(v).encode('utf-8'),
                    request_timeout_ms=5000
                )
                
                # Send message
                future = producer.send(topic, {'test': 'single_message', 'timestamp': time.time()})
                metadata = future.get(timeout=5)
                
                producer.close()
                
                duration_ms = int((time.time() - start_time) * 1000)
                self._record_result(
                    "produce_single",
                    "produce_consume",
                    TestStatus.PASSED,
                    duration_ms,
                    details={
                        "topic": metadata.topic,
                        "partition": metadata.partition,
                        "offset": metadata.offset
                    }
                )
            
            elif self.client_type == "confluent-kafka" and CONFLUENT_KAFKA_AVAILABLE:
                delivered = []
                
                def delivery_report(err, msg):
                    if err is not None:
                        raise Exception(f"Delivery failed: {err}")
                    delivered.append(msg)
                
                producer = Producer({
                    'bootstrap.servers': self.bootstrap_servers,
                    'client.id': 'compat-test'
                })
                
                producer.produce(
                    topic,
                    value=json.dumps({'test': 'single_message'}).encode('utf-8'),
                    callback=delivery_report
                )
                
                producer.flush(timeout=5)
                
                duration_ms = int((time.time() - start_time) * 1000)
                self._record_result(
                    "produce_single",
                    "produce_consume",
                    TestStatus.PASSED if delivered else TestStatus.FAILED,
                    duration_ms,
                    details={
                        "messages_delivered": len(delivered)
                    }
                )
            else:
                self._record_result(
                    "produce_single",
                    "produce_consume",
                    TestStatus.SKIPPED,
                    0,
                    error_message="Client library not available"
                )
                
        except Exception as e:
            duration_ms = int((time.time() - start_time) * 1000)
            self._record_result(
                "produce_single",
                "produce_consume",
                TestStatus.FAILED,
                duration_ms,
                error_message=str(e)
            )
    
    def test_produce_batch(self):
        """Test producing multiple messages in batch"""
        start_time = time.time()
        topic = f"compat-test-batch-{uuid.uuid4().hex[:8]}"
        num_messages = 100
        
        try:
            if self.client_type == "kafka-python" and KAFKA_PYTHON_AVAILABLE:
                producer = KafkaProducer(
                    bootstrap_servers=self.bootstrap_servers,
                    value_serializer=lambda v: json.dumps(v).encode('utf-8'),
                    batch_size=16384,
                    linger_ms=10
                )
                
                # Send batch
                futures = []
                for i in range(num_messages):
                    future = producer.send(topic, {'batch': i, 'timestamp': time.time()})
                    futures.append(future)
                
                # Wait for all
                for future in futures:
                    future.get(timeout=10)
                
                producer.close()
                
                duration_ms = int((time.time() - start_time) * 1000)
                self._record_result(
                    "produce_batch",
                    "produce_consume",
                    TestStatus.PASSED,
                    duration_ms,
                    details={
                        "messages_sent": num_messages,
                        "topic": topic
                    }
                )
            
            elif self.client_type == "confluent-kafka" and CONFLUENT_KAFKA_AVAILABLE:
                delivered_count = [0]
                
                def delivery_report(err, msg):
                    if err is None:
                        delivered_count[0] += 1
                
                producer = Producer({
                    'bootstrap.servers': self.bootstrap_servers,
                    'batch.size': 16384,
                    'linger.ms': 10
                })
                
                for i in range(num_messages):
                    producer.produce(
                        topic,
                        value=json.dumps({'batch': i}).encode('utf-8'),
                        callback=delivery_report
                    )
                
                producer.flush(timeout=10)
                
                duration_ms = int((time.time() - start_time) * 1000)
                self._record_result(
                    "produce_batch",
                    "produce_consume",
                    TestStatus.PASSED if delivered_count[0] == num_messages else TestStatus.FAILED,
                    duration_ms,
                    details={
                        "messages_sent": num_messages,
                        "messages_delivered": delivered_count[0]
                    }
                )
            else:
                self._record_result(
                    "produce_batch",
                    "produce_consume",
                    TestStatus.SKIPPED,
                    0,
                    error_message="Client library not available"
                )
                
        except Exception as e:
            duration_ms = int((time.time() - start_time) * 1000)
            self._record_result(
                "produce_batch",
                "produce_consume",
                TestStatus.FAILED,
                duration_ms,
                error_message=str(e)
            )
    
    def test_fetch_single(self):
        """Test fetching messages"""
        start_time = time.time()
        topic = f"compat-test-fetch-{uuid.uuid4().hex[:8]}"
        
        try:
            # First produce a message
            if self.client_type == "kafka-python" and KAFKA_PYTHON_AVAILABLE:
                # Produce
                producer = KafkaProducer(
                    bootstrap_servers=self.bootstrap_servers,
                    value_serializer=lambda v: json.dumps(v).encode('utf-8')
                )
                producer.send(topic, {'test': 'fetch_test'}).get(timeout=5)
                producer.close()
                
                # Consume
                consumer = KafkaConsumer(
                    topic,
                    bootstrap_servers=self.bootstrap_servers,
                    auto_offset_reset='earliest',
                    value_deserializer=lambda m: json.loads(m.decode('utf-8')),
                    consumer_timeout_ms=5000
                )
                
                messages = []
                for message in consumer:
                    messages.append(message.value)
                    if len(messages) >= 1:
                        break
                
                consumer.close()
                
                duration_ms = int((time.time() - start_time) * 1000)
                self._record_result(
                    "fetch_single",
                    "produce_consume",
                    TestStatus.PASSED if messages else TestStatus.FAILED,
                    duration_ms,
                    details={
                        "messages_fetched": len(messages)
                    }
                )
            
            elif self.client_type == "confluent-kafka" and CONFLUENT_KAFKA_AVAILABLE:
                # Produce
                producer = Producer({'bootstrap.servers': self.bootstrap_servers})
                producer.produce(topic, value=json.dumps({'test': 'fetch_test'}).encode('utf-8'))
                producer.flush()
                
                # Consume
                consumer = Consumer({
                    'bootstrap.servers': self.bootstrap_servers,
                    'group.id': f'test-group-{uuid.uuid4().hex[:8]}',
                    'auto.offset.reset': 'earliest'
                })
                
                consumer.subscribe([topic])
                
                messages = []
                timeout = time.time() + 5
                while time.time() < timeout:
                    msg = consumer.poll(1.0)
                    if msg and not msg.error():
                        messages.append(msg.value())
                        break
                
                consumer.close()
                
                duration_ms = int((time.time() - start_time) * 1000)
                self._record_result(
                    "fetch_single",
                    "produce_consume",
                    TestStatus.PASSED if messages else TestStatus.FAILED,
                    duration_ms,
                    details={
                        "messages_fetched": len(messages)
                    }
                )
            else:
                self._record_result(
                    "fetch_single",
                    "produce_consume",
                    TestStatus.SKIPPED,
                    0,
                    error_message="Client library not available"
                )
                
        except Exception as e:
            duration_ms = int((time.time() - start_time) * 1000)
            self._record_result(
                "fetch_single",
                "produce_consume",
                TestStatus.FAILED,
                duration_ms,
                error_message=str(e)
            )
    
    def test_compression_types(self):
        """Test different compression types"""
        compression_types = ['none', 'gzip', 'snappy', 'lz4', 'zstd']
        
        for compression in compression_types:
            start_time = time.time()
            topic = f"compat-test-{compression}-{uuid.uuid4().hex[:8]}"
            
            try:
                if self.client_type == "kafka-python" and KAFKA_PYTHON_AVAILABLE:
                    # Skip unsupported compression types
                    if compression in ['zstd'] and not hasattr(KafkaProducer, 'CODEC_ZSTD'):
                        self._record_result(
                            f"compression_{compression}",
                            "produce_consume",
                            TestStatus.SKIPPED,
                            0,
                            error_message="Compression type not supported"
                        )
                        continue
                    
                    producer = KafkaProducer(
                        bootstrap_servers=self.bootstrap_servers,
                        compression_type=compression if compression != 'none' else None,
                        value_serializer=lambda v: json.dumps(v).encode('utf-8')
                    )
                    
                    future = producer.send(topic, {'compression': compression, 'data': 'x' * 1000})
                    metadata = future.get(timeout=5)
                    producer.close()
                    
                    duration_ms = int((time.time() - start_time) * 1000)
                    self._record_result(
                        f"compression_{compression}",
                        "produce_consume",
                        TestStatus.PASSED,
                        duration_ms,
                        details={
                            "compression": compression,
                            "offset": metadata.offset
                        }
                    )
                
                elif self.client_type == "confluent-kafka" and CONFLUENT_KAFKA_AVAILABLE:
                    config = {
                        'bootstrap.servers': self.bootstrap_servers,
                        'compression.type': compression
                    }
                    
                    producer = Producer(config)
                    delivered = [False]
                    
                    def delivery_report(err, msg):
                        if err is None:
                            delivered[0] = True
                    
                    producer.produce(
                        topic,
                        value=json.dumps({'compression': compression}).encode('utf-8'),
                        callback=delivery_report
                    )
                    producer.flush(timeout=5)
                    
                    duration_ms = int((time.time() - start_time) * 1000)
                    self._record_result(
                        f"compression_{compression}",
                        "produce_consume",
                        TestStatus.PASSED if delivered[0] else TestStatus.FAILED,
                        duration_ms,
                        details={"compression": compression}
                    )
                    
            except Exception as e:
                duration_ms = int((time.time() - start_time) * 1000)
                self._record_result(
                    f"compression_{compression}",
                    "produce_consume",
                    TestStatus.FAILED,
                    duration_ms,
                    error_message=str(e)
                )
    
    def test_consumer_groups(self):
        """Test consumer group operations"""
        start_time = time.time()
        topic = f"compat-test-group-{uuid.uuid4().hex[:8]}"
        group_id = f"test-group-{uuid.uuid4().hex[:8]}"
        
        try:
            if self.client_type == "kafka-python" and KAFKA_PYTHON_AVAILABLE:
                # Create topic and produce messages
                producer = KafkaProducer(
                    bootstrap_servers=self.bootstrap_servers,
                    value_serializer=lambda v: json.dumps(v).encode('utf-8')
                )
                
                for i in range(10):
                    producer.send(topic, {'message': i})
                producer.flush()
                producer.close()
                
                # Consumer group
                consumer = KafkaConsumer(
                    topic,
                    bootstrap_servers=self.bootstrap_servers,
                    group_id=group_id,
                    auto_offset_reset='earliest',
                    enable_auto_commit=True,
                    consumer_timeout_ms=5000
                )
                
                messages = []
                for message in consumer:
                    messages.append(message)
                    if len(messages) >= 10:
                        break
                
                # Commit offsets
                consumer.commit()
                consumer.close()
                
                duration_ms = int((time.time() - start_time) * 1000)
                self._record_result(
                    "consumer_groups",
                    "consumer_groups",
                    TestStatus.PASSED,
                    duration_ms,
                    details={
                        "group_id": group_id,
                        "messages_consumed": len(messages)
                    }
                )
                
            else:
                self._record_result(
                    "consumer_groups",
                    "consumer_groups",
                    TestStatus.SKIPPED,
                    0,
                    error_message="Test not implemented for this client"
                )
                
        except Exception as e:
            duration_ms = int((time.time() - start_time) * 1000)
            self._record_result(
                "consumer_groups",
                "consumer_groups",
                TestStatus.FAILED,
                duration_ms,
                error_message=str(e)
            )
    
    def test_admin_operations(self):
        """Test admin operations (create/delete topics)"""
        start_time = time.time()
        topic_name = f"compat-admin-{uuid.uuid4().hex[:8]}"
        
        try:
            if self.client_type == "kafka-python" and KAFKA_PYTHON_AVAILABLE:
                admin = KafkaAdminClient(
                    bootstrap_servers=self.bootstrap_servers,
                    request_timeout_ms=5000
                )
                
                # Create topic
                topic = NewTopic(name=topic_name, num_partitions=3, replication_factor=1)
                admin.create_topics([topic])
                
                # List topics
                topics = admin.list_topics()
                
                # Delete topic
                admin.delete_topics([topic_name])
                
                admin.close()
                
                duration_ms = int((time.time() - start_time) * 1000)
                self._record_result(
                    "admin_operations",
                    "admin",
                    TestStatus.PASSED,
                    duration_ms,
                    details={
                        "topic_created": topic_name,
                        "total_topics": len(topics) if topics else 0
                    }
                )
                
            else:
                self._record_result(
                    "admin_operations",
                    "admin",
                    TestStatus.SKIPPED,
                    0,
                    error_message="Test not implemented for this client"
                )
                
        except Exception as e:
            duration_ms = int((time.time() - start_time) * 1000)
            self._record_result(
                "admin_operations",
                "admin",
                TestStatus.FAILED,
                duration_ms,
                error_message=str(e)
            )
    
    def test_produce_v2_throttle_time(self):
        """Regression test for ProduceResponse v2 throttle_time_ms position"""
        start_time = time.time()
        topic = f"compat-regression-v2-{uuid.uuid4().hex[:8]}"
        
        try:
            if self.client_type == "kafka-python" and KAFKA_PYTHON_AVAILABLE:
                # Force API version to trigger v2
                producer = KafkaProducer(
                    bootstrap_servers=self.bootstrap_servers,
                    api_version=(0, 10, 1),  # Forces ProduceRequest v2
                    request_timeout_ms=5000,
                    value_serializer=lambda v: json.dumps(v).encode('utf-8')
                )
                
                # This would timeout with the bug
                future = producer.send(topic, {'regression': 'produce_v2_test'})
                metadata = future.get(timeout=5)
                
                producer.close()
                
                duration_ms = int((time.time() - start_time) * 1000)
                self._record_result(
                    "produce_v2_throttle_time",
                    "regression",
                    TestStatus.PASSED,
                    duration_ms,
                    details={
                        "topic": metadata.topic,
                        "partition": metadata.partition,
                        "offset": metadata.offset,
                        "timestamp": metadata.timestamp
                    }
                )
                
            else:
                self._record_result(
                    "produce_v2_throttle_time",
                    "regression",
                    TestStatus.SKIPPED,
                    0,
                    error_message="Regression test specific to kafka-python"
                )
                
        except Exception as e:
            duration_ms = int((time.time() - start_time) * 1000)
            self._record_result(
                "produce_v2_throttle_time",
                "regression",
                TestStatus.FAILED,
                duration_ms,
                error_message=str(e),
                details={"traceback": traceback.format_exc()}
            )
    
    def save_results(self, output_dir: str = "/results"):
        """Save test results to JSON file"""
        os.makedirs(output_dir, exist_ok=True)
        
        timestamp = datetime.utcnow().strftime("%Y%m%d_%H%M%S")
        filename = f"{output_dir}/{self.client_type}_{timestamp}.json"
        
        results_dict = [asdict(r) for r in self.results]
        # Convert enum to string
        for r in results_dict:
            r['status'] = r['status'].value if isinstance(r['status'], TestStatus) else r['status']
        
        with open(filename, 'w') as f:
            json.dump(results_dict, f, indent=2, default=str)
        
        print(f"\n💾 Results saved to {filename}")
        
        # Print summary
        total = len(self.results)
        passed = sum(1 for r in self.results if r.status == TestStatus.PASSED)
        failed = sum(1 for r in self.results if r.status == TestStatus.FAILED)
        skipped = sum(1 for r in self.results if r.status == TestStatus.SKIPPED)
        
        print(f"\n📊 Test Summary:")
        print(f"  Total:   {total}")
        print(f"  ✅ Passed:  {passed} ({passed/total*100:.1f}%)")
        print(f"  ❌ Failed:  {failed} ({failed/total*100:.1f}%)")
        print(f"  ⏭️ Skipped: {skipped} ({skipped/total*100:.1f}%)")
        
        return filename


def main():
    """Main entry point"""
    print("=" * 60)
    print("Chronik Stream Python Client Compatibility Tests")
    print("=" * 60)
    
    # Determine which client to test
    test_mode = os.getenv('TEST_MODE', 'both')
    
    if test_mode == 'both':
        clients_to_test = []
        if KAFKA_PYTHON_AVAILABLE:
            clients_to_test.append('kafka-python')
        if CONFLUENT_KAFKA_AVAILABLE:
            clients_to_test.append('confluent-kafka')
    elif test_mode == 'kafka-python':
        clients_to_test = ['kafka-python'] if KAFKA_PYTHON_AVAILABLE else []
    elif test_mode == 'confluent-kafka':
        clients_to_test = ['confluent-kafka'] if CONFLUENT_KAFKA_AVAILABLE else []
    else:
        clients_to_test = []
    
    if not clients_to_test:
        print("❌ No client libraries available for testing")
        sys.exit(1)
    
    all_results = []
    
    for client_type in clients_to_test:
        print(f"\n🔧 Testing with {client_type}")
        print("-" * 40)
        
        tester = ChronikCompatibilityTests(client_type=client_type)
        results = tester.run_all_tests()
        tester.save_results()
        all_results.extend(results)
    
    # Overall summary
    print("\n" + "=" * 60)
    print("OVERALL RESULTS")
    print("=" * 60)
    
    total_failed = sum(1 for r in all_results if r.status == TestStatus.FAILED)
    
    if total_failed > 0:
        print(f"❌ {total_failed} test(s) failed")
        for r in all_results:
            if r.status == TestStatus.FAILED:
                print(f"  - {r.client}/{r.test_name}: {r.error_message}")
        sys.exit(1)
    else:
        print("✅ All tests passed!")
        sys.exit(0)


if __name__ == "__main__":
    main()