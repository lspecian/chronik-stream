#\!/usr/bin/env python3
"""Decode the metadata response."""

import struct

def decode_string(data, offset):
    """Decode a Kafka string (length + data)."""
    if offset + 2 > len(data):
        return None, offset
    
    length = struct.unpack('>h', data[offset:offset+2])[0]
    offset += 2
    
    if length == -1:
        return None, offset
    
    if offset + length > len(data):
        return None, offset
        
    string = data[offset:offset+length].decode('utf-8')
    return string, offset + length

def main():
    # The response hex from previous test
    response_hex = "000000010000000000000001000000010007302e302e302e3000002384ffff000e6368726f6e696b2d73747265616d0000000100000000"
    response = bytes.fromhex(response_hex)
    
    offset = 0
    
    # Correlation ID
    correlation_id = struct.unpack('>I', response[offset:offset+4])[0]
    offset += 4
    print(f"Correlation ID: {correlation_id}")
    
    # For v0, no throttle time
    
    # Broker count
    broker_count = struct.unpack('>I', response[offset:offset+4])[0]
    offset += 4
    print(f"Broker count: {broker_count}")
    
    # Brokers
    for i in range(broker_count):
        node_id = struct.unpack('>I', response[offset:offset+4])[0]
        offset += 4
        
        host, offset = decode_string(response, offset)
        port = struct.unpack('>I', response[offset:offset+4])[0]
        offset += 4
        
        print(f"  Broker {i}: node_id={node_id}, host={host}, port={port}")
    
    # For v0, no rack info
    
    # For v2+, cluster ID
    cluster_id, offset = decode_string(response, offset)
    print(f"Cluster ID: {cluster_id}")
    
    # Controller ID (v1+)
    controller_id = struct.unpack('>I', response[offset:offset+4])[0]
    offset += 4
    print(f"Controller ID: {controller_id}")
    
    # Topic count
    topic_count = struct.unpack('>I', response[offset:offset+4])[0]
    offset += 4
    print(f"Topic count: {topic_count}")
    
    print(f"Remaining bytes: {len(response) - offset}")

if __name__ == "__main__":
    main()
