#!/usr/bin/env python3
"""Check what API version we're actually requesting"""

import socket
import struct

# Send ApiVersions request with explicit v0
sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
sock.connect(('localhost', 9092))

# Build ApiVersions request
header = struct.pack('>h', 18)  # API key = 18 (ApiVersions)
header += struct.pack('>h', 0)  # API version = 0 (EXPLICITLY v0)
header += struct.pack('>i', 1)  # Correlation ID = 1
header += struct.pack('>h', -1)  # Null client ID

request = header  # No body for ApiVersions v0

print(f"Sending ApiVersions request:")
print(f"  API Key: 18 (ApiVersions)")
print(f"  API Version: 0 (v0)")
print(f"  Correlation ID: 1")
print(f"  Client ID: null")

# Send with length prefix
message = struct.pack('>i', len(request)) + request
sock.send(message)

# Read response
response_len_bytes = sock.recv(4)
response_len = struct.unpack('>i', response_len_bytes)[0]
print(f"\nResponse length: {response_len}")

response = sock.recv(response_len)

# Parse response expecting kafka-python's expected format
offset = 0

# Correlation ID
correlation_id = struct.unpack('>i', response[offset:offset+4])[0]
offset += 4
print(f"Correlation ID: {correlation_id}")

# Next should be error_code (2 bytes) if our fix worked
next_2_bytes = struct.unpack('>h', response[offset:offset+2])[0]
print(f"Next 2 bytes (should be error_code=0): {next_2_bytes}")

if next_2_bytes == 0:
    print("✓ Error code is 0 and comes first!")
    offset += 2

    # Now API array count
    api_count = struct.unpack('>i', response[offset:offset+4])[0]
    print(f"API array count: {api_count}")

    if api_count == 60:
        print("✓ API count is correct (60)")
        print("\n✓✓✓ FIX SUCCESSFUL! Response now matches kafka-python expectations!")
    else:
        print(f"✗ API count is wrong: {api_count}")
else:
    # Check if it's actually the array count
    next_4_bytes = struct.unpack('>i', response[offset-2:offset+2])[0]
    if next_4_bytes == 60:
        print(f"✗ Still sending array first (count={next_4_bytes})")
        print("✗ FIX NOT APPLIED")

sock.close()