#!/usr/bin/env python3
"""Test a simple valid request that should work."""

import socket
import struct

sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
sock.settimeout(5)
sock.connect(('localhost', 9092))

# Send metadata request with a client ID to ensure it's >= 14 bytes
request = b''
request += struct.pack('>h', 3)   # API Key (Metadata)
request += struct.pack('>h', 1)   # API Version 1
request += struct.pack('>i', 1)   # Correlation ID
request += struct.pack('>h', 4)   # Client ID length
request += b'test'                # Client ID
request += struct.pack('>i', -1)  # Topics: -1 means all topics

# Send with length prefix
length_prefix = struct.pack('>i', len(request))
full_request = length_prefix + request
print(f"Sending {len(full_request)} bytes (frame: {len(request)} bytes)")
sock.sendall(full_request)

# Try to read response
try:
    length_data = sock.recv(4)
    print(f"Received {len(length_data)} bytes for length")
    
    if len(length_data) == 4:
        response_length = struct.unpack('>i', length_data)[0]
        print(f"Response length: {response_length}")
        
        # Read response
        response = b''
        while len(response) < response_length:
            chunk = sock.recv(response_length - len(response))
            if not chunk:
                break
            response += chunk
        
        print(f"Response ({len(response)} bytes)")
        
        # Parse response
        if len(response) >= 4:
            corr_id = struct.unpack('>i', response[0:4])[0]
            print(f"Correlation ID: {corr_id}")
            
            # For metadata v1, next should be broker count (no throttle_time)
            if len(response) >= 8:
                broker_count = struct.unpack('>i', response[4:8])[0]
                print(f"Broker count: {broker_count}")
except socket.timeout:
    print("Socket timeout!")
except Exception as e:
    print(f"Error: {e}")
finally:
    sock.close()