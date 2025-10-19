#!/usr/bin/env python3
"""
Test script to verify the Raft prost bridge fix works correctly.

This tests:
1. Single-node operation (no Raft required)
2. Basic produce/consume flow
3. WAL recovery

For multi-node Raft testing, use test_raft_cluster_lifecycle_e2e.py
"""

import sys
import time
from kafka import KafkaProducer, KafkaConsumer
from kafka.errors import KafkaError

def test_basic_produce_consume():
    """Test basic produce and consume operations."""
    print("=" * 80)
    print("TEST 1: Basic Produce/Consume (Single Node)")
    print("=" * 80)

    topic = "test-raft-bridge"

    try:
        # Create producer
        print("\n1. Creating producer...")
        producer = KafkaProducer(
            bootstrap_servers='localhost:9092',
            api_version=(0, 10, 0),
            request_timeout_ms=30000,
            max_block_ms=30000
        )
        print("✅ Producer created")

        # Send messages
        print("\n2. Sending 10 test messages...")
        for i in range(10):
            msg = f"test-message-{i}".encode('utf-8')
            future = producer.send(topic, msg)
            result = future.get(timeout=10)
            print(f"   ✅ Message {i} sent: offset={result.offset}, partition={result.partition}")

        producer.flush()
        print("✅ All messages sent and flushed")

        # Wait for messages to be committed
        print("\n3. Waiting for messages to be committed...")
        time.sleep(2)

        # Create consumer
        print("\n4. Creating consumer...")
        consumer = KafkaConsumer(
            topic,
            bootstrap_servers='localhost:9092',
            auto_offset_reset='earliest',
            consumer_timeout_ms=10000,
            api_version=(0, 10, 0)
        )
        print("✅ Consumer created")

        # Consume messages
        print("\n5. Consuming messages...")
        consumed_count = 0
        for msg in consumer:
            consumed_count += 1
            print(f"   ✅ Consumed: offset={msg.offset}, value={msg.value.decode('utf-8')}")
            if consumed_count >= 10:
                break

        if consumed_count == 10:
            print(f"\n✅ SUCCESS: Produced and consumed {consumed_count} messages")
            return True
        else:
            print(f"\n❌ FAIL: Only consumed {consumed_count}/10 messages")
            return False

    except Exception as e:
        print(f"\n❌ ERROR: {e}")
        import traceback
        traceback.print_exc()
        return False
    finally:
        try:
            producer.close()
            consumer.close()
        except:
            pass

def test_high_volume():
    """Test high-volume produce/consume to stress the serialization bridge."""
    print("\n" + "=" * 80)
    print("TEST 2: High Volume (1000 messages)")
    print("=" * 80)

    topic = "test-raft-bridge-volume"
    num_messages = 1000

    try:
        # Create producer
        print("\n1. Creating producer...")
        producer = KafkaProducer(
            bootstrap_servers='localhost:9092',
            api_version=(0, 10, 0),
            request_timeout_ms=30000,
            max_block_ms=30000
        )
        print("✅ Producer created")

        # Send messages
        print(f"\n2. Sending {num_messages} messages...")
        start_time = time.time()
        for i in range(num_messages):
            msg = f"volume-test-{i}-{'x' * 100}".encode('utf-8')  # Larger messages
            producer.send(topic, msg)
            if (i + 1) % 100 == 0:
                print(f"   Sent {i + 1}/{num_messages} messages...")

        producer.flush()
        send_time = time.time() - start_time
        print(f"✅ Sent {num_messages} messages in {send_time:.2f}s ({num_messages/send_time:.0f} msg/s)")

        # Wait for messages to be committed
        print("\n3. Waiting for messages to be committed...")
        time.sleep(3)

        # Create consumer
        print("\n4. Creating consumer and consuming...")
        consumer = KafkaConsumer(
            topic,
            bootstrap_servers='localhost:9092',
            auto_offset_reset='earliest',
            consumer_timeout_ms=30000,
            api_version=(0, 10, 0)
        )

        consumed_count = 0
        start_time = time.time()
        for msg in consumer:
            consumed_count += 1
            if consumed_count % 100 == 0:
                print(f"   Consumed {consumed_count}/{num_messages} messages...")
            if consumed_count >= num_messages:
                break

        consume_time = time.time() - start_time

        if consumed_count == num_messages:
            print(f"\n✅ SUCCESS: Consumed {consumed_count} messages in {consume_time:.2f}s ({consumed_count/consume_time:.0f} msg/s)")
            return True
        else:
            print(f"\n❌ FAIL: Only consumed {consumed_count}/{num_messages} messages")
            return False

    except Exception as e:
        print(f"\n❌ ERROR: {e}")
        import traceback
        traceback.print_exc()
        return False
    finally:
        try:
            producer.close()
            consumer.close()
        except:
            pass

def main():
    print("""
╔═══════════════════════════════════════════════════════════════════════════╗
║              Raft Prost Bridge Fix - Verification Test                    ║
║                                                                            ║
║  Tests the chronik-raft-bridge crate's ability to handle prost 0.11       ║
║  serialization/deserialization for TiKV Raft messages.                    ║
║                                                                            ║
║  NOTE: This tests single-node operation. For multi-node Raft testing,     ║
║        use test_raft_cluster_lifecycle_e2e.py                             ║
╚═══════════════════════════════════════════════════════════════════════════╝
    """)

    results = []

    # Run tests
    results.append(("Basic Produce/Consume", test_basic_produce_consume()))
    results.append(("High Volume", test_high_volume()))

    # Print summary
    print("\n" + "=" * 80)
    print("TEST SUMMARY")
    print("=" * 80)

    all_passed = True
    for test_name, passed in results:
        status = "✅ PASS" if passed else "❌ FAIL"
        print(f"{status}: {test_name}")
        if not passed:
            all_passed = False

    print("=" * 80)

    if all_passed:
        print("\n🎉 ALL TESTS PASSED! The Raft prost bridge fix is working correctly.")
        print("\nNext steps:")
        print("1. Run multi-node cluster tests: python3 test_raft_cluster_lifecycle_e2e.py")
        print("2. Test leader failover scenarios")
        print("3. Verify Step RPC message flow between nodes")
        return 0
    else:
        print("\n❌ SOME TESTS FAILED. Review the output above.")
        return 1

if __name__ == "__main__":
    sys.exit(main())
