#!/usr/bin/env python3
"""Test script to verify the fix for v0.7.6 regression"""

import sys
import time
import json
import uuid
from kafka import KafkaProducer, KafkaConsumer
from kafka.errors import KafkaError

def test_fix(broker='localhost:9392'):
    """Test that messages are not lost or duplicated"""
    
    topic = f'test-fix-{uuid.uuid4().hex[:8]}'
    
    print("=" * 60)
    print("CHRONIK v0.7.7 FIX VERIFICATION")
    print("=" * 60)
    print(f"Broker: {broker}")
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
        if len(messages) > 10:
            print(f"  Issue: DUPLICATION (received {len(messages) - 10} extra messages)")
            from collections import Counter
            counts = Counter(received_ids)
            duplicated = {id: count for id, count in counts.items() if count > 1}
            print(f"  Duplicated messages: {duplicated}")
        else:
            print(f"  Issue: MESSAGE LOSS (lost {10 - len(messages)} messages)")
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
    print()
    
    # Calculate success rate
    success_rate = (len(messages) / 10) * 100
    print(f"  Success Rate: {success_rate}%")
    if success_rate == 100:
        print("  ✅ PERFECT: 100% message delivery with no duplication!")
    
    return True

def main():
    # Check if port argument provided
    if len(sys.argv) > 1:
        broker = f'localhost:{sys.argv[1]}'
    else:
        broker = 'localhost:9392'
    
    passed = test_fix(broker)
    
    print()
    print("=" * 60)
    if passed:
        print("✅✅✅ FIX VERIFIED ✅✅✅")
        print("v0.7.7 successfully fixes the regression!")
        print("No message loss, no duplication - working correctly!")
        sys.exit(0)
    else:
        print("✗✗✗ FIX INCOMPLETE ✗✗✗")
        print("Still experiencing issues with message delivery")
        sys.exit(1)

if __name__ == "__main__":
    main()