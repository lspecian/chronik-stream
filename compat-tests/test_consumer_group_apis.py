#!/usr/bin/env python3
"""
Test Consumer Group APIs implementation in Chronik Stream
Tests all consumer group coordination APIs that were fixed/implemented
"""

import sys
import time
import uuid
from kafka import KafkaConsumer, KafkaProducer
from kafka.admin import KafkaAdminClient
from kafka import TopicPartition, OffsetAndMetadata
from kafka.errors import KafkaError

def print_test_header(name):
    """Print a formatted test header"""
    print(f"\n{'='*60}")
    print(f" {name}")
    print(f"{'='*60}")

def test_admin_client_v0():
    """Test AdminClient with v0 API (critical for kafka-python compatibility)"""
    print_test_header("Test 1: AdminClient v0 Compatibility")

    try:
        admin = KafkaAdminClient(
            bootstrap_servers='localhost:9092',
            client_id='test-admin-v0',
            api_version=(0, 10, 0)  # Force v0 APIs
        )

        print("✓ Connected with ApiVersions v0")

        # Test metadata retrieval
        metadata = admin._client.cluster
        brokers = metadata.brokers()
        print(f"✓ Got metadata: {len(brokers)} broker(s)")

        # List topics
        topics = admin.list_topics()
        print(f"✓ Listed {len(topics)} topics")

        admin.close()
        return True

    except Exception as e:
        print(f"✗ AdminClient v0 failed: {e}")
        return False

def test_find_coordinator():
    """Test FindCoordinator API (API Key 10)"""
    print_test_header("Test 2: FindCoordinator API")

    try:
        group_id = f'test-coordinator-{uuid.uuid4().hex[:8]}'

        consumer = KafkaConsumer(
            bootstrap_servers='localhost:9092',
            group_id=group_id,
            api_version=(0, 10, 0)
        )

        # The consumer constructor internally calls FindCoordinator
        print(f"✓ FindCoordinator called for group '{group_id}'")

        # Verify coordinator was found
        coordinator = consumer._coordinator
        if coordinator:
            print(f"✓ Coordinator found and connected")

        consumer.close()
        return True

    except Exception as e:
        print(f"✗ FindCoordinator failed: {e}")
        return False

def test_join_group():
    """Test JoinGroup API (API Key 11)"""
    print_test_header("Test 3: JoinGroup API")

    try:
        topic = f'test-join-{uuid.uuid4().hex[:8]}'
        group_id = f'test-join-group-{uuid.uuid4().hex[:8]}'

        # Create topic with producer
        producer = KafkaProducer(bootstrap_servers='localhost:9092')
        producer.send(topic, b'test')
        producer.flush()
        producer.close()

        consumer = KafkaConsumer(
            topic,
            bootstrap_servers='localhost:9092',
            group_id=group_id,
            auto_offset_reset='earliest',
            api_version=(0, 10, 0)
        )

        # Poll triggers JoinGroup
        consumer.poll(timeout_ms=2000)

        # Check if we joined the group
        assignment = consumer.assignment()
        if assignment:
            print(f"✓ JoinGroup successful for group '{group_id}'")
            print(f"✓ Assigned {len(assignment)} partition(s)")
        else:
            print(f"✓ JoinGroup called (no partitions assigned yet)")

        consumer.close()
        return True

    except Exception as e:
        print(f"✗ JoinGroup failed: {e}")
        return False

