#!/usr/bin/env python3
"""Test script to verify v0.7.6 duplication fix based on user feedback"""

import sys
import time
import json
import uuid
from kafka import KafkaProducer, KafkaConsumer
from kafka.errors import KafkaError

def verify_v076():
    """This should pass when duplication is fixed"""
    
    broker = 'localhost:9092'
    topic = f'test-v076-{uuid.uuid4().hex[:8]}'
    
    print("=" * 60)
    print("CHRONIK v0.7.6 DUPLICATION FIX VERIFICATION")
    print("=" * 60)
    print(f"Topic: {topic}")
    print()
    
    # PHASE 1: Send 10 messages
    try:
        print("PHASE 1: Producing 10 messages")
        producer = KafkaProducer(
            bootstrap_servers=[broker],
            api_version=(0, 10, 0),
            value_serializer=lambda v: json.dumps(v).encode('utf-8')
        )
        
        sent_ids = []
        for i in range(10):
            future = producer.send(topic, {'id': i})
            metadata = future.get(timeout=10)
            sent_ids.append(i)
            print(f"  ✓ Sent message {i} to partition {metadata.partition} offset {metadata.offset}")
        
        producer.flush()
        producer.close()
        print(f"  ✓ Successfully produced {len(sent_ids)} messages")
        print()
        
    except Exception as e:
        print(f"  ✗ Failed to produce: {e}")
        return False
    
    # Wait for messages to be persisted
    time.sleep(2)
    
    # PHASE 2: Consume messages
    try:
        print("PHASE 2: Consuming messages")
        consumer = KafkaConsumer(
            topic,
            bootstrap_servers=[broker],
            api_version=(0, 10, 0),
            group_id=f'test-group-{uuid.uuid4().hex[:8]}',
            auto_offset_reset='earliest',
            enable_auto_commit=False,
            consumer_timeout_ms=5000,
            value_deserializer=lambda m: json.loads(m.decode('utf-8'))
        )
        
        messages = []
        for msg in consumer:
            messages.append(msg)
            print(f"  Received: offset={msg.offset}, id={msg.value['id']}")
        
        consumer.close()
        
        received_ids = [m.value['id'] for m in messages]
        print()
        print(f"  Total messages received: {len(messages)}")
        print(f"  Sent IDs:     {sorted(sent_ids)}")
        print(f"  Received IDs: {sorted(received_ids)}")
        print()
        
    except Exception as e:
        print(f"  ✗ Consumer failed: {e}")
        import traceback
        traceback.print_exc()
        return False
    
    # PHASE 3: Validation
    print("PHASE 3: Validation")
    
    # Check for exact match
    if len(messages) != 10:
        print(f"  ✗ FAIL: Expected 10 messages, got {len(messages)}")
        print(f"  This indicates {'duplication' if len(messages) > 10 else 'message loss'}")
        
        # Analyze duplication pattern
        if len(messages) > 10:
            from collections import Counter
            counts = Counter(received_ids)
            duplicated = {id: count for id, count in counts.items() if count > 1}
            print(f"  Duplicated messages: {duplicated}")
        
        return False
    
    # Check for duplicates
    if len(set(received_ids)) != 10:
        print(f"  ✗ FAIL: Duplicates found in received messages")
        from collections import Counter
        counts = Counter(received_ids)
        duplicated = {id: count for id, count in counts.items() if count > 1}
        print(f"  Duplicated messages: {duplicated}")
        return False
    
    # Check all IDs are present
    if set(received_ids) != set(range(10)):
        print(f"  ✗ FAIL: Missing or wrong IDs")
        missing = set(range(10)) - set(received_ids)
        extra = set(received_ids) - set(range(10))
        if missing:
            print(f"  Missing IDs: {sorted(missing)}")
        if extra:
            print(f"  Extra IDs: {sorted(extra)}")
        return False
    
    print("  ✅ PASS: No duplicates, all messages present")
    print("  ✅ PASS: Exactly 10 messages received")
    print("  ✅ PASS: All IDs match expected range")
    return True

def test_offset_tracking():
    """Test that offsets are properly tracked"""
    
    broker = 'localhost:9092'
    topic = f'test-offset-{uuid.uuid4().hex[:8]}'
    group_id = f'test-group-{uuid.uuid4().hex[:8]}'
    
    print()
    print("=" * 60)
    print("OFFSET TRACKING TEST")
    print("=" * 60)
    print(f"Topic: {topic}")
    print(f"Group: {group_id}")
    print()
    
    # Produce messages
    producer = KafkaProducer(
        bootstrap_servers=[broker],
        api_version=(0, 10, 0),
        value_serializer=lambda v: json.dumps(v).encode('utf-8')
    )
    
    for i in range(5):
        producer.send(topic, {'id': i})
    producer.flush()
    producer.close()
    print("  Produced 5 messages")
    
    time.sleep(1)
    
    # First consumer
    consumer1 = KafkaConsumer(
        topic,
        bootstrap_servers=[broker],
        api_version=(0, 10, 0),
        group_id=group_id,
        auto_offset_reset='earliest',
        enable_auto_commit=True,
        consumer_timeout_ms=2000,
        value_deserializer=lambda m: json.loads(m.decode('utf-8'))
    )
    
    count1 = 0
    for msg in consumer1:
        count1 += 1
    consumer1.close()
    print(f"  First consumer in group: {count1} messages")
    
    # Second consumer in same group
    consumer2 = KafkaConsumer(
        topic,
        bootstrap_servers=[broker],
        api_version=(0, 10, 0),
        group_id=group_id,
        auto_offset_reset='earliest',
        enable_auto_commit=True,
        consumer_timeout_ms=2000,
        value_deserializer=lambda m: json.loads(m.decode('utf-8'))
    )
    
    count2 = 0
    for msg in consumer2:
        count2 += 1
    consumer2.close()
    print(f"  Second consumer in same group: {count2} messages")
    
    if count2 == 0:
        print("  ✅ PASS: Offset tracking works correctly")
        return True
    else:
        print(f"  ✗ FAIL: Second consumer should get 0 messages, got {count2}")
        return False

def main():
    # Run main duplication test
    dup_test_passed = verify_v076()
    
    # Run offset tracking test
    offset_test_passed = test_offset_tracking()
    
    print()
    print("=" * 60)
    if dup_test_passed and offset_test_passed:
        print("✅✅✅ ALL TESTS PASSED ✅✅✅")
        print("v0.7.6 successfully fixes the duplication issue!")
        print("No duplicates, all messages present, offsets tracked correctly")
        sys.exit(0)
    else:
        print("✗✗✗ TESTS FAILED ✗✗✗")
        if not dup_test_passed:
            print("Duplication issue not fully resolved")
        if not offset_test_passed:
            print("Offset tracking issue detected")
        sys.exit(1)

if __name__ == "__main__":
    main()