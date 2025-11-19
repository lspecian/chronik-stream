#!/usr/bin/env python3
"""
Comprehensive test: Create 1000 topics with 3 partitions each,
send a message on each partition, and consume them all.

This verifies the complete P0 fix for WAL metadata recovery and broker registration.
"""

from kafka import KafkaProducer, KafkaConsumer
import time
import sys

# Configuration
BOOTSTRAP_SERVERS = ['localhost:9092', 'localhost:9093', 'localhost:9094']
NUM_TOPICS = 1000
PARTITIONS_PER_TOPIC = 3
API_VERSION = (0, 10, 0)

def test_1000_topics():
    print("=" * 80)
    print("COMPREHENSIVE TEST: 1000 Topics x 3 Partitions")
    print("=" * 80)
    print()

    # Step 1: Verify broker discovery
    print("Step 1: Verifying broker discovery...")
    consumer = KafkaConsumer(
        bootstrap_servers=BOOTSTRAP_SERVERS,
        api_version=API_VERSION,
        consumer_timeout_ms=1000
    )
    brokers = consumer._client.cluster.brokers()
    print(f"✅ Discovered {len(brokers)} brokers:")
    for broker in brokers:
        print(f"   - Broker {broker.nodeId}: {broker.host}:{broker.port}")
    consumer.close()

    if len(brokers) != 3:
        print(f"❌ FAILED: Expected 3 brokers, got {len(brokers)}")
        return False
    print()

    # Step 2: Produce messages to all partitions (topics will be auto-created)
    print(f"Step 2: Producing messages to all {NUM_TOPICS * PARTITIONS_PER_TOPIC} partitions...")
    print(f"   (Topics will be auto-created on first produce)")
    producer = KafkaProducer(
        bootstrap_servers=BOOTSTRAP_SERVERS,
        api_version=API_VERSION,
        acks=1,
        max_in_flight_requests_per_connection=100,
        linger_ms=10,
        batch_size=16384
    )

    start_time = time.time()
    produced_count = 0
    try:
        for topic_idx in range(NUM_TOPICS):
            topic_name = f"test-topic-{topic_idx:04d}"
            for partition in range(PARTITIONS_PER_TOPIC):
                message = f"Message for {topic_name} partition {partition}".encode('utf-8')
                future = producer.send(topic_name, message, partition=partition)
                produced_count += 1

                # Show progress every 300 messages
                if produced_count % 300 == 0:
                    print(f"   Produced {produced_count}/{NUM_TOPICS * PARTITIONS_PER_TOPIC} messages...")

        # Flush to ensure all messages are sent
        producer.flush()

        elapsed = time.time() - start_time
        print(f"✅ Produced {produced_count} messages in {elapsed:.2f}s ({produced_count/elapsed:.1f} msg/sec)")
    except Exception as e:
        print(f"❌ FAILED to produce messages: {e}")
        return False
    finally:
        producer.close()
    print()

    # Step 3: Consume messages from all partitions
    print(f"Step 3: Consuming messages from all {NUM_TOPICS * PARTITIONS_PER_TOPIC} partitions...")

    # Create topic-partition assignments
    from kafka import TopicPartition
    partitions = []
    for topic_idx in range(NUM_TOPICS):
        topic_name = f"test-topic-{topic_idx:04d}"
        for partition in range(PARTITIONS_PER_TOPIC):
            partitions.append(TopicPartition(topic_name, partition))

    consumer = KafkaConsumer(
        bootstrap_servers=BOOTSTRAP_SERVERS,
        api_version=API_VERSION,
        auto_offset_reset='earliest',
        consumer_timeout_ms=5000,
        max_poll_records=500
    )

    consumer.assign(partitions)

    start_time = time.time()
    consumed_count = 0
    try:
        for message in consumer:
            consumed_count += 1

            # Show progress every 300 messages
            if consumed_count % 300 == 0:
                print(f"   Consumed {consumed_count}/{NUM_TOPICS * PARTITIONS_PER_TOPIC} messages...")

            # Stop after consuming all expected messages
            if consumed_count >= NUM_TOPICS * PARTITIONS_PER_TOPIC:
                break

        elapsed = time.time() - start_time
        print(f"✅ Consumed {consumed_count} messages in {elapsed:.2f}s ({consumed_count/elapsed:.1f} msg/sec)")
    except Exception as e:
        print(f"❌ FAILED to consume messages: {e}")
        return False
    finally:
        consumer.close()
    print()

    # Verify counts
    expected_messages = NUM_TOPICS * PARTITIONS_PER_TOPIC
    if consumed_count == expected_messages:
        print("=" * 80)
        print(f"✅ SUCCESS! All {expected_messages} messages produced and consumed correctly")
        print("=" * 80)
        return True
    else:
        print("=" * 80)
        print(f"❌ FAILED: Expected {expected_messages} messages, consumed {consumed_count}")
        print("=" * 80)
        return False

if __name__ == "__main__":
    success = test_1000_topics()
    sys.exit(0 if success else 1)
