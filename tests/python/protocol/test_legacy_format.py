#!/usr/bin/env python3
"""Test legacy Kafka message format support"""

import struct
import socket
import time

def calculate_crc32(data):
    """Calculate CRC32 checksum"""
    import zlib
    return zlib.crc32(data) & 0xffffffff

def create_legacy_message_v0(key, value):
    """Create a v0 legacy message"""
    # Magic byte 0
    magic = 0
    # Attributes (no compression)
    attributes = 0
    
    # Encode key (-1 for null)
    if key:
        key_bytes = key.encode('utf-8')
        key_len = len(key_bytes)
    else:
        key_bytes = b''
        key_len = -1
        
    # Encode value
    value_bytes = value.encode('utf-8')
    value_len = len(value_bytes)
    
    # Build message without CRC
    msg = struct.pack('!bb', magic, attributes)  # magic + attributes
    if key_len >= 0:
        msg += struct.pack('!i', key_len) + key_bytes
    else:
        msg += struct.pack('!i', key_len)
    msg += struct.pack('!i', value_len) + value_bytes
    
    # Calculate CRC
    crc = calculate_crc32(msg)
    
    # Full message: CRC + magic + attributes + key + value
    return struct.pack('!I', crc) + msg

def create_produce_request_with_legacy_format(topic, partition, messages):
    """Create a ProduceRequest with legacy message format"""
    # Request header
    api_key = 0  # Produce
    api_version = 2  # Use v2 which should support legacy messages
    correlation_id = 42
    client_id = "test-legacy"
    
    # Build message set
    message_set = b''
    offset = 0
    for key, value in messages:
        message = create_legacy_message_v0(key, value)
        # MessageSet format: offset + message_size + message
        message_set += struct.pack('!q', offset)  # offset (8 bytes)
        message_set += struct.pack('!i', len(message))  # message size
        message_set += message
        offset += 1
    
    # ProduceRequest v2 body
    acks = 1
    timeout = 30000
    
    # Topic array (1 topic)
    topics = b''
    topics += struct.pack('!h', len(topic))  # topic name length
    topics += topic.encode('utf-8')
    
    # Partition array (1 partition)
    topics += struct.pack('!i', 1)  # partition array size
    topics += struct.pack('!i', partition)  # partition index
    topics += struct.pack('!i', len(message_set))  # record bytes length
    topics += message_set
    
    # Build request body
    body = struct.pack('!h', acks)
    body += struct.pack('!i', timeout)
    body += struct.pack('!i', 1)  # topic array size
    body += topics
    
    # Build full request
    client_id_bytes = client_id.encode('utf-8')
    
    header = struct.pack('!h', api_key)
    header += struct.pack('!h', api_version)
    header += struct.pack('!i', correlation_id)
    header += struct.pack('!h', len(client_id_bytes))
    header += client_id_bytes
    
    request = header + body
    
    # Add size prefix
    return struct.pack('!i', len(request)) + request

def main():
    # Connect to Chronik
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.connect(('localhost', 9092))
    
    print("Connected to Chronik on port 9092")
    
    # Send ApiVersions request first
    api_versions_req = struct.pack('!ihhhh', 
        7 + 6,  # size
        18,     # ApiVersions
        0,      # version
        1,      # correlation_id
        4)      # client_id length
    api_versions_req += b'test'
    
    sock.send(api_versions_req)
    
    # Read response
    size_bytes = sock.recv(4)
    size = struct.unpack('!i', size_bytes)[0]
    response = sock.recv(size)
    print(f"ApiVersions response size: {size}")
    
    time.sleep(0.1)
    
    # Send Produce request with legacy format
    print("\nSending ProduceRequest with legacy v0 format messages...")
    
    produce_req = create_produce_request_with_legacy_format(
        topic="chronik-default",
        partition=0,
        messages=[
            ("key1", "value1-legacy-v0"),
            ("key2", "value2-legacy-v0"),
            (None, "value3-no-key-legacy-v0"),
        ]
    )
    
    sock.send(produce_req)
    print(f"Sent {len(produce_req)} bytes")
    
    # Read response
    size_bytes = sock.recv(4)
    if len(size_bytes) == 4:
        size = struct.unpack('!i', size_bytes)[0]
        print(f"Response size: {size}")
        
        response = sock.recv(size)
        
        # Parse response header
        correlation_id = struct.unpack('!i', response[:4])[0]
        print(f"Correlation ID: {correlation_id}")
        
        # Check for error in response
        if len(response) > 4:
            # Skip throttle_time_ms
            offset = 8
            # Number of topics
            if offset < len(response):
                num_topics = struct.unpack('!i', response[offset:offset+4])[0]
                print(f"Number of topics in response: {num_topics}")
                
                if num_topics > 0:
                    offset += 4
                    # Topic name length
                    topic_name_len = struct.unpack('!h', response[offset:offset+2])[0]
                    offset += 2
                    topic_name = response[offset:offset+topic_name_len].decode('utf-8')
                    offset += topic_name_len
                    print(f"Topic: {topic_name}")
                    
                    # Number of partitions
                    num_partitions = struct.unpack('!i', response[offset:offset+4])[0]
                    offset += 4
                    print(f"Number of partitions: {num_partitions}")
                    
                    if num_partitions > 0:
                        # Partition index
                        partition_idx = struct.unpack('!i', response[offset:offset+4])[0]
                        offset += 4
                        # Error code
                        error_code = struct.unpack('!h', response[offset:offset+2])[0]
                        offset += 2
                        # Base offset
                        base_offset = struct.unpack('!q', response[offset:offset+8])[0]
                        offset += 8
                        
                        print(f"Partition {partition_idx}: error_code={error_code}, base_offset={base_offset}")
                        
                        if error_code == 0:
                            print("✓ Successfully produced messages with legacy v0 format!")
                        else:
                            print(f"✗ Error producing messages: {error_code}")
    
    sock.close()

if __name__ == "__main__":
    main()