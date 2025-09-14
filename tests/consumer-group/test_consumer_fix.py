#!/usr/bin/env python3
"""
Test consumer functionality after WAL integration fixes
"""

from kafka import KafkaProducer, KafkaConsumer
from kafka.errors import KafkaError
import time
import sys

def test_basic_consume():
    """Test basic produce/consume without consumer groups"""
    print("Testing Chronik v1.0.1 Consumer Fixes")
    print("=" * 50)
    
    # Create producer
    print("Creating producer...")
    producer = KafkaProducer(
        bootstrap_servers=['localhost:9092'],
        value_serializer=lambda v: v.encode('utf-8') if isinstance(v, str) else v
    )
    
    topic = 'test-consumer-fix'
    
    # Produce some messages
    print(f"\nProducing 5 messages to topic '{topic}'...")
    for i in range(5):
        message = f"Test message {i}"
        future = producer.send(topic, value=message)
        result = future.get(timeout=10)
        print(f"  Produced: {message} (offset={result.offset})")
    
    producer.flush()
    print("All messages produced successfully!")
    
    # Create consumer (without consumer group for basic test)
    print(f"\nCreating consumer for topic '{topic}'...")
    consumer = KafkaConsumer(
        topic,
        bootstrap_servers=['localhost:9092'],
        auto_offset_reset='earliest',
        enable_auto_commit=False,
        consumer_timeout_ms=5000,  # 5 second timeout
        value_deserializer=lambda m: m.decode('utf-8') if m else None
    )
    
    # Consume messages
    print("Consuming messages...")
    messages_received = []
    
    for message in consumer:
        messages_received.append(message)
        print(f"  Consumed: {message.value} (offset={message.offset}, partition={message.partition})")
    
    consumer.close()
    
    # Verify results
    print(f"\nResults:")
    print(f"  Messages produced: 5")
    print(f"  Messages consumed: {len(messages_received)}")
    
    if len(messages_received) == 5:
        print("✅ SUCCESS: All messages were consumed correctly!")
        return True
    else:
        print(f"❌ FAILURE: Expected 5 messages, got {len(messages_received)}")
        return False

def test_with_consumer_group():
    """Test with consumer group"""
    print("\n" + "=" * 50)
    print("Testing with Consumer Group")
    print("=" * 50)
    
    # Create producer
    producer = KafkaProducer(
        bootstrap_servers=['localhost:9092'],
        value_serializer=lambda v: v.encode('utf-8') if isinstance(v, str) else v
    )
    
    topic = 'test-group-consumer'
    group_id = 'test-group-1'
    
    # Produce messages
    print(f"\nProducing 3 messages to topic '{topic}'...")
    for i in range(3):
        message = f"Group message {i}"
        future = producer.send(topic, value=message)
        result = future.get(timeout=10)
        print(f"  Produced: {message} (offset={result.offset})")
    
    producer.flush()
    
    # Create consumer with group
    print(f"\nCreating consumer with group_id='{group_id}'...")
    consumer = KafkaConsumer(
        topic,
        bootstrap_servers=['localhost:9092'],
        group_id=group_id,
        auto_offset_reset='earliest',
        enable_auto_commit=True,
        consumer_timeout_ms=5000,
        value_deserializer=lambda m: m.decode('utf-8') if m else None
    )
    
    # Consume messages
    print("Consuming messages...")
    messages_received = []
    
    for message in consumer:
        messages_received.append(message)
        print(f"  Consumed: {message.value} (offset={message.offset})")
    
    consumer.close()
    
    # Verify
    print(f"\nResults:")
    print(f"  Messages produced: 3")
    print(f"  Messages consumed: {len(messages_received)}")
    
    if len(messages_received) == 3:
        print("✅ SUCCESS: Consumer group working correctly!")
        return True
    else:
        print(f"❌ FAILURE: Expected 3 messages, got {len(messages_received)}")
        return False

if __name__ == "__main__":
    try:
        # Test 1: Basic consume without group
        test1_passed = test_basic_consume()
        
        # Test 2: Consumer group
        test2_passed = test_with_consumer_group()
        
        # Summary
        print("\n" + "=" * 50)
        print("SUMMARY")
        print("=" * 50)
        print(f"Basic consume test: {'✅ PASSED' if test1_passed else '❌ FAILED'}")
        print(f"Consumer group test: {'✅ PASSED' if test2_passed else '❌ FAILED'}")
        
        if test1_passed and test2_passed:
            print("\n🎉 All tests PASSED! Consumer functionality is working!")
            sys.exit(0)
        else:
            print("\n⚠️ Some tests FAILED. Consumer still has issues.")
            sys.exit(1)
            
    except Exception as e:
        print(f"\n❌ ERROR: {e}")
        import traceback
        traceback.print_exc()
        sys.exit(1)