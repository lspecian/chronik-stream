#!/usr/bin/env python3
"""Test script to verify segment fetching works correctly."""

from kafka import KafkaProducer, KafkaConsumer
import json
import time

BOOTSTRAP_SERVERS = ['localhost:9092']
TOPIC = 'test-segment-fetch'

def produce_messages(count=50):
    """Produce messages to trigger segment creation."""
    producer = KafkaProducer(
        bootstrap_servers=BOOTSTRAP_SERVERS,
        value_serializer=lambda v: json.dumps(v).encode('utf-8'),
        api_version=(0, 10, 0)
    )

    print(f"Producing {count} messages to {TOPIC}...")
    for i in range(count):
        msg = {'id': i, 'data': f'message-{i}', 'payload': 'x' * 1000}  # 1KB each
        future = producer.send(TOPIC, msg)
        result = future.get(timeout=5)
        print(f"  Sent message {i} to partition {result.partition} offset {result.offset}")

        # Flush every 10 messages to trigger segment creation
        if (i + 1) % 10 == 0:
            producer.flush()
            print(f"  Flushed after message {i}")
            time.sleep(0.5)  # Give server time to write segments

    producer.close()
    print(f"✓ Produced {count} messages")

def consume_from_beginning():
    """Consume all messages from beginning."""
    consumer = KafkaConsumer(
        TOPIC,
        bootstrap_servers=BOOTSTRAP_SERVERS,
        auto_offset_reset='earliest',
        enable_auto_commit=False,
        api_version=(0, 10, 0),
        consumer_timeout_ms=5000
    )

    print(f"\nConsuming from {TOPIC} (from earliest)...")
    messages = []
    for msg in consumer:
        messages.append({
            'partition': msg.partition,
            'offset': msg.offset,
            'value': json.loads(msg.value.decode('utf-8'))
        })
        print(f"  Received offset {msg.offset}: id={messages[-1]['value']['id']}")

    consumer.close()
    print(f"\n✓ Consumed {len(messages)} messages")

    # Verify we got all messages in order
    expected_ids = list(range(50))
    actual_ids = [m['value']['id'] for m in messages]

    if actual_ids == expected_ids:
        print("✅ SUCCESS: All messages fetched in correct order!")
        return True
    else:
        print(f"❌ FAILURE: Expected {len(expected_ids)} messages, got {len(actual_ids)}")
        print(f"  Expected IDs: {expected_ids[:10]}...{expected_ids[-10:]}")
        print(f"  Actual IDs: {actual_ids[:10]}...{actual_ids[-10:] if len(actual_ids) > 10 else actual_ids}")
        return False

if __name__ == '__main__':
    print("=" * 60)
    print("Testing Segment Fetch Fix")
    print("=" * 60)

    # Step 1: Produce messages
    produce_messages(50)

    # Step 2: Wait for segments to be written
    print("\nWaiting 3 seconds for segments to be written...")
    time.sleep(3)

    # Step 3: Consume from beginning
    success = consume_from_beginning()

    print("\n" + "=" * 60)
    if success:
        print("TEST PASSED ✅")
    else:
        print("TEST FAILED ❌")
    print("=" * 60)
