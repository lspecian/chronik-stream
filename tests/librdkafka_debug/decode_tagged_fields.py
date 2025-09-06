#!/usr/bin/env python3
"""Decode the tagged fields from librdkafka"""

import struct

def decode_varint(data, offset):
    """Decode a unsigned varint from data"""
    result = 0
    shift = 0
    while True:
        byte = data[offset]
        offset += 1
        result |= (byte & 0x7F) << shift
        if (byte & 0x80) == 0:
            break
        shift += 7
    return result, offset

# The captured bytes
data = bytes.fromhex("00 03 00 0c 00 00 00 02 00 0e 67 6f 2d 74 65 73 74 2d 63 6c 69 65 6e 74 00 02 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 0b 74 65 73 74 2d 74 6f 70 69 63 00 01 00 00".replace(" ", ""))

print("=== Parsing Header ===")
offset = 8  # Skip api_key, api_version, correlation_id

# Client ID
client_id_len = data[offset] - 1
offset += 1
print(f"Client ID: {'null' if client_id_len < 0 else 'present'}")

# Tagged fields
print(f"\n=== Tagged Fields at offset {offset} ===")
num_tagged_fields, offset = decode_varint(data, offset)
print(f"Number of tagged fields: {num_tagged_fields}")

for i in range(num_tagged_fields):
    tag_id, offset = decode_varint(data, offset)
    print(f"\nTagged field {i}:")
    print(f"  Tag ID: {tag_id}")
    
    # Data length
    data_len, offset = decode_varint(data, offset)
    print(f"  Data length: {data_len}")
    
    # Data
    field_data = data[offset:offset+data_len]
    offset += data_len
    print(f"  Data: {field_data.hex()}")
    print(f"  Data as string: {field_data}")

print(f"\n=== After Tagged Fields ===")
print(f"Offset: {offset}")
print(f"Next bytes: {data[offset:offset+10].hex()}")

# This should be the topics array
topics_byte = data[offset]
print(f"\nTopics array byte: 0x{topics_byte:02x}")
if topics_byte == 0x02:
    print("Topics array: 1 topic (compact array, 0x02 = length 1)")
    offset += 1
    
    # Topic 0
    # UUID (16 bytes)
    topic_id = data[offset:offset+16]
    offset += 16
    print(f"  Topic ID (UUID): {topic_id.hex()}")
    
    # Name (compact string)
    name_len = data[offset] - 1
    offset += 1
    if name_len >= 0:
        name = data[offset:offset+name_len].decode('utf-8')
        offset += name_len
        print(f"  Topic name: '{name}'")
    else:
        print(f"  Topic name: null")
    
    # Tagged fields
    topic_tagged = data[offset]
    offset += 1
    print(f"  Topic tagged fields: {topic_tagged}")

print(f"\n=== Remaining Body ===")
print(f"Offset: {offset}")
print(f"Remaining bytes: {data[offset:].hex()}")

# Parse remaining
allow_auto = data[offset]
offset += 1
print(f"Allow auto topic creation: {bool(allow_auto)}")

include_ops = data[offset]
offset += 1
print(f"Include topic authorized operations: {bool(include_ops)}")

body_tagged = data[offset]
offset += 1
print(f"Body tagged fields: {body_tagged}")

print(f"\nTotal parsed: {offset} of {len(data)} bytes")
