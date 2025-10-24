#!/usr/bin/env python3
"""Test Raft cluster with event-driven leader synchronization."""

from kafka import KafkaProducer, KafkaConsumer, KafkaAdminClient
from kafka.admin import NewTopic
from kafka.errors import TopicAlreadyExistsError
import time

def test_cluster():
    print("=" * 60)
    print("Testing Raft Cluster with Event-Driven Leader Sync")
    print("=" * 60)

    # Test connectivity to all 3 nodes
    print("\n1. Testing connectivity to all 3 nodes...")
    for port in [9092, 9093, 9094]:
        try:
            admin = KafkaAdminClient(
                bootstrap_servers=f'localhost:{port}',
                request_timeout_ms=5000
            )
            topics = admin.list_topics()
            print(f"   ✅ Node on port {port}: {len(topics)} topics")
            admin.close()
        except Exception as e:
            print(f"   ❌ Node on port {port}: {e}")
            return False

    # Create topic with replication
    print("\n2. Creating topic 'raft-test' (3 partitions, RF=3)...")
    try:
        admin = KafkaAdminClient(bootstrap_servers='localhost:9092')
        topic = NewTopic(
            name='raft-test',
            num_partitions=3,
            replication_factor=3,
            topic_configs={'min.insync.replicas': '2'}
        )
        admin.create_topics([topic])
        print("   ✅ Topic created successfully")
        admin.close()
    except TopicAlreadyExistsError:
        print("   ℹ️  Topic already exists")
    except Exception as e:
        print(f"   ❌ Failed to create topic: {e}")
        return False

    time.sleep(2)  # Wait for topic creation to propagate

    # Produce messages
    print("\n3. Producing 10 messages to 'raft-test'...")
    try:
        producer = KafkaProducer(
            bootstrap_servers=['localhost:9092', 'localhost:9093', 'localhost:9094'],
            acks='all',  # Require all replicas to acknowledge
            api_version=(0, 10, 0)
        )

        for i in range(10):
            key = f'key-{i}'.encode()
            value = f'Event-driven sync test message {i}'.encode()
            future = producer.send('raft-test', key=key, value=value)
            result = future.get(timeout=10)
            print(f"   ✅ Msg {i}: partition={result.partition}, offset={result.offset}")

        producer.flush()
        producer.close()
        print("   ✅ All messages produced successfully")
    except Exception as e:
        print(f"   ❌ Failed to produce: {e}")
        return False

    time.sleep(1)  # Wait for replication

    # Consume messages
    print("\n4. Consuming messages from 'raft-test'...")
    try:
        consumer = KafkaConsumer(
            'raft-test',
            bootstrap_servers=['localhost:9092', 'localhost:9093', 'localhost:9094'],
            auto_offset_reset='earliest',
            enable_auto_commit=False,
            consumer_timeout_ms=5000,
            api_version=(0, 10, 0)
        )

        messages = []
        for msg in consumer:
            messages.append(msg)
            print(f"   ✅ Consumed: partition={msg.partition}, offset={msg.offset}, value={msg.value.decode()}")

        consumer.close()

        if len(messages) == 10:
            print(f"   ✅ All 10 messages consumed successfully")
        else:
            print(f"   ⚠️  Expected 10 messages, got {len(messages)}")
            return False
    except Exception as e:
        print(f"   ❌ Failed to consume: {e}")
        return False

    print("\n" + "=" * 60)
    print("✅ SUCCESS: Raft cluster with event-driven sync is working!")
    print("=" * 60)
    return True

if __name__ == '__main__':
    success = test_cluster()
    exit(0 if success else 1)
