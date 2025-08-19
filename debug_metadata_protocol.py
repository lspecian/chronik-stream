#!/usr/bin/env python3
"""Debug Metadata protocol structure for Kafka"""

import struct
import socket

def test_metadata_request():
    """Test sending a Metadata request to Chronik and analyze the response"""
    
    # Connect to Chronik
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.connect(('localhost', 9092))
    
    # Build Metadata request (API key 3, version 0)
    request = bytearray()
    
    # API key (3 = Metadata)
    request.extend(struct.pack('>h', 3))
    # API version (0)
    request.extend(struct.pack('>h', 0))
    # Correlation ID
    request.extend(struct.pack('>i', 1))
    # Client ID (-1 = null)
    request.extend(struct.pack('>h', -1))
    # Topics array (-1 = fetch all topics)
    request.extend(struct.pack('>i', -1))
    
    # Send request with size header
    size = len(request)
    sock.send(struct.pack('>i', size))
    sock.send(request)
    
    # Read response
    size_bytes = sock.recv(4)
    size = struct.unpack('>i', size_bytes)[0]
    print(f"Response size: {size}")
    
    response = sock.recv(size)
    print(f"Response bytes (first 100): {response[:100].hex()}")
    
    # Parse response
    pos = 0
    
    # Correlation ID
    correlation_id = struct.unpack('>i', response[pos:pos+4])[0]
    pos += 4
    print(f"Correlation ID: {correlation_id}")
    
    # For v0, there's NO throttle_time_ms field!
    # Brokers array should come next
    
    # Try to read brokers array length
    if pos < len(response):
        brokers_len_bytes = response[pos:pos+4]
        if len(brokers_len_bytes) == 4:
            brokers_len = struct.unpack('>i', brokers_len_bytes)[0]
            print(f"Brokers array length: {brokers_len}")
            pos += 4
            
            # Read first broker
            if brokers_len > 0 and pos + 4 <= len(response):
                node_id = struct.unpack('>i', response[pos:pos+4])[0]
                pos += 4
                print(f"First broker node ID: {node_id}")
                
                # Host string
                if pos + 2 <= len(response):
                    host_len = struct.unpack('>h', response[pos:pos+2])[0]
                    pos += 2
                    if host_len > 0 and pos + host_len <= len(response):
                        host = response[pos:pos+host_len].decode('utf-8')
                        pos += host_len
                        print(f"Host: {host}")
                
                # Port
                if pos + 4 <= len(response):
                    port = struct.unpack('>i', response[pos:pos+4])[0]
                    pos += 4
                    print(f"Port: {port}")
    
    sock.close()

if __name__ == "__main__":
    try:
        test_metadata_request()
    except Exception as e:
        print(f"Error: {e}")
        import traceback
        traceback.print_exc()