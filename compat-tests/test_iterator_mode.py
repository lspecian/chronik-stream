#!/usr/bin/env python3
"""Test kafka-python iterator mode to ensure no crashes when polling past the end"""

import sys
import time
import uuid
from kafka import KafkaProducer, KafkaConsumer

def test_iterator_mode():
    """Test that iterator mode doesn't crash when no more messages are available"""
    broker = 'localhost:9092'
    topic = f'test-iter-{uuid.uuid4().hex[:8]}'
    group = f'group-iter-{uuid.uuid4().hex[:8]}'
    
    print(f"Testing iterator mode with topic: {topic}")
    
    # Produce a small number of messages
    producer = KafkaProducer(
        bootstrap_servers=[broker],
        api_version=(0, 10, 0)
    )
    
    num_messages = 3
    for i in range(num_messages):
        producer.send(topic, value=f"msg-{i}".encode('utf-8'), partition=0)
    producer.flush()
    producer.close()
    print(f"✓ Produced {num_messages} messages")
    
    time.sleep(1)
    
    # Consumer with iterator mode
    consumer = KafkaConsumer(
        topic,
        bootstrap_servers=[broker],
        api_version=(0, 10, 0),
        group_id=group,
        auto_offset_reset='earliest',
        enable_auto_commit=True,
        consumer_timeout_ms=2000  # 2 second timeout
    )
    
    messages_received = []
    try:
        print("Starting iterator mode consumption...")
        for message in consumer:
            messages_received.append(message.value)
            print(f"  Received: {message.value}")
        
        # If we get here without exception, the timeout worked correctly
        print(f"✓ Iterator completed gracefully after timeout")
        
    except Exception as e:
        print(f"✗ Iterator failed with: {e}")
        return False
    finally:
        consumer.close()
    
    if len(messages_received) == num_messages:
        print(f"✓ Received all {num_messages} messages")
        return True
    else:
        print(f"✗ Expected {num_messages} messages, got {len(messages_received)}")
        return False

def test_multiple_polls_past_end():
    """Test multiple polls past the end of the topic"""
    broker = 'localhost:9092'
    topic = f'test-poll-{uuid.uuid4().hex[:8]}'
    group = f'group-poll-{uuid.uuid4().hex[:8]}'
    
    print(f"\nTesting multiple polls past end with topic: {topic}")
    
    # Produce messages
    producer = KafkaProducer(
        bootstrap_servers=[broker],
        api_version=(0, 10, 0)
    )
    
    for i in range(2):
        producer.send(topic, value=f"msg-{i}".encode('utf-8'))
    producer.flush()
    producer.close()
    print("✓ Produced 2 messages")
    
    time.sleep(1)
    
    # Consumer that will poll multiple times
    consumer = KafkaConsumer(
        topic,
        bootstrap_servers=[broker],
        api_version=(0, 10, 0),
        group_id=group,
        auto_offset_reset='earliest',
        enable_auto_commit=True,
        max_poll_records=1  # Get one message at a time
    )
    
    try:
        # First poll - should get message
        batch1 = consumer.poll(timeout_ms=1000)
        count1 = sum(len(msgs) for msgs in batch1.values())
        print(f"  Poll 1: Got {count1} messages")
        
        # Second poll - should get another message
        batch2 = consumer.poll(timeout_ms=1000)
        count2 = sum(len(msgs) for msgs in batch2.values())
        print(f"  Poll 2: Got {count2} messages")
        
        # Third poll - no more messages, should return empty
        batch3 = consumer.poll(timeout_ms=1000)
        count3 = sum(len(msgs) for msgs in batch3.values())
        print(f"  Poll 3: Got {count3} messages (expected 0)")
        
        # Fourth poll - still no messages
        batch4 = consumer.poll(timeout_ms=1000)
        count4 = sum(len(msgs) for msgs in batch4.values())
        print(f"  Poll 4: Got {count4} messages (expected 0)")
        
        consumer.close()
        
        # Success if we didn't crash
        print("✓ Multiple polls past end succeeded without crash")
        return True
        
    except Exception as e:
        print(f"✗ Failed with: {e}")
        import traceback
        traceback.print_exc()
        return False

def main():
    print("=" * 60)
    print("Kafka Iterator Mode Test")
    print("=" * 60)
    
    test1 = test_iterator_mode()
    test2 = test_multiple_polls_past_end()
    
    print("\n" + "=" * 60)
    if test1 and test2:
        print("✓ ALL TESTS PASSED - No crashes when polling past end!")
        sys.exit(0)
    else:
        print("✗ SOME TESTS FAILED")
        sys.exit(1)

if __name__ == "__main__":
    main()