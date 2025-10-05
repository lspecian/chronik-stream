#!/usr/bin/env python3
"""
Reproduce Chronik v1.3.22 Multi-Batch Segment Bug

This script demonstrates the difference between:
1. Single-batch write (works) - Chronik team's test
2. Multi-batch write (fails) - Real-world scenario

Usage:
    python3 reproduce_multi_batch_bug.py
"""

from kafka import KafkaProducer, KafkaConsumer
import json
import time

def test_single_batch():
    """
    Test Case 1: Single batch with 20 records (WORKS)
    This is what the Chronik team tested.
    """
    print("=" * 80)
    print("TEST 1: Single Batch (20 records in 1 batch)")
    print("=" * 80)

    topic = 'chronik.test.single-batch'

    # Configure producer to batch all messages together
    producer = KafkaProducer(
        bootstrap_servers=['localhost:9092'],
        value_serializer=lambda v: json.dumps(v).encode('utf-8'),
        api_version=(0, 10, 0),
        linger_ms=100,      # Wait 100ms to batch messages
        batch_size=16384,   # Large batch size
    )

    print(f"Producing 20 messages to topic '{topic}'...")

    # Send all 20 messages quickly
    for i in range(20):
        msg = {
            'id': i,
            'deck': f'deck-{i}',
            'cards': list(range(i, i + 50))  # Some payload
        }
        producer.send(topic, value=msg)

    # Flush all at once - creates ONE batch
    producer.flush()
    print("✅ Flushed all messages (should be 1 batch)")

    # Give Chronik time to write
    time.sleep(2)

    # Consume and verify
    consumer = KafkaConsumer(
        topic,
        bootstrap_servers=['localhost:9092'],
        auto_offset_reset='earliest',
        consumer_timeout_ms=10000,
        api_version=(0, 10, 0),
        group_id=None
    )

    messages = list(consumer)
    consumer.close()

    print(f"\n📊 RESULT:")
    print(f"   Messages received: {len(messages)}/20")
    print(f"   Offsets: {sorted([m.offset for m in messages])}")

    if len(messages) == 20:
        print("   ✅ PASS - All messages readable")
        return True
    else:
        print(f"   ❌ FAIL - Only {len(messages)}/20 messages readable")
        return False

    producer.close()
    print()


def test_multi_batch():
    """
    Test Case 2: Multiple batches (FAILS in Chronik v1.3.22)
    This is what happens with real applications.
    """
    print("\n" + "=" * 80)
    print("TEST 2: Multiple Batches (20 records in ~10 batches)")
    print("=" * 80)

    topic = 'chronik.test.multi-batch'

    # Configure producer to create separate batches
    producer = KafkaProducer(
        bootstrap_servers=['localhost:9092'],
        value_serializer=lambda v: json.dumps(v).encode('utf-8'),
        api_version=(0, 10, 0),
        linger_ms=0,        # Don't wait - flush immediately
        batch_size=1024,    # Small batch size
    )

    print(f"Producing 20 messages to topic '{topic}' with separate flushes...")

    # Send messages with flushes every 2 messages
    # This creates multiple separate batches
    for i in range(20):
        msg = {
            'id': i,
            'deck': f'deck-{i}',
            'cards': list(range(i, i + 50))  # Some payload
        }
        producer.send(topic, value=msg)

        # Flush every 2 messages to create separate batches
        if (i + 1) % 2 == 0:
            producer.flush()
            time.sleep(0.01)  # Small delay between batches

    # Final flush
    producer.flush()
    print("✅ Flushed messages in ~10 separate batches")

    # Give Chronik time to write
    time.sleep(3)

    # Consume and verify
    consumer = KafkaConsumer(
        topic,
        bootstrap_servers=['localhost:9092'],
        auto_offset_reset='earliest',
        consumer_timeout_ms=10000,
        api_version=(0, 10, 0),
        group_id=None
    )

    messages = list(consumer)
    consumer.close()

    print(f"\n📊 RESULT:")
    print(f"   Messages received: {len(messages)}/20")
    print(f"   Offsets: {sorted([m.offset for m in messages])}")

    if len(messages) == 20:
        print("   ✅ PASS - All messages readable")
        return True
    else:
        print(f"   ❌ FAIL - Only {len(messages)}/20 messages readable")
        print(f"   Missing offsets: {sorted(set(range(20)) - set(m.offset for m in messages))}")
        return False

    producer.close()
    print()


if __name__ == '__main__':
    print("\n🔬 Chronik v1.3.22 Multi-Batch Bug Reproduction\n")

    try:
        # Test 1: Single batch (should work)
        test1_passed = test_single_batch()

        # Test 2: Multiple batches (should fail in v1.3.22, pass after fix)
        test2_passed = test_multi_batch()

        print("\n" + "=" * 80)
        print("SUMMARY")
        print("=" * 80)
        print(f"  Test 1 (Single Batch): {'✅ PASS' if test1_passed else '❌ FAIL'}")
        print(f"  Test 2 (Multi-Batch):  {'✅ PASS' if test2_passed else '❌ FAIL (BUG!)'}")

        if not test2_passed:
            print("\n🐛 BUG CONFIRMED: Multi-batch segment reading is broken")
            print("   Chronik's segment reader stops after the first batch")
            exit(1)
        else:
            print("\n✅ All tests passed!")
            exit(0)

    except Exception as e:
        print(f"\n❌ Error running tests: {e}")
        import traceback
        traceback.print_exc()
        exit(1)
