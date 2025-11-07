#!/usr/bin/env python3
"""
Test script for 3-node cluster broker discovery fix (v2.2.1)

This script tests the critical bug fix where broker metadata was not synchronized
across cluster nodes via Raft consensus.

Usage:
    1. Start 3-node cluster manually (see instructions below)
    2. Run: python3 tests/test_cluster_broker_discovery.py

Expected behavior:
- All 3 nodes should report complete broker list: [1, 2, 3]
- Producer/consumer operations should succeed using any node
"""

import sys
import time
from kafka import KafkaProducer, KafkaConsumer, KafkaAdminClient
from kafka.admin import NewTopic
from kafka.errors import KafkaError

# Test configuration
NODE_1_ADDR = 'localhost:19092'
NODE_2_ADDR = 'localhost:19093'
NODE_3_ADDR = 'localhost:19094'

ALL_NODES = [NODE_1_ADDR, NODE_2_ADDR, NODE_3_ADDR]

def test_broker_discovery():
    """Test that all nodes return complete broker metadata"""
    print("=" * 60)
    print("Testing Broker Discovery")
    print("=" * 60)

    for node_addr in ALL_NODES:
        print(f"\n📍 Testing metadata from {node_addr}...")

        try:
            admin = KafkaAdminClient(
                bootstrap_servers=node_addr,
                request_timeout_ms=10000
            )

            # Get cluster metadata
            cluster_metadata = admin._client.cluster
            brokers = cluster_metadata.brokers()

            print(f"   Broker count: {len(brokers)}")
            for broker in brokers:
                print(f"   - Broker {broker.nodeId}: {broker.host}:{broker.port}")

            # Assert complete broker list
            broker_ids = [broker.nodeId for broker in brokers]
            assert len(broker_ids) == 3, f"Expected 3 brokers, got {len(broker_ids)}"
            assert set(broker_ids) == {1, 2, 3}, f"Expected brokers [1, 2, 3], got {broker_ids}"

            print(f"   ✅ Node {node_addr} has complete broker metadata")

            admin.close()

        except Exception as e:
            print(f"   ❌ ERROR: {e}")
            return False

    print("\n✅ ALL NODES HAVE COMPLETE BROKER METADATA")
    return True

def test_produce_consume():
    """Test producer/consumer operations across nodes"""
    print("\n" + "=" * 60)
    print("Testing Producer/Consumer Operations")
    print("=" * 60)

    topic_name = "test-cluster-topic"
    test_messages = [
        {"key": "key1", "value": "Hello from node 1"},
        {"key": "key2", "value": "Hello from node 2"},
        {"key": "key3", "value": "Hello from node 3"},
    ]

    try:
        # Create topic (if needed)
        print(f"\n📝 Creating topic '{topic_name}'...")
        admin = KafkaAdminClient(bootstrap_servers=NODE_1_ADDR)

        try:
            new_topic = NewTopic(name=topic_name, num_partitions=3, replication_factor=3)
            admin.create_topics([new_topic], timeout_ms=10000)
            print(f"   ✅ Topic created successfully")
        except Exception as e:
            if "TopicExistsError" in str(e) or "already exists" in str(e):
                print(f"   ℹ️  Topic already exists")
            else:
                raise

        admin.close()

        # Produce messages to different nodes
        print(f"\n📤 Producing messages...")
        for i, (node_addr, msg) in enumerate(zip(ALL_NODES, test_messages)):
            print(f"   Producing to {node_addr}: {msg['value']}")

            producer = KafkaProducer(
                bootstrap_servers=node_addr,
                value_serializer=lambda v: v.encode('utf-8'),
                key_serializer=lambda k: k.encode('utf-8') if k else None,
                request_timeout_ms=10000
            )

            future = producer.send(topic_name, key=msg['key'], value=msg['value'])
            result = future.get(timeout=10)

            print(f"      ✅ Message sent: partition={result.partition}, offset={result.offset}")

            producer.close()

        # Wait for messages to be replicated
        print(f"\n⏳ Waiting for replication (5 seconds)...")
        time.sleep(5)

        # Consume messages from different node than production
        print(f"\n📥 Consuming messages from {NODE_2_ADDR}...")

        consumer = KafkaConsumer(
            topic_name,
            bootstrap_servers=NODE_2_ADDR,
            group_id='test-consumer-group',
            auto_offset_reset='earliest',
            consumer_timeout_ms=10000,
            enable_auto_commit=False
        )

        consumed_messages = []
        for message in consumer:
            key = message.key.decode('utf-8') if message.key else None
            value = message.value.decode('utf-8') if message.value else None
            consumed_messages.append((key, value))
            print(f"   Consumed: key={key}, value={value}, partition={message.partition}, offset={message.offset}")

            if len(consumed_messages) >= len(test_messages):
                break

        consumer.close()

        # Verify all messages were consumed
        assert len(consumed_messages) == len(test_messages), \
            f"Expected {len(test_messages)} messages, consumed {len(consumed_messages)}"

        consumed_keys = [msg[0] for msg in consumed_messages]
        expected_keys = [msg['key'] for msg in test_messages]
        assert set(consumed_keys) == set(expected_keys), \
            f"Expected keys {expected_keys}, got {consumed_keys}"

        print(f"\n✅ PRODUCER/CONSUMER OPERATIONS SUCCESSFUL")
        return True

    except Exception as e:
        print(f"\n❌ ERROR during produce/consume: {e}")
        import traceback
        traceback.print_exc()
        return False

def main():
    """Run all tests"""
    print("🚀 Chronik Cluster Broker Discovery Test (v2.2.1)")
    print("=" * 60)
    print()
    print("Prerequisites:")
    print("  - 3-node cluster running on localhost:19092-19094")
    print("  - Each node configured with cluster mode enabled")
    print()

    # Wait for cluster to be ready
    print("⏳ Waiting for cluster to be ready (10 seconds)...")
    time.sleep(10)

    # Run tests
    all_passed = True

    # Test 1: Broker discovery
    if not test_broker_discovery():
        all_passed = False

    # Test 2: Producer/Consumer
    if not test_produce_consume():
        all_passed = False

    # Final result
    print("\n" + "=" * 60)
    if all_passed:
        print("✅ ALL TESTS PASSED")
        print("=" * 60)
        print()
        print("Verified:")
        print("  ✅ Broker metadata synchronized across all nodes")
        print("  ✅ Kafka clients can discover all brokers from any node")
        print("  ✅ Producer/consumer operations work correctly")
        print()
        return 0
    else:
        print("❌ SOME TESTS FAILED")
        print("=" * 60)
        return 1

if __name__ == '__main__':
    sys.exit(main())
