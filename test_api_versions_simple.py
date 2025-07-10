#!/usr/bin/env python3
"""Simple API versions test."""

import socket
import struct

sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
sock.settimeout(5)
sock.connect(('localhost', 9092))

# Send API versions request
request = b''
request += struct.pack('>h', 18)  # API Key (ApiVersions)
request += struct.pack('>h', 0)   # API Version 0
request += struct.pack('>i', 1)   # Correlation ID
request += struct.pack('>h', 7)   # Client ID length
request += b'sarama'              # Client ID

# Send with length prefix
length_prefix = struct.pack('>i', len(request))
full_request = length_prefix + request
print(f"Sending {len(full_request)} bytes: {full_request.hex()}")
sock.sendall(full_request)

# Try to read response length
try:
    length_data = sock.recv(4)
    print(f"Received {len(length_data)} bytes for length: {length_data.hex()}")
    
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
        
        print(f"Response ({len(response)} bytes): {response.hex()}")
        
        # Parse response
        if len(response) >= 4:
            corr_id = struct.unpack('>i', response[0:4])[0]
            print(f"Correlation ID: {corr_id}")
            
            if len(response) >= 6:
                error_code = struct.unpack('>h', response[4:6])[0]
                print(f"Error code: {error_code}")
except socket.timeout:
    print("Socket timeout!")
except Exception as e:
    print(f"Error: {e}")
finally:
    sock.close()