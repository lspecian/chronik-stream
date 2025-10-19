#!/usr/bin/env python3
"""Simple test to check metadata API response"""

from kafka import KafkaAdminClient
from kafka.errors import KafkaError
import sys

try:
    print("Creating Kafka admin client...")
    admin = KafkaAdminClient(
        bootstrap_servers='localhost:9092',
        client_id='metadata-test',
        request_timeout_ms=10000
    )

    print("Getting cluster metadata...")
    metadata = admin._client.cluster

    brokers = metadata.brokers()
    print(f"\nBrokers ({len(brokers)} total):")
    for broker in brokers:
        print(f"  - Broker {broker.nodeId}: {broker.host}:{broker.port}")

    print("\n✅ SUCCESS: Metadata request completed")
    admin.close()
    sys.exit(0)

except Exception as e:
    print(f"\n❌ ERROR: {e}")
    import traceback
    traceback.print_exc()
    sys.exit(1)
