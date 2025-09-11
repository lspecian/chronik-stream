#!/usr/bin/env python3
"""Test Fetch API functionality with Chronik Stream"""

import json
import sys
import time
import uuid
from kafka import KafkaProducer, KafkaConsumer
from kafka.errors import KafkaError

def test_produce_and_consume():
    """Test producing messages and then consuming them"""
    broker = 'localhost:9092'
    topic = f'test-fetch-{uuid.uuid4().hex[:8]}'
    
    print(f"Testing Fetch API with topic: {topic}")
    
    # Create producer
    try:
        producer = KafkaProducer(
            bootstrap_servers=[broker],
            api_version=(0, 10, 0),
            value_serializer=lambda v: json.dumps(v).encode('utf-8')
        )
        print("✓ Producer created successfully")
    except Exception as e:
        print(f"✗ Failed to create producer: {e}")
        return False
    
    # Send some messages
    messages_sent = []
    try:
        for i in range(5):
            message = {'id': i, 'message': f'Test message {i}'}
            future = producer.send(topic, value=message)
            record_metadata = future.get(timeout=10)
            messages_sent.append(message)
            print(f"✓ Sent message {i} to partition {record_metadata.partition} at offset {record_metadata.offset}")
        
        producer.flush()
        print(f"✓ Successfully sent {len(messages_sent)} messages")
    except Exception as e:
        print(f"✗ Failed to send messages: {e}")
        return False
    finally:
        producer.close()
    
    # Give the server a moment to process
    time.sleep(1)
    
    # Create consumer and try to fetch messages
    try:
        consumer = KafkaConsumer(
            topic,
            bootstrap_servers=[broker],
            api_version=(0, 10, 0),
            auto_offset_reset='earliest',
            enable_auto_commit=False,
            consumer_timeout_ms=5000,  # 5 second timeout
            value_deserializer=lambda m: json.loads(m.decode('utf-8')) if m else None
        )
        print("✓ Consumer created successfully")
    except Exception as e:
        print(f"✗ Failed to create consumer: {e}")
        return False
    
    # Try to consume messages
    messages_received = []
    try:
        print("Attempting to consume messages...")
        for message in consumer:
            messages_received.append(message.value)
            print(f"✓ Received message at offset {message.offset}: {message.value}")
        
        if len(messages_received) == 0:
            print("✗ No messages received (consumer timeout)")
            return False
        
        print(f"✓ Successfully consumed {len(messages_received)} messages")
        
        # Verify we got all messages
        if len(messages_received) == len(messages_sent):
            print("✓ All sent messages were received")
            return True
        else:
            print(f"✗ Message count mismatch: sent {len(messages_sent)}, received {len(messages_received)}")
            return False
            
    except Exception as e:
        print(f"✗ Failed to consume messages: {e}")
        return False
    finally:
        consumer.close()

def main():
    print("=" * 60)
    print("Chronik Stream Fetch API Test")
    print("=" * 60)
    
    # Run the test
    success = test_produce_and_consume()
    
    print("=" * 60)
    if success:
        print("✓ Fetch API test PASSED")
        sys.exit(0)
    else:
        print("✗ Fetch API test FAILED")
        sys.exit(1)

if __name__ == "__main__":
    main()