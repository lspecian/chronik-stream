#!/usr/bin/env python3
"""Analyze the captured Metadata v12 request"""

import struct

# The captured bytes
data = bytes.fromhex("00 03 00 0c 00 00 00 02 00 0e 67 6f 2d 74 65 73 74 2d 63 6c 69 65 6e 74 00 02 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 0b 74 65 73 74 2d 74 6f 70 69 63 00 01 00 00".replace(" ", ""))

print("Total bytes:", len(data))
print("\n=== HEADER ===")

# Parse header
offset = 0
api_key = struct.unpack('>h', data[offset:offset+2])[0]
offset += 2
print(f"API Key: {api_key} (Metadata)")

api_version = struct.unpack('>h', data[offset:offset+2])[0]
offset += 2
print(f"API Version: {api_version}")

correlation_id = struct.unpack('>i', data[offset:offset+4])[0]
offset += 4
print(f"Correlation ID: {correlation_id}")

print(f"Header consumed: {offset} bytes")

# Client ID (compact string in v9+)
print("\n=== CLIENT ID ===")
client_id_len = data[offset] - 1  # Compact string: actual length = byte - 1
offset += 1
print(f"Client ID length byte: 0x{data[offset-1]:02x} ({data[offset-1]}), actual length: {client_id_len}")

if client_id_len >= 0:
    client_id = data[offset:offset+client_id_len].decode('utf-8')
    offset += client_id_len
    print(f"Client ID: '{client_id}'")
else:
    print("Client ID: null")

# Tagged fields
print("\n=== TAGGED FIELDS (header) ===")
tagged_fields = data[offset]
offset += 1
print(f"Tagged fields byte: 0x{tagged_fields:02x} ({tagged_fields})")

print(f"\nTotal header size: {offset} bytes")

# Now the request body
print("\n=== REQUEST BODY ===")
print(f"Remaining bytes for body: {len(data) - offset}")

# Topics array (compact array in v9+)
print("\n--- Topics Array ---")
topics_len_byte = data[offset]
print(f"Topics array byte: 0x{topics_len_byte:02x} ({topics_len_byte})")
offset += 1

if topics_len_byte == 0:
    print("Topics: NULL (fetch all topics)")
    topics_count = -1
else:
    topics_count = topics_len_byte - 1
    print(f"Topics count: {topics_count}")
    
    for i in range(topics_count):
        print(f"\n  Topic {i}:")
        # v10+ has topic_id (UUID)
        topic_id = data[offset:offset+16]
        offset += 16
        print(f"    Topic ID (UUID): {topic_id.hex()}")
        
        # Topic name (compact string)
        name_len = data[offset] - 1
        offset += 1
        if name_len >= 0:
            topic_name = data[offset:offset+name_len].decode('utf-8')
            offset += name_len
            print(f"    Topic name: '{topic_name}'")
        else:
            print(f"    Topic name: null")
        
        # Tagged fields for topic
        topic_tagged = data[offset]
        offset += 1
        print(f"    Tagged fields: {topic_tagged}")

# allow_auto_topic_creation 
print("\n--- Allow Auto Topic Creation ---")
allow_auto = data[offset]
offset += 1
print(f"Allow auto topic creation: {bool(allow_auto)}")

# include_topic_authorized_operations (v8+)
print("\n--- Include Topic Authorized Operations ---")
include_ops = data[offset]
offset += 1
print(f"Include topic authorized operations: {bool(include_ops)}")

# Tagged fields for body
print("\n--- Tagged Fields (body) ---")
body_tagged = data[offset]
offset += 1
print(f"Body tagged fields: {body_tagged}")

print(f"\nTotal bytes consumed: {offset}")
print(f"Remaining bytes: {len(data) - offset}")

if offset < len(data):
    print("\nUnparsed bytes:", data[offset:].hex())
