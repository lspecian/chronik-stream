#!/usr/bin/env python3
"""
Integration test for Chronik v1.0.2: Consumer fetch after WAL buffer flush

This test verifies that consumers can still fetch messages after the in-memory 
buffer has been flushed (after ~30 seconds), using the WAL as a fallback source.
"""

from kafka import KafkaProducer, KafkaConsumer
from kafka.errors import KafkaError
import time
import sys
import json

def test_consumer_after_flush():
    """Test that consumer can fetch messages after buffer flush via WAL"""
    print("=" * 60)
    print("Chronik v1.0.2 WAL Consumer Test - After Buffer Flush")
    print("=" * 60)
    
    topic = 'test-wal-consumer'
    
    # PHASE 1: Produce messages
    print("\n[PHASE 1] Producing messages...")
    producer = KafkaProducer(
        bootstrap_servers=['localhost:9092'],
        value_serializer=lambda v: json.dumps(v).encode('utf-8'),
        api_version=(0, 10, 0)  # Use v2 for testing
    )
    
    messages_sent = []
    for i in range(5):
        message = {
            'id': i,
            'test': 'WAL consumer test',
            'timestamp': time.time()
        }
        future = producer.send(topic, value=message)
        result = future.get(timeout=10)
        messages_sent.append((result.offset, message))
        print(f"  ✓ Sent message {i}: offset={result.offset}")
    
    producer.flush()
    producer.close()
    print(f"  Total messages sent: {len(messages_sent)}")
    
    # PHASE 2: Quick consume to verify immediate availability
    print("\n[PHASE 2] Quick consume test (buffer should have messages)...")
    consumer_quick = KafkaConsumer(
        topic,
        bootstrap_servers=['localhost:9092'],
        auto_offset_reset='earliest',
        enable_auto_commit=False,
        consumer_timeout_ms=2000,
        value_deserializer=lambda m: json.loads(m.decode('utf-8')) if m else None
    )
    
    quick_messages = []
    for message in consumer_quick:
        quick_messages.append(message)
        print(f"  ✓ Quick consume: offset={message.offset}, value={message.value['id']}")
    
    consumer_quick.close()
    print(f"  Quick consume got {len(quick_messages)} messages")
    
    if len(quick_messages) == 0:
        print("  ⚠️ WARNING: No messages in buffer - may already be flushed")
    
    # PHASE 3: Wait for buffer flush (simulate 35 seconds)
    flush_wait_time = 35
    print(f"\n[PHASE 3] Waiting {flush_wait_time} seconds for buffer flush...")
    print("  (This simulates the buffer TTL expiring)")
    for i in range(flush_wait_time, 0, -5):
        print(f"  {i} seconds remaining...")
        time.sleep(5)
    print("  ✓ Buffer should now be flushed")
    
    # PHASE 4: Consume after flush (should use WAL)
    print("\n[PHASE 4] Consuming after buffer flush (testing WAL path)...")
    consumer_after_flush = KafkaConsumer(
        topic,
        bootstrap_servers=['localhost:9092'],
        auto_offset_reset='earliest',
        enable_auto_commit=False,
        consumer_timeout_ms=5000,
        value_deserializer=lambda m: json.loads(m.decode('utf-8')) if m else None
    )
    
    wal_messages = []
    for message in consumer_after_flush:
        wal_messages.append(message)
        print(f"  ✓ WAL consume: offset={message.offset}, value={message.value}")
    
    consumer_after_flush.close()
    
    # PHASE 5: Verify results
    print("\n[PHASE 5] Results Verification")
    print("=" * 60)
    print(f"Messages sent:                 {len(messages_sent)}")
    print(f"Messages consumed (immediate):  {len(quick_messages)}")
    print(f"Messages consumed (after flush): {len(wal_messages)}")
    
    # Success criteria: Messages should be retrievable after flush
    if len(wal_messages) == len(messages_sent):
        print("\n✅ SUCCESS: All messages retrieved after buffer flush!")
        print("   WAL-based fetch is working correctly!")
        return True
    elif len(wal_messages) > 0:
        print(f"\n⚠️ PARTIAL: Retrieved {len(wal_messages)}/{len(messages_sent)} messages after flush")
        print("   WAL is partially working but may have issues")
        return False
    else:
        print("\n❌ FAILURE: No messages retrieved after buffer flush!")
        print("   WAL fallback is NOT working - consumers lose data after flush")
        return False

if __name__ == "__main__":
    try:
        print("Starting WAL consumer test...")
        print("This test takes ~40 seconds to complete\n")
        
        success = test_consumer_after_flush()
        
        if success:
            print("\n🎉 Chronik v1.0.2 WAL integration verified!")
            sys.exit(0)
        else:
            print("\n⚠️ WAL integration needs further debugging")
            sys.exit(1)
            
    except Exception as e:
        print(f"\n❌ ERROR: {e}")
        import traceback
        traceback.print_exc()
        sys.exit(1)