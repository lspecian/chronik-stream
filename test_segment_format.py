#!/usr/bin/env python3
"""Test script to verify v2 segment format with dual storage"""

import struct
import json
import time
from kafka import KafkaProducer
from kafka.errors import KafkaError

def produce_messages():
    """Produce test messages to trigger segment creation"""
    producer = KafkaProducer(
        bootstrap_servers='localhost:9092',
        acks=1,
        value_serializer=lambda v: json.dumps(v).encode('utf-8')
    )
    
    print("Producing 5 test messages...")
    for i in range(5):
        message = {"id": i, "msg": f"Test message {i}"}
        future = producer.send('segment-test', value=message)
        try:
            record_metadata = future.get(timeout=10)
            print(f"  Message {i} sent to offset {record_metadata.offset}")
        except KafkaError as e:
            print(f"  Failed to send message {i}: {e}")
    
    producer.flush()
    print("All messages flushed")
    producer.close()
    
def check_segment_format():
    """Check if segments are created with v2 format"""
    import os
    import glob
    
    # Wait for segment to be written
    time.sleep(2)
    
    segment_files = glob.glob('data/segments/segment-test/0/*.segment')
    if not segment_files:
        print("No segment files found!")
        return False
    
    for segment_file in segment_files:
        print(f"\nChecking segment: {segment_file}")
        with open(segment_file, 'rb') as f:
            # Read header
            magic = f.read(4)
            if magic != b'CHRN':
                print(f"  Invalid magic: {magic}")
                continue
                
            version_bytes = f.read(2)
            version = struct.unpack('>H', version_bytes)[0]
            print(f"  Segment version: {version}")
            
            if version == 2:
                print("  ✓ V2 segment format detected!")
                
                # Read sizes from header
                metadata_size = struct.unpack('>I', f.read(4))[0]
                raw_kafka_size = struct.unpack('>Q', f.read(8))[0]
                indexed_data_size = struct.unpack('>Q', f.read(8))[0]
                index_size = struct.unpack('>Q', f.read(8))[0]
                
                print(f"  Metadata size: {metadata_size} bytes")
                print(f"  Raw Kafka batches: {raw_kafka_size} bytes")
                print(f"  Indexed records: {indexed_data_size} bytes")
                print(f"  Index data: {index_size} bytes")
                
                if raw_kafka_size > 0:
                    print("  ✓ Raw Kafka batches present!")
                    return True
                else:
                    print("  ✗ No raw Kafka batches found")
            else:
                print(f"  Still using v{version} format")
    
    return False

if __name__ == "__main__":
    print("=== Testing V2 Segment Format ===\n")
    
    # Produce messages
    produce_messages()
    
    # Check segment format
    if check_segment_format():
        print("\n✓ V2 segment format with dual storage is working!")
    else:
        print("\n✗ V2 segment format NOT detected - implementation incomplete")