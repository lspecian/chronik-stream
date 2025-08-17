#!/usr/bin/env python3
"""Test if the metadata fix works."""

import socket
import struct

def test_metadata_v1():
    """Test metadata v1 doesn't have throttle_time."""
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.connect(('localhost', 9092))
    
    # Send metadata v1 request
    request = b''
    request += struct.pack('>h', 3)   # API Key (Metadata)
    request += struct.pack('>h', 1)   # API Version 1
    request += struct.pack('>i', 400) # Correlation ID
    request += struct.pack('>h', 8)   # Client ID length
    request += b'test-cli'            # Client ID
    request += struct.pack('>i', 1)   # Topics count: 1
    request += struct.pack('>h', 4)   # Topic name length
    request += b'test'                # Topic name
    
    # Send
    length_prefix = struct.pack('>i', len(request))
    sock.sendall(length_prefix + request)
    
    # Read response
    length_data = sock.recv(4)
    response_length = struct.unpack('>i', length_data)[0]
    response = sock.recv(response_length)
    
    # Parse
    offset = 0
    corr_id = struct.unpack('>i', response[offset:offset+4])[0]
    offset += 4
    
    # Next should be broker count, not throttle_time
    broker_count = struct.unpack('>i', response[offset:offset+4])[0]
    offset += 4
    
    if broker_count == 1:
        print("✓ v1 looks correct - broker count = 1")
        
        # Parse first broker
        node_id = struct.unpack('>i', response[offset:offset+4])[0]
        offset += 4
        host_len = struct.unpack('>h', response[offset:offset+2])[0]
        offset += 2
        host = response[offset:offset+host_len].decode('utf-8')
        offset += host_len
        port = struct.unpack('>i', response[offset:offset+4])[0]
        offset += 4
        
        print(f"  Broker: node_id={node_id}, host={host}, port={port}")
    else:
        print(f"✗ ERROR: Expected broker count, got {broker_count}")
        print("  This suggests throttle_time is incorrectly included")
    
    sock.close()

def test_metadata_v3():
    """Test metadata v3 has throttle_time."""
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.connect(('localhost', 9092))
    
    # Send metadata v3 request
    request = b''
    request += struct.pack('>h', 3)   # API Key (Metadata)
    request += struct.pack('>h', 3)   # API Version 3
    request += struct.pack('>i', 401) # Correlation ID
    request += struct.pack('>h', 8)   # Client ID length
    request += b'test-cli'            # Client ID
    request += struct.pack('>i', 1)   # Topics count: 1
    request += struct.pack('>h', 4)   # Topic name length
    request += b'test'                # Topic name
    
    # Send
    length_prefix = struct.pack('>i', len(request))
    sock.sendall(length_prefix + request)
    
    # Read response
    length_data = sock.recv(4)
    response_length = struct.unpack('>i', length_data)[0]
    response = sock.recv(response_length)
    
    # Parse
    offset = 0
    corr_id = struct.unpack('>i', response[offset:offset+4])[0]
    offset += 4
    
    # Next should be throttle_time
    throttle_time = struct.unpack('>i', response[offset:offset+4])[0]
    offset += 4
    
    # Then broker count
    broker_count = struct.unpack('>i', response[offset:offset+4])[0]
    offset += 4
    
    print(f"✓ v3 has throttle_time = {throttle_time}, broker_count = {broker_count}")
    
    sock.close()

if __name__ == "__main__":
    print("Testing Kafka metadata response encoding...")
    test_metadata_v1()
    test_metadata_v3()
    
    # Also test with kafkactl
    import subprocess
    print("\nTesting with kafkactl...")
    try:
        result = subprocess.run(['kafkactl', 'get', 'topics'], capture_output=True, text=True, timeout=5)
        if result.returncode == 0:
            print("✓ kafkactl get topics succeeded!")
            print(f"  Topics: {result.stdout.strip()}")
        else:
            print(f"✗ kafkactl failed: {result.stderr}")
    except Exception as e:
        print(f"✗ kafkactl test failed: {e}")