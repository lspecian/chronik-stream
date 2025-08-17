#!/usr/bin/env python3
"""Test metadata request/response with chronik."""

import socket
import struct
import sys

def send_metadata_request(host='localhost', port=9092):
    """Send a metadata request and print the response."""
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.connect((host, port))
    
    # Build metadata request
    # API Key: 3 (Metadata)
    # API Version: 1
    # Correlation ID: 1
    # Topics: -1 (all topics)
    
    request = b''
    request += struct.pack('>h', 3)  # API Key
    request += struct.pack('>h', 1)  # API Version
    request += struct.pack('>i', 1)  # Correlation ID
    request += struct.pack('>h', 0)  # Client ID length (null)
    request += struct.pack('>i', -1) # Topics array: -1 means all topics
    
    # Send with length prefix
    length_prefix = struct.pack('>i', len(request))
    sock.sendall(length_prefix + request)
    
    print(f"Sent metadata request ({len(request)} bytes)")
    
    # Read response length
    length_data = sock.recv(4)
    if len(length_data) < 4:
        print("Failed to read response length")
        return
        
    response_length = struct.unpack('>i', length_data)[0]
    print(f"Response length: {response_length} bytes")
    
    # Read response
    response = b''
    while len(response) < response_length:
        chunk = sock.recv(min(4096, response_length - len(response)))
        if not chunk:
            break
        response += chunk
    
    print(f"Received {len(response)} bytes")
    
    # Parse response header
    if len(response) >= 4:
        correlation_id = struct.unpack('>i', response[0:4])[0]
        print(f"Correlation ID: {correlation_id}")
        
        # Print hex dump
        print("\nHex dump of response:")
        for i in range(0, min(len(response), 256), 16):
            hex_str = ' '.join(f'{b:02x}' for b in response[i:i+16])
            ascii_str = ''.join(chr(b) if 32 <= b < 127 else '.' for b in response[i:i+16])
            print(f"{i:04x}: {hex_str:<48} {ascii_str}")
    
    sock.close()

if __name__ == "__main__":
    send_metadata_request()