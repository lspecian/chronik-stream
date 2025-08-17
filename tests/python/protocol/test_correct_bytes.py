#!/usr/bin/env python3
"""Test with correct byte count."""

import socket
import struct

# Build request  
request = b''
request += struct.pack('>h', 18)  # API Key (ApiVersions) - 2 bytes
request += struct.pack('>h', 0)   # API Version 0 - 2 bytes
request += struct.pack('>i', 100) # Correlation ID - 4 bytes
request += struct.pack('>h', 6)   # Client ID length - 2 bytes (sarama is 6 chars)
request += b'sarama'              # Client ID - 6 bytes

print(f"Request without length prefix ({len(request)} bytes): {request.hex()}")

# Add length prefix
length_prefix = struct.pack('>i', len(request))
full_request = length_prefix + request

print(f"Full request with length prefix ({len(full_request)} bytes): {full_request.hex()}")

# Send it
sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
sock.settimeout(5)
sock.connect(('localhost', 9092))
sock.sendall(full_request)

# Read response
try:
    length_data = sock.recv(4)
    if len(length_data) == 4:
        response_length = struct.unpack('>i', length_data)[0]
        print(f"\nResponse length: {response_length}")
        
        response = b''
        while len(response) < response_length:
            chunk = sock.recv(response_length - len(response))
            if not chunk:
                break
            response += chunk
        
        print(f"Response ({len(response)} bytes): {response.hex()}")
        
        # Parse
        offset = 0
        corr_id = struct.unpack('>i', response[offset:offset+4])[0]
        offset += 4
        print(f"  Correlation ID: {corr_id}")
        
        if offset < len(response):
            error_code = struct.unpack('>h', response[offset:offset+2])[0]
            offset += 2
            print(f"  Error code: {error_code}")
            
            if error_code == 0 and offset + 4 <= len(response):
                api_count = struct.unpack('>i', response[offset:offset+4])[0]
                offset += 4
                print(f"  API count: {api_count}")
                
                # Show APIs
                for i in range(min(5, api_count)):
                    if offset + 6 <= len(response):
                        api_key = struct.unpack('>h', response[offset:offset+2])[0]
                        offset += 2
                        min_ver = struct.unpack('>h', response[offset:offset+2])[0]
                        offset += 2  
                        max_ver = struct.unpack('>h', response[offset:offset+2])[0]
                        offset += 2
                        print(f"    API {api_key}: v{min_ver}-{max_ver}")
                        
except Exception as e:
    print(f"Error: {e}")
finally:
    sock.close()