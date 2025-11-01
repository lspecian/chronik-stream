#!/usr/bin/env python3
"""
Test current branch implementation
"""
from kafka import KafkaProducer, KafkaConsumer
import time

def test_produce_consume():
    """Test basic produce/consume"""

    print("=" * 60)
    print("Testing current branch implementation")
    print("=" * 60)

    # Create producer
    producer = KafkaProducer(
        bootstrap_servers='localhost:9092',
        api_version=(0, 10, 0)
    )

    # Produce messages
    topic = 'test-current-branch'
    num_messages = 100

    print(f"\n1. Producing {num_messages} messages...")
    start = time.time()
    for i in range(num_messages):
        producer.send(topic, f"Message {i}".encode())
    producer.flush()
    elapsed = time.time() - start
    rate = num_messages / elapsed
    print(f"   ✓ Produced {num_messages} messages in {elapsed:.2f}s ({rate:.0f} msg/s)")

    producer.close()

    # Consume messages
    print(f"\n2. Consuming messages...")
    consumer = KafkaConsumer(
        topic,
        bootstrap_servers='localhost:9092',
        auto_offset_reset='earliest',
        enable_auto_commit=False,
        api_version=(0, 10, 0)
    )

    messages = []
    timeout = time.time() + 10
    while len(messages) < num_messages and time.time() < timeout:
        for msg in consumer:
            messages.append(msg.value.decode())
            if len(messages) >= num_messages:
                break

    consumer.close()

    print(f"   ✓ Consumed {len(messages)} messages")

    # Verify
    if len(messages) == num_messages:
        print(f"\n3. Verification:")
        print(f"   ✓ All {num_messages} messages received")
        print(f"   ✓ No data loss")
        print(f"\n✅ TEST PASSED - Basic produce/consume works!")
        return True
    else:
        print(f"\n❌ TEST FAILED - Expected {num_messages}, got {len(messages)}")
        return False

if __name__ == '__main__':
    success = test_produce_consume()
    exit(0 if success else 1)
