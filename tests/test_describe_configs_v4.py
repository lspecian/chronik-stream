#!/usr/bin/env python3
"""
Test DescribeConfigs API v4 with Java Kafka AdminClient.
This reproduces the exact buffer underflow issue reported in Kafka UI.
"""

import sys
import struct
from kafka.admin import AdminClient, ConfigResource

def test_describe_configs_v4():
    """Test DescribeConfigs with Java-style AdminClient"""

    print("=" * 80)
    print("DescribeConfigs v4 Buffer Underflow Test")
    print("=" * 80)
    print()

    # Connect to Chronik
    admin_client = AdminClient({
        'bootstrap.servers': 'localhost:9092',
        'client.id': 'test-admin-client',
        'request.timeout.ms': 10000,
    })

    print("✓ AdminClient created successfully")
    print()

    try:
        # Create ConfigResource for broker 0
        resource = ConfigResource(ConfigResource.Type.BROKER, "0")

        print("→ Sending DescribeConfigs request for broker 0...")
        print(f"  Resource type: {resource.restype} (BROKER)")
        print(f"  Resource name: {resource.name}")
        print()

        # This should trigger the buffer underflow in Chronik v1.3.57
        futures = admin_client.describe_configs([resource])

        # Wait for response
        for resource, future in futures.items():
            print(f"→ Waiting for response for {resource}...")
            try:
                config_dict = future.result(timeout=10)
                print(f"✓ SUCCESS: Received config response")
                print(f"  Configs returned: {len(config_dict)} entries")

                # Print first few configs
                for i, (key, value) in enumerate(list(config_dict.items())[:5]):
                    print(f"    [{i}] {key} = {value.value}")
                    print(f"        source: {value.source}")
                    print(f"        is_sensitive: {value.is_sensitive}")
                    print(f"        is_read_only: {value.is_read_only}")

                if len(config_dict) > 5:
                    print(f"    ... and {len(config_dict) - 5} more")

                print()
                print("=" * 80)
                print("✓ TEST PASSED: DescribeConfigs v4 works correctly!")
                print("=" * 80)
                return True

            except Exception as e:
                print(f"✗ FAILED: {type(e).__name__}: {e}")
                print()
                print("Expected error: BufferUnderflowException or SchemaException")
                print("This indicates Chronik's DescribeConfigs response is malformed")
                print()
                print("=" * 80)
                print("✗ TEST FAILED: DescribeConfigs v4 buffer underflow confirmed")
                print("=" * 80)
                return False

    except Exception as e:
        print(f"✗ Unexpected error: {type(e).__name__}: {e}")
        import traceback
        traceback.print_exc()
        return False

def test_admin_client_init():
    """Test AdminClient initialization (FindCoordinator/Metadata issue)"""

    print()
    print("=" * 80)
    print("AdminClient Initialization Test")
    print("=" * 80)
    print()

    try:
        # This tests the "No resolvable bootstrap urls" issue
        admin_client = AdminClient({
            'bootstrap.servers': 'localhost:9092',
            'client.id': 'test-admin-init',
        })

        print("✓ AdminClient initialized successfully")
        print()

        # Try to list topics (requires metadata)
        print("→ Listing topics to verify metadata works...")
        metadata = admin_client.list_topics(timeout=10)

        print(f"✓ Metadata retrieved successfully")
        print(f"  Cluster ID: {metadata.cluster_id}")
        print(f"  Controller ID: {metadata.controller_id}")
        print(f"  Brokers: {len(metadata.brokers)}")

        for broker_id, broker in metadata.brokers.items():
            print(f"    Broker {broker_id}: {broker.host}:{broker.port}")

        print(f"  Topics: {len(metadata.topics)}")
        for topic_name in list(metadata.topics.keys())[:5]:
            print(f"    - {topic_name}")

        if len(metadata.topics) > 5:
            print(f"    ... and {len(metadata.topics) - 5} more")

        print()
        print("=" * 80)
        print("✓ TEST PASSED: AdminClient initialization works!")
        print("=" * 80)
        return True

    except Exception as e:
        print(f"✗ FAILED: {type(e).__name__}: {e}")

        if "No resolvable bootstrap urls" in str(e):
            print()
            print("This is the exact error reported with Apache Flink!")
            print("AdminClient cannot parse Chronik's Metadata response")

        print()
        print("=" * 80)
        print("✗ TEST FAILED: AdminClient initialization failed")
        print("=" * 80)
        import traceback
        traceback.print_exc()
        return False

if __name__ == '__main__':
    print()
    print("╔" + "=" * 78 + "╗")
    print("║" + " " * 15 + "Chronik AdminClient Compatibility Test" + " " * 24 + "║")
    print("║" + " " * 20 + "Testing v1.3.57 Critical Issues" + " " * 26 + "║")
    print("╚" + "=" * 78 + "╝")
    print()

    results = []

    # Test 1: AdminClient initialization
    results.append(("AdminClient Init", test_admin_client_init()))

    # Test 2: DescribeConfigs v4
    results.append(("DescribeConfigs v4", test_describe_configs_v4()))

    # Summary
    print()
    print("=" * 80)
    print("TEST SUMMARY")
    print("=" * 80)

    passed = sum(1 for _, result in results if result)
    total = len(results)

    for test_name, result in results:
        status = "✓ PASS" if result else "✗ FAIL"
        print(f"  {status}: {test_name}")

    print()
    print(f"Results: {passed}/{total} tests passed")
    print("=" * 80)

    sys.exit(0 if passed == total else 1)
