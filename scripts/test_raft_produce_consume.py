#!/usr/bin/env python3
"""
Test Raft cluster with produce and consume operations.

This script:
1. Creates a topic with 3 partitions
2. Produces 1000 messages (round-robin across partitions)
3. Consumes all messages
4. Verifies count matches
5. Reports success/failure

Requirements:
    pip install kafka-python

Usage:
    ./scripts/test_raft_produce_consume.py [--bootstrap-server localhost:9092]
"""

import sys
import time
import argparse
from kafka import KafkaProducer, KafkaConsumer
from kafka.admin import KafkaAdminClient, NewTopic
from kafka.errors import TopicAlreadyExistsError, KafkaError

def create_topic(bootstrap_server, topic_name, num_partitions=3, replication_factor=1):
    """Create a Kafka topic with specified partitions."""
    print(f"Creating topic '{topic_name}' with {num_partitions} partitions...")

    try:
        admin_client = KafkaAdminClient(
            bootstrap_servers=bootstrap_server,
            client_id='test-admin'
        )

        topic = NewTopic(
            name=topic_name,
            num_partitions=num_partitions,
            replication_factor=replication_factor
        )

        admin_client.create_topics(new_topics=[topic], validate_only=False)
        print(f"Topic '{topic_name}' created successfully")
        time.sleep(2)  # Wait for topic creation to propagate
        return True

    except TopicAlreadyExistsError:
        print(f"Topic '{topic_name}' already exists (using existing)")
        return True
    except Exception as e:
        print(f"ERROR: Failed to create topic: {e}")
        return False

def produce_messages(bootstrap_server, topic_name, num_messages=1000):
    """Produce messages to the topic."""
    print(f"\nProducing {num_messages} messages to '{topic_name}'...")

    try:
        producer = KafkaProducer(
            bootstrap_servers=bootstrap_server,
            value_serializer=lambda v: v.encode('utf-8'),
            acks='all',  # Wait for all replicas
            retries=3
        )

        success_count = 0
        start_time = time.time()

        for i in range(num_messages):
            message = f"test-message-{i}"

            try:
                future = producer.send(topic_name, value=message)
                record_metadata = future.get(timeout=10)
                success_count += 1

                if (i + 1) % 100 == 0:
                    print(f"  Produced {i + 1}/{num_messages} messages...")

            except KafkaError as e:
                print(f"ERROR: Failed to send message {i}: {e}")

        producer.flush()
        producer.close()

        elapsed = time.time() - start_time
        rate = success_count / elapsed if elapsed > 0 else 0

        print(f"Produced {success_count}/{num_messages} messages in {elapsed:.2f}s ({rate:.0f} msg/s)")
        return success_count

    except Exception as e:
        print(f"ERROR: Producer failed: {e}")
        return 0

def consume_messages(bootstrap_server, topic_name, expected_count=1000, timeout_sec=30):
    """Consume messages from the topic."""
    print(f"\nConsuming messages from '{topic_name}'...")

    try:
        consumer = KafkaConsumer(
            topic_name,
            bootstrap_servers=bootstrap_server,
            auto_offset_reset='earliest',
            enable_auto_commit=True,
            group_id='test-consumer-group',
            value_deserializer=lambda m: m.decode('utf-8'),
            consumer_timeout_ms=timeout_sec * 1000
        )

        consumed_count = 0
        partition_counts = {}
        start_time = time.time()

        for message in consumer:
            consumed_count += 1
            partition = message.partition
            partition_counts[partition] = partition_counts.get(partition, 0) + 1

            if consumed_count % 100 == 0:
                print(f"  Consumed {consumed_count} messages...")

            if consumed_count >= expected_count:
                break

        consumer.close()

        elapsed = time.time() - start_time
        rate = consumed_count / elapsed if elapsed > 0 else 0

        print(f"Consumed {consumed_count}/{expected_count} messages in {elapsed:.2f}s ({rate:.0f} msg/s)")
        print(f"Partition distribution: {partition_counts}")

        return consumed_count

    except Exception as e:
        print(f"ERROR: Consumer failed: {e}")
        return 0

def main():
    parser = argparse.ArgumentParser(description='Test Raft cluster produce/consume')
    parser.add_argument(
        '--bootstrap-server',
        default='localhost:9092',
        help='Kafka bootstrap server (default: localhost:9092)'
    )
    parser.add_argument(
        '--topic',
        default='raft-test-topic',
        help='Topic name (default: raft-test-topic)'
    )
    parser.add_argument(
        '--messages',
        type=int,
        default=1000,
        help='Number of messages to produce (default: 1000)'
    )
    parser.add_argument(
        '--partitions',
        type=int,
        default=3,
        help='Number of partitions (default: 3)'
    )

    args = parser.parse_args()

    print("=" * 60)
    print("Raft Cluster Produce/Consume Test")
    print("=" * 60)
    print(f"Bootstrap server: {args.bootstrap_server}")
    print(f"Topic: {args.topic}")
    print(f"Messages: {args.messages}")
    print(f"Partitions: {args.partitions}")
    print("=" * 60)

    # Step 1: Create topic
    if not create_topic(args.bootstrap_server, args.topic, args.partitions):
        print("\nFAILED: Topic creation failed")
        sys.exit(1)

    # Step 2: Produce messages
    produced_count = produce_messages(args.bootstrap_server, args.topic, args.messages)
    if produced_count != args.messages:
        print(f"\nFAILED: Expected {args.messages} messages, produced {produced_count}")
        sys.exit(1)

    # Step 3: Consume messages
    consumed_count = consume_messages(args.bootstrap_server, args.topic, args.messages)

    # Step 4: Verify
    print("\n" + "=" * 60)
    print("Test Results")
    print("=" * 60)
    print(f"Produced: {produced_count}")
    print(f"Consumed: {consumed_count}")
    print(f"Match: {produced_count == consumed_count}")

    if produced_count == consumed_count == args.messages:
        print("\nSUCCESS: All messages produced and consumed correctly!")
        sys.exit(0)
    else:
        print(f"\nFAILED: Message count mismatch (expected {args.messages})")
        sys.exit(1)

if __name__ == '__main__':
    main()
