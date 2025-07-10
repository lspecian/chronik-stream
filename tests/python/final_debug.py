#!/usr/bin/env python3
"""Final attempt to understand the issue."""

import socket
import struct

# Theory: Maybe the issue is that something is adding 4 bytes
# Let's see if the response length includes or excludes the mysterious bytes

def test_response_length():
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.connect(('localhost', 9092))
    
    # Send v1 request  
    request = b''
    request += struct.pack('>h', 3)      # Metadata
    request += struct.pack('>h', 1)      # Version 1
    request += struct.pack('>i', 9999)   # Correlation ID
    request += struct.pack('>h', -1)     # Client ID null
    request += struct.pack('>i', -1)     # Topics all
    
    full_request = struct.pack('>I', len(request)) + request
    sock.send(full_request)
    
    # Read response
    resp_len_bytes = sock.recv(4)
    resp_len = struct.unpack('>I', resp_len_bytes)[0]
    print(f"Response length field says: {resp_len} bytes")
    
    response = sock.recv(resp_len)
    print(f"Actually received: {len(response)} bytes")
    
    # Now let's decode it two ways:
    print("\nDecoding as if NO throttle_time_ms (correct for v1):")
    offset = 0
    corr_id = struct.unpack('>I', response[offset:offset+4])[0]
    offset += 4
    print(f"  Correlation ID: {corr_id}")
    
    # Try to read broker count directly
    broker_count = struct.unpack('>i', response[offset:offset+4])[0]
    print(f"  Broker count: {broker_count}")
    
    print("\nDecoding as if throttle_time_ms exists (wrong for v1):")
    offset = 0
    corr_id = struct.unpack('>I', response[offset:offset+4])[0]
    offset += 4
    print(f"  Correlation ID: {corr_id}")
    
    throttle = struct.unpack('>i', response[offset:offset+4])[0]
    offset += 4
    print(f"  Throttle time: {throttle}")
    
    broker_count = struct.unpack('>i', response[offset:offset+4])[0]
    print(f"  Broker count: {broker_count}")
    
    print("\nConclusion: The response DOES contain throttle_time_ms for v1 when it shouldn't!")
    
    sock.close()

test_response_length()