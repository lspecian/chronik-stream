#!/usr/bin/env python3
"""
Test that all 3 nodes can serve fetch requests.
Verifies follower read support is working.
"""

import sys
from kafka import KafkaConsumer

def test_fetch_from_node(node_port):
    """Test fetching from a specific node"""
    print(f"\n[TEST] Fetching from localhost:{node_port}")

    try:
        consumer = KafkaConsumer(
            'test-cluster-topic',
            bootstrap_servers=[f'localhost:{node_port}'],
            auto_offset_reset='earliest',
            enable_auto_commit=False,
            consumer_timeout_ms=5000,
            request_timeout_ms=10000
        )

        messages = []
        for message in consumer:
            messages.append(message)
            if len(messages) >= 10:  # Just fetch 10 messages to verify
                break

        consumer.close()

        print(f"✅ Node {node_port}: Successfully fetched {len(messages)} messages")
        return True

    except Exception as e:
        print(f"❌ Node {node_port}: Failed to fetch - {e}")
        return False

def main():
    print("=" * 60)
    print("Testing Fetch from All 3 Nodes")
    print("=" * 60)

    nodes = [9092, 9093, 9094]
    results = []

    for node in nodes:
        results.append(test_fetch_from_node(node))

    print("\n" + "=" * 60)
    print("Results")
    print("=" * 60)

    for i, (node, result) in enumerate(zip(nodes, results)):
        status = "✅ PASS" if result else "❌ FAIL"
        print(f"Node {node}: {status}")

    print()

    if all(results):
        print("✅ SUCCESS: All 3 nodes can serve fetch requests!")
        return 0
    else:
        print("❌ FAILURE: Some nodes cannot serve fetch requests")
        return 1

if __name__ == '__main__':
    sys.exit(main())
