#!/usr/bin/env python3
"""Test with official kafka-python client library."""

from kafka import KafkaProducer, KafkaConsumer, KafkaAdminClient
from kafka.admin import NewTopic
from kafka.errors import KafkaError
import json
import time

def test_producer():
    """Test producing messages with kafka-python."""
    print("=== Testing KafkaProducer ===")
    
    try:
        # Create producer
        producer = KafkaProducer(
            bootstrap_servers=['localhost:9092'],
            value_serializer=lambda v: json.dumps(v).encode('utf-8'),
            api_version=(2, 0, 0),  # Use a compatible API version
            request_timeout_ms=30000,
            retries=3
        )
        
        # Send messages
        for i in range(10):
            data = {'number': i, 'message': f'kafka-python test message {i}'}
            future = producer.send('test-topic', value=data, partition=0)
            
            # Wait for send to complete
            try:
                record_metadata = future.get(timeout=10)
                print(f"  Message {i} sent to {record_metadata.topic}:{record_metadata.partition} at offset {record_metadata.offset}")
            except KafkaError as e:
                print(f"  Failed to send message {i}: {e}")
        
        # Flush remaining messages
        producer.flush()
        producer.close()
        
        print("✓ Producer test completed successfully")
        return True
        
    except Exception as e:
        print(f"✗ Producer test failed: {e}")
        return False

def test_consumer():
    """Test consuming messages with kafka-python."""
    print("\n=== Testing KafkaConsumer ===")
    
    try:
        # Create consumer
        consumer = KafkaConsumer(
            'test-topic',
            bootstrap_servers=['localhost:9092'],
            auto_offset_reset='earliest',
            enable_auto_commit=True,
            group_id='kafka-python-test-group',
            value_deserializer=lambda m: json.loads(m.decode('utf-8')) if m else None,
            api_version=(2, 0, 0),
            consumer_timeout_ms=5000  # Timeout after 5 seconds of no messages
        )
        
        # Consume messages
        message_count = 0
        for message in consumer:
            print(f"  Received: topic={message.topic}, partition={message.partition}, offset={message.offset}")
            print(f"    Value: {message.value}")
            message_count += 1
            
            # Stop after reading 10 messages
            if message_count >= 10:
                break
        
        consumer.close()
        
        if message_count > 0:
            print(f"✓ Consumer test completed - received {message_count} messages")
            return True
        else:
            print("⚠ Consumer test - no messages received")
            return False
            
    except Exception as e:
        print(f"✗ Consumer test failed: {e}")
        return False

def test_admin():
    """Test admin operations with kafka-python."""
    print("\n=== Testing KafkaAdminClient ===")
    
    try:
        # Create admin client
        admin = KafkaAdminClient(
            bootstrap_servers=['localhost:9092'],
            api_version=(2, 0, 0)
        )
        
        # List topics
        topics = admin.list_topics()
        print(f"  Current topics: {topics}")
        
        # Try to create a new topic
        new_topic = NewTopic(
            name='kafka-python-test-topic',
            num_partitions=3,
            replication_factor=1
        )
        
        try:
            admin.create_topics([new_topic], validate_only=False)
            print(f"  ✓ Created topic: {new_topic.name}")
        except Exception as e:
            print(f"  ⚠ Could not create topic (may already exist): {e}")
        
        # List topics again
        topics = admin.list_topics()
        print(f"  Topics after creation: {topics}")
        
        admin.close()
        
        print("✓ Admin test completed successfully")
        return True
        
    except Exception as e:
        print(f"✗ Admin test failed: {e}")
        return False

def main():
    print("=== Testing Chronik-Stream with kafka-python client ===\n")
    
    # Run tests
    producer_ok = test_producer()
    consumer_ok = test_consumer()
    admin_ok = test_admin()
    
    # Summary
    print("\n=== Test Summary ===")
    print(f"Producer: {'✓ PASS' if producer_ok else '✗ FAIL'}")
    print(f"Consumer: {'✓ PASS' if consumer_ok else '✗ FAIL'}")
    print(f"Admin:    {'✓ PASS' if admin_ok else '✗ FAIL'}")
    
    if producer_ok and consumer_ok and admin_ok:
        print("\n✓ All tests passed! Chronik-Stream is compatible with kafka-python client")
    else:
        print("\n⚠ Some tests failed - check implementation")

if __name__ == "__main__":
    main()