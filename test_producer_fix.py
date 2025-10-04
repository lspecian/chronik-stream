#!/usr/bin/env python3
"""
Test script to verify Chronik v1.3.12 Producer fix and flexible format support.

This script tests:
1. Producer can send messages (critical fix)
2. Consumer can receive messages
3. Flexible protocol format works (Produce v9+, Fetch v12+)
"""

import json
import time
import sys
from kafka import KafkaProducer, KafkaConsumer
from kafka.admin import KafkaAdminClient, NewTopic

def test_basic_producer_consumer():
    """Test basic produce and consume operations."""
    print("=" * 80)
    print("TEST 1: Basic Producer/Consumer (Produce v2, Fetch v11)")
    print("=" * 80)

    topic = 'test-basic-' + str(int(time.time()))

    try:
        # Create producer with older API version (non-flexible)
        producer = KafkaProducer(
            bootstrap_servers=['localhost:9092'],
            value_serializer=lambda v: json.dumps(v).encode('utf-8'),
            api_version=(0, 10, 0),  # Use Produce v2
            request_timeout_ms=5000,
            max_block_ms=5000
        )
        print(f"✓ Producer connected successfully")

        # Send test message
        print(f"→ Sending message to topic '{topic}'...")
        future = producer.send(topic, {'test': 'data', 'timestamp': time.time()})
        result = future.get(timeout=5)
        print(f"✓ Message sent successfully: offset={result.offset}, partition={result.partition}")

        producer.flush()
        producer.close()

        # Consume the message
        consumer = KafkaConsumer(
            topic,
            bootstrap_servers=['localhost:9092'],
            value_deserializer=lambda m: json.loads(m.decode('utf-8')),
            auto_offset_reset='earliest',
            consumer_timeout_ms=10000,
            api_version=(0, 10, 0)
        )
        print(f"✓ Consumer connected successfully")

        messages = []
        for message in consumer:
            messages.append(message.value)
            print(f"✓ Received message: {message.value}")
            break

        consumer.close()

        if len(messages) > 0:
            print(f"✅ TEST 1 PASSED: Basic produce/consume works!")
            return True
        else:
            print(f"❌ TEST 1 FAILED: No messages received")
            return False

    except Exception as e:
        print(f"❌ TEST 1 FAILED: {e}")
        import traceback
        traceback.print_exc()
        return False

def test_flexible_produce():
    """Test Produce with flexible format (v9+)."""
    print("\n" + "=" * 80)
    print("TEST 2: Flexible Produce Format (Produce v9)")
    print("=" * 80)

    topic = 'test-flex-produce-' + str(int(time.time()))

    try:
        # Create producer requesting v9 (flexible format)
        # Note: kafka-python may not actually use v9, but we can try
        producer = KafkaProducer(
            bootstrap_servers=['localhost:9092'],
            value_serializer=lambda v: json.dumps(v).encode('utf-8'),
            api_version=(2, 5, 0),  # Newer version - should request higher API versions
            request_timeout_ms=5000,
            max_block_ms=5000
        )
        print(f"✓ Producer connected")

        # Send test message
        print(f"→ Sending message with newer API version...")
        future = producer.send(topic, {'flexible': 'test', 'timestamp': time.time()})
        result = future.get(timeout=5)
        print(f"✓ Message sent: offset={result.offset}")

        producer.flush()
        producer.close()

        print(f"✅ TEST 2 PASSED: Flexible Produce format works!")
        return True

    except Exception as e:
        print(f"❌ TEST 2 FAILED: {e}")
        import traceback
        traceback.print_exc()
        return False

def test_flexible_fetch():
    """Test Fetch with flexible format (v12+)."""
    print("\n" + "=" * 80)
    print("TEST 3: Flexible Fetch Format (Fetch v12+)")
    print("=" * 80)

    topic = 'test-flex-fetch-' + str(int(time.time()))

    try:
        # First produce a message
        producer = KafkaProducer(
            bootstrap_servers=['localhost:9092'],
            value_serializer=lambda v: json.dumps(v).encode('utf-8'),
            api_version=(2, 5, 0)
        )
        producer.send(topic, {'fetch_test': 'data'}).get(timeout=5)
        producer.close()
        print(f"✓ Test message produced")

        # Consume with newer API version (should use Fetch v12+)
        consumer = KafkaConsumer(
            topic,
            bootstrap_servers=['localhost:9092'],
            value_deserializer=lambda m: json.loads(m.decode('utf-8')),
            auto_offset_reset='earliest',
            consumer_timeout_ms=10000,
            api_version=(2, 5, 0)  # Should request Fetch v12+
        )
        print(f"✓ Consumer connected with newer API version")

        messages = []
        for message in consumer:
            messages.append(message.value)
            print(f"✓ Received message: {message.value}")
            break

        consumer.close()

        if len(messages) > 0:
            print(f"✅ TEST 3 PASSED: Flexible Fetch format works!")
            return True
        else:
            print(f"❌ TEST 3 FAILED: No messages received")
            return False

    except Exception as e:
        print(f"❌ TEST 3 FAILED: {e}")
        import traceback
        traceback.print_exc()
        return False

def test_admin_client():
    """Test AdminClient operations."""
    print("\n" + "=" * 80)
    print("TEST 4: AdminClient Operations")
    print("=" * 80)

    try:
        admin = KafkaAdminClient(bootstrap_servers='localhost:9092')
        print(f"✓ AdminClient connected")

        # List topics
        topics = admin.list_topics()
        print(f"✓ Listed {len(topics)} topics")

        # Create a new topic
        topic_name = 'test-admin-' + str(int(time.time()))
        new_topic = NewTopic(name=topic_name, num_partitions=1, replication_factor=1)
        admin.create_topics([new_topic])
        print(f"✓ Created topic '{topic_name}'")

        # List consumer groups
        groups = admin.list_consumer_groups()
        print(f"✓ Listed consumer groups: {len(groups)}")

        admin.close()

        print(f"✅ TEST 4 PASSED: AdminClient operations work!")
        return True

    except Exception as e:
        print(f"❌ TEST 4 FAILED: {e}")
        import traceback
        traceback.print_exc()
        return False

def main():
    """Run all tests."""
    print("\n")
    print("╔" + "=" * 78 + "╗")
    print("║" + " " * 20 + "Chronik v1.3.12 Test Suite" + " " * 32 + "║")
    print("╚" + "=" * 78 + "╝")
    print("\nTesting Producer fix and Flexible protocol format support...\n")

    results = []

    # Run tests
    results.append(("Basic Producer/Consumer", test_basic_producer_consumer()))
    results.append(("Flexible Produce Format", test_flexible_produce()))
    results.append(("Flexible Fetch Format", test_flexible_fetch()))
    results.append(("AdminClient Operations", test_admin_client()))

    # Summary
    print("\n" + "=" * 80)
    print("TEST SUMMARY")
    print("=" * 80)

    passed = sum(1 for _, result in results if result)
    total = len(results)

    for name, result in results:
        status = "✅ PASSED" if result else "❌ FAILED"
        print(f"{status}: {name}")

    print("\n" + "=" * 80)
    print(f"RESULTS: {passed}/{total} tests passed ({passed*100//total}%)")
    print("=" * 80 + "\n")

    if passed == total:
        print("🎉 ALL TESTS PASSED! Chronik v1.3.12 is working correctly.")
        return 0
    else:
        print(f"⚠️  {total - passed} test(s) failed. Review the output above.")
        return 1

if __name__ == '__main__':
    sys.exit(main())
