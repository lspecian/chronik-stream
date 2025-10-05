#!/usr/bin/env python3
"""Simple test to verify segment reader fix"""

import sys
import time
from kafka import KafkaProducer, KafkaConsumer
from kafka.errors import KafkaError

def test_batch_size(num_messages):
    """Test with specific number of messages"""
    topic = f'test-{num_messages}'

    # Produce messages
    producer = KafkaProducer(
        bootstrap_servers='localhost:9092',
        api_version=(0, 10, 0)
    )

    print(f"\n{'='*60}")
    print(f"TEST: {num_messages} messages")
    print(f"{'='*60}")

    for i in range(num_messages):
        key = f'key-{i}'.encode()
        value = f'message-{i}'.encode()
        producer.send(topic, key=key, value=value)

    producer.flush()
    producer.close()
    print(f"✓ Produced {num_messages} messages to topic '{topic}'")

    # Wait for persistence
    time.sleep(2)

    # Consume messages
    consumer = KafkaConsumer(
        topic,
        bootstrap_servers='localhost:9092',
        auto_offset_reset='earliest',
        consumer_timeout_ms=5000,
        api_version=(0, 10, 0)
    )

    messages = []
    for msg in consumer:
        messages.append(msg)

    consumer.close()

    # Check results
    expected = num_messages
    received = len(messages)

    print(f"Expected: {expected}")
    print(f"Received: {received}")

    if received == expected:
        print(f"✅ PASS - All {expected} messages received")
        return True
    else:
        print(f"❌ FAIL - Expected {expected}, got {received}")
        print(f"   Data loss: {expected - received} messages ({100*(expected-received)/expected:.1f}%)")
        return False

if __name__ == '__main__':
    # Test with requested sizes
    test_sizes = [20, 50, 200, 2000]

    results = {}
    for size in test_sizes:
        try:
            results[size] = test_batch_size(size)
        except Exception as e:
            print(f"❌ FAIL - Exception: {e}")
            results[size] = False

    # Summary
    print(f"\n{'='*60}")
    print("SUMMARY")
    print(f"{'='*60}")
    for size, passed in results.items():
        status = "✅ PASS" if passed else "❌ FAIL"
        print(f"{status} - {size} messages")

    # Exit code
    all_passed = all(results.values())
    sys.exit(0 if all_passed else 1)
