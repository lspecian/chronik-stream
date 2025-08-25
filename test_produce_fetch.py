#!/usr/bin/env python3
"""Test produce and fetch directly"""

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

def test_metadata():
    """Test metadata request"""
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.connect(('localhost', 9092))
    
    # Request for all topics (empty array)
    request_body = struct.pack('>i', 0)  # 0 topics
    
    print("Testing Metadata...")
    response = send_kafka_request(sock, 3, 0, 1, request_body)
    
    if response:
        print(f"✓ Metadata response: {len(response)} bytes")
        # Parse correlation ID
        corr_id = struct.unpack('>i', response[:4])[0]
        print(f"  Correlation ID: {corr_id}")
        
        # Parse brokers
        pos = 4
        broker_count = struct.unpack('>i', response[pos:pos+4])[0]
        print(f"  Brokers: {broker_count}")
        return True
    else:
        print("✗ No metadata response")
        return False
    
    sock.close()

def test_produce():
    """Test produce request"""
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.connect(('localhost', 9092))
    
    # Build produce request for v2 (supports magic byte 2)
    # Acks: 1 (wait for leader)
    # Timeout: 30000ms
    # Topics: 1
    #   Topic: test-topic
    #   Partitions: 1
    #     Partition: 0
    #     Records: (in v2 message format)
    
    request_body = struct.pack('>h', 1)  # Acks
    request_body += struct.pack('>i', 30000)  # Timeout
    request_body += struct.pack('>i', 1)  # Topics array size
    
    # Topic
    topic_name = b"test-topic"
    request_body += struct.pack('>h', len(topic_name))
    request_body += topic_name
    request_body += struct.pack('>i', 1)  # Partitions array size
    
    # Partition
    request_body += struct.pack('>i', 0)  # Partition ID
    
    # Create a simple v2 record batch
    # For simplicity, create minimal batch with one record
    import zlib
    
    # Build record
    record = bytearray()
    # Length (varint) - we'll fill this later
    record.append(0)  # Placeholder
    # Attributes (0 = no compression, no timestamp type)
    record.append(0)
    # Timestamp delta (varint 0)
    record.append(0)
    # Offset delta (varint 0)
    record.append(0)
    # Key length (varint -1 for null)
    record.append(0xFF)  # -1 in varint (actually 0x01 for -1 in zigzag encoding)
    record.append(0x01)
    # Value length and value
    value = b"Hello Chronik!"
    # Encode length as varint
    value_len = len(value)
    if value_len < 128:
        record.append(value_len << 1)  # Positive varint encoding
    record.extend(value)
    # Headers count (varint 0)
    record.append(0)
    
    # Fix record length at start
    record[0] = len(record) - 1
    
    # Build record batch
    batch = bytearray()
    batch.extend(struct.pack('>q', 0))  # Base offset
    batch.extend(struct.pack('>i', len(record) + 49))  # Batch length (will update)
    batch.extend(struct.pack('>i', 0))  # Partition leader epoch
    batch.append(2)  # Magic byte (v2)
    
    # CRC placeholder (4 bytes) - from here CRC is calculated
    crc_start = len(batch)
    batch.extend(struct.pack('>I', 0))  # CRC placeholder
    
    # Attributes (2 bytes)
    batch.extend(struct.pack('>h', 0))  # No compression, create time
    # Last offset delta (4 bytes)
    batch.extend(struct.pack('>i', 0))
    # First timestamp (8 bytes)
    batch.extend(struct.pack('>q', int(time.time() * 1000)))
    # Max timestamp (8 bytes)
    batch.extend(struct.pack('>q', int(time.time() * 1000)))
    # Producer ID (8 bytes)
    batch.extend(struct.pack('>q', -1))
    # Producer epoch (2 bytes)
    batch.extend(struct.pack('>h', -1))
    # Base sequence (4 bytes)
    batch.extend(struct.pack('>i', -1))
    # Records count (4 bytes)
    batch.extend(struct.pack('>i', 1))
    
    # Add record
    batch.extend(record)
    
    # Fix batch length (from partition leader epoch to end)
    batch_len = len(batch) - 12  # Exclude base offset and length field
    batch[8:12] = struct.pack('>i', batch_len)
    
    # Calculate and set CRC32 (from attributes to end)
    crc_data = batch[crc_start + 4:]  # Everything after CRC field
    crc = zlib.crc32(crc_data) & 0xffffffff
    batch[crc_start:crc_start + 4] = struct.pack('>I', crc)
    
    # Add batch size and batch to request
    request_body += struct.pack('>i', len(batch))
    request_body += batch
    
    print("\nTesting Produce...")
    response = send_kafka_request(sock, 0, 2, 2, request_body)
    
    if response:
        print(f"✓ Produce response: {len(response)} bytes")
        # Parse response
        corr_id = struct.unpack('>i', response[:4])[0]
        print(f"  Correlation ID: {corr_id}")
        
        pos = 4
        topics_count = struct.unpack('>i', response[pos:pos+4])[0]
        pos += 4
        print(f"  Topics: {topics_count}")
        
        for _ in range(topics_count):
            # Topic name
            name_len = struct.unpack('>h', response[pos:pos+2])[0]
            pos += 2
            topic_name = response[pos:pos+name_len].decode('utf-8')
            pos += name_len
            print(f"    Topic: {topic_name}")
            
            # Partitions
            partitions_count = struct.unpack('>i', response[pos:pos+4])[0]
            pos += 4
            
            for _ in range(partitions_count):
                partition_id = struct.unpack('>i', response[pos:pos+4])[0]
                pos += 4
                error_code = struct.unpack('>h', response[pos:pos+2])[0]
                pos += 2
                base_offset = struct.unpack('>q', response[pos:pos+8])[0]
                pos += 8
                print(f"      Partition {partition_id}: error={error_code}, offset={base_offset}")
                
                if error_code == 0:
                    return True
    else:
        print("✗ No produce response")
    
    sock.close()
    return False

