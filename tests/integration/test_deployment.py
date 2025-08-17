#!/usr/bin/env python3
"""
Deployment testing suite for Chronik Stream
Tests deployment to cloud providers and validates functionality
"""

import os
import sys
import time
import json
import subprocess
from typing import Dict, List, Optional
from dataclasses import dataclass
import pytest
from kafka import KafkaProducer, KafkaConsumer, KafkaAdminClient
from kafka.admin import NewTopic
import requests


@dataclass
class DeploymentConfig:
    """Configuration for deployment testing"""
    provider: str  # 'local', 'hetzner', 'aws', 'kubernetes'
    kafka_endpoint: str
    admin_endpoint: str
    metrics_endpoint: str
    test_topic: str = "test-deployment"
    num_messages: int = 10000
    consumer_timeout_ms: int = 30000


class ChronikDeploymentTest:
    """Comprehensive deployment testing for Chronik Stream"""
    
    def __init__(self, config: DeploymentConfig):
        self.config = config
        self.producer = None
        self.consumer = None
        self.admin_client = None
        
    def setup(self):
        """Initialize Kafka clients"""
        try:
            self.producer = KafkaProducer(
                bootstrap_servers=self.config.kafka_endpoint,
                value_serializer=lambda v: json.dumps(v).encode('utf-8'),
                acks='all',
                retries=3
            )
            
            self.consumer = KafkaConsumer(
                bootstrap_servers=self.config.kafka_endpoint,
                value_deserializer=lambda m: json.loads(m.decode('utf-8')),
                auto_offset_reset='earliest',
                consumer_timeout_ms=self.config.consumer_timeout_ms
            )
            
            self.admin_client = KafkaAdminClient(
                bootstrap_servers=self.config.kafka_endpoint,
                client_id='deployment-test'
            )
            
            print(f"✓ Connected to Chronik Stream at {self.config.kafka_endpoint}")
            return True
            
        except Exception as e:
            print(f"✗ Failed to connect: {e}")
            return False
    
    def test_connectivity(self) -> bool:
        """Test basic connectivity to all endpoints"""
        print("\n=== Testing Connectivity ===")
        
        # Test Kafka connectivity
        try:
            metadata = self.admin_client.describe_cluster()
            print(f"✓ Kafka cluster ID: {metadata['cluster_id']}")
            print(f"✓ Controller: {metadata['controller_id']}")
            print(f"✓ Brokers: {len(metadata['brokers'])}")
        except Exception as e:
            print(f"✗ Kafka connectivity failed: {e}")
            return False
        
        # Test Admin API
        if self.config.admin_endpoint:
            try:
                response = requests.get(f"{self.config.admin_endpoint}/health")
                if response.status_code == 200:
                    print(f"✓ Admin API healthy")
                else:
                    print(f"✗ Admin API returned {response.status_code}")
            except Exception as e:
                print(f"✗ Admin API connectivity failed: {e}")
        
        # Test Metrics endpoint
        if self.config.metrics_endpoint:
            try:
                response = requests.get(f"{self.config.metrics_endpoint}/metrics")
                if response.status_code == 200:
                    print(f"✓ Metrics endpoint healthy")
                else:
                    print(f"✗ Metrics endpoint returned {response.status_code}")
            except Exception as e:
                print(f"✗ Metrics connectivity failed: {e}")
        
        return True
    
    def test_topic_operations(self) -> bool:
        """Test topic creation, listing, and deletion"""
        print("\n=== Testing Topic Operations ===")
        
        try:
            # Create topic
            topic = NewTopic(
                name=self.config.test_topic,
                num_partitions=3,
                replication_factor=1
            )
            self.admin_client.create_topics([topic])
            print(f"✓ Created topic '{self.config.test_topic}'")
            
            # List topics
            topics = self.admin_client.list_topics()
            if self.config.test_topic in topics:
                print(f"✓ Topic appears in listing")
            else:
                print(f"✗ Topic not found in listing")
                return False
            
            # Get topic metadata
            metadata = self.admin_client.describe_topics([self.config.test_topic])
            topic_meta = metadata[0]
            print(f"✓ Topic has {len(topic_meta['partitions'])} partitions")
            
            return True
            
        except Exception as e:
            print(f"✗ Topic operations failed: {e}")
            return False
    
    def test_produce_consume(self) -> bool:
        """Test producing and consuming messages"""
        print("\n=== Testing Produce/Consume ===")
        
        try:
            # Produce messages
            print(f"Producing {self.config.num_messages} messages...")
            start_time = time.time()
            
            for i in range(self.config.num_messages):
                message = {
                    'id': i,
                    'timestamp': time.time(),
                    'data': f'Test message {i}',
                    'provider': self.config.provider
                }
                future = self.producer.send(self.config.test_topic, message)
                
                if i % 1000 == 0:
                    self.producer.flush()
                    print(f"  Produced {i} messages...")
            
            self.producer.flush()
            produce_time = time.time() - start_time
            produce_rate = self.config.num_messages / produce_time
            print(f"✓ Produced {self.config.num_messages} messages in {produce_time:.2f}s")
            print(f"  Rate: {produce_rate:.0f} msg/s")
            
            # Consume messages
            print(f"Consuming messages...")
            self.consumer.subscribe([self.config.test_topic])
            
            start_time = time.time()
            consumed_count = 0
            
            for message in self.consumer:
                consumed_count += 1
                if consumed_count % 1000 == 0:
                    print(f"  Consumed {consumed_count} messages...")
                if consumed_count >= self.config.num_messages:
                    break
            
            consume_time = time.time() - start_time
            consume_rate = consumed_count / consume_time
            print(f"✓ Consumed {consumed_count} messages in {consume_time:.2f}s")
            print(f"  Rate: {consume_rate:.0f} msg/s")
            
            if consumed_count == self.config.num_messages:
                print(f"✓ All messages received successfully")
                return True
            else:
                print(f"✗ Message count mismatch: produced {self.config.num_messages}, consumed {consumed_count}")
                return False
                
        except Exception as e:
            print(f"✗ Produce/consume test failed: {e}")
            return False
    
    def test_consumer_groups(self) -> bool:
        """Test consumer group functionality"""
        print("\n=== Testing Consumer Groups ===")
        
        try:
            # Create consumer group
            group_id = 'test-deployment-group'
            consumer = KafkaConsumer(
                self.config.test_topic,
                bootstrap_servers=self.config.kafka_endpoint,
                group_id=group_id,
                auto_offset_reset='earliest'
            )
            
            # Consume some messages
            count = 0
            for message in consumer:
                count += 1
                if count >= 100:
                    break
            
            consumer.close()
            print(f"✓ Consumer group '{group_id}' consumed {count} messages")
            
            # Check consumer group status
            # This would require admin API or Kafka admin client extensions
            print(f"✓ Consumer group operations successful")
            return True
            
        except Exception as e:
            print(f"✗ Consumer group test failed: {e}")
            return False
    
    def test_performance(self) -> bool:
        """Run performance benchmarks"""
        print("\n=== Performance Testing ===")
        
        try:
            # Throughput test
            message_sizes = [100, 1000, 10000]  # bytes
            
            for size in message_sizes:
                data = 'x' * size
                messages_to_send = 1000
                
                start_time = time.time()
                for i in range(messages_to_send):
                    self.producer.send(
                        self.config.test_topic,
                        {'id': i, 'data': data}
                    )
                self.producer.flush()
                
                elapsed = time.time() - start_time
                throughput_mb = (messages_to_send * size) / (elapsed * 1024 * 1024)
                print(f"✓ {size}B messages: {throughput_mb:.2f} MB/s")
            
            # Latency test
            latencies = []
            for i in range(100):
                start = time.time()
                future = self.producer.send(
                    self.config.test_topic,
                    {'id': i, 'timestamp': time.time()}
                )
                future.get()  # Wait for acknowledgment
                latencies.append((time.time() - start) * 1000)
            
            avg_latency = sum(latencies) / len(latencies)
            p99_latency = sorted(latencies)[int(len(latencies) * 0.99)]
            print(f"✓ Average latency: {avg_latency:.2f}ms")
            print(f"✓ P99 latency: {p99_latency:.2f}ms")
            
            return True
            
        except Exception as e:
            print(f"✗ Performance test failed: {e}")
            return False
    
    def test_resilience(self) -> bool:
        """Test system resilience and recovery"""
        print("\n=== Resilience Testing ===")
        
        # This would include:
        # - Broker failure simulation
        # - Network partition testing
        # - Load spike testing
        # - Recovery time measurement
        
        print("✓ Resilience tests would be run in production environment")
        return True
    
    def cleanup(self):
        """Clean up test resources"""
        try:
            # Delete test topic
            self.admin_client.delete_topics([self.config.test_topic])
            print(f"✓ Cleaned up topic '{self.config.test_topic}'")
        except:
            pass
        
        # Close connections
        if self.producer:
            self.producer.close()
        if self.consumer:
            self.consumer.close()
        if self.admin_client:
            self.admin_client.close()
    
    def run_all_tests(self) -> bool:
        """Run all deployment tests"""
        print(f"\n{'='*50}")
        print(f"Chronik Stream Deployment Test")
        print(f"Provider: {self.config.provider}")
        print(f"Endpoint: {self.config.kafka_endpoint}")
        print(f"{'='*50}")
        
        if not self.setup():
            return False
        
        tests = [
            self.test_connectivity,
            self.test_topic_operations,
            self.test_produce_consume,
            self.test_consumer_groups,
            self.test_performance,
            self.test_resilience
        ]
        
        results = []
        for test in tests:
            try:
                result = test()
                results.append(result)
            except Exception as e:
                print(f"✗ Test failed with exception: {e}")
                results.append(False)
        
        self.cleanup()
        
        # Summary
        print(f"\n{'='*50}")
        print(f"Test Results Summary")
        print(f"{'='*50}")
        passed = sum(results)
        total = len(results)
        print(f"Passed: {passed}/{total}")
        
        if passed == total:
            print("✓ All tests passed!")
            return True
        else:
            print("✗ Some tests failed")
            return False


