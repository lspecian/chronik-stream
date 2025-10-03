#!/usr/bin/env python3
import socket
import struct
import time

# Create a simple ApiVersions request v0
request = bytes.fromhex(
    '00000012'  # Length: 18
    '0012'      # API Key: 18 (ApiVersions)
    '0000'      # API Version: 0
    '00000001'  # Correlation ID: 1
    '0006'      # Client ID length: 6
    '746573746572'  # Client ID: "tester"
)

sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
sock.connect(('localhost', 9095))
sock.settimeout(2)

print("Sending request...")
sock.sendall(request)

print("Waiting for response...")
try:
    # Read the response length
    response_len_bytes = sock.recv(4)
    print(f"Got response length bytes: {response_len_bytes.hex()}")

    if len(response_len_bytes) == 4:
        response_len = struct.unpack('>I', response_len_bytes)[0]
        print(f"Response length: {response_len}")

        # Read the response
        response_data = sock.recv(response_len)
        print(f"Response data ({len(response_data)} bytes): {response_data.hex()}")
    else:
        print(f"Failed to read full response length, got {len(response_len_bytes)} bytes")
except socket.timeout:
    print("Timeout waiting for response")
except Exception as e:
    print(f"Error: {e}")

sock.close()