#!/usr/bin/env python3
"""Test creating a topic before producing messages"""

import struct
import socket

def encode_string(s):
    """Encode a string as Kafka protocol string (length + data)"""
    if s is None:
        return struct.pack('>h', -1)
    encoded = s.encode('utf-8')
    return struct.pack('>h', len(encoded)) + encoded

def send_create_topics_request(host, port, topic_name, num_partitions=1, replication_factor=1):
    """Send a CreateTopics request"""
    
    # Build create topics request body
    body = b''
    body += struct.pack('>i', 1)  # topic_count
    
    # Topic data
    body += encode_string(topic_name)
    body += struct.pack('>i', num_partitions)  # num_partitions
    body += struct.pack('>h', replication_factor)  # replication_factor
    body += struct.pack('>i', 0)  # replica_assignments count
    body += struct.pack('>i', 0)  # config_entries count
    
    body += struct.pack('>i', 30000)  # timeout_ms
    body += struct.pack('>b', 1)  # validate_only = false
    
    # Request header
    header = b''
    header += struct.pack('>h', 19)  # API key (CreateTopics)
    header += struct.pack('>h', 5)  # API version
    header += struct.pack('>i', 789)  # Correlation ID
    header += encode_string('test-client')  # Client ID
    
    # Full request
    request = header + body
    request_with_length = struct.pack('>i', len(request)) + request
    
    # Send request
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.connect((host, port))
    sock.send(request_with_length)
    
    # Read response
    response_length_data = sock.recv(4)
    response_length = struct.unpack('>i', response_length_data)[0]
    response_data = sock.recv(response_length)
    
    # Parse response header
    correlation_id = struct.unpack('>i', response_data[:4])[0]
    
    # Parse response body
    pos = 4
    throttle_time_ms = struct.unpack('>i', response_data[pos:pos+4])[0]
    pos += 4
    
    topic_count = struct.unpack('>i', response_data[pos:pos+4])[0]
    pos += 4
    
    results = []
    for _ in range(topic_count):
        topic_name_len = struct.unpack('>h', response_data[pos:pos+2])[0]
        pos += 2
        topic_name = response_data[pos:pos+topic_name_len].decode('utf-8')
        pos += topic_name_len
        
        error_code = struct.unpack('>h', response_data[pos:pos+2])[0]
        pos += 2
        
        # Skip error message if present
        error_msg_len = struct.unpack('>h', response_data[pos:pos+2])[0]
        pos += 2
        if error_msg_len > 0:
            pos += error_msg_len
            
        results.append({
            'topic': topic_name,
            'error_code': error_code
        })
    
    sock.close()
    return correlation_id, results

# Test creating a topic
print("Creating topic 'test-topic'...")
corr_id, results = send_create_topics_request('localhost', 9092, 'test-topic', 3, 1)
print(f"Correlation ID: {corr_id}")
for result in results:
    print(f"Topic: {result['topic']}, Error Code: {result['error_code']}")
    if result['error_code'] == 0:
        print("✓ Topic created successfully")
    elif result['error_code'] == 36:  # TOPIC_ALREADY_EXISTS
        print("✓ Topic already exists")
    else:
        print(f"✗ Error creating topic: {result['error_code']}")