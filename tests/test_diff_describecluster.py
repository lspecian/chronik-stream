#!/usr/bin/env python3
"""
Test to analyze field boundaries and find the 126 bytes error
Based on the user's debugging plan
"""

import struct
import binascii

# Our Chronik response that Java client rejects
hex_response = "0000007b000000000000ffff00164d6b55304f4545774e546c46526b5934516a45324f5100000001000000010000000100093132372e302e302e3100002388ffff7fffffff"
response = bytes.fromhex(hex_response)

print("=" * 80)
print("CHRONIK DESCRIBECLUSTER V0 RESPONSE ANALYSIS")
print("=" * 80)
print(f"Total response size: {len(response)} bytes")
print(f"Hex: {hex_response}")
print()

# Field-by-field analysis
print("FIELD-BY-FIELD BREAKDOWN:")
print("-" * 80)
offset = 0

# Correlation ID (4 bytes)
corr_id = struct.unpack('>i', response[offset:offset+4])[0]
print(f"[{offset:3d}:{offset+4:3d}] Correlation ID:        {corr_id} (0x{response[offset:offset+4].hex()})")
offset += 4

# Throttle time (4 bytes)
throttle = struct.unpack('>i', response[offset:offset+4])[0]
print(f"[{offset:3d}:{offset+4:3d}] Throttle time:         {throttle} (0x{response[offset:offset+4].hex()})")
offset += 4

# Error code (2 bytes)
error_code = struct.unpack('>h', response[offset:offset+2])[0]
print(f"[{offset:3d}:{offset+2:3d}] Error code:            {error_code} (0x{response[offset:offset+2].hex()})")
offset += 2

# Error message (nullable string) - 2 bytes length + data
msg_len = struct.unpack('>h', response[offset:offset+2])[0]
print(f"[{offset:3d}:{offset+2:3d}] Error msg length:      {msg_len} (0x{response[offset:offset+2].hex()}) {'<-- NULL' if msg_len == -1 else ''}")
offset += 2
if msg_len > 0:
    msg = response[offset:offset+msg_len].decode()
    print(f"[{offset:3d}:{offset+msg_len:3d}] Error message:         '{msg}'")
    offset += msg_len

# Cluster ID (nullable string) - 2 bytes length + data
cluster_len = struct.unpack('>h', response[offset:offset+2])[0]
print(f"[{offset:3d}:{offset+2:3d}] Cluster ID length:     {cluster_len} (0x{response[offset:offset+2].hex()})")
offset += 2
if cluster_len > 0:
    cluster_id = response[offset:offset+cluster_len].decode()
    print(f"[{offset:3d}:{offset+cluster_len:3d}] Cluster ID:            '{cluster_id}'")
    offset += cluster_len

# Controller ID (4 bytes)
controller = struct.unpack('>i', response[offset:offset+4])[0]
print(f"[{offset:3d}:{offset+4:3d}] Controller ID:         {controller} (0x{response[offset:offset+4].hex()})")
offset += 4

# Brokers array count (4 bytes)
broker_count = struct.unpack('>i', response[offset:offset+4])[0]
print(f"[{offset:3d}:{offset+4:3d}] Broker count:          {broker_count} (0x{response[offset:offset+4].hex()})")
offset += 4

print()
print("BROKER ARRAY DETAILS:")
print("-" * 80)

for i in range(broker_count):
    print(f"Broker #{i}:")

    # Broker ID (4 bytes)
    broker_id = struct.unpack('>i', response[offset:offset+4])[0]
    print(f"  [{offset:3d}:{offset+4:3d}] ID:                  {broker_id} (0x{response[offset:offset+4].hex()})")
    offset += 4

    # Host (string - NOT nullable in v0) - 2 bytes length + data
    host_len = struct.unpack('>h', response[offset:offset+2])[0]
    print(f"  [{offset:3d}:{offset+2:3d}] Host length:         {host_len} (0x{response[offset:offset+2].hex()})")
    offset += 2
    if host_len > 0:
        host = response[offset:offset+host_len].decode()
        print(f"  [{offset:3d}:{offset+host_len:3d}] Host:                '{host}'")
        offset += host_len

    # Port (4 bytes)
    port = struct.unpack('>i', response[offset:offset+4])[0]
    print(f"  [{offset:3d}:{offset+4:3d}] Port:                {port} (0x{response[offset:offset+4].hex()})")
    offset += 4

    # Rack (nullable string) - 2 bytes length + data
    rack_len = struct.unpack('>h', response[offset:offset+2])[0]
    print(f"  [{offset:3d}:{offset+2:3d}] Rack length:         {rack_len} (0x{response[offset:offset+2].hex()}) {'<-- NULL' if rack_len == -1 else ''}")
    offset += 2
    if rack_len > 0:
        rack = response[offset:offset+rack_len].decode()
        print(f"  [{offset:3d}:{offset+rack_len:3d}] Rack:                '{rack}'")
        offset += rack_len

# Authorized operations (4 bytes)
print()
print("FINAL FIELD:")
print("-" * 80)
auth_ops = struct.unpack('>i', response[offset:offset+4])[0]
print(f"[{offset:3d}:{offset+4:3d}] Authorized ops:        {auth_ops} (0x{response[offset:offset+4].hex()}) = 0x{auth_ops:08x}")
offset += 4

print()
print("=" * 80)
print("ANALYSIS SUMMARY:")
print("-" * 80)
print(f"Total parsed:           {offset} bytes")
print(f"Response size:          {len(response)} bytes")
print(f"Match:                  {'✅ EXACT' if offset == len(response) else '❌ MISMATCH'}")

# Look for where Java might be seeing 126
print()
print("=" * 80)
print("SEARCHING FOR 126 (0x7E) BYTE VALUE:")
print("-" * 80)

# Direct search for 0x7E
found_7e = False
for i in range(len(response)):
    if response[i] == 0x7E:
        print(f"  Found 0x7E at offset {i}")
        found_7e = True

if not found_7e:
    print("  No direct 0x7E byte found")

# Check 2-byte values that could be 126
print()
print("SEARCHING FOR 126 AS 2-BYTE VALUE:")
print("-" * 80)
found_126 = False
for i in range(len(response)-1):
    val = struct.unpack('>H', response[i:i+2])[0]
    if val == 126:
        print(f"  Offset {i}: bytes {response[i]:02x} {response[i+1]:02x} = {val}")
        found_126 = True

if not found_126:
    print("  No 2-byte sequence equals 126")

# Check if any field could be misread as 126
print()
print("=" * 80)
print("HYPOTHESIS: Java client field alignment issue")
print("-" * 80)
print()
print("The Java error says: 'Error reading byte array of 126 byte(s): only 56 byte(s) available'")
print("This happens at DescribeClusterResponseData.java:118")
print()
print("Looking at our response:")
print(f"  - We send authorized_operations = 0x7fffffff (2147483647)")
print(f"  - This is at offset {offset-4}")
print(f"  - Total response is {len(response)} bytes")
print()
print("Potential issues to investigate:")
print("  1. Is the broker array encoded correctly?")
print("  2. Is the error_message field causing misalignment?")
print("  3. Is authorized_operations field in the right position?")
print()
print("Java client expectations for DescribeCluster v0:")
print("  - error_message: NULLABLE_STRING (we send null = 0xffff)")
print("  - cluster_id: NULLABLE_STRING (we send 22 chars)")
print("  - brokers: ARRAY with id, host, port, rack")
print("  - authorized_operations: INT32 (we send 0x7fffffff)")