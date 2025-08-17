#!/usr/bin/env python3
"""Test Chronik Stream end-to-end functionality."""

import socket
import struct
import time
import json

def encode_string(s):
    """Encode a string for Kafka protocol (length + data)."""
    if s is None:
        return struct.pack('>h', -1)
    data = s.encode('utf-8')
    return struct.pack('>h', len(data)) + data

def create_metadata_request():
    """Create a simple Metadata request."""
    # Request header
    api_key = 3  # Metadata
    api_version = 1  # Simple version
    correlation_id = 1
    client_id = encode_string("test-client")
    
    # Request body - empty topics array means all topics
    topics = struct.pack('>i', 0)  # Empty array
    
    # Build request
    header = struct.pack('>hhI', api_key, api_version, correlation_id)
    request = header + client_id + topics
    
    # Add length prefix
    return struct.pack('>I', len(request)) + request

def read_response(sock):
    """Read a response from the socket."""
    # Read length
    length_bytes = sock.recv(4)
    if len(length_bytes) < 4:
        raise Exception("Failed to read response length")
    
    length = struct.unpack('>I', length_bytes)[0]
    print(f"Response length: {length}")
    
    # Read response
    response = b''
    while len(response) < length:
        chunk = sock.recv(length - len(response))
        if not chunk:
            raise Exception("Connection closed while reading response")
        response += chunk
    
    return response

def decode_string(data, offset):
    """Decode a string from data."""
    if offset + 2 > len(data):
        return None, offset
    
    length = struct.unpack('>h', data[offset:offset+2])[0]
    offset += 2
    
    if length < 0:
        return None, offset
    
    if offset + length > len(data):
        return None, offset
    
    string = data[offset:offset+length].decode('utf-8')
    return string, offset + length

def parse_metadata_response(response):
    """Parse metadata response."""
    offset = 0
    
    # Correlation ID
    correlation_id = struct.unpack('>I', response[offset:offset+4])[0]
    offset += 4
    print(f"Correlation ID: {correlation_id}")
    
    # Brokers array
    broker_count = struct.unpack('>i', response[offset:offset+4])[0]
    offset += 4
    print(f"\nBrokers ({broker_count}):")
    
    for i in range(broker_count):
        node_id = struct.unpack('>i', response[offset:offset+4])[0]
        offset += 4
        
        host, offset = decode_string(response, offset)
        port = struct.unpack('>i', response[offset:offset+4])[0]
        offset += 4
        
        print(f"  Broker {node_id}: {host}:{port}")
    
    # Controller ID (v1+)
    if offset + 4 <= len(response):
        controller_id = struct.unpack('>i', response[offset:offset+4])[0]
        offset += 4
        print(f"\nController ID: {controller_id}")
    
    # Topics array
    topic_count = struct.unpack('>i', response[offset:offset+4])[0]
    offset += 4
    print(f"\nTopics ({topic_count}):")
    
    for i in range(topic_count):
        error_code = struct.unpack('>h', response[offset:offset+2])[0]
        offset += 2
        
        topic_name, offset = decode_string(response, offset)
        is_internal = struct.unpack('>b', response[offset:offset+1])[0]
        offset += 1
        
        print(f"\n  Topic: {topic_name}")
        print(f"    Error: {error_code}")
        print(f"    Internal: {bool(is_internal)}")
        
        # Partitions
        partition_count = struct.unpack('>i', response[offset:offset+4])[0]
        offset += 4
        print(f"    Partitions ({partition_count}):")
        
        for j in range(partition_count):
            p_error_code = struct.unpack('>h', response[offset:offset+2])[0]
            offset += 2
            
            partition_id = struct.unpack('>i', response[offset:offset+4])[0]
            offset += 4
            
            leader = struct.unpack('>i', response[offset:offset+4])[0]
            offset += 4
            
            # Replicas array
            replica_count = struct.unpack('>i', response[offset:offset+4])[0]
            offset += 4
            replicas = []
            for k in range(replica_count):
                replica = struct.unpack('>i', response[offset:offset+4])[0]
                offset += 4
                replicas.append(replica)
            
            # ISR array
            isr_count = struct.unpack('>i', response[offset:offset+4])[0]
            offset += 4
            isr = []
            for k in range(isr_count):
                node = struct.unpack('>i', response[offset:offset+4])[0]
                offset += 4
                isr.append(node)
            
            print(f"      Partition {partition_id}: leader={leader}, replicas={replicas}, isr={isr}, error={p_error_code}")

def main():
    """Test Chronik Stream functionality."""
    host = 'localhost'
    port = 9092
    
    print(f"Testing Chronik Stream at {host}:{port}\n")
    
    # Test 1: Check metadata
    print("1. Testing Metadata request...")
    try:
        with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
            sock.settimeout(5)  # 5 second timeout
            sock.connect((host, port))
            print("Connected!")
            
            # Send metadata request
            request = create_metadata_request()
            print(f"Sending request ({len(request)} bytes)...")
            sock.sendall(request)
            
            # Read response
            response = read_response(sock)
            print(f"Received response ({len(response)} bytes)\n")
            
            # Parse response
            parse_metadata_response(response)
            
    except Exception as e:
        print(f"Error: {e}")
        return
    
    print("\n✓ Chronik Stream is responding to Kafka protocol requests!")
    
    # Test 2: Check if there are any topics
    print("\n2. Checking available topics...")
    # Topics were listed in metadata response above
    
    print("\nNote: To create topics and produce/consume messages, the metadata store must be properly initialized.")
    print("The 'events' topic should be created by the produce handler when first used.")

if __name__ == "__main__":
    main()