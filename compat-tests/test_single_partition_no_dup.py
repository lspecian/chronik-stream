#!/usr/bin/env python3
"""Test that verifies no duplication when sending to a single partition"""

import sys
import time
import json
import uuid
from kafka import KafkaProducer, KafkaConsumer
from kafka.errors import KafkaError

def test_single_partition():
    """Test sending all messages to partition 0 only"""
    
    broker = 'localhost:9092'
    topic = f'test-single-{uuid.uuid4().hex[:8]}'
    
    print("=" * 60)
    print("SINGLE PARTITION DUPLICATION TEST")
    print("=" * 60)
    print(f"Topic: {topic}")
    print()
    
    # PHASE 1: Send 10 messages to partition 0 only
    try:
        print("PHASE 1: Producing 10 messages to partition 0")
        producer = KafkaProducer(
            bootstrap_servers=[broker],
            api_version=(0, 10, 0),
            value_serializer=lambda v: json.dumps(v).encode('utf-8')
        )
        
        sent_ids = []
        for i in range(10):
            # Force partition 0
            future = producer.send(topic, {'id': i}, partition=0)
            metadata = future.get(timeout=10)
            sent_ids.append(i)
            print(f"  ✓ Sent message {i} to partition {metadata.partition} offset {metadata.offset}")
        
        producer.flush()
        producer.close()
        print(f"  ✓ Successfully produced {len(sent_ids)} messages to partition 0")
        print()
        
    except Exception as e:
        print(f"  ✗ Failed to produce: {e}")
        return False
    
    # Wait for messages to be persisted
    time.sleep(2)
    
    # PHASE 2: Consume messages from partition 0
    try:
        print("PHASE 2: Consuming messages from partition 0")
        from kafka import TopicPartition
        
        consumer = KafkaConsumer(
            bootstrap_servers=[broker],
            api_version=(0, 10, 0),
            group_id=f'test-group-{uuid.uuid4().hex[:8]}',
            auto_offset_reset='earliest',
            enable_auto_commit=False,
            consumer_timeout_ms=5000,
            value_deserializer=lambda m: json.loads(m.decode('utf-8'))
        )
        
        # Assign to partition 0 only (don't subscribe to topic)
        consumer.assign([TopicPartition(topic, 0)])
        
        messages = []
        for msg in consumer:
            messages.append(msg)
            print(f"  Received: partition={msg.partition}, offset={msg.offset}, id={msg.value['id']}")
        
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
    
    # Check offsets are sequential
    offsets = [m.offset for m in messages]
    expected_offsets = list(range(10))
    if offsets != expected_offsets:
        print(f"  ✗ FAIL: Offsets not sequential")
        print(f"  Expected offsets: {expected_offsets}")
        print(f"  Received offsets: {offsets}")
        return False
    
    print("  ✅ PASS: No duplicates, all messages present")
    print("  ✅ PASS: Exactly 10 messages received")
    print("  ✅ PASS: All IDs match expected range")
    print("  ✅ PASS: Offsets are sequential (0-9)")
    return True

def main():
    passed = test_single_partition()
    
    print()
    print("=" * 60)
    if passed:
        print("✅✅✅ SINGLE PARTITION TEST PASSED ✅✅✅")
        print("No duplication when sending to a single partition!")
        sys.exit(0)
    else:
        print("✗✗✗ SINGLE PARTITION TEST FAILED ✗✗✗")
        print("Duplication issue present even with single partition")
        sys.exit(1)

if __name__ == "__main__":
    main()