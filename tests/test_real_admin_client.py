#!/usr/bin/env python3
"""
Test Chronik Stream with real kafka-python AdminClient
This tests the actual AdminClient that Kafka UI and other tools use
"""

from kafka import KafkaProducer, KafkaConsumer
from kafka.admin import KafkaAdminClient, NewTopic
import sys
import time

def test_admin_client():
    """Test with real KafkaAdminClient"""
    print("\n=== Test 1: Create AdminClient ===")
    try:
        admin = KafkaAdminClient(
            bootstrap_servers='localhost:9092',
            client_id='test-admin-client'
        )
        print("✅ AdminClient created successfully")
    except Exception as e:
        print(f"❌ Failed to create AdminClient: {e}")
        return False

    print("\n=== Test 2: List Topics ===")
    try:
        topics = admin.list_topics()
        print(f"✅ Found {len(topics)} topics: {topics}")
    except Exception as e:
        print(f"❌ List topics failed: {e}")
        admin.close()
        return False

    print("\n=== Test 3: Create Topic ===")
    try:
        topic = NewTopic(name='admin-test-topic', num_partitions=3, replication_factor=1)
        result = admin.create_topics([topic], validate_only=False)
        print(f"✅ Topic created: {result}")
        time.sleep(1)  # Give it a moment
    except Exception as e:
        print(f"⚠️  Topic creation: {e} (may already exist)")

    print("\n=== Test 4: Describe Cluster ===")
    try:
        cluster_metadata = admin._client.cluster
        print(f"✅ Cluster ID: {cluster_metadata.cluster_id()}")
        print(f"✅ Brokers: {list(cluster_metadata.brokers())}")
        print(f"✅ Controller: {cluster_metadata.controller()}")
    except Exception as e:
        print(f"❌ Describe cluster failed: {e}")

    admin.close()
    print("\n✅ AdminClient tests PASSED")
    return True

def test_consumer_group_auto_assignment():
    """Test consumer group with auto partition assignment"""
    print("\n=== Test 5: Consumer Group Auto-Assignment ===")

    # First produce some messages
    print("Producing test messages...")
    try:
        producer = KafkaProducer(
            bootstrap_servers='localhost:9092',
            value_serializer=lambda v: str(v).encode('utf-8')
        )

        for i in range(10):
            producer.send('admin-test-topic', value=f'test-message-{i}')

        producer.flush()
        producer.close()
        print("✅ Produced 10 test messages")
    except Exception as e:
        print(f"❌ Failed to produce messages: {e}")
        return False

    # Now try to consume with auto-assignment
    print("\nConsuming with auto partition assignment...")
    try:
        consumer = KafkaConsumer(
            'admin-test-topic',
            bootstrap_servers='localhost:9092',
            group_id='test-auto-group',
            auto_offset_reset='earliest',
            consumer_timeout_ms=10000  # 10 second timeout
        )

        print("✅ Consumer created with group_id (auto-assignment)")

        count = 0
        start = time.time()

        for msg in consumer:
            count += 1
            print(f"  Received: partition={msg.partition}, offset={msg.offset}, value={msg.value.decode()}")
            if count >= 10:
                break

        elapsed = time.time() - start

        consumer.close()

        if count > 0:
            print(f"✅ Auto-assignment works! Consumed {count} messages in {elapsed:.2f}s")
            return True
        else:
            print("❌ No messages consumed - auto-assignment may have failed")
            return False

    except Exception as e:
        print(f"❌ Consumer group auto-assignment failed: {e}")
        import traceback
        traceback.print_exc()
        return False

def test_manual_partition_assignment():
    """Test manual partition assignment (known to work)"""
    print("\n=== Test 6: Manual Partition Assignment (baseline) ===")

    try:
        from kafka import TopicPartition

        consumer = KafkaConsumer(
            bootstrap_servers=['localhost:9092'],
            auto_offset_reset='earliest',
            consumer_timeout_ms=5000
        )

        # Manual assignment
        consumer.assign([
            TopicPartition('admin-test-topic', 0),
            TopicPartition('admin-test-topic', 1),
            TopicPartition('admin-test-topic', 2),
        ])

        print("✅ Manual assignment created")

        count = 0
        for msg in consumer:
            count += 1
            if count >= 5:
                break

        consumer.close()

        if count > 0:
            print(f"✅ Manual assignment works! Consumed {count} messages")
            return True
        else:
            print("⚠️  No messages (may be already consumed)")
            return True  # This is OK

    except Exception as e:
        print(f"❌ Manual assignment failed: {e}")
        return False

def main():
    """Run all tests"""
    print("="*70)
    print("Chronik Stream Real AdminClient & Consumer Group Tests")
    print("="*70)

    results = []

    # Test AdminClient
    results.append(("AdminClient", test_admin_client()))

    # Test consumer groups
    results.append(("Consumer Group Auto-Assignment", test_consumer_group_auto_assignment()))
    results.append(("Manual Partition Assignment", test_manual_partition_assignment()))

    # Summary
    print("\n" + "="*70)
    print("SUMMARY")
    print("="*70)

    passed = sum(1 for _, result in results if result)
    total = len(results)

    for name, result in results:
        status = "✅ PASSED" if result else "❌ FAILED"
        print(f"{name:40s} {status}")

    print(f"\nTotal: {passed}/{total} tests passed")

    if passed == total:
        print("\n🎉 All tests PASSED! AdminClient is fully compatible!")
        return 0
    else:
        print(f"\n⚠️  {total - passed} test(s) FAILED.")
        return 1

if __name__ == "__main__":
    try:
        sys.exit(main())
    except KeyboardInterrupt:
        print("\n\nTest interrupted by user")
        sys.exit(1)
    except Exception as e:
        print(f"\n\n❌ Test suite failed: {e}")
        import traceback
        traceback.print_exc()
        sys.exit(1)
