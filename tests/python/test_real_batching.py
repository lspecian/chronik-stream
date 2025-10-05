#!/usr/bin/env python3
"""
Exact reproduction of user's reported batch bug.
This test enables REAL producer batching (multiple records in single Kafka batch).

Key difference from previous tests:
- WRONG: 20 separate batches, each with 1 record (our previous tests)
- RIGHT:  1-2 batches, containing all 20 records (user's actual scenario)
"""

from kafka import KafkaProducer, KafkaConsumer
import json
import time
import sys

def test_with_batching():
    """
    Test with producer batching ENABLED.
    This is what real producers (confluent-kafka-go, Java clients) do.
    """
    print("="*60)
    print("TEST: Producer with batching ENABLED (real-world scenario)")
    print("="*60)

    # Producer with batching (simulates confluent-kafka-go behavior)
    producer = KafkaProducer(
        bootstrap_servers=['localhost:9092'],
        value_serializer=lambda v: json.dumps(v).encode('utf-8'),
        api_version=(0, 10, 0),

        # CRITICAL: Enable batching (this is the key difference!)
        linger_ms=100,        # Wait 100ms to batch messages
        batch_size=16384,     # Batch when reaching 16KB
        compression_type=None,
    )

    topic = 'test.real.batching'
    print(f"\nProducing 20 messages rapidly (will be batched)...")

    # Send messages rapidly - producer will batch them!
    for i in range(20):
        msg = {'id': i, 'data': f'message-{i}', 'payload': 'x' * 100}
        producer.send(topic, value=msg)
        # NO flush() here - let messages accumulate in batch

    # Flush once at the end (forces batch to be sent)
    producer.flush()
    print("✅ All 20 messages sent and flushed")

    # CRITICAL: Wait for segment rotation
    # Messages might still be in buffer, need to force segment flush
    print("\nWaiting 3 seconds for Chronik to persist to segment...")
    time.sleep(3)

    # Now consume
    print("\nConsuming messages...")
    consumer = KafkaConsumer(
        topic,
        bootstrap_servers=['localhost:9092'],
        auto_offset_reset='earliest',
        consumer_timeout_ms=15000,
        api_version=(0, 10, 0),
        group_id=None  # No consumer group
    )

    count = 0
    offsets = []
    for msg in consumer:
        count += 1
        offsets.append(msg.offset)
        data = json.loads(msg.value.decode('utf-8'))
        print(f"  {count}. Offset {msg.offset}: {data['data']}")

    print(f"\n{'='*60}")
    print(f"Expected: 20/20 messages")
    print(f"Actual:   {count}/20 messages")
    print(f"Offsets:  {offsets[:10]}{'...' if len(offsets) > 10 else ''}")

    if count == 20:
        print("✅ PASS: All messages readable")
        print("="*60)
        return True
    else:
        data_loss_pct = ((20 - count) / 20) * 100
        print(f"❌ FAIL: Only {count}/20 messages readable ({data_loss_pct:.0f}% data loss)")
        print("="*60)
        print("\n🐛 BUG REPRODUCED!")
        print("This is the exact bug the user reported.")
        return False

def test_without_batching():
    """
    Test with batching DISABLED (each message in separate batch).
    This is what our previous tests did - it works fine!
    """
    print("\n" + "="*60)
    print("CONTROL TEST: Producer with batching DISABLED")
    print("="*60)

    producer = KafkaProducer(
        bootstrap_servers=['localhost:9092'],
        value_serializer=lambda v: json.dumps(v).encode('utf-8'),
        api_version=(0, 10, 0),

        # NO batching - each message sent immediately
        linger_ms=0,
        batch_size=1,
    )

    topic = 'test.no.batching'
    print(f"\nProducing 20 messages individually (no batching)...")

    for i in range(20):
        msg = {'id': i, 'data': f'message-{i}'}
        producer.send(topic, value=msg)
        producer.flush()  # Flush after EACH message

    print("✅ All 20 messages sent (individually)")
    time.sleep(2)

    consumer = KafkaConsumer(
        topic,
        bootstrap_servers=['localhost:9092'],
        auto_offset_reset='earliest',
        consumer_timeout_ms=10000,
        api_version=(0, 10, 0),
        group_id=None
    )

    count = sum(1 for _ in consumer)

    print(f"\n{'='*60}")
    print(f"Expected: 20/20 messages")
    print(f"Actual:   {count}/20 messages")

    if count == 20:
        print("✅ PASS: All messages readable (as expected)")
        print("="*60)
        return True
    else:
        print(f"❌ UNEXPECTED FAILURE: Only {count}/20 readable")
        print("="*60)
        return False

if __name__ == '__main__':
    print("\n🔬 Chronik v1.3.22 Batch Bug Reproduction Test")
    print("This test demonstrates the difference between:")
    print("  - Batched production (FAILS in v1.3.22)")
    print("  - Individual production (WORKS in v1.3.22)")
    print()

    # Test 1: Without batching (should work)
    control_passed = test_without_batching()

    # Test 2: With batching (should fail in v1.3.22)
    batched_passed = test_with_batching()

    # Summary
    print("\n" + "="*60)
    print("SUMMARY")
    print("="*60)
    print(f"Control test (no batching):  {'✅ PASS' if control_passed else '❌ FAIL'}")
    print(f"Batched test (real scenario): {'✅ PASS' if batched_passed else '❌ FAIL (BUG!)'}")
    print("="*60)

    if not batched_passed:
        print("\n🚨 The bug is REPRODUCED!")
        print("Multiple records in a single Kafka batch are not being read correctly.")
        sys.exit(1)
    else:
        print("\n✅ No bug detected - all tests passed")
        sys.exit(0)
