#!/usr/bin/env python3
"""Test segment rotation by producing messages and monitoring segment files."""

import socket
import struct
import time
import os
import glob

def encode_string(s):
    """Encode string in Kafka format (length prefix + bytes)"""
    if s is None:
        return struct.pack('>h', -1)
    encoded = s.encode('utf-8')
    return struct.pack('>h', len(encoded)) + encoded

def send_produce(host, port, topic, partition, messages):
    """Send a produce request with messages"""
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.connect((host, port))
    
    api_key = 0  # Produce
    api_version = 3  # Use v3 for simplicity
    correlation_id = 1
    client_id = "test-producer"
    
    # Request header
    header = struct.pack('>hhI', api_key, api_version, correlation_id) + encode_string(client_id)
    
    # Request body
    body = struct.pack('>h', -1)  # transactional_id (null)
    body += struct.pack('>h', 1000)  # acks (1 = leader ack)
    body += struct.pack('>i', 30000)  # timeout_ms
    
    # Topics array
    body += struct.pack('>i', 1)  # topic count
    body += encode_string(topic)
    
    # Partitions array
    body += struct.pack('>i', 1)  # partition count
    body += struct.pack('>i', partition)  # partition index
    
    # Create record batch (simplified)
    records = b''
    for msg in messages:
        msg_bytes = msg.encode('utf-8')
        # Simplified record format
        record = struct.pack('>b', 0)  # attributes
        record += struct.pack('>q', 0)  # timestamp delta
        record += struct.pack('>i', 0)  # offset delta
        record += struct.pack('>i', -1)  # key length (null)
        record += struct.pack('>i', len(msg_bytes))  # value length
        record += msg_bytes
        records += record
    
    # Record batch header
    batch = struct.pack('>q', 0)  # base offset
    batch += struct.pack('>i', len(records) + 49)  # batch length
    batch += struct.pack('>i', -1)  # partition leader epoch
    batch += struct.pack('>b', 2)  # magic
    batch += struct.pack('>i', 0)  # crc (placeholder)
    batch += struct.pack('>h', 0)  # attributes
    batch += struct.pack('>i', len(messages) - 1)  # last offset delta
    batch += struct.pack('>q', int(time.time() * 1000))  # first timestamp
    batch += struct.pack('>q', int(time.time() * 1000))  # max timestamp
    batch += struct.pack('>q', -1)  # producer id
    batch += struct.pack('>h', -1)  # producer epoch
    batch += struct.pack('>i', -1)  # base sequence
    batch += struct.pack('>i', len(messages))  # record count
    batch += records
    
    body += struct.pack('>i', len(batch))  # records size
    body += batch
    
    # Send request
    request = header + body
    sock.send(struct.pack('>i', len(request)))
    sock.send(request)
    
    # Read response
    response_size = struct.unpack('>i', sock.recv(4))[0]
    response = sock.recv(response_size)
    
    sock.close()
    
    # Parse response to check for errors
    pos = 4  # skip correlation_id
    topic_count = struct.unpack('>i', response[pos:pos+4])[0]
    if topic_count > 0:
        print(f"  Produced {len(messages)} messages to {topic}:{partition}")
        return True
    return False

def check_segments(data_dir="./data"):
    """Check segment files in the data directory"""
    segment_pattern = f"{data_dir}/segments/*/*/*.segment"
    segments = glob.glob(segment_pattern)
    
    segment_info = []
    for segment in segments:
        stat = os.stat(segment)
        segment_info.append({
            'path': segment,
            'size': stat.st_size,
            'mtime': stat.st_mtime
        })
    
    return segment_info

def main():
    print("=== Testing Segment Rotation ===")
    
    # Check initial segments
    initial_segments = check_segments()
    print(f"Initial segments: {len(initial_segments)}")
    for seg in initial_segments:
        print(f"  {seg['path']}: {seg['size']} bytes")
    
    # Produce messages over time to trigger rotation
    print("\nProducing messages to trigger rotation...")
    
    # The rotation happens after 30 seconds or 256MB
    # We'll produce messages and wait to trigger time-based rotation
    
    for i in range(5):
        messages = [f"Test message {i}-{j}" for j in range(100)]
        send_produce("localhost", 9092, "rotation-test", 0, messages)
        print(f"Batch {i+1}: Sent 100 messages")
        
        # Check segments
        current_segments = check_segments()
        if len(current_segments) > len(initial_segments):
            print(f"  ✓ New segment created! Total segments: {len(current_segments)}")
            initial_segments = current_segments
        
        # Wait to potentially trigger time-based rotation (30 seconds)
        if i == 2:
            print("  Waiting 35 seconds to trigger time-based rotation...")
            time.sleep(35)
        else:
            time.sleep(2)
    
    # Final check
    final_segments = check_segments()
    print(f"\nFinal segments: {len(final_segments)}")
    for seg in final_segments:
        print(f"  {seg['path']}: {seg['size']} bytes")
    
    if len(final_segments) > len(initial_segments):
        print("\n✓ Segment rotation is working!")
    else:
        print("\n⚠ No new segments created - rotation may not be working")

if __name__ == "__main__":
    main()