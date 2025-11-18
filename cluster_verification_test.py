#!/usr/bin/env python3
"""
Cluster Verification Test
Tests all 3 cluster nodes are working correctly after v2.2.8 fixes
"""

from kafka import KafkaProducer, KafkaConsumer
from kafka.admin import KafkaAdminClient, NewTopic
from kafka.errors import TopicAlreadyExistsError
import time
import sys

BOOTSTRAP_SERVERS = ['localhost:9092', 'localhost:9093', 'localhost:9094']

def test_cluster():
    print("=" * 60)
    print("Chronik Cluster Verification Test - v2.2.8")
    print("=" * 60)
    print()

    # Test 1: Admin operations
    print("Test 1: Creating test topic...")
    admin = KafkaAdminClient(
        bootstrap_servers=BOOTSTRAP_SERVERS,
        request_timeout_ms=10000
    )

    topic_name = "cluster-verify"
    try:
        topic = NewTopic(
            name=topic_name,
            num_partitions=3,
            replication_factor=3
        )
        admin.create_topics([topic])
        print("✓ Topic created successfully")
    except TopicAlreadyExistsError:
        print("✓ Topic already exists")
    except Exception as e:
        print(f"✗ Failed to create topic: {e}")
        return False

    time.sleep(1)

    # Test 2: Produce messages to all 3 partitions
    print("\nTest 2: Producing messages to all 3 partitions...")
    producer = KafkaProducer(
        bootstrap_servers=BOOTSTRAP_SERVERS,
        acks='all',
        retries=3,
        max_in_flight_requests_per_connection=1,
        request_timeout_ms=10000
    )

    messages_produced = {}
    for partition in range(3):
        for i in range(10):
            key = f"key-p{partition}-{i}".encode('utf-8')
            value = f"Test message for partition {partition}, index {i}".encode('utf-8')
            try:
                future = producer.send(
                    topic_name,
                    key=key,
                    value=value,
                    partition=partition
                )
                metadata = future.get(timeout=10)
                if partition not in messages_produced:
                    messages_produced[partition] = 0
                messages_produced[partition] += 1
            except Exception as e:
                print(f"✗ Failed to produce to partition {partition}: {e}")
                return False

    producer.flush()
    producer.close()

    for partition, count in sorted(messages_produced.items()):
        print(f"✓ Partition {partition}: {count} messages produced")

    # Give cluster time to replicate
    print("\nWaiting for replication...")
    time.sleep(3)

    # Test 3: Consume messages from all partitions
    print("\nTest 3: Consuming messages from all partitions...")
    consumer = KafkaConsumer(
        topic_name,
        bootstrap_servers=BOOTSTRAP_SERVERS,
        auto_offset_reset='earliest',
        consumer_timeout_ms=10000,
        group_id='cluster-verify-group',
        max_poll_interval_ms=30000
    )

    messages_consumed = {}
    start_time = time.time()
    timeout = 15  # 15 second timeout

    try:
        for message in consumer:
            partition = message.partition
            if partition not in messages_consumed:
                messages_consumed[partition] = 0
            messages_consumed[partition] += 1

            # Check if we've consumed all messages
            total_consumed = sum(messages_consumed.values())
            if total_consumed >= 30:  # 3 partitions * 10 messages
                break

            # Check timeout
            if time.time() - start_time > timeout:
                print(f"\n⚠️  Timeout after {timeout}s")
                break
    except Exception as e:
        print(f"\n✗ Consumer error: {e}")
    finally:
        consumer.close()

    print()
    for partition, count in sorted(messages_consumed.items()):
        print(f"✓ Partition {partition}: {count} messages consumed")

    # Verify results
    print("\n" + "=" * 60)
    print("RESULTS:")
    print("=" * 60)

    total_produced = sum(messages_produced.values())
    total_consumed = sum(messages_consumed.values())

    print(f"Total produced: {total_produced}")
    print(f"Total consumed: {total_consumed}")
    print()

    if total_consumed == total_produced:
        print("✓ SUCCESS: All messages produced and consumed correctly!")
        print("✓ Cluster is fully operational with 3-node replication")
        return True
    else:
        print(f"✗ FAILED: Produced {total_produced} but consumed {total_consumed}")
        print("✗ Cluster may have consume issues")
        return False

if __name__ == '__main__':
    try:
        success = test_cluster()
        sys.exit(0 if success else 1)
    except KeyboardInterrupt:
        print("\n\nTest interrupted by user")
        sys.exit(1)
    except Exception as e:
        print(f"\n✗ Unexpected error: {e}")
        import traceback
        traceback.print_exc()
        sys.exit(1)
