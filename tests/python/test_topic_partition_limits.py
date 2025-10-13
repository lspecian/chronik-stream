#!/usr/bin/env python3
"""Test only the limit enforcement, not actual creation of 10k partitions"""

from kafka.admin import KafkaAdminClient, NewTopic
import sys

admin_client = KafkaAdminClient(
    bootstrap_servers='localhost:9092',
    client_id='limits-test'
)

print("=" * 70)
print("CHRONIK PARTITION LIMITS ENFORCEMENT TEST")
print("=" * 70)

# Test 1: Over limit (10,001) - should be REJECTED
print("\n1. Creating topic with 10,001 partitions (should be REJECTED)...")
try:
    admin_client.delete_topics(['test-over-limit'])
    import time; time.sleep(1)
except:
    pass

topic = NewTopic('test-over-limit', num_partitions=10001, replication_factor=1)
result = admin_client.create_topics([topic], timeout_ms=5000)
topics = admin_client.list_topics()
if 'test-over-limit' in topics:
    print("   ✗ FAILED: Topic was created when it should have been rejected!")
    sys.exit(1)
else:
    print("   ✓ PASSED: Topic with 10,001 partitions was correctly REJECTED")

# Test 2: Zero partitions - should be REJECTED
print("\n2. Creating topic with 0 partitions (should be REJECTED)...")
try:
    admin_client.delete_topics(['test-zero'])
    import time; time.sleep(1)
except:
    pass

topic = NewTopic('test-zero', num_partitions=0, replication_factor=1)
result = admin_client.create_topics([topic], timeout_ms=5000)
topics = admin_client.list_topics()
if 'test-zero' in topics:
    print("   ✗ FAILED: Topic was created when it should have been rejected!")
    sys.exit(1)
else:
    print("   ✓ PASSED: Topic with 0 partitions was correctly REJECTED")

# Test 3: Negative partitions - should be REJECTED (client-side or server-side)
print("\n3. Creating topic with -5 partitions (should be REJECTED)...")
try:
    topic = NewTopic('test-negative', num_partitions=-5, replication_factor=1)
    result = admin_client.create_topics([topic], timeout_ms=5000)
    topics = admin_client.list_topics()
    if 'test-negative' in topics:
        print("   ✗ FAILED: Topic was created when it should have been rejected!")
        sys.exit(1)
    else:
        print("   ✓ PASSED: Topic with negative partitions was correctly REJECTED")
except Exception as e:
    print(f"   ✓ PASSED: Rejected (client or server): {e}")

# Test 4: Valid count (100)
print("\n4. Creating topic with 100 partitions (should SUCCEED)...")
try:
    admin_client.delete_topics(['test-valid'])
    import time; time.sleep(1)
except:
    pass

topic = NewTopic('test-valid', num_partitions=100, replication_factor=1)
result = admin_client.create_topics([topic], timeout_ms=5000)
topics = admin_client.list_topics()
if 'test-valid' in topics:
    print("   ✓ PASSED: Topic with 100 partitions created successfully")
else:
    print("   ✗ FAILED: Topic should have been created")
    sys.exit(1)

admin_client.close()

print("\n" + "=" * 70)
print("ALL LIMIT TESTS PASSED!")
print("Chronik correctly enforces:")
print("  - Maximum 10,000 partitions per topic")
print("  - Minimum 1 partition per topic")
print("=" * 70)
