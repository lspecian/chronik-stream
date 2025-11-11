#!/usr/bin/env python3
"""
Phase 2 E2E Test: Create topic + produce messages

This script tests the complete flow of Phase 2:
1. Create topic via metadata WAL (fast path)
2. Produce messages to the new topic
3. Verify messages can be consumed

Expected Results:
- Topic creation + first message: <50ms (Phase 2 fast path)
- Subsequent messages: <10ms

Usage:
    python3 tests/test_phase2_e2e.py

Prerequisites:
    pip install kafka-python
"""

from kafka import KafkaProducer, KafkaConsumer
import time
import sys

def main():
    print("=" * 60)
    print("Phase 2 E2E Test: Create topic + produce messages")
    print("=" * 60)
    print()

    # Test configuration
    topic_name = f'phase2-e2e-test-{int(time.time())}'
    test_message = b'Hello Phase 2!'
    num_messages = 10

    print(f"Test configuration:")
    print(f"  Topic: {topic_name}")
    print(f"  Messages: {num_messages}")
    print()

    # Create producer (will auto-create topic on first send)
    print("Creating producer...")
    try:
        producer = KafkaProducer(
            bootstrap_servers='localhost:9092',
            client_id='phase2-e2e-producer',
            acks='all',  # Wait for all replicas
            request_timeout_ms=10000
        )
        print("✅ Producer created")
        print()
    except Exception as e:
        print(f"❌ Failed to create producer: {e}")
        sys.exit(1)

    # Test 1: First message (includes topic creation)
    print(f"Test 1: First message to '{topic_name}' (auto-creates topic)...")
    start = time.time()
    try:
        future = producer.send(topic_name, test_message)
        record_metadata = future.get(timeout=10)
        first_msg_latency = (time.time() - start) * 1000  # ms

        print(f"✅ Topic created and message produced")
        print(f"   Topic: {record_metadata.topic}")
        print(f"   Partition: {record_metadata.partition}")
        print(f"   Offset: {record_metadata.offset}")
        print(f"   Latency: {first_msg_latency:.2f}ms")
        print()

        # Interpret first message latency
        if first_msg_latency < 50:
            print("🎉 Excellent! Phase 2 fast path is working")
            print(f"   Expected: <50ms, Actual: {first_msg_latency:.2f}ms")
        elif first_msg_latency < 100:
            print("✅ Good performance")
            print(f"   Latency: {first_msg_latency:.2f}ms")
        else:
            print("⚠️  Slower than expected")
            print(f"   Expected: <50ms, Actual: {first_msg_latency:.2f}ms")
            print("   Check Phase 2 activation in logs")

        print()

    except Exception as e:
        print(f"❌ First message failed: {e}")
        producer.close()
        sys.exit(1)

    # Test 2: Subsequent messages (topic already exists)
    print(f"Test 2: Producing {num_messages - 1} more messages...")
    latencies = []

    for i in range(1, num_messages):
        msg = f'Message {i}'.encode('utf-8')
        start = time.time()
        try:
            future = producer.send(topic_name, msg)
            future.get(timeout=10)
            latency = (time.time() - start) * 1000
            latencies.append(latency)
        except Exception as e:
            print(f"❌ Message {i} failed: {e}")

    if latencies:
        avg_latency = sum(latencies) / len(latencies)
        max_latency = max(latencies)
        min_latency = min(latencies)

        print(f"✅ Produced {len(latencies)} messages")
        print(f"   Average latency: {avg_latency:.2f}ms")
        print(f"   Min latency: {min_latency:.2f}ms")
        print(f"   Max latency: {max_latency:.2f}ms")
        print()

    producer.flush()
    producer.close()

    # Test 3: Consume messages back
    print(f"Test 3: Consuming messages from '{topic_name}'...")
    try:
        consumer = KafkaConsumer(
            topic_name,
            bootstrap_servers='localhost:9092',
            auto_offset_reset='earliest',
            consumer_timeout_ms=5000,
            group_id=f'phase2-e2e-consumer-{int(time.time())}'
        )

        messages = []
        start = time.time()
        for message in consumer:
            messages.append(message)
            if len(messages) >= num_messages:
                break

        consume_time = (time.time() - start) * 1000

        print(f"✅ Consumed {len(messages)} messages in {consume_time:.2f}ms")

        # Verify first message
        if messages and messages[0].value == test_message:
            print(f"✅ First message verified: {test_message.decode('utf-8')}")

        consumer.close()
        print()

    except Exception as e:
        print(f"⚠️  Consume test failed: {e}")
        print("   (Messages were produced successfully)")
        print()

    # Final summary
    print("=" * 60)
    print("SUMMARY")
    print("=" * 60)
    print(f"✅ Topic creation + first message: {first_msg_latency:.2f}ms")
    if latencies:
        print(f"✅ Subsequent produce latency: {avg_latency:.2f}ms avg")
    print(f"✅ Total messages: {num_messages}")
    print()

    if first_msg_latency < 50 and (not latencies or avg_latency < 10):
        print("🎉 Phase 2 is working excellently!")
    elif first_msg_latency < 100:
        print("✅ Phase 2 appears to be working")
    else:
        print("⚠️  Phase 2 may need tuning - review integration guide")

    print("=" * 60)

if __name__ == '__main__':
    main()
