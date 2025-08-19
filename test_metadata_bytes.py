#!/usr/bin/env python3
"""Test MetadataResponse encoding by comparing with actual Kafka broker"""

import struct
import socket

def send_metadata_request(host='localhost', port=9092):
    """Send a Metadata request and analyze the response"""
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.connect((host, port))
    
    # Build Metadata request v1 (which Java client commonly uses)
    request = bytearray()
    request.extend(struct.pack('>h', 3))   # API key (3 = Metadata)
    request.extend(struct.pack('>h', 1))   # API version (1)
    request.extend(struct.pack('>i', 123)) # Correlation ID  
    request.extend(struct.pack('>h', -1))  # Client ID (-1 = null)
    request.extend(struct.pack('>i', -1))  # Topics array (-1 = all topics)
    request.extend(b'\x00')                # Allow auto topic creation (false for v4+)
    
    # Send request
    size = len(request)
    sock.send(struct.pack('>i', size))
    sock.send(request)
    
    # Read response
    size_bytes = sock.recv(4)
    if len(size_bytes) < 4:
        print("Connection closed")
        return
        
    size = struct.unpack('>i', size_bytes)[0]
    print(f"Response size: {size}")
    
    response = bytearray()
    while len(response) < size:
        chunk = sock.recv(min(4096, size - len(response)))
        if not chunk:
            break
        response.extend(chunk)
    
    print(f"Response bytes ({len(response)} total):")
    print(f"First 100 bytes: {response[:100].hex()}")
    
    # Parse response
    pos = 0
    
    # Correlation ID
    correlation_id = struct.unpack('>i', response[pos:pos+4])[0]
    pos += 4
    print(f"Correlation ID: {correlation_id}")
    
    # For v1, there's NO throttle_time_ms field!
    # Brokers array comes directly after correlation ID
    
    # Brokers array
    brokers_len = struct.unpack('>i', response[pos:pos+4])[0]
    print(f"Brokers array length: {brokers_len}")
    pos += 4
    
    if brokers_len > 0:
        # Parse first broker
        node_id = struct.unpack('>i', response[pos:pos+4])[0]
        pos += 4
        print(f"  Broker node ID: {node_id}")
        
        # Host string
        host_len = struct.unpack('>h', response[pos:pos+2])[0]
        pos += 2
        if host_len > 0:
            host = response[pos:pos+host_len].decode('utf-8')
            pos += host_len
            print(f"  Host: {host}")
        
        # Port
        port = struct.unpack('>i', response[pos:pos+4])[0]
        pos += 4
        print(f"  Port: {port}")
        
        # Rack (v1+)
        rack_len = struct.unpack('>h', response[pos:pos+2])[0]
        pos += 2
        if rack_len > 0:
            rack = response[pos:pos+rack_len].decode('utf-8')
            pos += rack_len
            print(f"  Rack: {rack}")
        else:
            print(f"  Rack: null")
    
    # Controller ID (v1+)
    controller_id = struct.unpack('>i', response[pos:pos+4])[0]
    pos += 4
    print(f"Controller ID: {controller_id}")
    
    # Topics array
    topics_len = struct.unpack('>i', response[pos:pos+4])[0]
    pos += 4
    print(f"Topics array length: {topics_len}")
    
    sock.close()

if __name__ == "__main__":
    send_metadata_request()