#!/usr/bin/env python3
"""Simple fetch test"""

import socket
import struct

def test_fetch(topic="test-topic"):
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.connect(('localhost', 9092))
    
    # Build Fetch v0 request
    request = bytearray()
    
    # Header
    request.extend(struct.pack('>h', 1))  # API key (Fetch)
    request.extend(struct.pack('>h', 0))  # API version
    request.extend(struct.pack('>i', 1))  # Correlation ID
    request.extend(struct.pack('>h', 4))  # Client ID length
    request.extend(b"test")               # Client ID
    
    # Body
    request.extend(struct.pack('>i', -1))  # ReplicaId
    request.extend(struct.pack('>i', 1000))  # MaxWaitTime
    request.extend(struct.pack('>i', 1))  # MinBytes
    request.extend(struct.pack('>i', 1))  # Topics array size
    
    # Topic
    topic_name = topic.encode('utf-8')
    request.extend(struct.pack('>h', len(topic_name)))
    request.extend(topic_name)
    request.extend(struct.pack('>i', 1))  # Partitions array size
    
    # Partition
    request.extend(struct.pack('>i', 0))  # Partition ID
    request.extend(struct.pack('>q', 0))  # FetchOffset
    request.extend(struct.pack('>i', 1000000))  # MaxBytes
    
    # Send with size prefix
    size = struct.pack('>i', len(request))
    sock.send(size + request)
    
    # Read response
    size_bytes = sock.recv(4)
    if size_bytes:
        response_size = struct.unpack('>i', size_bytes)[0]
        response = sock.recv(response_size)
        
        print(f"Response size: {response_size}")
        
        # Parse response
        corr_id = struct.unpack('>i', response[:4])[0]
        print(f"Correlation ID: {corr_id}")
        
        topics_count = struct.unpack('>i', response[4:8])[0]
        print(f"Topics count: {topics_count}")
        
        if topics_count > 0:
            pos = 8
            # Topic name
            name_len = struct.unpack('>h', response[pos:pos+2])[0]
            pos += 2
            topic_name = response[pos:pos+name_len].decode('utf-8')
            pos += name_len
            print(f"Topic: {topic_name}")
            
            # Partitions
            partitions_count = struct.unpack('>i', response[pos:pos+4])[0]
            pos += 4
            print(f"Partitions: {partitions_count}")
            
            if partitions_count > 0:
                partition_id = struct.unpack('>i', response[pos:pos+4])[0]
                pos += 4
                error_code = struct.unpack('>h', response[pos:pos+2])[0]
                pos += 2
                high_watermark = struct.unpack('>q', response[pos:pos+8])[0]
                pos += 8
                print(f"Partition {partition_id}: error={error_code}, high_watermark={high_watermark}")
                
                # Records length
                records_len = struct.unpack('>i', response[pos:pos+4])[0]
                pos += 4
                print(f"Records bytes: {records_len}")
                
                if records_len > 0:
                    print(f"✓ SUCCESS: Got {records_len} bytes of data!")
                    # Try to parse first few bytes
                    print(f"First bytes: {response[pos:pos+min(50, records_len)].hex()}")
                else:
                    print("No records returned")
    
    sock.close()

if __name__ == "__main__":
    print("Testing fetch for test-topic...")
    test_fetch("test-topic")
    
    print("\nTesting fetch for unknown topic...")
    test_fetch("unknown")