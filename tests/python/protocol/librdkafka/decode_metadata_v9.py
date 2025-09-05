#!/usr/bin/env python3
"""Decode the captured Metadata v9 requests"""

import binascii

def decode_request(hex_str, request_num):
    data = bytes.fromhex(hex_str)
    pos = 0
    
    print(f"\n=== Request #{request_num} ===")
    
    # API key (2 bytes)
    api_key = int.from_bytes(data[pos:pos+2], 'big')
    pos += 2
    print(f"API Key: {api_key} (Metadata)")
    
    # API version (2 bytes)
    api_version = int.from_bytes(data[pos:pos+2], 'big')
    pos += 2
    print(f"API Version: {api_version}")
    
    # Correlation ID (4 bytes)
    correlation_id = int.from_bytes(data[pos:pos+4], 'big')
    pos += 4
    print(f"Correlation ID: {correlation_id}")
    
    # Client ID (normal string, not compact, even for v9!)
    client_id_len = int.from_bytes(data[pos:pos+2], 'big')
    pos += 2
    client_id = data[pos:pos+client_id_len].decode('utf-8')
    pos += client_id_len
    print(f"Client ID: '{client_id}' (len={client_id_len})")
    
    # Tagged fields for header (v9 is flexible)
    header_tagged_fields = data[pos]
    pos += 1
    print(f"Header tagged fields: {header_tagged_fields}")
    
    # Topics array (compact array for v9)
    topics_byte = data[pos]
    pos += 1
    print(f"Topics byte: 0x{topics_byte:02x}")
    
    if topics_byte == 0:
        print("Topics: NULL (all topics)")
    elif topics_byte == 1:
        print("Topics: Empty array")
    else:
        topic_count = topics_byte - 1
        print(f"Topics: {topic_count} topics")
        # Would parse topics here if present
    
    # For v4+: allow_auto_topic_creation
    if pos < len(data):
        allow_auto_create = data[pos]
        pos += 1
        print(f"Allow auto topic creation: {bool(allow_auto_create)}")
    
    # For v8+: include_cluster_authorized_operations (missing in request #1!)
    if api_version >= 8 and pos < len(data):
        include_cluster = data[pos]
        pos += 1
        print(f"Include cluster authorized ops: {bool(include_cluster)}")
    
    # For v8+: include_topic_authorized_operations (missing in request #1!)
    if api_version >= 8 and pos < len(data):
        include_topic = data[pos]
        pos += 1
        print(f"Include topic authorized ops: {bool(include_topic)}")
    
    # Tagged fields for body
    if pos < len(data):
        body_tagged_fields = data[pos]
        pos += 1
        print(f"Body tagged fields: {body_tagged_fields}")
    
    if pos < len(data):
        print(f"WARNING: {len(data) - pos} unparsed bytes remain!")
    
    return pos == len(data)

# Decode the captured requests
req1 = "0003000900000002000f746573742d6c696272646b61666b61000100000000"
req2 = "0003000900000003000f746573742d6c696272646b61666b61000000000001000000"

decode_request(req1, 1)
decode_request(req2, 2)