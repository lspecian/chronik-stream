#!/usr/bin/env python3
"""Test Sarama compatibility by simulating its behavior."""

import socket
import struct
import time

def read_exact(sock, n):
    """Read exactly n bytes from socket."""
    data = b''
    while len(data) < n:
        chunk = sock.recv(n - len(data))
        if not chunk:
            raise Exception(f"Connection closed, needed {n} bytes, got {len(data)}")
        data += chunk
    return data

def test_sarama_sequence():
    """Test the sequence of requests that Sarama sends."""
    
    print("1. Testing ApiVersions request...")
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.connect(('localhost', 9092))
    
    # Send ApiVersions request
    api_key = 18
    api_version = 0
    correlation_id = 1
    client_id = b'sarama'
    
    request = struct.pack('>h', api_key)
    request += struct.pack('>h', api_version)
    request += struct.pack('>i', correlation_id)
    request += struct.pack('>h', len(client_id))
    request += client_id
    
    sock.send(struct.pack('>I', len(request)))
    sock.send(request)
    print(f"   Sent ApiVersions request ({len(request) + 4} bytes)")
    
    # Read response
    resp_len = struct.unpack('>I', read_exact(sock, 4))[0]
    print(f"   Response length: {resp_len}")
    
    response = read_exact(sock, resp_len)
    print(f"   Received {len(response)} bytes")
    
    # Verify correlation ID is first
    corr_id = struct.unpack('>I', response[:4])[0]
    if corr_id != correlation_id:
        print(f"   ERROR: Expected correlation ID {correlation_id}, got {corr_id}")
        return
    
    print(f"   ✓ Correlation ID matches: {corr_id}")
    
    # Parse error code
    error_code = struct.unpack('>h', response[4:6])[0]
    print(f"   Error code: {error_code}")
    
    if error_code != 0:
        print(f"   ERROR: Non-zero error code")
        return
        
    print("\n2. Testing Metadata request...")
    
    # Send Metadata request for all topics
    api_key = 3  # Metadata
    api_version = 1
    correlation_id = 2
    
    request = struct.pack('>h', api_key)
    request += struct.pack('>h', api_version)
    request += struct.pack('>i', correlation_id)
    request += struct.pack('>h', len(client_id))
    request += client_id
    # Topics array (-1 for all topics)
    request += struct.pack('>i', -1)
    
    sock.send(struct.pack('>I', len(request)))
    sock.send(request)
    print(f"   Sent Metadata request ({len(request) + 4} bytes)")
    
    # Read response
    resp_len = struct.unpack('>I', read_exact(sock, 4))[0]
    print(f"   Response length: {resp_len}")
    
    response = read_exact(sock, resp_len)
    print(f"   Received {len(response)} bytes")
    
    # Verify correlation ID
    corr_id = struct.unpack('>I', response[:4])[0]
    if corr_id != correlation_id:
        print(f"   ERROR: Expected correlation ID {correlation_id}, got {corr_id}")
        return
    
    print(f"   ✓ Correlation ID matches: {corr_id}")
    
    # Parse response
    offset = 4
    # Skip throttle_time_ms for v1
    # No throttle time in v1
    
    # Brokers array
    broker_count = struct.unpack('>i', response[offset:offset+4])[0]
    offset += 4
    print(f"   Broker count: {broker_count}")
    
    for i in range(broker_count):
        node_id = struct.unpack('>i', response[offset:offset+4])[0]
        offset += 4
        
        host_len = struct.unpack('>h', response[offset:offset+2])[0]
        offset += 2
        host = response[offset:offset+host_len].decode('utf-8')
        offset += host_len
        
        port = struct.unpack('>i', response[offset:offset+4])[0]
        offset += 4
        
        # Rack (v1+)
        rack_len = struct.unpack('>h', response[offset:offset+2])[0]
        offset += 2
        if rack_len > 0:
            rack = response[offset:offset+rack_len].decode('utf-8')
            offset += rack_len
        else:
            rack = None
            
        print(f"   Broker {i}: node_id={node_id}, host={host}, port={port}, rack={rack}")
    
    # Controller ID (v1+)
    controller_id = struct.unpack('>i', response[offset:offset+4])[0]
    offset += 4
    print(f"   Controller ID: {controller_id}")
    
    print("\n✓ All tests passed!")
    sock.close()

if __name__ == "__main__":
    try:
        test_sarama_sequence()
    except Exception as e:
        print(f"ERROR: {e}")
        import traceback
        traceback.print_exc()