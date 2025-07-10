#!/usr/bin/env python3
"""Test raw bytes to understand the issue."""

import socket
import struct

# Build request
request = b''
request += struct.pack('>h', 18)  # API Key (ApiVersions) - 2 bytes
request += struct.pack('>h', 0)   # API Version 0 - 2 bytes
request += struct.pack('>i', 100) # Correlation ID - 4 bytes
request += struct.pack('>h', 7)   # Client ID length - 2 bytes
request += b'sarama'              # Client ID - 7 bytes

print(f"Request without length prefix ({len(request)} bytes): {request.hex()}")

# Add length prefix
length_prefix = struct.pack('>i', len(request))
full_request = length_prefix + request

print(f"Full request with length prefix ({len(full_request)} bytes): {full_request.hex()}")
print(f"Length prefix value: {struct.unpack('>i', length_prefix)[0]}")

# Show byte breakdown
print("\nByte breakdown:")
print(f"  Bytes 0-3 (length): {full_request[0:4].hex()} = {struct.unpack('>i', full_request[0:4])[0]}")
print(f"  Bytes 4-5 (api_key): {full_request[4:6].hex()} = {struct.unpack('>h', full_request[4:6])[0]}")
print(f"  Bytes 6-7 (api_version): {full_request[6:8].hex()} = {struct.unpack('>h', full_request[6:8])[0]}")
print(f"  Bytes 8-11 (correlation_id): {full_request[8:12].hex()} = {struct.unpack('>i', full_request[8:12])[0]}")
print(f"  Bytes 12-13 (client_id_len): {full_request[12:14].hex()} = {struct.unpack('>h', full_request[12:14])[0]}")
print(f"  Bytes 14-20 (client_id): {full_request[14:21].hex()} = '{full_request[14:21].decode()}'")

# Send it
sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
sock.settimeout(5)
sock.connect(('localhost', 9092))
sock.sendall(full_request)
sock.close()