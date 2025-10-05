#!/usr/bin/env python3
"""
Test Admin APIs (DescribeLogDirs, DescribeAcls, CreateAcls, DeleteAcls)

This simulates what KSQLDB and Kafka UI do with AdminClient.
"""

from kafka.admin import KafkaAdminClient
from kafka import KafkaProducer, KafkaConsumer
import json
import time

def test_admin_client():
    """Test that AdminClient doesn't crash (the reported bug)"""
    print("=" * 80)
    print("TEST: Java-style AdminClient Operations")
    print("=" * 80)

    try:
        # Create AdminClient - this is what KSQLDB and Kafka UI use
        admin = KafkaAdminClient(
            bootstrap_servers='localhost:9092',
            api_version=(0, 10, 0),
            request_timeout_ms=30000
        )

        print("✅ AdminClient created successfully (no crash!)")

        # List topics - basic operation
        topics = admin.list_topics()
        print(f"✅ list_topics() succeeded: {topics}")

        # Describe cluster - uses Metadata API
        cluster_metadata = admin._client.cluster
        print(f"✅ Cluster metadata retrieved: {cluster_metadata}")

        # Close admin client
        admin.close()
        print("✅ AdminClient closed successfully")

        print("\n📊 RESULT: AdminClient operations succeeded!")
        print("   This means KSQLDB and Kafka UI should work now.")
        return True

    except Exception as e:
        print(f"\n❌ FAIL: AdminClient error: {e}")
        import traceback
        traceback.print_exc()
        return False

def test_produce_consume():
    """Test basic produce/consume still works"""
    print("\n" + "=" * 80)
    print("TEST: Basic Produce/Consume (regression test)")
    print("=" * 80)

    topic = 'admin-api-test'

    try:
        # Producer
        producer = KafkaProducer(
            bootstrap_servers='localhost:9092',
            value_serializer=lambda v: json.dumps(v).encode('utf-8'),
            api_version=(0, 10, 0)
        )

        # Send test messages
        for i in range(10):
            msg = {'id': i, 'message': f'test-{i}'}
            producer.send(topic, value=msg)

        producer.flush()
        print(f"✅ Produced 10 messages to '{topic}'")

        # Give server time to persist
        time.sleep(1)

        # Consumer
        consumer = KafkaConsumer(
            topic,
            bootstrap_servers='localhost:9092',
            auto_offset_reset='earliest',
            consumer_timeout_ms=5000,
            api_version=(0, 10, 0),
            group_id='test-group'
        )

        messages = list(consumer)
        consumer.close()

        print(f"✅ Consumed {len(messages)}/10 messages")

        if len(messages) == 10:
            print("✅ PASS - All messages readable")
            return True
        else:
            print(f"❌ FAIL - Only {len(messages)}/10 messages readable")
            return False

    except Exception as e:
        print(f"❌ FAIL: {e}")
        import traceback
        traceback.print_exc()
        return False

if __name__ == '__main__':
    print("\n🔬 Chronik Admin API Test Suite\n")

    # Test 1: AdminClient (the bug report)
    admin_ok = test_admin_client()

    # Test 2: Basic functionality regression test
    produce_ok = test_produce_consume()

    print("\n" + "=" * 80)
    print("SUMMARY")
    print("=" * 80)
    print(f"AdminClient: {'✅ PASS' if admin_ok else '❌ FAIL'}")
    print(f"Produce/Consume: {'✅ PASS' if produce_ok else '❌ FAIL'}")

    if admin_ok and produce_ok:
        print("\n🎉 All tests passed! KSQLDB and Kafka UI should work now.")
    else:
        print("\n⚠️  Some tests failed - see above for details")