def test_fetch():
    """Test fetch request"""
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.connect(('localhost', 9092))
    
    # Build Fetch request (API key 1, version 0)
    request_body = struct.pack('>i', -1)  # ReplicaId
    request_body += struct.pack('>i', 1000)  # MaxWaitTime
    request_body += struct.pack('>i', 1)  # MinBytes
    request_body += struct.pack('>i', 1)  # Topics array size
    
    # Topic
    topic_name = b"test-topic"
    request_body += struct.pack('>h', len(topic_name))
    request_body += topic_name
    request_body += struct.pack('>i', 1)  # Partitions array size
    
    # Partition
    request_body += struct.pack('>i', 0)  # Partition ID
    request_body += struct.pack('>q', 0)  # FetchOffset
    request_body += struct.pack('>i', 1000000)  # MaxBytes
    
    print("\nTesting Fetch...")
    response = send_kafka_request(sock, 1, 0, 3, request_body)
    
    if response:
        print(f"✓ Fetch response: {len(response)} bytes")
        # Parse correlation ID
        corr_id = struct.unpack('>i', response[:4])[0]
        print(f"  Correlation ID: {corr_id}")
        
        # Parse response
        pos = 4
        topics_count = struct.unpack('>i', response[pos:pos+4])[0]
        pos += 4
        print(f"  Topics: {topics_count}")
        
        for _ in range(topics_count):
            # Topic name
            name_len = struct.unpack('>h', response[pos:pos+2])[0]
            pos += 2
            topic_name = response[pos:pos+name_len].decode('utf-8')
            pos += name_len
            print(f"    Topic: {topic_name}")
            
            # Partitions
            partitions_count = struct.unpack('>i', response[pos:pos+4])[0]
            pos += 4
            
            for _ in range(partitions_count):
                partition_id = struct.unpack('>i', response[pos:pos+4])[0]
                pos += 4
                error_code = struct.unpack('>h', response[pos:pos+2])[0]
                pos += 2
                high_watermark = struct.unpack('>q', response[pos:pos+8])[0]
                pos += 8
                
                print(f"      Partition {partition_id}: error={error_code}, high_watermark={high_watermark}")
                
                # Records length
                records_len = struct.unpack('>i', response[pos:pos+4])[0]
                pos += 4
                print(f"        Records bytes: {records_len}")
                
                if records_len > 0:
                    print(f"        ✓ Got data! First bytes: {response[pos:pos+min(50, records_len)].hex()[:100]}...")
                    return True
                pos += max(0, records_len)
    else:
        print("✗ No fetch response")
    
    sock.close()
    return False

if __name__ == "__main__":
    print("=" * 50)
    print("Testing Chronik Stream Kafka Compatibility")
    print("=" * 50)
    
    # Test metadata
    if not test_metadata():
        print("\n✗ Metadata test failed")
        exit(1)
    
    # Test produce
    if not test_produce():
        print("\n✗ Produce test failed")
        exit(1)
    
    # Give server time to process
    time.sleep(1)
    
    # Test fetch
    if test_fetch():
        print("\n" + "="*50)
        print("✓ SUCCESS: All tests passed!")
        print("✓ Messages can be produced and consumed!")
        print("="*50)
    else:
        print("\n✗ Fetch test failed - no data returned")