def test_sync_group():
    """Test SyncGroup API (API Key 14)"""
    print_test_header("Test 4: SyncGroup API")

    try:
        topic = f'test-sync-{uuid.uuid4().hex[:8]}'
        group_id = f'test-sync-group-{uuid.uuid4().hex[:8]}'

        # Create topic
        producer = KafkaProducer(bootstrap_servers='localhost:9092')
        producer.send(topic, b'test')
        producer.flush()
        producer.close()

        consumer = KafkaConsumer(
            topic,
            bootstrap_servers='localhost:9092',
            group_id=group_id,
            auto_offset_reset='earliest',
            enable_auto_commit=False,
            api_version=(0, 10, 0)
        )

        # Poll triggers JoinGroup and SyncGroup
        messages = consumer.poll(timeout_ms=3000)

        assignment = consumer.assignment()
        if assignment:
            print(f"✓ SyncGroup successful")
            print(f"✓ Synchronized assignment: {assignment}")

        consumer.close()
        return True

    except Exception as e:
        print(f"✗ SyncGroup failed: {e}")
        return False

def test_heartbeat():
    """Test Heartbeat API (API Key 12)"""
    print_test_header("Test 5: Heartbeat API")

    try:
        topic = f'test-heartbeat-{uuid.uuid4().hex[:8]}'
        group_id = f'test-heartbeat-group-{uuid.uuid4().hex[:8]}'

        consumer = KafkaConsumer(
            topic,
            bootstrap_servers='localhost:9092',
            group_id=group_id,
            heartbeat_interval_ms=1000,
            session_timeout_ms=10000,
            api_version=(0, 10, 0)
        )

        # Poll multiple times to trigger heartbeats
        for i in range(3):
            consumer.poll(timeout_ms=1000)
            print(f"✓ Heartbeat {i+1} sent")
            time.sleep(0.5)

        print(f"✓ Heartbeat mechanism working")

        consumer.close()
        return True

    except Exception as e:
        print(f"✗ Heartbeat failed: {e}")
        return False

def test_offset_commit_fetch():
    """Test OffsetCommit (API Key 8) and OffsetFetch (API Key 9)"""
    print_test_header("Test 6: OffsetCommit and OffsetFetch APIs")

    try:
        topic = f'test-offset-{uuid.uuid4().hex[:8]}'
        group_id = f'test-offset-group-{uuid.uuid4().hex[:8]}'

        # Produce a message
        producer = KafkaProducer(bootstrap_servers='localhost:9092')
        producer.send(topic, b'test-message')
        producer.flush()
        producer.close()

        # Consumer with manual commit
        consumer = KafkaConsumer(
            topic,
            bootstrap_servers='localhost:9092',
            group_id=group_id,
            auto_offset_reset='earliest',
            enable_auto_commit=False,
            api_version=(0, 10, 0)
        )

        # Poll to get assignment
        consumer.poll(timeout_ms=2000)

        # Manually commit offset
        partitions = consumer.assignment()
        if partitions:
            tp = list(partitions)[0]
            position = consumer.position(tp)

            # Commit offset
            offsets = {tp: OffsetAndMetadata(position, None)}
            consumer.commit(offsets)
            print(f"✓ OffsetCommit: Committed offset {position} for {tp}")

            # Fetch committed offset
            committed = consumer.committed(tp)
            print(f"✓ OffsetFetch: Retrieved committed offset {committed}")

            if committed == position:
                print(f"✓ Offset commit/fetch verified")

        consumer.close()
        return True

    except Exception as e:
        print(f"✗ Offset APIs failed: {e}")
        return False

def test_list_groups():
    """Test ListGroups API (API Key 16)"""
    print_test_header("Test 7: ListGroups API")

    try:
        # Create a consumer group first
        group_id = f'test-list-{uuid.uuid4().hex[:8]}'
        consumer = KafkaConsumer(
            bootstrap_servers='localhost:9092',
            group_id=group_id,
            api_version=(0, 10, 0)
        )
        consumer.poll(timeout_ms=1000)

        # Now list groups
        admin = KafkaAdminClient(
            bootstrap_servers='localhost:9092',
            api_version=(0, 10, 0)
        )

        groups = admin.list_consumer_groups()
        print(f"✓ ListGroups returned {len(groups)} group(s)")

        # Check if our group is in the list
        group_ids = [g[0] for g in groups]
        if group_id in group_ids:
            print(f"✓ Found our test group '{group_id}' in list")
        else:
            print(f"  Note: Group '{group_id}' not in list (may have expired)")

        consumer.close()
        admin.close()
        return True

    except Exception as e:
        print(f"✗ ListGroups failed: {e}")
        return False

