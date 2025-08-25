#!/usr/bin/env python3
"""Test persistent metadata functionality"""

import socket
import struct
import time
import sys

def encode_string(s):
    """Encode string in Kafka wire format"""
    if s is None:
        return struct.pack('>h', -1)
    data = s.encode('utf-8')
    return struct.pack('>h', len(data)) + data

def encode_bytes(b):
    """Encode bytes in Kafka wire format"""
    if b is None:
        return struct.pack('>i', -1)
    return struct.pack('>i', len(b)) + b

def send_produce_request(host, port, topic, partition, key, value):
    """Send a produce request"""
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.connect((host, port))
    
    # Build produce request (API key 0, version 2)
    api_key = 0
    api_version = 2
    correlation_id = 1
    client_id = "test-client"
    
    # Request header
    header = struct.pack('>hhI', api_key, api_version, correlation_id) + encode_string(client_id)
    
    # Produce request body
    acks = 1
    timeout_ms = 1000
    topic_count = 1
    
    # Topic data
    topic_data = encode_string(topic)
    partition_count = 1
    
    # Partition data
    partition_data = struct.pack('>i', partition)  # partition id
    
    # Record batch (simplified - single record)
    # This is complex - using legacy format for simplicity
    record_set = b''
    
    # Message set (legacy format)
    offset = 0
    message_size = 0
    
    # Message
    crc = 0
    magic = 0
    attributes = 0
    timestamp = -1
    
    key_bytes = encode_bytes(key.encode('utf-8') if key else None)
    value_bytes = encode_bytes(value.encode('utf-8') if value else None)
    
    message = struct.pack('>ibbq', crc, magic, attributes, timestamp) + key_bytes + value_bytes
    message_size = len(message)
    
    record_set = struct.pack('>qi', offset, message_size) + message
    record_set_size = len(record_set)
    
    partition_data += struct.pack('>i', record_set_size) + record_set
    
    topic_data += struct.pack('>i', partition_count) + partition_data
    
    body = struct.pack('>hi', acks, timeout_ms) + struct.pack('>i', topic_count) + topic_data
    
    # Send request
    request = header + body
    request_size = len(request)
    
    sock.send(struct.pack('>i', request_size))
    sock.send(request)
    
    # Read response
    response_size_bytes = sock.recv(4)
    if len(response_size_bytes) < 4:
        print("No response received")
        sock.close()
        return
    
    response_size = struct.unpack('>i', response_size_bytes)[0]
    response = sock.recv(response_size)
    
    # Parse response header
    correlation_id = struct.unpack('>i', response[:4])[0]
    print(f"Produce response - Correlation ID: {correlation_id}")
    
    # Parse response body (simplified)
    pos = 4
    topic_count = struct.unpack('>i', response[pos:pos+4])[0]
    pos += 4
    
    for _ in range(topic_count):
        # Read topic name
        name_len = struct.unpack('>h', response[pos:pos+2])[0]
        pos += 2
        topic_name = response[pos:pos+name_len].decode('utf-8')
        pos += name_len
        
        # Read partition responses
        partition_count = struct.unpack('>i', response[pos:pos+4])[0]
        pos += 4
        
        for _ in range(partition_count):
            partition_id = struct.unpack('>i', response[pos:pos+4])[0]
            error_code = struct.unpack('>h', response[pos+4:pos+6])[0]
            offset = struct.unpack('>q', response[pos+6:pos+14])[0]
            pos += 14
            
            if error_code == 0:
                print(f"  Topic: {topic_name}, Partition: {partition_id}, Offset: {offset}, Status: SUCCESS")
            else:
                print(f"  Topic: {topic_name}, Partition: {partition_id}, Error: {error_code}")
    
    sock.close()

