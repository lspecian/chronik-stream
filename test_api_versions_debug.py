#!/usr/bin/env python3
"""Debug API versions response structure."""

import socket
import struct

sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
sock.settimeout(5)
sock.connect(('localhost', 9092))

# Send API versions request v0
request = b''
request += struct.pack('>h', 18)  # API Key (ApiVersions)
request += struct.pack('>h', 0)   # API Version 0
request += struct.pack('>i', 100) # Correlation ID (use 100 to track)
request += struct.pack('>h', 7)   # Client ID length
request += b'sarama'              # Client ID

# Send with length prefix
length_prefix = struct.pack('>i', len(request))
full_request = length_prefix + request
print(f"Request ({len(full_request)} bytes): {full_request.hex()}")
sock.sendall(full_request)

# Read response
try:
    length_data = sock.recv(4)
    print(f"Response length bytes: {length_data.hex()}")
    
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
        
        print(f"\nResponse ({len(response)} bytes): {response.hex()}")
        
        # Parse response header + body
        offset = 0
        
        # Response header - just correlation ID
        corr_id = struct.unpack('>i', response[offset:offset+4])[0]
        offset += 4
        print(f"\nResponse Header:")
        print(f"  Correlation ID: {corr_id} (expected 100)")
        
        # Response body
        print(f"\nResponse Body (starting at offset {offset}):")
        
        # Should be error_code first
        if offset < len(response):
            error_code = struct.unpack('>h', response[offset:offset+2])[0]
            offset += 2
            print(f"  Error code: {error_code}")
            
            # Then api_versions array
            if offset < len(response) and error_code == 0:
                api_count = struct.unpack('>i', response[offset:offset+4])[0]
                offset += 4
                print(f"  API count: {api_count}")
                
                # Show first few APIs
                for i in range(min(3, api_count)):
                    if offset + 6 <= len(response):
                        api_key = struct.unpack('>h', response[offset:offset+2])[0]
                        offset += 2
                        min_ver = struct.unpack('>h', response[offset:offset+2])[0]
                        offset += 2
                        max_ver = struct.unpack('>h', response[offset:offset+2])[0]
                        offset += 2
                        print(f"    API {api_key}: v{min_ver}-{max_ver}")
                        
except socket.timeout:
    print("Socket timeout!")
except Exception as e:
    print(f"Error: {e}")
finally:
    sock.close()