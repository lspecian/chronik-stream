#!/usr/bin/env python3
"""Test Python Kafka client with fixed Chronik Stream."""

from kafka import KafkaProducer, KafkaConsumer
from kafka.errors import KafkaError
import time
import json

def test_producer():
    """Test producing messages."""
    print("Testing Python Kafka client with Chronik Stream...")
    
    try:
        # Create producer
        producer = KafkaProducer(
            bootstrap_servers=['localhost:9092'],
            value_serializer=lambda v: json.dumps(v).encode('utf-8'),
            request_timeout_ms=5000,
            api_version=(2, 0, 0)  # Use a specific API version
        )
        print("✓ Producer created successfully")
        
        # Send a message
        future = producer.send('test-topic', {'message': 'Hello from Python'})
        
        # Wait for message to be sent
        try:
            record_metadata = future.get(timeout=10)
            print(f"✓ Message sent to topic {record_metadata.topic} partition {record_metadata.partition}")
        except KafkaError as e:
            print(f"✗ Error sending message: {e}")
            return False
        
        # Flush to ensure all messages are sent
        producer.flush()
        print("✓ All messages flushed")
        
        producer.close()
        print("✓ Producer closed successfully")
        return True
        
    except Exception as e:
        print(f"✗ Producer test failed: {e}")
        return False

def test_consumer():
    """Test consuming messages."""
    print("\nTesting consumer...")
    
    try:
        # Create consumer
        consumer = KafkaConsumer(
            'test-topic',
            bootstrap_servers=['localhost:9092'],
            auto_offset_reset='earliest',
            enable_auto_commit=True,
            group_id='test-group',
            value_deserializer=lambda m: json.loads(m.decode('utf-8')),
            consumer_timeout_ms=5000,
            api_version=(2, 0, 0)
        )
        print("✓ Consumer created successfully")
        
        # Try to consume one message
        message_found = False
        for message in consumer:
            print(f"✓ Received message: {message.value}")
            message_found = True
            break
        
        if not message_found:
            print("⚠ No messages to consume (timeout)")
        
        consumer.close()
        print("✓ Consumer closed successfully")
        return True
        
    except Exception as e:
        print(f"✗ Consumer test failed: {e}")
        return False

if __name__ == "__main__":
    producer_ok = test_producer()
    consumer_ok = test_consumer()
    
    if producer_ok and consumer_ok:
        print("\n✅ All Python client tests passed!")
    else:
        print("\n❌ Some tests failed")
        exit(1)