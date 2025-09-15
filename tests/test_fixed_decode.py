#!/usr/bin/env python3
"""Properly decode the response after our fix"""

import socket
import struct

response_hex = "000000010000003c00000000000900010000000d00020000000700030000000c"

# Parse the hex
response = bytes.fromhex(response_hex)
print(f"Response bytes: {response_hex}")

offset = 0

# Correlation ID (4 bytes)
correlation_id = struct.unpack('>i', response[offset:offset+4])[0]
offset += 4
print(f"Correlation ID: {correlation_id}")

# What are the next bytes?
print(f"\nNext 8 bytes: {response[offset:offset+8].hex()}")

# Interpretation 1: Array first (original)
print("\nIf array comes first:")
array_count = struct.unpack('>i', response[offset:offset+4])[0]
print(f"  Array count: {array_count}")  # Should be 60
offset_1 = offset + 4
first_api = struct.unpack('>hhh', response[offset_1:offset_1+6])
print(f"  First API: key={first_api[0]}, min={first_api[1]}, max={first_api[2]}")

# Interpretation 2: Error code first (what kafka-python expects)
print("\nIf error_code comes first:")
offset = 4  # Reset
error_code = struct.unpack('>h', response[offset:offset+2])[0]
print(f"  Error code: {error_code}")  # Should be 0
offset_2 = offset + 2
# But wait, next bytes are 00003c00 which is NOT a valid array count
# 003c is 60 in the SECOND position

print(f"  Next 4 bytes after error: {response[offset_2:offset_2+4].hex()}")
weird_count = struct.unpack('>i', response[offset_2:offset_2+4])[0]
print(f"  Array count would be: {weird_count}")

print("\n✗ Our response is STILL sending array first, not error_code first!")