#!/usr/bin/env python3
"""Decode the ApiVersions response."""

import struct

# The response hex from the test
response_hex = "0000000100000000000a0000000000080000010000000b00000300000009000008000000080000090000000700000b0000000700000c0000000400000d0000000400000e0000000500001200000003000000000000"
response = bytes.fromhex(response_hex)

offset = 0

# Correlation ID
corr_id = struct.unpack('>i', response[offset:offset+4])[0]
offset += 4
print(f"Correlation ID: {corr_id}")

# Error code
error_code = struct.unpack('>h', response[offset:offset+2])[0]
offset += 2
print(f"Error code: {error_code}")

# API count
api_count = struct.unpack('>i', response[offset:offset+4])[0]
offset += 4
print(f"API count: {api_count}")

print("\nSupported APIs:")
api_names = {
    0: 'Produce', 1: 'Fetch', 2: 'ListOffsets', 3: 'Metadata',
    8: 'OffsetCommit', 9: 'OffsetFetch', 10: 'FindCoordinator',
    11: 'JoinGroup', 12: 'Heartbeat', 13: 'LeaveGroup', 14: 'SyncGroup',
    15: 'DescribeGroups', 16: 'ListGroups', 17: 'SaslHandshake',
    18: 'ApiVersions', 19: 'CreateTopics', 20: 'DeleteTopics'
}

for i in range(api_count):
    api_key = struct.unpack('>h', response[offset:offset+2])[0]
    offset += 2
    min_version = struct.unpack('>h', response[offset:offset+2])[0]
    offset += 2
    max_version = struct.unpack('>h', response[offset:offset+2])[0]
    offset += 2
    
    api_name = api_names.get(api_key, f'Unknown({api_key})')
    print(f"  {api_key:2d} {api_name:20s}: v{min_version}-{max_version}")

# Throttle time (if present)
if offset < len(response):
    throttle_time = struct.unpack('>i', response[offset:offset+4])[0]
    print(f"\nThrottle time: {throttle_time}ms")