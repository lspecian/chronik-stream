#!/usr/bin/env python3
"""Test produce with correct Kafka record batch format (magic v2)."""

import socket
import struct
import time
import zlib

def encode_varint(value):
    """Encode a signed integer as a varint."""
    # Zigzag encoding for signed values
    if value < 0:
        zigzag = ((-value) * 2) - 1
    else:
        zigzag = value * 2
    
    result = bytearray()
    while zigzag >= 0x80:
        result.append((zigzag & 0x7F) | 0x80)
        zigzag >>= 7
    result.append(zigzag & 0x7F)
    return bytes(result)

def encode_varlong(value):
    """Encode a signed long as a varlong."""
    # Zigzag encoding for signed values
    if value < 0:
        zigzag = ((-value) * 2) - 1
    else:
        zigzag = value * 2
    
    result = bytearray()
    while zigzag >= 0x80:
        result.append((zigzag & 0x7F) | 0x80)
        zigzag >>= 7
    result.append(zigzag & 0x7F)
    return bytes(result)

def encode_string(s):
    """Encode string in Kafka format (length prefix + bytes)"""
    if s is None:
        return struct.pack('>h', -1)
    encoded = s.encode('utf-8')
    return struct.pack('>h', len(encoded)) + encoded

def create_record(key, value, offset_delta=0, timestamp_delta=0):
    """Create a single Kafka record with proper varint encoding."""
    record = bytearray()
    
    # Attributes (1 byte)
    record.append(0)  # No compression, no special attributes
    
    # Timestamp delta (varlong)
    record.extend(encode_varlong(timestamp_delta))
    
    # Offset delta (varint)
    record.extend(encode_varint(offset_delta))
    
    # Key length and data (varint + bytes)
    if key is None:
        record.extend(encode_varint(-1))
    else:
        key_bytes = key.encode('utf-8') if isinstance(key, str) else key
        record.extend(encode_varint(len(key_bytes)))
        record.extend(key_bytes)
    
    # Value length and data (varint + bytes)
    if value is None:
        record.extend(encode_varint(-1))
    else:
        value_bytes = value.encode('utf-8') if isinstance(value, str) else value
        record.extend(encode_varint(len(value_bytes)))
        record.extend(value_bytes)
    
    # Headers array (varint for count)
    record.extend(encode_varint(0))  # No headers
    
    return bytes(record)

def create_record_batch(messages, base_offset=0, producer_id=-1, producer_epoch=-1):
    """Create a Kafka record batch with proper format."""
    # Encode all records first
    records_data = bytearray()
    for i, msg in enumerate(messages):
        record = create_record(None, msg, offset_delta=i, timestamp_delta=0)
        # Each record is prefixed with its length as varint
        records_data.extend(encode_varint(len(record)))
        records_data.extend(record)
    
    # Build the batch
    batch = bytearray()
    
    # Base offset (8 bytes)
    batch.extend(struct.pack('>q', base_offset))
    
    # We'll fill in batch length later (4 bytes placeholder)
    batch_length_pos = len(batch)
    batch.extend(struct.pack('>i', 0))
    
    # Partition leader epoch (4 bytes)
    batch.extend(struct.pack('>i', -1))
    
    # Magic (1 byte)
    batch.append(2)  # Magic v2
    
    # CRC placeholder (4 bytes) - will be calculated over everything after this field
    crc_pos = len(batch)
    batch.extend(struct.pack('>I', 0))
    
    # Start of data for CRC calculation
    crc_start = len(batch)
    
    # Attributes (2 bytes)
    batch.extend(struct.pack('>h', 0))  # No compression, no special flags
    
    # Last offset delta (4 bytes)
    batch.extend(struct.pack('>i', len(messages) - 1))
    
    # First timestamp (8 bytes)
    current_time_ms = int(time.time() * 1000)
    batch.extend(struct.pack('>q', current_time_ms))
    
    # Max timestamp (8 bytes)
    batch.extend(struct.pack('>q', current_time_ms))
    
    # Producer ID (8 bytes)
    batch.extend(struct.pack('>q', producer_id))
    
    # Producer epoch (2 bytes)
    batch.extend(struct.pack('>h', producer_epoch))
    
    # Base sequence (4 bytes)
    batch.extend(struct.pack('>i', -1))
    
    # Record count (4 bytes)
    batch.extend(struct.pack('>i', len(messages)))
    
    # Records
    batch.extend(records_data)
    
    # Fill in batch length (excluding base offset and batch length fields)
    batch_length = len(batch) - batch_length_pos - 4
    batch[batch_length_pos:batch_length_pos + 4] = struct.pack('>i', batch_length)
    
    # Calculate and fill in CRC (over everything after CRC field)
    crc_data = batch[crc_start:]
    crc = zlib.crc32(crc_data) & 0xffffffff
    batch[crc_pos:crc_pos + 4] = struct.pack('>I', crc)
    
    return bytes(batch)

def send_produce(host, port, topic, partition, messages):
    """Send a produce request with properly formatted messages."""
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.connect((host, port))
    
    api_key = 0  # Produce
    api_version = 3  # Use v3
    correlation_id = 1
    client_id = "test-producer"
    
    # Request header
    header = struct.pack('>hhI', api_key, api_version, correlation_id) + encode_string(client_id)
    
    # Request body
    body = struct.pack('>h', -1)  # transactional_id (null)
    body += struct.pack('>h', 1)  # acks (1 = leader ack)
    body += struct.pack('>i', 30000)  # timeout_ms
    
    # Topics array
    body += struct.pack('>i', 1)  # topic count
    body += encode_string(topic)
    
    # Partitions array
    body += struct.pack('>i', 1)  # partition count
    body += struct.pack('>i', partition)  # partition index
    
    # Create proper record batch
    batch = create_record_batch(messages)
    
    # Records size and data
    body += struct.pack('>i', len(batch))
    body += batch
    
    # Send request
    request = header + body
    sock.send(struct.pack('>i', len(request)))
    sock.send(request)
    
    # Read response
    response_size = struct.unpack('>i', sock.recv(4))[0]
    response = sock.recv(response_size)
    
    sock.close()
    
    # Basic response parsing
    if len(response) > 8:
        correlation_id = struct.unpack('>i', response[0:4])[0]
        topic_count = struct.unpack('>i', response[4:8])[0]
        if topic_count > 0:
            print(f"  ✓ Produced {len(messages)} messages to {topic}:{partition}")
            return True
        else:
            print(f"  ✗ Failed to produce messages")
            return False
    return False

def main():
    print("=== Testing Produce with Correct Format ===")
    
    # Test with small batches first
    print("\nSending test messages...")
    
    for i in range(3):
        messages = [f"Correct format message {i}-{j}" for j in range(10)]
        success = send_produce("localhost", 9092, "test-topic", 0, messages)
        if success:
            print(f"  Batch {i+1}: Successfully sent {len(messages)} messages")
        else:
            print(f"  Batch {i+1}: Failed")
        time.sleep(1)
    
    print("\nTest complete!")

if __name__ == "__main__":
    main()