#!/usr/bin/env python3
"""Decode metadata response in detail."""

import struct

def decode_metadata_v1(response):
    """Decode metadata response v1."""
    offset = 0
    
    # Correlation ID
    corr_id = struct.unpack('>i', response[offset:offset+4])[0]
    offset += 4
    print(f"Correlation ID: {corr_id}")
    
    # The rest depends on if there's a throttle time
    # For v1, there's no throttle time, so next is brokers array
    
    # Brokers array
    broker_count = struct.unpack('>i', response[offset:offset+4])[0]
    offset += 4
    print(f"Broker count: {broker_count}")
    
    for i in range(broker_count):
        node_id = struct.unpack('>i', response[offset:offset+4])[0]
        offset += 4
        
        host_len = struct.unpack('>h', response[offset:offset+2])[0]
        offset += 2
        host = response[offset:offset+host_len].decode('utf-8')
        offset += host_len
        
        port = struct.unpack('>i', response[offset:offset+4])[0]
        offset += 4
        
        # Rack (v1+)
        rack_len = struct.unpack('>h', response[offset:offset+2])[0]
        offset += 2
        if rack_len >= 0:
            rack = response[offset:offset+rack_len].decode('utf-8')
            offset += rack_len
        else:
            rack = None
            
        print(f"  Broker {i}: ID={node_id}, Host={host}, Port={port}, Rack={rack}")
    
    # Cluster ID (v2+) - not in v1
    # Controller ID (v1+)
    controller_id = struct.unpack('>i', response[offset:offset+4])[0]
    offset += 4
    print(f"Controller ID: {controller_id}")
    
    # Topics array
    topic_count = struct.unpack('>i', response[offset:offset+4])[0]
    offset += 4
    print(f"Topic count: {topic_count}")
    
    print(f"\nBytes consumed: {offset}/{len(response)}")
    print(f"Remaining bytes: {response[offset:].hex()}")

# Test with the response we got
response_hex = "000000020000000000000001000000010007302e302e302e3000002384ffff000000000100000000"
response = bytes.fromhex(response_hex)

print("Decoding Metadata v1 response:")
decode_metadata_v1(response)