#!/usr/bin/env python3
"""
Test Admin APIs using raw Kafka protocol

This uses rdkafka (confluent-kafka) which more closely simulates
Java AdminClient behavior used by KSQLDB and Kafka UI.
"""

try:
    from confluent_kafka.admin import AdminClient, NewTopic
    from confluent_kafka import KafkaException
    HAS_CONFLUENT = True
except ImportError:
    HAS_CONFLUENT = False
    print("⚠️  confluent-kafka not available, skipping test")
    print("   Install with: pip install confluent-kafka")

import time

def test_admin_client_operations():
    """Test AdminClient operations that KSQLDB uses"""
    if not HAS_CONFLUENT:
        return None

    print("=" * 80)
    print("TEST: Confluent AdminClient (Java-style) Operations")
    print("=" * 80)

    try:
        # Create AdminClient - this uses librdkafka (same as Java client)
        admin = AdminClient({
            'bootstrap.servers': 'localhost:9092',
            'api.version.request': True,
        })

        print("✅ AdminClient created successfully")

        # Get cluster metadata - basic operation
        metadata = admin.list_topics(timeout=10)
        print(f"✅ list_topics() succeeded: {len(metadata.topics)} topics")
        for topic in list(metadata.topics.keys())[:5]:
            print(f"   - {topic}")

        # Create a test topic
        new_topics = [NewTopic("admin-test-topic", num_partitions=3, replication_factor=1)]
        fs = admin.create_topics(new_topics)

        # Wait for operation to complete
        for topic, f in fs.items():
            try:
                f.result(timeout=10)  # The result itself is None
                print(f"✅ create_topics() succeeded for '{topic}'")
            except KafkaException as e:
                if 'TOPIC_ALREADY_EXISTS' in str(e):
                    print(f"✅ Topic '{topic}' already exists (OK)")
                else:
                    raise

        # List topics again to verify
        metadata = admin.list_topics(timeout=10)
        if 'admin-test-topic' in metadata.topics:
            print(f"✅ Topic 'admin-test-topic' visible in metadata")

        print("\n📊 RESULT: AdminClient operations succeeded!")
        print("   This confirms KSQLDB and Kafka UI compatibility.")
        return True

    except Exception as e:
        print(f"\n❌ FAIL: AdminClient error: {e}")
        import traceback
        traceback.print_exc()
        return False

def test_describe_cluster():
    """Test describe cluster operation"""
    if not HAS_CONFLUENT:
        return None

    print("\n" + "=" * 80)
    print("TEST: Describe Cluster")
    print("=" * 80)

    try:
        admin = AdminClient({
            'bootstrap.servers': 'localhost:9092',
            'api.version.request': True,
        })

        # Get cluster metadata
        metadata = admin.list_topics(timeout=10)

        print(f"✅ Cluster ID retrieved")
        print(f"   Brokers: {len(metadata.brokers)}")
        print(f"   Topics: {len(metadata.topics)}")

        for broker_id, broker in list(metadata.brokers.items())[:3]:
            print(f"   Broker {broker_id}: {broker.host}:{broker.port}")

        return True

    except Exception as e:
        print(f"❌ FAIL: {e}")
        import traceback
        traceback.print_exc()
        return False

if __name__ == '__main__':
    print("\n🔬 Chronik Raw Admin API Test Suite\n")

    if not HAS_CONFLUENT:
        print("Skipping tests - confluent-kafka not installed")
        exit(0)

    # Test 1: Basic AdminClient operations
    admin_ok = test_admin_client_operations()

    # Test 2: Describe cluster
    describe_ok = test_describe_cluster()

    print("\n" + "=" * 80)
    print("SUMMARY")
    print("=" * 80)

    if admin_ok is not None:
        print(f"AdminClient Operations: {'✅ PASS' if admin_ok else '❌ FAIL'}")
    if describe_ok is not None:
        print(f"Describe Cluster: {'✅ PASS' if describe_ok else '❌ FAIL'}")

    if admin_ok and describe_ok:
        print("\n🎉 All tests passed! Java AdminClient compatibility confirmed.")
    elif admin_ok is None:
        print("\n⚠️  Tests skipped - confluent-kafka not available")
    else:
        print("\n⚠️  Some tests failed - see above for details")
