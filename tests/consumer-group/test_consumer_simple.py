#!/usr/bin/env python3
"""
Simple consumer test to verify consumer fixes
"""

from kafka import KafkaConsumer
from kafka.errors import KafkaError
import time
import sys

def test_simple_consume():
    """Test consuming messages from test-topic"""
    print("Simple Consumer Test")
    print("=" * 50)
    
    # Create consumer - no consumer group for simple test
    print("Creating consumer for topic 'test-topic'...")
    consumer = KafkaConsumer(
        'test-topic',
        bootstrap_servers=['localhost:9092'],
        auto_offset_reset='earliest',
        enable_auto_commit=False,
        consumer_timeout_ms=5000,  # 5 second timeout
        value_deserializer=lambda m: m.decode('utf-8') if m else None
    )
    
    # Consume messages
    print("Attempting to consume messages...")
    messages_received = []
    
    try:
        for message in consumer:
            messages_received.append(message)
            print(f"  ✓ Consumed: {message.value}")
            print(f"    Topic: {message.topic}")
            print(f"    Partition: {message.partition}")
            print(f"    Offset: {message.offset}")
    except StopIteration:
        print("Consumer timeout reached (5 seconds)")
    
    consumer.close()
    
    # Results
    print(f"\nResults:")
    print(f"  Messages consumed: {len(messages_received)}")
    
    if len(messages_received) > 0:
        print("✅ SUCCESS: Consumer is working! Messages were retrieved.")
        return True
    else:
        print("❌ FAILURE: No messages were consumed (WAL integration issue?)")
        return False

if __name__ == "__main__":
    try:
        success = test_simple_consume()
        sys.exit(0 if success else 1)
    except Exception as e:
        print(f"\n❌ ERROR: {e}")
        import traceback
        traceback.print_exc()
        sys.exit(1)