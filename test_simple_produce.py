#!/usr/bin/env python3
"""Simpler produce test"""

import socket
import struct

def test_produce():
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.connect(('localhost', 9092))
    
    # Build minimal Produce v0 request (simpler format)
    # API key 0, version 0
    request = bytearray()
    
    # Header
    request.extend(struct.pack('>h', 0))  # API key (Produce)
    request.extend(struct.pack('>h', 0))  # API version
    request.extend(struct.pack('>i', 1))  # Correlation ID
    request.extend(struct.pack('>h', 4))  # Client ID length
    request.extend(b"test")               # Client ID
    
    # Body
    request.extend(struct.pack('>h', 1))  # RequiredAcks
    request.extend(struct.pack('>i', 30000))  # Timeout
    request.extend(struct.pack('>i', 1))  # Topics array size
    
    # Topic
    topic_name = b"test-topic"
    request.extend(struct.pack('>h', len(topic_name)))
    request.extend(topic_name)
    request.extend(struct.pack('>i', 1))  # Partitions array size
    
    # Partition
    request.extend(struct.pack('>i', 0))  # Partition ID
    
    # MessageSet (v0/v1 format)
    message_set = bytearray()
    
    # Single message
    message = bytearray()
    message.extend(struct.pack('>b', 0))  # Magic byte 0 (v0)
    message.extend(struct.pack('>b', 0))  # Attributes
    # Key (-1 for null)
    message.extend(struct.pack('>i', -1))
    # Value
    value = b"Hello Chronik!"
    message.extend(struct.pack('>i', len(value)))
    message.extend(value)
    
    # Calculate CRC32 for message (from magic byte to end)
    import zlib
    crc = zlib.crc32(message) & 0xffffffff
    
    # Build complete message with offset, size, CRC, and message
    message_set.extend(struct.pack('>q', 0))  # Offset
    message_set.extend(struct.pack('>i', 4 + len(message)))  # Message size (CRC + message)
    message_set.extend(struct.pack('>I', crc))  # CRC
    message_set.extend(message)
    
    # Add message set size and content to request
    request.extend(struct.pack('>i', len(message_set)))
    request.extend(message_set)
    
    # Send with size prefix
    size = struct.pack('>i', len(request))
    sock.send(size + request)
    
    # Read response
    size_bytes = sock.recv(4)
    if size_bytes:
        response_size = struct.unpack('>i', size_bytes)[0]
        response = sock.recv(response_size)
        
        print(f"Response size: {response_size}")
        print(f"Response: {response.hex()[:100]}...")
        
        # Parse basic response
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
                base_offset = struct.unpack('>q', response[pos:pos+8])[0]
                pos += 8
                print(f"Partition {partition_id}: error={error_code}, offset={base_offset}")
                
                if error_code == 0:
                    print("✓ SUCCESS: Message produced!")
                else:
                    print(f"✗ Error code: {error_code}")
    
    sock.close()

if __name__ == "__main__":
    print("Testing simple produce v0...")
    test_produce()