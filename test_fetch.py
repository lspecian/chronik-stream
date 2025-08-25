#!/usr/bin/env python3
"""Test Fetch API directly"""

import socket
import struct
import time

def send_kafka_request(sock, api_key, api_version, correlation_id, request_body):
    """Send a Kafka request and get response"""
    # Build request header
    header = struct.pack('>hhih', 
                        api_key,           # API key
                        api_version,       # API version
                        correlation_id,    # Correlation ID
                        len("test-client")  # Client ID length
                        )
    header += b"test-client"  # Client ID
    
    # Full request
    full_request = header + request_body
    
    # Send with size prefix
    size = struct.pack('>i', len(full_request))
    sock.send(size + full_request)
    
    # Read response size
    size_bytes = sock.recv(4)
    if not size_bytes:
        return None
    response_size = struct.unpack('>i', size_bytes)[0]
    
    # Read full response
    response = b''
    while len(response) < response_size:
        chunk = sock.recv(min(4096, response_size - len(response)))
        if not chunk:
            break
        response += chunk
    
    return response

def test_fetch():
    # Connect to server
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.connect(('localhost', 9092))
    
    # Build Fetch request (API key 1, version 0)
    # ReplicaId: -1 (consumer)
    # MaxWaitTime: 1000ms
    # MinBytes: 1
    # Topics: 1
    #   Topic: demo-topic
    #   Partitions: 1
    #     Partition: 0
    #     FetchOffset: 0
    #     MaxBytes: 1000000
    
    request_body = struct.pack('>i', -1)  # ReplicaId
    request_body += struct.pack('>i', 1000)  # MaxWaitTime
    request_body += struct.pack('>i', 1)  # MinBytes
    request_body += struct.pack('>i', 1)  # Topics array size
    
    # Topic
    topic_name = b"demo-topic"
    request_body += struct.pack('>h', len(topic_name))
    request_body += topic_name
    request_body += struct.pack('>i', 1)  # Partitions array size
    
    # Partition
    request_body += struct.pack('>i', 0)  # Partition ID
    request_body += struct.pack('>q', 0)  # FetchOffset
    request_body += struct.pack('>i', 1000000)  # MaxBytes
    
    print("Sending Fetch request...")
    response = send_kafka_request(sock, 1, 0, 1, request_body)
    
    if response:
        print(f"Received response: {len(response)} bytes")
        # Parse correlation ID
        corr_id = struct.unpack('>i', response[:4])[0]
        print(f"Correlation ID: {corr_id}")
        
        # Parse response
        pos = 4
        topics_count = struct.unpack('>i', response[pos:pos+4])[0]
        pos += 4
        print(f"Topics count: {topics_count}")
        
        for _ in range(topics_count):
            # Topic name
            name_len = struct.unpack('>h', response[pos:pos+2])[0]
            pos += 2
            topic_name = response[pos:pos+name_len].decode('utf-8')
            pos += name_len
            print(f"Topic: {topic_name}")
            
            # Partitions
            partitions_count = struct.unpack('>i', response[pos:pos+4])[0]
            pos += 4
            print(f"  Partitions: {partitions_count}")
            
            for _ in range(partitions_count):
                partition_id = struct.unpack('>i', response[pos:pos+4])[0]
                pos += 4
                error_code = struct.unpack('>h', response[pos:pos+2])[0]
                pos += 2
                high_watermark = struct.unpack('>q', response[pos:pos+8])[0]
                pos += 8
                
                print(f"    Partition {partition_id}: error={error_code}, high_watermark={high_watermark}")
                
                # Records length
                records_len = struct.unpack('>i', response[pos:pos+4])[0]
                pos += 4
                print(f"      Records bytes: {records_len}")
                
                if records_len > 0:
                    print(f"      Records data: {response[pos:pos+min(100, records_len)].hex()[:100]}...")
                pos += max(0, records_len)
    else:
        print("No response received")
    
    sock.close()

if __name__ == "__main__":
    test_fetch()