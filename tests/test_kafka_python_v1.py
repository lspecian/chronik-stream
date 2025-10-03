#!/usr/bin/env python3
"""Test what kafka-python expects for Metadata v1 response."""

from kafka.protocol.metadata import MetadataResponse_v0, MetadataResponse_v1, MetadataResponse_v3
import io

print("=== Kafka-python Metadata Response Schemas ===\n")

print("MetadataResponse_v0 fields:")
# v0 has: brokers, topics
print("  - brokers array")
print("  - topics array")
print()

print("MetadataResponse_v1 fields:")
# v1 adds: controller_id
print("  - brokers array")
print("  - controller_id (NEW in v1)")
print("  - topics array")
print()

print("MetadataResponse_v3 fields:")
# v3 adds: throttle_time_ms
print("  - throttle_time_ms (NEW in v3)")
print("  - brokers array")
print("  - cluster_id")
print("  - controller_id")
print("  - topics array")
print()

# Try to decode our test response
test_response = bytes.fromhex("0000007b00000001000000010000000114746573742d62726f6b65722d312d68302d70333931000000099100000001ffffffff000000010014746573742d746f7069632d792d7066306531000000010000000000000000010000000100000001000000010000000100000001")

print("=== Testing decode of our response ===")
print(f"Response bytes (len={len(test_response)}): {test_response.hex()}")
print()

# Skip correlation ID (first 4 bytes)
body = test_response[4:]
print(f"Body without correlation ID (len={len(body)}): {body.hex()}")

# Try v1 decode
try:
    print("\nTrying MetadataResponse_v1.decode()...")
    decoded = MetadataResponse_v1.decode(io.BytesIO(body))
    print("SUCCESS! Decoded as v1")
    print(f"  Brokers: {len(decoded.brokers)}")
    print(f"  Controller ID: {decoded.controller_id}")
    print(f"  Topics: {len(decoded.topics)}")
except Exception as e:
    print(f"FAILED: {e}")

# Try v0 decode (should fail on extra controller_id field)
try:
    print("\nTrying MetadataResponse_v0.decode()...")
    decoded = MetadataResponse_v0.decode(io.BytesIO(body))
    print("Decoded as v0 (shouldn't work if controller_id present)")
    print(f"  Brokers: {len(decoded.brokers)}")
    print(f"  Topics: {len(decoded.topics)}")
except Exception as e:
    print(f"Expected failure for v0: {e}")