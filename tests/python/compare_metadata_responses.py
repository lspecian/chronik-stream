#!/usr/bin/env python3
"""Compare metadata responses between real Kafka and chronik."""

import socket
import struct
import sys

def send_metadata_request(host, port, api_version=1):
    """Send metadata request and return raw response."""
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.connect((host, port))
    
    # Build metadata request
    request = b''
    request += struct.pack('>h', 3)   # API Key (Metadata)
    request += struct.pack('>h', api_version)  # API Version
    request += struct.pack('>i', 42)  # Correlation ID
    request += struct.pack('>h', 8)   # Client ID length
    request += b'kafkactl'            # Client ID
    
    # For v1+: topics array (-1 for all)
    if api_version >= 1:
        request += struct.pack('>i', -1)  # -1 means all topics
    
    # Send with length prefix
    length_prefix = struct.pack('>i', len(request))
    sock.sendall(length_prefix + request)
    
    # Read response length
    length_data = sock.recv(4)
    response_length = struct.unpack('>i', length_data)[0]
    
    # Read response
    response = b''
    while len(response) < response_length:
        chunk = sock.recv(min(4096, response_length - len(response)))
        if not chunk:
            break
        response += chunk
    
    sock.close()
    return response

def decode_string(data, offset):
    """Decode a string from the data."""
    if offset + 2 > len(data):
        return None, offset
    
    length = struct.unpack('>h', data[offset:offset+2])[0]
    offset += 2
    
    if length < 0:
        return None, offset
    
    if offset + length > len(data):
        return None, offset
        
    string_data = data[offset:offset+length].decode('utf-8', errors='replace')
    return string_data, offset + length

def analyze_metadata_response_v1(response, name):
    """Analyze metadata response v1 format."""
    print(f"\n=== {name} Metadata Response Analysis (v1) ===")
    print(f"Total length: {len(response)} bytes")
    
    offset = 0
    
    # Correlation ID
    if offset + 4 <= len(response):
        corr_id = struct.unpack('>i', response[offset:offset+4])[0]
        print(f"Correlation ID: {corr_id}")
        offset += 4
    
    # Check if there's a throttle time (v3+, but sometimes included)
    potential_throttle = None
    if offset + 4 <= len(response):
        potential_throttle = struct.unpack('>i', response[offset:offset+4])[0]
        # If it looks like a reasonable throttle time (0-10000ms), it might be throttle time
        if 0 <= potential_throttle <= 10000:
            print(f"Throttle time (unexpected for v1): {potential_throttle}ms")
            offset += 4
        else:
            # Reset, it's probably broker count
            potential_throttle = None
    
    # Brokers array
    if offset + 4 <= len(response):
        broker_count = struct.unpack('>i', response[offset:offset+4])[0]
        print(f"Broker count: {broker_count}")
        offset += 4
        
        for i in range(broker_count):
            if offset + 4 <= len(response):
                node_id = struct.unpack('>i', response[offset:offset+4])[0]
                offset += 4
                
                host, offset = decode_string(response, offset)
                
                if offset + 4 <= len(response):
                    port = struct.unpack('>i', response[offset:offset+4])[0]
                    offset += 4
                    
                    # Rack (v1+)
                    rack, offset = decode_string(response, offset)
                    
                    print(f"  Broker {i}: ID={node_id}, Host={host}, Port={port}, Rack={rack}")
    
    # Controller ID (v1+)
    if offset + 4 <= len(response):
        controller_id = struct.unpack('>i', response[offset:offset+4])[0]
        print(f"Controller ID: {controller_id}")
        offset += 4
    
    # Topics array
    if offset + 4 <= len(response):
        topic_count = struct.unpack('>i', response[offset:offset+4])[0]
        print(f"Topic count: {topic_count}")
        offset += 4
    
    print(f"\nBytes consumed: {offset}/{len(response)}")
    if offset < len(response):
        print(f"Remaining bytes: {response[offset:].hex()}")
    
    # Print hex dump of first 128 bytes
    print("\nHex dump (first 128 bytes):")
    for i in range(0, min(128, len(response)), 16):
        hex_str = ' '.join(f'{b:02x}' for b in response[i:i+16])
        ascii_str = ''.join(chr(b) if 32 <= b < 127 else '.' for b in response[i:i+16])
        print(f"{i:04x}: {hex_str:<48} {ascii_str}")

def main():
    # Test real Kafka
    print("Testing Real Kafka on localhost:9095...")
    try:
        kafka_response = send_metadata_request('localhost', 9095, api_version=1)
        analyze_metadata_response_v1(kafka_response, "Real Kafka")
    except Exception as e:
        print(f"Error connecting to real Kafka: {e}")
    
    # Test chronik
    print("\n\nTesting Chronik on localhost:9092...")
    try:
        chronik_response = send_metadata_request('localhost', 9092, api_version=1)
        analyze_metadata_response_v1(chronik_response, "Chronik")
    except Exception as e:
        print(f"Error connecting to chronik: {e}")

if __name__ == "__main__":
    main()