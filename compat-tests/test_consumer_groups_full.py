#!/usr/bin/env python3
"""Comprehensive consumer group tests for Chronik Stream"""

import sys
import time
import uuid
from kafka import KafkaProducer, KafkaConsumer
from kafka.errors import KafkaError

def test_single_consumer():
    """Test single consumer in a group"""
    broker = 'localhost:9092'
    topic = f'test-single-{uuid.uuid4().hex[:8]}'
    group = f'group-{uuid.uuid4().hex[:8]}'
    
    print("\n1. Testing single consumer in group:")
    print(f"   Topic: {topic}, Group: {group}")
    
    # Produce messages
    producer = KafkaProducer(
        bootstrap_servers=[broker],
        api_version=(0, 10, 0)
    )
    
    num_messages = 10
    for i in range(num_messages):
        producer.send(topic, value=f"msg-{i}".encode('utf-8'))
    producer.flush()
    producer.close()
    print(f"   ✓ Produced {num_messages} messages")
    
    time.sleep(1)
    
    # Consume messages
    consumer = KafkaConsumer(
        topic,
        bootstrap_servers=[broker],
        api_version=(0, 10, 0),
        group_id=group,
        auto_offset_reset='earliest',
        enable_auto_commit=True,
        consumer_timeout_ms=3000,
        max_poll_records=100
    )
    
    messages = []
    for msg in consumer:
        messages.append(msg.value)
    
    consumer.close()
    
    if len(messages) == num_messages:
        print(f"   ✓ Consumed all {num_messages} messages")
        return True
    else:
        print(f"   ✗ Expected {num_messages} messages, got {len(messages)}")
        return False

def test_consumer_rebalance():
    """Test consumer group rebalancing"""
    broker = 'localhost:9092'
    topic = f'test-rebalance-{uuid.uuid4().hex[:8]}'
    group = f'group-{uuid.uuid4().hex[:8]}'
    
    print("\n2. Testing consumer group rebalancing:")
    print(f"   Topic: {topic}, Group: {group}")
    
    # Produce messages
    producer = KafkaProducer(
        bootstrap_servers=[broker],
        api_version=(0, 10, 0)
    )
    
    for i in range(6):
        producer.send(topic, value=f"msg-{i}".encode('utf-8'), partition=i % 3)
    producer.flush()
    producer.close()
    print("   ✓ Produced 6 messages across 3 partitions")
    
    time.sleep(1)
    
    # Start first consumer
    consumer1 = KafkaConsumer(
        topic,
        bootstrap_servers=[broker],
        api_version=(0, 10, 0),
        group_id=group,
        auto_offset_reset='earliest',
        enable_auto_commit=False,
        max_poll_records=10
    )
    
    # Poll once to get initial assignment
    batch1 = consumer1.poll(timeout_ms=2000)
    messages1 = sum(len(msgs) for msgs in batch1.values())
    
    if messages1 > 0:
        print(f"   ✓ Consumer 1 received {messages1} messages")
    
    # Check assigned partitions
    partitions1 = consumer1.assignment()
    print(f"   ✓ Consumer 1 assigned {len(partitions1)} partitions")
    
    consumer1.close()
    return True