def send_fetch_request(host, port, topic, partition, offset):
    """Send a fetch request"""
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.connect((host, port))
    
    # Build fetch request (API key 1, version 0)
    api_key = 1
    api_version = 0
    correlation_id = 2
    client_id = "test-client"
    
    # Request header
    header = struct.pack('>hhI', api_key, api_version, correlation_id) + encode_string(client_id)
    
    # Fetch request body
    replica_id = -1
    max_wait_time = 1000
    min_bytes = 1
    topic_count = 1
    
    # Topic data
    topic_data = encode_string(topic)
    partition_count = 1
    
    # Partition data
    partition_data = struct.pack('>iqi', partition, offset, 100000)  # partition, offset, max_bytes
    
    topic_data += struct.pack('>i', partition_count) + partition_data
    
    body = struct.pack('>iii', replica_id, max_wait_time, min_bytes)
    body += struct.pack('>i', topic_count) + topic_data
    
    # Send request
    request = header + body
    request_size = len(request)
    
    sock.send(struct.pack('>i', request_size))
    sock.send(request)
    
    # Read response
    response_size_bytes = sock.recv(4)
    if len(response_size_bytes) < 4:
        print("No response received")
        sock.close()
        return
    
    response_size = struct.unpack('>i', response_size_bytes)[0]
    response = sock.recv(response_size)
    
    # Parse response
    correlation_id = struct.unpack('>i', response[:4])[0]
    print(f"Fetch response - Correlation ID: {correlation_id}")
    
    # Parse topics (simplified)
    pos = 4
    topic_count = struct.unpack('>i', response[pos:pos+4])[0]
    pos += 4
    
    for _ in range(topic_count):
        # Read topic name
        name_len = struct.unpack('>h', response[pos:pos+2])[0]
        pos += 2
        topic_name = response[pos:pos+name_len].decode('utf-8')
        pos += name_len
        
        # Read partitions
        partition_count = struct.unpack('>i', response[pos:pos+4])[0]
        pos += 4
        
        for _ in range(partition_count):
            partition_id = struct.unpack('>i', response[pos:pos+4])[0]
            error_code = struct.unpack('>h', response[pos+4:pos+6])[0]
            high_watermark = struct.unpack('>q', response[pos+6:pos+14])[0]
            message_set_size = struct.unpack('>i', response[pos+14:pos+18])[0]
            pos += 18
            
            print(f"  Topic: {topic_name}, Partition: {partition_id}, HW: {high_watermark}, Messages: {message_set_size} bytes")
            
            if message_set_size > 0 and error_code == 0:
                # Parse messages (simplified)
                msg_end = pos + message_set_size
                msg_count = 0
                while pos < msg_end and pos < len(response):
                    # Read offset
                    msg_offset = struct.unpack('>q', response[pos:pos+8])[0]
                    msg_size = struct.unpack('>i', response[pos+8:pos+12])[0]
                    pos += 12
                    
                    if msg_size > 0 and pos + msg_size <= len(response):
                        # Skip CRC, magic, attributes, timestamp
                        msg_pos = pos + 4 + 1 + 1 + 8
                        
                        # Read key
                        key_size = struct.unpack('>i', response[msg_pos:msg_pos+4])[0]
                        msg_pos += 4
                        if key_size > 0:
                            key = response[msg_pos:msg_pos+key_size].decode('utf-8', errors='replace')
                            msg_pos += key_size
                        else:
                            key = None
                        
                        # Read value
                        if msg_pos + 4 <= len(response):
                            value_size = struct.unpack('>i', response[msg_pos:msg_pos+4])[0]
                            msg_pos += 4
                            if value_size > 0 and msg_pos + value_size <= len(response):
                                value = response[msg_pos:msg_pos+value_size].decode('utf-8', errors='replace')
                            else:
                                value = None
                        else:
                            value = None
                        
                        print(f"    Message {msg_count}: Offset={msg_offset}, Key={key}, Value={value}")
                        msg_count += 1
                        
                    pos += msg_size
            else:
                pos += message_set_size
    
    sock.close()

if __name__ == "__main__":
    host = "localhost"
    port = 9092
    
    print("=== Testing Persistent Metadata ===")
    print()
    
    # Test 1: Produce a message
    print("1. Producing message to test-topic...")
    send_produce_request(host, port, "test-topic", 0, "persist-key-1", "persist-value-1")
    
    time.sleep(1)
    
    # Test 2: Fetch the message
    print("\n2. Fetching messages from test-topic...")
    send_fetch_request(host, port, "test-topic", 0, 0)
    
    print("\n3. Server should persist metadata to disk.")
    print("   Check ./data/metadata/metadata.json")
    
    print("\nTest complete! Restart server to verify persistence.")