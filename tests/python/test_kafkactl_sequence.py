#!/usr/bin/env python3
"""Simulate the exact sequence kafkactl uses."""

import socket
import struct
import time

def send_request(sock, api_key, api_version, correlation_id, client_id, body=b''):
    """Send a Kafka request."""
    request = b''
    request += struct.pack('>h', api_key)
    request += struct.pack('>h', api_version)
    request += struct.pack('>i', correlation_id)
    request += struct.pack('>h', len(client_id))
    request += client_id
    request += body
    
    length_prefix = struct.pack('>i', len(request))
    sock.sendall(length_prefix + request)
    
    # Read response
    length_data = sock.recv(4)
    if len(length_data) < 4:
        return None
        
    response_length = struct.unpack('>i', length_data)[0]
    response = b''
    while len(response) < response_length:
        chunk = sock.recv(min(4096, response_length - len(response)))
        if not chunk:
            break
        response += chunk
    
    return response

def main():
    """Test the sequence of requests kafkactl might make."""
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.connect(('localhost', 9092))
    
    client_id = b'kafkactl'
    
    # 1. First, kafkactl might send ApiVersions to negotiate versions
    print("1. Sending ApiVersions request...")
    response = send_request(sock, 18, 0, 1, client_id)
    if response:
        print(f"   Received {len(response)} bytes")
        # Just check if we got a response
        if len(response) >= 6:
            corr_id = struct.unpack('>i', response[0:4])[0]
            error_code = struct.unpack('>h', response[4:6])[0]
            print(f"   Correlation ID: {corr_id}, Error code: {error_code}")
    
    # 2. Then metadata request for all topics
    print("\n2. Sending Metadata request (all topics)...")
    # For API version 1: topic array with -1 for all topics
    metadata_body = struct.pack('>i', -1)  # -1 means all topics
    response = send_request(sock, 3, 1, 2, client_id, metadata_body)
    if response:
        print(f"   Received {len(response)} bytes")
        if len(response) >= 6:
            corr_id = struct.unpack('>i', response[0:4])[0]
            # Parse brokers count at offset 4
            if len(response) >= 8:
                broker_count = struct.unpack('>i', response[4:8])[0]
                print(f"   Correlation ID: {corr_id}, Broker count: {broker_count}")
    
    # 3. Try a newer metadata version
    print("\n3. Sending Metadata request v9 (with topic state filter)...")
    # For v9: topics array (-1), allow_auto_topic_creation (true), 
    # include_cluster_authorized_operations (false), include_topic_authorized_operations (false)
    metadata_body = struct.pack('>i', -1)  # -1 for all topics
    metadata_body += struct.pack('b', 1)   # allow_auto_topic_creation = true
    metadata_body += struct.pack('b', 0)   # include_cluster_authorized_operations = false
    metadata_body += struct.pack('b', 0)   # include_topic_authorized_operations = false
    
    response = send_request(sock, 3, 9, 3, client_id, metadata_body)
    if response:
        print(f"   Received {len(response)} bytes")
        print(f"   First 32 bytes hex: {response[:32].hex()}")
    
    sock.close()

if __name__ == "__main__":
    main()