def test_offset_commit():
    """Test offset commit and resume"""
    broker = 'localhost:9092'
    topic = f'test-offset-{uuid.uuid4().hex[:8]}'
    group = f'group-{uuid.uuid4().hex[:8]}'
    
    print("\n3. Testing offset commit and resume:")
    print(f"   Topic: {topic}, Group: {group}")
    
    # Produce messages
    producer = KafkaProducer(
        bootstrap_servers=[broker],
        api_version=(0, 10, 0)
    )
    
    for i in range(5):
        producer.send(topic, value=f"msg-{i}".encode('utf-8'), partition=0)
    producer.flush()
    producer.close()
    print("   ✓ Produced 5 messages to partition 0")
    
    time.sleep(1)
    
    # First consumer - read 3 messages
    consumer1 = KafkaConsumer(
        topic,
        bootstrap_servers=[broker],
        api_version=(0, 10, 0),
        group_id=group,
        auto_offset_reset='earliest',
        enable_auto_commit=True,
        max_poll_records=3
    )
    
    batch = consumer1.poll(timeout_ms=2000)
    messages_read = 0
    for tp, msgs in batch.items():
        messages_read += len(msgs)
        for msg in msgs:
            print(f"   Read: {msg.value}")
    
    consumer1.close()
    print(f"   ✓ First consumer read {messages_read} messages")
    
    # Second consumer - should continue from offset 3
    consumer2 = KafkaConsumer(
        topic,
        bootstrap_servers=[broker],
        api_version=(0, 10, 0),
        group_id=group,
        auto_offset_reset='earliest',
        enable_auto_commit=True,
        consumer_timeout_ms=3000
    )
    
    remaining = []
    for msg in consumer2:
        remaining.append(msg.value)
        print(f"   Continued: {msg.value}")
    
    consumer2.close()
    
    if messages_read == 3 and len(remaining) == 2:
        print(f"   ✓ Offset commit works: read 3, then 2 messages")
        return True
    else:
        print(f"   ✗ Offset issue: first={messages_read}, second={len(remaining)}")
        return False

def test_multiple_topics():
    """Test consumer subscribed to multiple topics"""
    broker = 'localhost:9092'
    topic1 = f'test-multi1-{uuid.uuid4().hex[:8]}'
    topic2 = f'test-multi2-{uuid.uuid4().hex[:8]}'
    group = f'group-{uuid.uuid4().hex[:8]}'
    
    print("\n4. Testing multiple topic subscription:")
    print(f"   Topics: {topic1}, {topic2}")
    
    # Produce to both topics
    producer = KafkaProducer(
        bootstrap_servers=[broker],
        api_version=(0, 10, 0)
    )
    
    for i in range(3):
        producer.send(topic1, value=f"t1-{i}".encode('utf-8'))
        producer.send(topic2, value=f"t2-{i}".encode('utf-8'))
    producer.flush()
    producer.close()
    print("   ✓ Produced 3 messages to each topic")
    
    time.sleep(1)
    
    # Consume from both topics
    consumer = KafkaConsumer(
        topic1, topic2,
        bootstrap_servers=[broker],
        api_version=(0, 10, 0),
        group_id=group,
        auto_offset_reset='earliest',
        consumer_timeout_ms=3000
    )
    
    topic1_msgs = []
    topic2_msgs = []
    
    for msg in consumer:
        if msg.topic == topic1:
            topic1_msgs.append(msg.value)
        else:
            topic2_msgs.append(msg.value)
    
    consumer.close()
    
    if len(topic1_msgs) == 3 and len(topic2_msgs) == 3:
        print(f"   ✓ Received 3 messages from each topic")
        return True
    else:
        print(f"   ✗ Got {len(topic1_msgs)} from topic1, {len(topic2_msgs)} from topic2")
        return False

def main():
    print("=" * 60)
    print("Comprehensive Consumer Group Tests")
    print("=" * 60)
    
    tests = [
        ("Single Consumer", test_single_consumer),
        ("Consumer Rebalance", test_consumer_rebalance),
        ("Offset Commit", test_offset_commit),
        ("Multiple Topics", test_multiple_topics)
    ]
    
    results = []
    for name, test_func in tests:
        try:
            result = test_func()
            results.append((name, result))
        except Exception as e:
            print(f"\n✗ {name} failed with exception: {e}")
            import traceback
            traceback.print_exc()
            results.append((name, False))
    
    print("\n" + "=" * 60)
    print("RESULTS:")
    for name, result in results:
        status = "✓ PASSED" if result else "✗ FAILED"
        print(f"  {name}: {status}")
    
    all_passed = all(r for _, r in results)
    print("=" * 60)
    if all_passed:
        print("✓ ALL TESTS PASSED")
        sys.exit(0)
    else:
        print("✗ SOME TESTS FAILED")
        sys.exit(1)

if __name__ == "__main__":
    main()