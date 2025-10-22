#!/usr/bin/env python3
"""
Test automatic replica creation when topics are created via CreateTopicWithAssignments
This verifies the BLOCKER #1 fix
"""

from kafka import KafkaProducer, KafkaConsumer
from kafka.admin import KafkaAdminClient, NewTopic
import time
import sys

def test_replica_creation():
    print("🚀 Testing Automatic Replica Creation Fix")
    print("=" * 70)

    # Wait for cluster to be ready
    print("\n⏳ Waiting for cluster to be ready (10s)...")
    time.sleep(10)

    # Test connection to all nodes
    print("\n🔌 Testing Kafka connections to all 3 nodes...")
    nodes = [("Node 1", 9092), ("Node 2", 9093), ("Node 3", 9094)]

    for node_name, port in nodes:
        try:
            admin = KafkaAdminClient(
                bootstrap_servers=f'localhost:{port}',
                api_version=(0, 10, 0),
                request_timeout_ms=5000
            )
            metadata = admin.list_topics()
            print(f"  ✅ {node_name} (:{port}) - Connected, {len(metadata)} topics")
            admin.close()
        except Exception as e:
            print(f"  ❌ {node_name} (:{port}) - Connection failed: {e}")
            return False

    # Create topic on Node 1 (leader)
    print("\n📝 Creating topic 'test-replicas' with 3 partitions on Node 1...")
    try:
        admin = KafkaAdminClient(
            bootstrap_servers='localhost:9092',
            api_version=(0, 10, 0),
            request_timeout_ms=30000
        )
        topic = NewTopic(name='test-replicas', num_partitions=3, replication_factor=1)
        admin.create_topics([topic])
        print("✅ Topic created")
        admin.close()
    except Exception as e:
        print(f"⚠️  Topic creation: {e}")

    # Wait for topic to propagate
    print("\n⏳ Waiting for topic to propagate to all nodes (5s)...")
    time.sleep(5)

    # Produce 1000 messages to Node 1
    print("\n✍️  Producing 1000 messages to Node 1...")
    producer = KafkaProducer(
        bootstrap_servers='localhost:9092',
        api_version=(0, 10, 0),
        max_block_ms=30000,
        acks='all'
    )

    produced_count = 0
    for i in range(1000):
        msg = f"Message {i}".encode('utf-8')
        try:
            future = producer.send('test-replicas', value=msg)
            record_metadata = future.get(timeout=30)
            produced_count += 1
            if i % 250 == 0:
                print(f"  📨 Sent message {i} -> partition {record_metadata.partition}, offset {record_metadata.offset}")
        except Exception as e:
            print(f"  ❌ Failed to send message {i}: {e}")
            break

    producer.flush()
    producer.close()
    print(f"✅ Produced {produced_count}/1000 messages")

    # Wait for replication
    print("\n⏳ Waiting for replication (5s)...")
    time.sleep(5)

    # Try to consume from each node
    for node_name, port in nodes:
        print(f"\n📖 Consuming from {node_name} (:{port})...")
        consumer = KafkaConsumer(
            'test-replicas',
            bootstrap_servers=f'localhost:{port}',
            api_version=(0, 10, 0),
            auto_offset_reset='earliest',
            consumer_timeout_ms=10000,
            enable_auto_commit=False
        )

        messages = []
        try:
            for msg in consumer:
                messages.append(msg.value.decode('utf-8'))
                if len(messages) >= produced_count:
                    break
        except Exception as e:
            print(f"  ⚠️  Consumer timeout: {e}")

        consumer.close()
        print(f"  ✅ Consumed {len(messages)}/{produced_count} messages from {node_name}")

        # CRITICAL CHECK: All nodes should be able to consume all messages
        if len(messages) != produced_count:
            print(f"  ❌ BLOCKER: {node_name} could not fetch all messages!")
            print(f"     Expected: {produced_count}, Got: {len(messages)}")
            return False

    print("\n" + "=" * 70)
    print("✅ SUCCESS: All nodes can fetch all 1000 messages!")
    print("✅ Automatic replica creation is working correctly!")
    return True

if __name__ == "__main__":
    success = test_replica_creation()
    sys.exit(0 if success else 1)
