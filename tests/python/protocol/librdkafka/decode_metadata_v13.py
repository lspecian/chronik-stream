#!/usr/bin/env python3
"""Decode the captured Metadata v13 requests"""

import binascii

def decode_request(hex_str, request_num):
    data = bytes.fromhex(hex_str)
    pos = 0
    
    print(f"\n=== Request #{request_num} ===")
    print(f"Total size: {len(data)} bytes")
    
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
    
    # Client ID - THIS IS THE KEY QUESTION!
    # For v13 (flexible), should this be compact string or normal string?
    print(f"\nBytes at position {pos}: {data[pos:pos+4].hex()}")
    
    # Try normal string first (like librdkafka seems to use)
    client_id_len = int.from_bytes(data[pos:pos+2], 'big')
    pos += 2
    print(f"Client ID length (as INT16): {client_id_len}")
    
    if client_id_len > 0 and client_id_len < 100:
        client_id = data[pos:pos+client_id_len].decode('utf-8')
        pos += client_id_len
        print(f"Client ID: '{client_id}'")
    else:
        print("Client ID length seems wrong, might be compact string")
        return
    
    # Header tagged fields (for flexible versions)
    header_tagged = data[pos]
    pos += 1
    print(f"Header tagged fields: {header_tagged}")
    
    # Topics array
    print(f"\nTopic array byte at {pos}: 0x{data[pos]:02x}")
    topics_byte = data[pos]
    pos += 1
    
    if topics_byte == 0:
        print("Topics: NULL (all topics)")
    elif topics_byte == 1:
        print("Topics: Empty array")
    else:
        topic_count = topics_byte - 1
        print(f"Topics: {topic_count} topics")
        if topic_count == 1:
            # Read topic name (compact string)
            topic_len = data[pos] - 1
            pos += 1
            if topic_len > 0:
                topic_name = data[pos:pos+topic_len].decode('utf-8')
                pos += topic_len
                print(f"  Topic name: '{topic_name}'")
            # Topic ID (UUID - 16 bytes)
            topic_id = data[pos:pos+16].hex()
            pos += 16
            print(f"  Topic ID: {topic_id}")
    
    # include_cluster_authorized_operations (v8+)
    if pos < len(data):
        include_cluster = data[pos]
        pos += 1
        print(f"Include cluster authorized ops: {bool(include_cluster)}")
    
    # Body tagged fields
    if pos < len(data):
        body_tagged = data[pos]
        pos += 1
        print(f"Body tagged fields: {body_tagged}")
    
    if pos < len(data):
        print(f"\nWARNING: {len(data) - pos} unparsed bytes remain!")
        print(f"Remaining: {data[pos:].hex()}")
    
    return pos == len(data)

# Decode the captured requests
reqs = [
    ("0003000d00000002000f746573742d6c696272646b61666b610001000000", 1),
    ("0003000d00000003000f746573742d6c696272646b61666b610000000000010000", 2),
    ("0003000d00000004000f746573742d6c696272646b61666b610002000000000000000000000000000000000b746573742d746f70696300010000", 3),
]

for hex_str, num in reqs:
    decode_request(hex_str, num)