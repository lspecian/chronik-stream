#!/usr/bin/env python3
"""Debug the exact response kafka-python receives"""

import socket
import struct

# Send ApiVersions request v0
sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
sock.connect(('localhost', 9092))

# Build ApiVersions request v0
header = struct.pack('>h', 18)  # API key
header += struct.pack('>h', 0)  # API version
header += struct.pack('>i', 1)  # Correlation ID
header += struct.pack('>h', -1)  # Null client ID

request = header  # No body

# Send with length prefix
message = struct.pack('>i', len(request)) + request
sock.send(message)

# Read response
response_len_bytes = sock.recv(4)
response_len = struct.unpack('>i', response_len_bytes)[0]
print(f"Response length: {response_len}")

response = sock.recv(response_len)
print(f"Full response hex: {response.hex()}")

# Decode step by step
offset = 0

# Correlation ID (always first)
correlation_id = struct.unpack('>i', response[offset:offset+4])[0]
offset += 4
print(f"Correlation ID: {correlation_id}")

# Next field - could be error_code (int16) or array count (int32)
# Try as int16 first
next_as_int16 = struct.unpack('>h', response[offset:offset+2])[0]
print(f"Next 2 bytes as int16: {next_as_int16}")

# Try as int32
next_as_int32 = struct.unpack('>i', response[offset:offset+4])[0]
print(f"Next 4 bytes as int32: {next_as_int32}")

# If it's error_code (int16=0), then next should be array count
if next_as_int16 == 0:
    print("Looks like error_code=0 came first")
    offset += 2

    # Now should be array count
    array_count = struct.unpack('>i', response[offset:offset+4])[0]
    offset += 4
    print(f"API array count: {array_count}")

    # First API
    api_key = struct.unpack('>h', response[offset:offset+2])[0]
    offset += 2
    min_ver = struct.unpack('>h', response[offset:offset+2])[0]
    offset += 2
    max_ver = struct.unpack('>h', response[offset:offset+2])[0]
    offset += 2
    print(f"First API: key={api_key}, min={min_ver}, max={max_ver}")

    print("\n✓ Response format matches kafka-python expectations!")
else:
    print(f"Error: Unexpected structure. First value after correlation_id is {next_as_int16}")

sock.close()