#!/usr/bin/env python3
"""Test metadata response in detail."""

import socket
import struct

def read_exact(sock, n):
    """Read exactly n bytes from socket."""
    data = b''
    while len(data) < n:
        chunk = sock.recv(n - len(data))
        if not chunk:
            raise Exception(f"Connection closed, needed {n} bytes, got {len(data)}")
        data += chunk
    return data

def test_metadata():
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.connect(('localhost', 9092))
    
    # Send Metadata request
    api_key = 3
    api_version = 1
    correlation_id = 1
    client_id = b'test'
    
    request = struct.pack('>h', api_key)
    request += struct.pack('>h', api_version)
    request += struct.pack('>i', correlation_id)
    request += struct.pack('>h', len(client_id))
    request += client_id
    request += struct.pack('>i', -1)  # All topics
    
    sock.send(struct.pack('>I', len(request)))
    sock.send(request)
    
    # Read response
    resp_len = struct.unpack('>I', read_exact(sock, 4))[0]
    response = read_exact(sock, resp_len)
    
    print(f"Response length: {resp_len}")
    print(f"Response hex: {response.hex()}")
    
    # Parse manually
    offset = 0
    
    # Correlation ID
    corr_id = struct.unpack('>I', response[offset:offset+4])[0]
    offset += 4
    print(f"\nCorrelation ID: {corr_id}")
    
    # Broker count
    broker_count = struct.unpack('>i', response[offset:offset+4])[0]
    offset += 4
    print(f"Broker count: {broker_count}")
    
    for i in range(broker_count):
        # Node ID
        node_id = struct.unpack('>i', response[offset:offset+4])[0]
        offset += 4
        
        # Host length
        host_len = struct.unpack('>h', response[offset:offset+2])[0]
        offset += 2
        
        # Host
        host = response[offset:offset+host_len].decode('utf-8')
        offset += host_len
        
        # Port
        port = struct.unpack('>i', response[offset:offset+4])[0]
        offset += 4
        
        # Rack length
        rack_len = struct.unpack('>h', response[offset:offset+2])[0]
        offset += 2
        
        print(f"Broker {i}: node_id={node_id}, host='{host}', port={port}, rack_len={rack_len}")
    
    # Controller ID
    controller_id = struct.unpack('>i', response[offset:offset+4])[0]
    offset += 4
    print(f"Controller ID: {controller_id}")
    
    # Topics array
    topic_count = struct.unpack('>i', response[offset:offset+4])[0]
    offset += 4
    print(f"Topic count: {topic_count}")
    
    sock.close()

if __name__ == "__main__":
    test_metadata()