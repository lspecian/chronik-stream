#!/usr/bin/env python3
"""Test exact kafkactl behavior."""

import socket
import struct
import subprocess
import time

def capture_kafkactl_request():
    """Run kafkactl and capture what it sends."""
    # Start a dummy server to capture kafkactl's request
    server = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    server.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    server.bind(('localhost', 9093))
    server.listen(1)
    
    # Run kafkactl in background
    proc = subprocess.Popen(['kafkactl', '--brokers', 'localhost:9093', 'get', 'topics'],
                           stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    
    # Accept connection
    conn, addr = server.accept()
    
    # Read length prefix
    length_data = conn.recv(4)
    if len(length_data) == 4:
        length = struct.unpack('>i', length_data)[0]
        print(f"Request length: {length}")
        
        # Read request
        request = conn.recv(length)
        print(f"Request bytes ({len(request)}): {request.hex()}")
        
        # Parse request header
        if len(request) >= 4:
            api_key = struct.unpack('>h', request[0:2])[0]
            api_version = struct.unpack('>h', request[2:4])[0]
            print(f"API Key: {api_key}, Version: {api_version}")
            
            if len(request) >= 8:
                corr_id = struct.unpack('>i', request[4:8])[0]
                print(f"Correlation ID: {corr_id}")
    
    conn.close()
    server.close()
    proc.kill()

def test_with_chronik():
    """Test the exact request with chronik."""
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.connect(('localhost', 9092))
    
    # Send API versions request exactly as kafkactl would
    # Based on Sarama library defaults:
    # API Key 18 (ApiVersions), Version 0, Correlation ID, Client ID
    request = b''
    request += struct.pack('>h', 18)  # API Key (ApiVersions)
    request += struct.pack('>h', 0)   # API Version 0
    request += struct.pack('>i', 1)   # Correlation ID
    request += struct.pack('>h', 7)   # Client ID length
    request += b'sarama'              # Client ID (Sarama is kafkactl's library)
    
    # Send with length prefix
    length_prefix = struct.pack('>i', len(request))
    print(f"\nSending to chronik: {(length_prefix + request).hex()}")
    sock.sendall(length_prefix + request)
    
    # Try to read response
    try:
        length_data = sock.recv(4)
        if len(length_data) == 4:
            response_length = struct.unpack('>i', length_data)[0]
            print(f"Response length: {response_length}")
            
            response = sock.recv(response_length)
            print(f"Response bytes ({len(response)}): {response[:20].hex()}...")
        else:
            print(f"Got incomplete length: {len(length_data)} bytes")
    except Exception as e:
        print(f"Error reading response: {e}")
    
    sock.close()

if __name__ == "__main__":
    print("=== Capturing kafkactl request ===")
    try:
        capture_kafkactl_request()
    except Exception as e:
        print(f"Capture failed: {e}")
    
    print("\n=== Testing with chronik ===")
    test_with_chronik()