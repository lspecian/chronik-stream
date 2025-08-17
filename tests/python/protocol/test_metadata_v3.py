#!/usr/bin/env python3
"""Test metadata v3 request which should include throttle_time_ms."""

import socket
import struct

def test_v3():
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.connect(('localhost', 9092))
    
    # Build metadata request for v3
    request = b''
    request += struct.pack('>h', 3)      # API key (Metadata)
    request += struct.pack('>h', 3)      # API version 3 (should include throttle_time_ms)
    request += struct.pack('>i', 100)    # Correlation ID
    request += struct.pack('>h', -1)     # Client ID (null)
    request += struct.pack('>i', -1)     # Topics array (-1 = all)
    request += struct.pack('b', 1)       # allow_auto_topic_creation (v3+)
    
    # Send
    full_request = struct.pack('>I', len(request)) + request
    print(f"Sending v3 request ({len(full_request)} bytes)")
    sock.send(full_request)
    
    # Receive
    resp_len = struct.unpack('>I', sock.recv(4))[0]
    response = sock.recv(resp_len)
    
    print(f"\nResponse for v3 ({resp_len} bytes):")
    
    offset = 0
    corr_id = struct.unpack('>I', response[offset:offset+4])[0]
    print(f"Correlation ID: {corr_id}")
    offset += 4
    
    # For v3, throttle_time_ms SHOULD be here
    throttle_time = struct.unpack('>i', response[offset:offset+4])[0]
    print(f"Throttle time ms: {throttle_time}")
    offset += 4
    
    # Then brokers
    broker_count = struct.unpack('>i', response[offset:offset+4])[0]
    print(f"Broker count: {broker_count}")
    
    sock.close()

if __name__ == "__main__":
    test_v3()