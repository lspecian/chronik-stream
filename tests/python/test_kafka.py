#!/usr/bin/env python3
"""Test Kafka protocol implementation with direct socket communication."""

import socket
import struct
import time

def encode_string(s):
    """Encode a string as Kafka compact string (UNSIGNED_VARINT length + UTF-8 bytes + 1)."""
    if s is None:
        return b'\x00'  # null string
    
    data = s.encode('utf-8')
    length = len(data) + 1  # +1 for compact string
    
    # Encode length as unsigned varint
    length_bytes = b''
    while length > 127:
        length_bytes += bytes([(length & 0x7F) | 0x80])
        length >>= 7
    length_bytes += bytes([length])
    
    return length_bytes + data

def create_api_versions_request():
    """Create an ApiVersions request."""
    # Request header
    api_key = 18  # ApiVersions
    api_version = 3
    correlation_id = 1
    client_id = encode_string("test-client")
    
    # Tagged fields (empty)
    tagged_fields = b'\x00'
    
    # Build request
    header = struct.pack('>hhI', api_key, api_version, correlation_id)
    request = header + client_id + tagged_fields
    
    # Add length prefix
    return struct.pack('>I', len(request)) + request

def create_metadata_request():
    """Create a Metadata request."""
    # Request header
    api_key = 3  # Metadata
    api_version = 9
    correlation_id = 2
    client_id = encode_string("test-client")
    
    # Tagged fields (empty)
    tagged_fields = b'\x00'
    
    # Request body
    # Topics array (null = all topics)
    topics = b'\x00'  # null array
    
    # Allow auto topic creation
    allow_auto_topic_creation = b'\x01'  # true
    
    # Include cluster authorized operations
    include_cluster_authorized_operations = b'\x00'  # false
    
    # Include topic authorized operations  
    include_topic_authorized_operations = b'\x00'  # false
    
    # Tagged fields
    body_tagged_fields = b'\x00'
    
    body = topics + allow_auto_topic_creation + include_cluster_authorized_operations + include_topic_authorized_operations + body_tagged_fields
    
    # Build request
    header = struct.pack('>hhI', api_key, api_version, correlation_id)
    request = header + client_id + tagged_fields + body
    
    # Add length prefix
    return struct.pack('>I', len(request)) + request

def read_response(sock):
    """Read a response from the socket."""
    # Read length
    length_bytes = sock.recv(4)
    if len(length_bytes) < 4:
        raise Exception("Failed to read response length")
    
    length = struct.unpack('>I', length_bytes)[0]
    
    # Read response
    response = b''
    while len(response) < length:
        chunk = sock.recv(length - len(response))
        if not chunk:
            raise Exception("Connection closed while reading response")
        response += chunk
    
    return response

def main():
    """Test the Kafka protocol implementation."""
    host = 'localhost'
    port = 9092
    
    print(f"Connecting to {host}:{port}...")
    
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
        sock.connect((host, port))
        print("Connected!")
        
        # Test 1: ApiVersions request
        print("\nSending ApiVersions request...")
        request = create_api_versions_request()
        sock.sendall(request)
        
        try:
            response = read_response(sock)
            print(f"Received ApiVersions response: {len(response)} bytes")
            
            # Parse correlation ID
            correlation_id = struct.unpack('>I', response[:4])[0]
            print(f"Correlation ID: {correlation_id}")
            
            # Check if we have tagged fields byte
            if len(response) > 4:
                # Skip tagged fields byte
                offset = 5
                
                # Parse error code
                if len(response) > offset + 2:
                    error_code = struct.unpack('>h', response[offset:offset+2])[0]
                    print(f"Error code: {error_code}")
        except Exception as e:
            print(f"Error reading ApiVersions response: {e}")
        
        # Test 2: Metadata request
        print("\nSending Metadata request...")
        request = create_metadata_request()
        sock.sendall(request)
        
        try:
            response = read_response(sock)
            print(f"Received Metadata response: {len(response)} bytes")
            
            # Parse correlation ID
            correlation_id = struct.unpack('>I', response[:4])[0]
            print(f"Correlation ID: {correlation_id}")
        except Exception as e:
            print(f"Error reading Metadata response: {e}")

if __name__ == "__main__":
    main()