#!/usr/bin/env python3
"""
Test script for Phase 2: RaftCluster leadership checks

Tests:
1. Create topic on leader node
2. Produce to leader (should succeed)
3. Produce to follower (should return NOT_LEADER_FOR_PARTITION error)
"""

from kafka import KafkaProducer, KafkaAdminClient
from kafka.admin import NewTopic
from kafka.errors import KafkaError, NotLeaderForPartitionError
import time
import sys

# Node configuration
NODES = {
    1: "localhost:9092",
    2: "localhost:9093",  # Leader
    3: "localhost:9094",
}

LEADER_NODE = 2
FOLLOWER_NODE = 1

def create_topic(bootstrap_server, topic_name="test-leadership", partitions=1):
    """Create a topic by producing to it (auto-create)"""
    print(f"\n=== Creating topic '{topic_name}' on {bootstrap_server} (via auto-create) ===")
    try:
        producer = KafkaProducer(
            bootstrap_servers=bootstrap_server,
            client_id="test-admin",
            api_version=(0, 10, 0),
            request_timeout_ms=10000
        )

        # Send a dummy message to auto-create topic
        future = producer.send(topic_name, b"topic_creation_message")
        future.get(timeout=10)

        producer.close()

        print(f"✅ Topic '{topic_name}' created successfully (auto-create)")
        time.sleep(2)  # Wait for topic to propagate
        return True

    except Exception as e:
        print(f"❌ Failed to create topic: {e}")
        return False

def test_produce_to_leader(bootstrap_server, topic_name="test-leadership"):
    """Test producing to the leader node (should succeed)"""
    print(f"\n=== Test 1: Produce to LEADER (Node {LEADER_NODE}) ===")
    print(f"Connecting to: {bootstrap_server}")

    try:
        producer = KafkaProducer(
            bootstrap_servers=bootstrap_server,
            client_id="test-producer-leader",
            api_version=(0, 10, 0),
            acks=1,
            retries=0,
            request_timeout_ms=10000
        )

        # Send message
        future = producer.send(topic_name, b"test message to leader")
        metadata = future.get(timeout=10)

        producer.close()

        print(f"✅ SUCCESS: Message sent to partition {metadata.partition}, offset {metadata.offset}")
        return True

    except Exception as e:
        print(f"❌ FAILED: {e}")
        return False

def test_produce_to_follower(bootstrap_server, topic_name="test-leadership"):
    """Test producing to a follower node (should fail with NOT_LEADER_FOR_PARTITION)"""
    print(f"\n=== Test 2: Produce to FOLLOWER (Node {FOLLOWER_NODE}) ===")
    print(f"Connecting to: {bootstrap_server}")

    try:
        producer = KafkaProducer(
            bootstrap_servers=bootstrap_server,
            client_id="test-producer-follower",
            api_version=(0, 10, 0),
            acks=1,
            retries=0,  # Don't retry - we want to see the error
            request_timeout_ms=10000
        )

        # Send message
        future = producer.send(topic_name, b"test message to follower")

        try:
            metadata = future.get(timeout=10)
            producer.close()
            print(f"❌ UNEXPECTED SUCCESS: Message was accepted by follower!")
            print(f"   Partition: {metadata.partition}, Offset: {metadata.offset}")
            return False

        except NotLeaderForPartitionError as e:
            producer.close()
            print(f"✅ EXPECTED ERROR: Got NOT_LEADER_FOR_PARTITION error")
            print(f"   Error: {e}")
            return True

        except KafkaError as e:
            producer.close()
            error_name = type(e).__name__
            if "NotLeader" in error_name or "LEADER" in str(e):
                print(f"✅ EXPECTED ERROR: Got leadership error ({error_name})")
                print(f"   Error: {e}")
                return True
            else:
                print(f"❌ UNEXPECTED ERROR: {error_name}")
                print(f"   Error: {e}")
                return False

    except Exception as e:
        print(f"❌ FAILED with unexpected exception: {e}")
        return False

def main():
    """Run all tests"""
    print("=" * 70)
    print("Phase 2 Leadership Check Tests")
    print("=" * 70)
    print(f"\nCluster Configuration:")
    print(f"  Leader Node: {LEADER_NODE} ({NODES[LEADER_NODE]})")
    print(f"  Follower Node: {FOLLOWER_NODE} ({NODES[FOLLOWER_NODE]})")

    # Create topic on leader
    if not create_topic(NODES[LEADER_NODE]):
        print("\n❌ Failed to create topic, aborting tests")
        sys.exit(1)

    # Test 1: Produce to leader (should succeed)
    test1_pass = test_produce_to_leader(NODES[LEADER_NODE])

    # Test 2: Produce to follower (should fail with NOT_LEADER_FOR_PARTITION)
    test2_pass = test_produce_to_follower(NODES[FOLLOWER_NODE])

    # Summary
    print("\n" + "=" * 70)
    print("Test Summary")
    print("=" * 70)
    print(f"Test 1 (Produce to Leader):   {'✅ PASS' if test1_pass else '❌ FAIL'}")
    print(f"Test 2 (Produce to Follower): {'✅ PASS' if test2_pass else '❌ FAIL'}")
    print("=" * 70)

    if test1_pass and test2_pass:
        print("\n🎉 All tests PASSED! Phase 2 leadership checks working correctly!")
        sys.exit(0)
    else:
        print("\n❌ Some tests FAILED!")
        sys.exit(1)

if __name__ == "__main__":
    main()