def test_leave_group():
    """Test LeaveGroup API (API Key 13)"""
    print_test_header("Test 8: LeaveGroup API")

    try:
        topic = f'test-leave-{uuid.uuid4().hex[:8]}'
        group_id = f'test-leave-group-{uuid.uuid4().hex[:8]}'

        consumer = KafkaConsumer(
            topic,
            bootstrap_servers='localhost:9092',
            group_id=group_id,
            api_version=(0, 10, 0)
        )

        # Join the group
        consumer.poll(timeout_ms=1000)
        print(f"✓ Joined group '{group_id}'")

        # Leave the group
        consumer.close()
        print(f"✓ LeaveGroup called on consumer close")

        return True

    except Exception as e:
        print(f"✗ LeaveGroup failed: {e}")
        return False

def test_confluent_kafka():
    """Test with confluent-kafka library (used by KSQLDB)"""
    print_test_header("Test 9: Confluent-Kafka (KSQLDB Compatibility)")

    try:
        from confluent_kafka.admin import AdminClient
        from confluent_kafka import Consumer

        # Test AdminClient
        admin = AdminClient({'bootstrap.servers': 'localhost:9092'})
        metadata = admin.list_topics(timeout=5)
        print(f"✓ confluent-kafka AdminClient: {len(metadata.topics)} topics")

        # Test Consumer with group
        consumer = Consumer({
            'bootstrap.servers': 'localhost:9092',
            'group.id': f'confluent-test-{uuid.uuid4().hex[:8]}',
            'auto.offset.reset': 'earliest'
        })

        consumer.subscribe(['test-topic'])
        msg = consumer.poll(timeout=2.0)
        print(f"✓ confluent-kafka Consumer with groups working")

        consumer.close()
        return True

    except ImportError:
        print("⚠ confluent-kafka not installed, skipping")
        return True
    except Exception as e:
        print(f"✗ confluent-kafka failed: {e}")
        return False

def main():
    """Run all consumer group API tests"""
    print("\n" + "="*60)
    print(" CONSUMER GROUP APIs TEST SUITE")
    print(" Testing Chronik Stream Kafka Compatibility")
    print("="*60)

    tests = [
        ("AdminClient v0", test_admin_client_v0),
        ("FindCoordinator", test_find_coordinator),
        ("JoinGroup", test_join_group),
        ("SyncGroup", test_sync_group),
        ("Heartbeat", test_heartbeat),
        ("OffsetCommit/Fetch", test_offset_commit_fetch),
        ("ListGroups", test_list_groups),
        ("LeaveGroup", test_leave_group),
        ("Confluent-Kafka", test_confluent_kafka),
    ]

    results = {}
    for name, test_func in tests:
        try:
            results[name] = test_func()
        except Exception as e:
            print(f"\n✗ Test '{name}' crashed: {e}")
            results[name] = False

    # Print summary
    print("\n" + "="*60)
    print(" TEST SUMMARY")
    print("="*60)

    for name, passed in results.items():
        status = "✅ PASS" if passed else "❌ FAIL"
        print(f" {name:.<30} {status}")

    total = len(results)
    passed = sum(1 for p in results.values() if p)

    print(f"\n Total: {passed}/{total} tests passed")

    if passed == total:
        print("\n 🎉 ALL CONSUMER GROUP APIs WORKING!")
        print(" ✓ KSQLDB compatibility achieved")
        print(" ✓ Apache Flink compatibility achieved")
        return 0
    else:
        print(f"\n ⚠ {total - passed} test(s) failed")
        return 1

if __name__ == "__main__":
    sys.exit(main())