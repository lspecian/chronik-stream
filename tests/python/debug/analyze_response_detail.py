#!/usr/bin/env python3
"""Analyze the response in detail."""

import struct

# Response from chronik - just correlation ID and the beginning
# Full response shows: correlation_id=99 (0x63), then the issue starts
hex_data = "0000006300000001000000010000000100"  # First 17 bytes

# Convert to bytes
data = bytes.fromhex(hex_data)

print("=== Detailed Analysis ===")
print(f"Total bytes shown: {len(data)}")

offset = 0

# Correlation ID
corr_id = struct.unpack('>i', data[offset:offset+4])[0]
print(f"\nOffset {offset:02d}-{offset+3:02d}: {data[offset:offset+4].hex()} = Correlation ID: {corr_id}")
offset += 4

# Next 4 bytes (this is the issue)
val1 = struct.unpack('>i', data[offset:offset+4])[0]
print(f"Offset {offset:02d}-{offset+3:02d}: {data[offset:offset+4].hex()} = Value: {val1}")
print(f"           ^ If this is v1, this should be broker count")
print(f"           ^ If this is v3+, this is throttle time")
offset += 4

# Next 4 bytes
val2 = struct.unpack('>i', data[offset:offset+4])[0]
print(f"Offset {offset:02d}-{offset+3:02d}: {data[offset:offset+4].hex()} = Value: {val2}")
print(f"           ^ This looks like broker count = 1")
offset += 4

# Broker info starts
# Node ID
node_id = struct.unpack('>i', data[offset:offset+4])[0]
print(f"\nOffset {offset:02d}-{offset+3:02d}: {data[offset:offset+4].hex()} = Broker Node ID: {node_id}")
offset += 4

# Host length
host_len = struct.unpack('>h', data[offset:offset+2])[0]
print(f"Offset {offset:02d}-{offset+1:02d}: {data[offset:offset+2].hex()} = Host string length: {host_len}")
offset += 2

# Host string
host = data[offset:offset+host_len].decode('utf-8')
print(f"Offset {offset:02d}-{offset+host_len-1:02d}: {data[offset:offset+host_len].hex()} = Host: '{host}'")
offset += host_len

# Port
port = struct.unpack('>i', data[offset:offset+4])[0]
print(f"Offset {offset:02d}-{offset+3:02d}: {data[offset:offset+4].hex()} = Port: {port}")
offset += 4

# Rack
rack_len = struct.unpack('>h', data[offset:offset+2])[0]
print(f"Offset {offset:02d}-{offset+1:02d}: {data[offset:offset+2].hex()} = Rack length: {rack_len} (-1 = null)")
offset += 2

# Controller ID (v1+)
controller_id = struct.unpack('>i', data[offset:offset+4])[0]
print(f"\nOffset {offset:02d}-{offset+3:02d}: {data[offset:offset+4].hex()} = Controller ID: {controller_id}")
offset += 4

# Topics count
topic_count = struct.unpack('>i', data[offset:offset+4])[0]
print(f"Offset {offset:02d}-{offset+3:02d}: {data[offset:offset+4].hex()} = Topic count: {topic_count}")

print("\n=== CONCLUSION ===")
print("The response has an extra 4 bytes at offset 4-7 with value 1")
print("This is being interpreted as throttle_time by kafkactl")
print("But for v1, there should be NO throttle_time field!")