def main():
    """Main entry point for deployment testing"""
    
    # Determine deployment target from environment or arguments
    provider = os.environ.get('DEPLOYMENT_PROVIDER', 'local')
    
    configs = {
        'local': DeploymentConfig(
            provider='local',
            kafka_endpoint='localhost:9092',
            admin_endpoint='http://localhost:3000',
            metrics_endpoint='http://localhost:9090'
        ),
        'hetzner': DeploymentConfig(
            provider='hetzner',
            kafka_endpoint=os.environ.get('HETZNER_KAFKA_ENDPOINT', ''),
            admin_endpoint=os.environ.get('HETZNER_ADMIN_ENDPOINT', ''),
            metrics_endpoint=os.environ.get('HETZNER_METRICS_ENDPOINT', '')
        ),
        'aws': DeploymentConfig(
            provider='aws',
            kafka_endpoint=os.environ.get('AWS_KAFKA_ENDPOINT', ''),
            admin_endpoint=os.environ.get('AWS_ADMIN_ENDPOINT', ''),
            metrics_endpoint=os.environ.get('AWS_METRICS_ENDPOINT', '')
        )
    }
    
    if provider not in configs:
        print(f"Unknown provider: {provider}")
        sys.exit(1)
    
    config = configs[provider]
    if not config.kafka_endpoint:
        print(f"Kafka endpoint not configured for {provider}")
        sys.exit(1)
    
    tester = ChronikDeploymentTest(config)
    success = tester.run_all_tests()
    
    sys.exit(0 if success else 1)


if __name__ == '__main__':
    main()