#!/usr/bin/env python3
"""Test segment rotation with correctly formatted Kafka messages."""

import socket
import struct
import time
import zlib
import os
import glob
import json

def encode_varint(value):
    """Encode a signed integer as a varint."""
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
    record.append(0)  # Attributes
    record.extend(encode_varlong(timestamp_delta))
    record.extend(encode_varint(offset_delta))
    
    # Key
    if key is None:
        record.extend(encode_varint(-1))
    else:
        key_bytes = key.encode('utf-8') if isinstance(key, str) else key
        record.extend(encode_varint(len(key_bytes)))
        record.extend(key_bytes)
    
    # Value
    if value is None:
        record.extend(encode_varint(-1))
    else:
        value_bytes = value.encode('utf-8') if isinstance(value, str) else value
        record.extend(encode_varint(len(value_bytes)))
        record.extend(value_bytes)
    
    # Headers (none)
    record.extend(encode_varint(0))
    
    return bytes(record)

def create_record_batch(messages, base_offset=0):
    """Create a Kafka record batch."""
    records_data = bytearray()
    for i, msg in enumerate(messages):
        record = create_record(None, msg, offset_delta=i, timestamp_delta=0)
        records_data.extend(encode_varint(len(record)))
        records_data.extend(record)
    
    batch = bytearray()
    batch.extend(struct.pack('>q', base_offset))  # Base offset
    batch_length_pos = len(batch)
    batch.extend(struct.pack('>i', 0))  # Batch length (placeholder)
    batch.extend(struct.pack('>i', -1))  # Partition leader epoch
    batch.append(2)  # Magic v2
    crc_pos = len(batch)
    batch.extend(struct.pack('>I', 0))  # CRC (placeholder)
    crc_start = len(batch)
    batch.extend(struct.pack('>h', 0))  # Attributes
    batch.extend(struct.pack('>i', len(messages) - 1))  # Last offset delta
    current_time_ms = int(time.time() * 1000)
    batch.extend(struct.pack('>q', current_time_ms))  # First timestamp
    batch.extend(struct.pack('>q', current_time_ms))  # Max timestamp
    batch.extend(struct.pack('>q', -1))  # Producer ID
    batch.extend(struct.pack('>h', -1))  # Producer epoch
    batch.extend(struct.pack('>i', -1))  # Base sequence
    batch.extend(struct.pack('>i', len(messages)))  # Record count
    batch.extend(records_data)
    
    # Fill in batch length
    batch_length = len(batch) - batch_length_pos - 4
    batch[batch_length_pos:batch_length_pos + 4] = struct.pack('>i', batch_length)
    
    # Calculate CRC
    crc_data = batch[crc_start:]
    crc = zlib.crc32(crc_data) & 0xffffffff
    batch[crc_pos:crc_pos + 4] = struct.pack('>I', crc)
    
    return bytes(batch)

def send_produce(host, port, topic, partition, messages):
    """Send a produce request."""
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.connect((host, port))
    
    # Build request
    header = struct.pack('>hhI', 0, 3, 1) + encode_string("test-producer")
    body = struct.pack('>h', -1)  # transactional_id
    body += struct.pack('>h', 1)  # acks
    body += struct.pack('>i', 30000)  # timeout
    body += struct.pack('>i', 1)  # topic count
    body += encode_string(topic)
    body += struct.pack('>i', 1)  # partition count
    body += struct.pack('>i', partition)
    
    batch = create_record_batch(messages)
    body += struct.pack('>i', len(batch))
    body += batch
    
    request = header + body
    sock.send(struct.pack('>i', len(request)))
    sock.send(request)
    
    # Read response
    response_size = struct.unpack('>i', sock.recv(4))[0]
    response = sock.recv(response_size)
    sock.close()
    
    return len(response) > 8

def check_segments(data_dir="./data"):
    """Check segment files and metadata."""
    # Check physical segment files
    segment_files = glob.glob(f"{data_dir}/segments/*/*/*.segment")
    
    # Check metadata
    metadata_path = f"{data_dir}/metadata/metadata.json"
    segments_in_metadata = 0
    if os.path.exists(metadata_path):
        with open(metadata_path, 'r') as f:
            metadata = json.load(f)
            segments_in_metadata = len(metadata.get('segments', {}))
    
    return len(segment_files), segments_in_metadata

def main():
    print("=== Testing Segment Rotation ===")
    
    # Check initial state
    files_before, meta_before = check_segments()
    print(f"\nInitial state: {files_before} segment files, {meta_before} in metadata")
    
    # Send messages to trigger rotation
    print("\nProducing messages to test rotation...")
    
    # Test time-based rotation (30 seconds)
    print("\n1. Testing time-based rotation (30 second threshold)...")
    
    # Send a batch of messages
    messages = [f"Time rotation test message {i}" for i in range(100)]
    if send_produce("localhost", 9092, "rotation-test", 0, messages):
        print(f"   Sent {len(messages)} messages")
    
    # Check segments
    files_after, meta_after = check_segments()
    print(f"   Segments: {files_after} files, {meta_after} in metadata")
    
    # Wait for rotation interval
    print("   Waiting 35 seconds for time-based rotation...")
    time.sleep(35)
    
    # Send another batch to trigger rotation
    messages = [f"After rotation message {i}" for i in range(100)]
    if send_produce("localhost", 9092, "rotation-test", 0, messages):
        print(f"   Sent {len(messages)} more messages")
    
    # Check if new segment was created
    files_final, meta_final = check_segments()
    print(f"   Final: {files_final} files, {meta_final} in metadata")
    
    if files_final > files_after:
        print("   ✓ Time-based rotation successful!")
    else:
        print("   ⚠ Time-based rotation may not have triggered")
    
    # Test size-based rotation
    print("\n2. Testing size-based rotation (256MB threshold)...")
    print("   Note: This would require sending ~256MB of data")
    print("   Skipping for now to avoid long test duration")
    
    print(f"\n=== Summary ===")
    print(f"Segment files created: {files_final - files_before}")
    print(f"Metadata entries added: {meta_final - meta_before}")
    
    # List all segment files
    print("\nSegment files:")
    for segment in sorted(glob.glob("./data/segments/*/*/*.segment")):
        stat = os.stat(segment)
        print(f"  {segment}: {stat.st_size} bytes")

if __name__ == "__main__":
    main()