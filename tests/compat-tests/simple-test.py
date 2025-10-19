#!/usr/bin/env python3
"""Simple test to verify Kafka connectivity"""

from kafka import KafkaProducer, KafkaConsumer
from kafka.admin import KafkaAdminClient
import json
import time

def test_basic():
    """Test basic Kafka connectivity"""
    
    print("Testing connection to localhost:9092...")
    
    # Test 1: Admin client
    try:
        admin = KafkaAdminClient(
            bootstrap_servers='localhost:9092',
            request_timeout_ms=5000,
            api_version=(0, 10, 0)
        )
        metadata = admin._client.cluster
        print(f"✅ Connected! Brokers: {metadata.brokers()}")
        admin.close()
    except Exception as e:
        print(f"❌ Admin client failed: {e}")
        return False
    
    # Test 2: Producer
    try:
        producer = KafkaProducer(
            bootstrap_servers='localhost:9092',
            request_timeout_ms=5000,
            api_version=(0, 10, 0)
        )
        
        # Send a test message
        future = producer.send('test-topic', b'test-message')
        result = future.get(timeout=5)
        print(f"✅ Produced message to partition {result.partition} at offset {result.offset}")
        producer.close()
    except Exception as e:
        print(f"❌ Producer failed: {e}")
        return False
    
    # Test 3: Consumer
    try:
        consumer = KafkaConsumer(
            'test-topic',
            bootstrap_servers='localhost:9092',
            auto_offset_reset='earliest',
            consumer_timeout_ms=5000,
            api_version=(0, 10, 0)
        )
        
        # Try to consume
        for message in consumer:
            print(f"✅ Consumed message: {message.value}")
            break
        
        consumer.close()
    except Exception as e:
        print(f"❌ Consumer failed: {e}")
        return False
    
    print("\n✅ All tests passed!")
    return True

if __name__ == "__main__":
    success = test_basic()
    exit(0 if success else 1)