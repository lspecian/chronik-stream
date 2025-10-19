#!/usr/bin/env python3
"""
Test Kafka client operations against 3-node Raft cluster
"""

import time
from kafka import KafkaProducer, KafkaConsumer, KafkaAdminClient
from kafka.admin import NewTopic
from kafka.errors import KafkaError

def test_cluster():
    print("=" * 60)
    print("Testing 3-Node Raft Cluster with Kafka Python Client")
    print("=" * 60)

    # Connect to cluster (all 3 nodes as bootstrap servers)
    bootstrap_servers = ['localhost:9092', 'localhost:9093', 'localhost:9094']

    print(f"\n1. Connecting to cluster: {bootstrap_servers}")

    try:
        # Create admin client
        admin = KafkaAdminClient(
            bootstrap_servers=bootstrap_servers,
            client_id='test-admin',
            request_timeout_ms=10000,
        )
        print("✓ Admin client connected")

        # List brokers
        print("\n2. Listing brokers...")
        cluster_metadata = admin._client.cluster
        brokers = cluster_metadata.brokers()
        print(f"✓ Found {len(brokers)} broker(s):")
        for broker in brokers:
            print(f"  - Broker {broker.nodeId}: {broker.host}:{broker.port}")

        if len(brokers) == 0:
            print("✗ FAILURE: No brokers found - cluster metadata is broken!")
            return False

        # Create topic
        topic_name = "test-raft-cluster"
        print(f"\n3. Creating topic '{topic_name}'...")
        topic = NewTopic(name=topic_name, num_partitions=3, replication_factor=1)
        try:
            admin.create_topics([topic], validate_only=False)
            print(f"✓ Topic '{topic_name}' created")
        except Exception as e:
            if "already exists" in str(e).lower():
                print(f"✓ Topic '{topic_name}' already exists")
            else:
                raise

        time.sleep(2)  # Wait for topic metadata to propagate

        # Produce messages
        print(f"\n4. Producing 10 messages to topic '{topic_name}'...")
        producer = KafkaProducer(
            bootstrap_servers=bootstrap_servers,
            client_id='test-producer',
            acks='all',  # Wait for all in-sync replicas
            retries=3,
        )

        for i in range(10):
            message = f"Test message {i} from Python client".encode('utf-8')
            future = producer.send(topic_name, message)
            record_metadata = future.get(timeout=10)
            print(f"  ✓ Message {i} sent to partition {record_metadata.partition} at offset {record_metadata.offset}")

        producer.flush()
        producer.close()
        print("✓ All messages produced successfully")

        # Consume messages
        print(f"\n5. Consuming messages from topic '{topic_name}'...")
        consumer = KafkaConsumer(
            topic_name,
            bootstrap_servers=bootstrap_servers,
            auto_offset_reset='earliest',
            enable_auto_commit=False,
            consumer_timeout_ms=5000,
            client_id='test-consumer',
        )

        messages_consumed = 0
        for message in consumer:
            print(f"  ✓ Consumed: partition={message.partition}, offset={message.offset}, value={message.value.decode('utf-8')}")
            messages_consumed += 1

        consumer.close()

        if messages_consumed == 10:
            print(f"✓ Successfully consumed all {messages_consumed} messages")
        else:
            print(f"✗ FAILURE: Expected 10 messages, got {messages_consumed}")
            return False

        print("\n" + "=" * 60)
        print("✓ ALL TESTS PASSED - Cluster is working correctly!")
        print("=" * 60)
        return True

    except Exception as e:
        print(f"\n✗ FAILURE: {type(e).__name__}: {e}")
        import traceback
        traceback.print_exc()
        return False

if __name__ == "__main__":
    success = test_cluster()
    exit(0 if success else 1)
