#!/usr/bin/env python3
"""
Test script to capture and analyze Produce v9 response from chronik-server
"""
import socket
import struct
import sys

def send_produce_v9():
    """Send a Produce v9 request and capture the response"""
    
    # Connect to server
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.connect(('localhost', 9092))
    
    # First, send ApiVersions request to establish protocol
    api_versions_request = bytes.fromhex(
        "00 00 00 12"  # Length: 18
        "00 12 00 03"  # ApiKey: 18 (ApiVersions), Version: 3
        "00 00 00 01"  # CorrelationId: 1
        "00"           # Tagged fields (0)
        "07 74 65 73 74 2d 31 00"  # Client ID: "test-1" (compact)
        "00"           # Tagged fields
    )
    sock.send(api_versions_request)
    
    # Read ApiVersions response
    length_bytes = sock.recv(4)
    response_length = struct.unpack('>I', length_bytes)[0]
    api_versions_response = sock.recv(response_length)
    print(f"ApiVersions response received: {response_length} bytes")
    
    # Send Produce v9 request (the one from the debug logs)
    produce_request = bytes.fromhex(
        "00 00 00 79"  # Length: 121
        "00 00 00 09"  # ApiKey: 0 (Produce), Version: 9
        "00 00 00 02"  # CorrelationId: 2
        "00"           # Tagged fields
        "07 74 65 73 74 2d 31"  # ClientId: "test-1" (compact string, length 6)
        "00"           # Transactional ID: null (0x00 for null in compact)
        "ff ff"        # Acks: -1
        "00 00 75 30"  # Timeout: 30000ms
        "00"           # Tagged fields
        "02"           # Topics array: 1 topic (compact array, count+1 = 2)
        "0b 74 65 73 74 2d 74 6f 70 69 63"  # Topic: "test-topic" (compact)
        "02"           # Partitions array: 1 partition
        "00 00 00 01"  # Partition: 1
        "00 00 00 62"  # Records length: 98 bytes
        
        # Record batch (98 bytes)
        "00 00 00 00 00 00 00 00"  # Base offset: 0
        "00 00 00 56"              # Batch length: 86
        "00 00 00 00"              # Leader epoch: 0
        "02"                       # Magic: 2
        "61 a9 dc 8a"              # CRC32C
        "00 00"                    # Attributes: 0
        "00 00 00 00"              # Last offset delta: 0
        "00 00 01 99 0d ef 39 9f"  # Base timestamp
        "00 00 01 99 0d ef 39 9f"  # Max timestamp
        "ff ff ff ff ff ff ff ff"  # Producer ID: -1
        "ff ff"                    # Producer epoch: -1
        "ff ff ff ff"              # Base sequence: -1
        "ff ff ff ff"              # Records count: -1
        
        # Record (rest of bytes)
        "48 00 00 00"              # Length (varint): 36
        "10"                       # Key length (zigzag): 8
        "74 65 73 74 2d 6b 65 79"  # Key: "test-key"
        "2c"                       # Value length (zigzag): 22
        "48 65 6c 6c 6f 20 66 72 6f 6d 20 6c 69 62 72 64 6b 61 66 6b 61 21"  # Value
        "00"                       # Headers count: 0
        
        "00"  # Tagged fields for partition
        "00"  # Tagged fields for topic
    )
    
    sock.send(produce_request)
    
    # Read Produce response
    response_length_bytes = sock.recv(4)
    if len(response_length_bytes) < 4:
        print("ERROR: Failed to read response length")
        return
        
    response_length = struct.unpack('>I', response_length_bytes)[0]
    print(f"Response length: {response_length}")
    
    response = sock.recv(response_length)
    print(f"Response bytes ({len(response)}): {response.hex()}")
    
    # Parse the response
    offset = 0
    correlation_id = struct.unpack('>I', response[offset:offset+4])[0]
    offset += 4
    print(f"Correlation ID: {correlation_id}")
    
    # Tagged fields
    tagged_fields_len = response[offset]
    offset += 1
    print(f"Tagged fields length: {tagged_fields_len}")
    
    # Throttle time (v1+)
    throttle_time = struct.unpack('>I', response[offset:offset+4])[0]
    offset += 4
    print(f"Throttle time: {throttle_time}ms")
    
    # Topics array (compact in v9)
    topics_count_raw = response[offset]
    topics_count = topics_count_raw - 1  # Compact array encoding
    offset += 1
    print(f"Topics count: {topics_count}")
    
    # Parse topic
    # Topic name (compact string)
    topic_name_len = response[offset] - 1
    offset += 1
    topic_name = response[offset:offset+topic_name_len].decode('utf-8')
    offset += topic_name_len
    print(f"Topic name: {topic_name}")
    
    # Partitions array (compact)
    partitions_count_raw = response[offset]
    partitions_count = partitions_count_raw - 1
    offset += 1
    print(f"Partitions count: {partitions_count}")
    
    # Parse partition
    partition_index = struct.unpack('>I', response[offset:offset+4])[0]
    offset += 4
    error_code = struct.unpack('>h', response[offset:offset+2])[0]
    offset += 2
    base_offset = struct.unpack('>q', response[offset:offset+8])[0]
    offset += 8
    log_append_time = struct.unpack('>q', response[offset:offset+8])[0]
    offset += 8
    log_start_offset = struct.unpack('>q', response[offset:offset+8])[0]
    offset += 8
    
    print(f"Partition {partition_index}:")
    print(f"  Error code: {error_code}")
    print(f"  Base offset: {base_offset}")
    print(f"  Log append time: {log_append_time}")
    print(f"  Log start offset: {log_start_offset}")
    
    # Tagged fields for partition
    partition_tagged = response[offset]
    offset += 1
    print(f"  Partition tagged fields: {partition_tagged}")
    
    # Tagged fields for topic
    topic_tagged = response[offset]
    offset += 1
    print(f"  Topic tagged fields: {topic_tagged}")
    
    # Tagged fields for response
    response_tagged = response[offset] if offset < len(response) else 0
    if offset < len(response):
        offset += 1
    print(f"Response tagged fields: {response_tagged}")
    
    print(f"Total bytes consumed: {offset}/{len(response)}")
    
    sock.close()

if __name__ == "__main__":
    send_produce_v9()