#!/usr/bin/env python3
"""Test the exact sequence kafkactl uses"""

import socket
import struct
import time

def send_request(sock, api_key, api_version, correlation_id, client_id, body=b''):
    """Send a Kafka request"""
    # Build request header
    request = struct.pack('>hhi', api_key, api_version, correlation_id)
    
    # Add client ID
    if client_id:
        request += struct.pack('>h', len(client_id))
        request += client_id.encode('utf-8')
    else:
        request += struct.pack('>h', -1)  # null client ID
    
    # Add body
    request += body
    
    # Send with length prefix
    sock.send(struct.pack('>i', len(request)) + request)

def read_response(sock):
    """Read a Kafka response"""
    # Read length
    length_bytes = sock.recv(4)
    if len(length_bytes) < 4:
        return None
    
    length = struct.unpack('>i', length_bytes)[0]
    
    # Read response
    response = b''
    while len(response) < length:
        chunk = sock.recv(length - len(response))
        if not chunk:
            break
        response += chunk
    
    return response

def test_kafkactl_sequence():
    """Test the sequence kafkactl uses"""
    s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    s.connect(('localhost', 9092))
    
    print("Connected to Chronik...")
    
    # 1. Send API versions request (what kafkactl does first)
    print("\n1. Sending API versions request (correlation_id=0)")
    send_request(s, api_key=18, api_version=3, correlation_id=0, client_id="sarama")
    
    response = read_response(s)
    if response:
        correlation_id = struct.unpack('>i', response[:4])[0]
        print(f"   Response correlation ID: {correlation_id}")
        print(f"   Response starts with: {response[:20].hex()}")
    
    # 2. Send metadata request (what kafkactl does next)
    print("\n2. Sending metadata request (correlation_id=1)")
    # Metadata request body for v9 (empty topics array)
    metadata_body = struct.pack('>b', 0)  # topics array (null)
    metadata_body += struct.pack('>b', 1)  # allow_auto_topic_creation
    metadata_body += struct.pack('>b', 1)  # include_cluster_authorized_operations
    metadata_body += struct.pack('>b', 1)  # include_topic_authorized_operations
    
    send_request(s, api_key=3, api_version=9, correlation_id=1, client_id="sarama", body=metadata_body)
    
    response = read_response(s)
    if response:
        correlation_id = struct.unpack('>i', response[:4])[0]
        print(f"   Response correlation ID: {correlation_id}")
        print(f"   Response starts with: {response[:20].hex()}")
        
        # Check if this matches what kafkactl expects
        if correlation_id != 1:
            print(f"   ERROR: Expected correlation ID 1, got {correlation_id}")
    
    s.close()

if __name__ == "__main__":
    test_kafkactl_sequence()