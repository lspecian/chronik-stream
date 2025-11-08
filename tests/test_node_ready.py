#!/usr/bin/env python3
"""
Minimal test to reproduce NodeNotReadyError in v2.2.3 cluster mode.
"""

from kafka import KafkaAdminClient
from kafka.errors import NodeNotReadyError
import traceback

print("Testing Chronik v2.2.3 cluster mode connection...")
print("=" * 60)

try:
    print("\n1. Attempting to connect to cluster...")
    print("   Bootstrap servers: localhost:9092, localhost:9093, localhost:9094")

    admin = KafkaAdminClient(
        bootstrap_servers=['localhost:9092', 'localhost:9093', 'localhost:9094'],
        client_id='test-node-ready',
        request_timeout_ms=10000,
        api_version=(2, 5, 0)  # Use a specific version
    )

    print("✅ Connection successful!")
    print(f"   Bootstrap servers: localhost:9092, localhost:9093, localhost:9094")

    print("\n2. Listing topics...")
    topics = admin.list_topics()
    print(f"✅ Topics: {topics}")

    admin.close()
    print("\n✅ Test PASSED - No NodeNotReadyError!")

except NodeNotReadyError as e:
    print(f"\n❌ NodeNotReadyError occurred: {e}")
    print("\nFull traceback:")
    traceback.print_exc()
    print("\nThis confirms the issue from the test report.")

except Exception as e:
    print(f"\n❌ Unexpected error: {type(e).__name__}: {e}")
    traceback.print_exc()
