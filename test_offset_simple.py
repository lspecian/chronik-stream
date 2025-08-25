#!/usr/bin/env python3
import socket
import struct

def encode_string(s):
    """Encode string in Kafka format (length prefix + bytes)"""
    if s is None:
        return struct.pack('>h', -1)
    encoded = s.encode('utf-8')
    return struct.pack('>h', len(encoded)) + encoded

def test_offset_commit():
    """Test a simple OffsetCommit request"""
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.connect(("localhost", 9092))
    
    api_key = 8  # OffsetCommit
    api_version = 2  # Use v2 for simplicity
    correlation_id = 1
    client_id = "test-client"
    
    # Request header
    header = struct.pack('>hhI', api_key, api_version, correlation_id) + encode_string(client_id)
    
    # Request body: group_id, generation_id, member_id, topics
    body = encode_string("test-group")  # group_id
    body += struct.pack('>i', 0)  # generation_id
    body += encode_string("test-member")  # member_id
    
    # Topics array (1 topic)
    body += struct.pack('>i', 1)  # topic count
    body += encode_string("test-topic")  # topic name
    
    # Partitions array (1 partition)
    body += struct.pack('>i', 1)  # partition count
    body += struct.pack('>i', 0)  # partition index
    body += struct.pack('>q', 100)  # offset
    body += encode_string("metadata")  # metadata
    
    # Send request
    request = struct.pack('>i', len(header) + len(body)) + header + body
    sock.send(request)
    
    # Read response
    response_size = struct.unpack('>i', sock.recv(4))[0]
    response = sock.recv(response_size)
    
    print(f"Response size: {response_size}")
    print(f"Response bytes: {response.hex()}")
    
    # Parse response
    pos = 0
    correlation_id = struct.unpack('>i', response[pos:pos+4])[0]
    pos += 4
    print(f"Correlation ID: {correlation_id}")
    
    # For v2, no throttle_time_ms
    # Topic count
    if pos < len(response):
        topic_count = struct.unpack('>i', response[pos:pos+4])[0]
        pos += 4
        print(f"Topic count: {topic_count}")
        
        for i in range(topic_count):
            # Topic name
            name_len = struct.unpack('>h', response[pos:pos+2])[0]
            pos += 2
            topic_name = response[pos:pos+name_len].decode('utf-8')
            pos += name_len
            print(f"Topic: {topic_name}")
            
            # Partition count
            partition_count = struct.unpack('>i', response[pos:pos+4])[0]
            pos += 4
            print(f"Partition count: {partition_count}")
            
            for j in range(partition_count):
                partition_index = struct.unpack('>i', response[pos:pos+4])[0]
                pos += 4
                error_code = struct.unpack('>h', response[pos:pos+2])[0]
                pos += 2
                print(f"  Partition {partition_index}: error={error_code}")
    
    sock.close()

if __name__ == "__main__":
    test_offset_commit()