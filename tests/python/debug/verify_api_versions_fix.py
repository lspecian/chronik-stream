#!/usr/bin/env python3
"""Verify API versions response structure after fix"""

import socket
import struct

def test_api_versions():
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.connect(('localhost', 9092))
    
    # Create API versions request v3
    # Header: api_key=18, version=3, correlation_id=100, client_id=null
    # Body: client_software_name=null, client_software_version=null, tagged_fields=0
    
    header = struct.pack('>hhiB', 18, 3, 100, 0)  # client_id=null (compact string)
    body = struct.pack('>BBB', 0, 0, 0)  # all nulls and empty tagged fields
    
    request = header + body
    length = struct.pack('>i', len(request))
    
    print(f"Sending request - Length: {len(request)}, Correlation ID: 100")
    sock.send(length + request)
    
    # Read response
    resp_len_data = sock.recv(4)
    resp_len = struct.unpack('>i', resp_len_data)[0]
    
    response = sock.recv(resp_len)
    print(f"\nResponse length: {resp_len}")
    
    # Parse response
    offset = 0
    
    # Correlation ID (first 4 bytes)
    corr_id = struct.unpack('>i', response[offset:offset+4])[0]
    print(f"Correlation ID: {corr_id}")
    offset += 4
    
    # Tagged fields (empty for response header in v3)
    tagged_fields_len = response[offset]
    print(f"Response header tagged fields: {tagged_fields_len}")
    offset += 1
    
    # Error code
    error_code = struct.unpack('>h', response[offset:offset+2])[0]
    print(f"Error code: {error_code}")
    offset += 2
    
    # API versions array (compact array)
    array_len = response[offset] - 1  # Compact array encoding
    print(f"API versions count: {array_len}")
    offset += 1
    
    print("\nSupported APIs:")
    for i in range(array_len):
        api_key = struct.unpack('>h', response[offset:offset+2])[0]
        offset += 2
        min_ver = struct.unpack('>h', response[offset:offset+2])[0]
        offset += 2
        max_ver = struct.unpack('>h', response[offset:offset+2])[0]
        offset += 2
        # Skip tagged fields for each API
        api_tagged = response[offset]
        offset += 1
        
        print(f"  API {api_key}: v{min_ver}-{max_ver}")
    
    # Throttle time
    throttle_time = struct.unpack('>i', response[offset:offset+4])[0]
    print(f"\nThrottle time: {throttle_time}ms")
    offset += 4
    
    # Response tagged fields
    resp_tagged = response[offset]
    print(f"Response tagged fields: {resp_tagged}")
    
    sock.close()

if __name__ == "__main__":
    test_api_versions()