#!/usr/bin/env python3
"""Fix the metadata response analysis."""

def analyze_kafka_response():
    """Analyze real Kafka response."""
    # Real Kafka response hex
    response = bytes.fromhex("0000002a00000001000000010009636c6f63616c686f73740000238700ff0000000100000000")
    
    print("=== Real Kafka Metadata Response (v1) ===")
    print(f"Total: {len(response)} bytes")
    
    offset = 0
    
    # Correlation ID
    corr_id = struct.unpack('>i', response[offset:offset+4])[0]
    print(f"[{offset:02d}] Correlation ID: {corr_id}")
    offset += 4
    
    # No throttle time in v1
    
    # Brokers array
    broker_count = struct.unpack('>i', response[offset:offset+4])[0]
    print(f"[{offset:02d}] Broker count: {broker_count}")
    offset += 4
    
    # Broker 0
    node_id = struct.unpack('>i', response[offset:offset+4])[0]
    print(f"[{offset:02d}] Broker ID: {node_id}")
    offset += 4
    
    # Host string
    host_len = struct.unpack('>h', response[offset:offset+2])[0]
    offset += 2
    host = response[offset:offset+host_len].decode('utf-8')
    print(f"[{offset-2:02d}] Host: {host} (len={host_len})")
    offset += host_len
    
    # Port
    port = struct.unpack('>i', response[offset:offset+4])[0]
    print(f"[{offset:02d}] Port: {port}")
    offset += 4
    
    # Rack (-1 = null)
    rack_len = struct.unpack('>h', response[offset:offset+2])[0]
    print(f"[{offset:02d}] Rack: null (len={rack_len})")
    offset += 2
    
    # Controller ID
    controller_id = struct.unpack('>i', response[offset:offset+4])[0]
    print(f"[{offset:02d}] Controller ID: {controller_id}")
    offset += 4
    
    # Topics array
    topic_count = struct.unpack('>i', response[offset:offset+4])[0]
    print(f"[{offset:02d}] Topic count: {topic_count}")
    offset += 4
    
    print(f"\nTotal consumed: {offset}/{len(response)} bytes")

def analyze_chronik_response():
    """Analyze chronik response."""
    # Chronik response hex
    response = bytes.fromhex("0000002a0000000000000001000000010007302e302e302e3000002384ffff000e6368726f6e696b2d73747265616d000000010000000")
    
    print("\n=== Chronik Metadata Response ===")
    print(f"Total: {len(response)} bytes")
    
    offset = 0
    
    # Correlation ID
    corr_id = struct.unpack('>i', response[offset:offset+4])[0]
    print(f"[{offset:02d}] Correlation ID: {corr_id}")
    offset += 4
    
    # THIS IS THE BUG! Chronik includes throttle time for v1
    throttle = struct.unpack('>i', response[offset:offset+4])[0]
    print(f"[{offset:02d}] Throttle time: {throttle}ms (SHOULD NOT BE HERE FOR v1!)")
    offset += 4
    
    # Brokers array
    broker_count = struct.unpack('>i', response[offset:offset+4])[0]
    print(f"[{offset:02d}] Broker count: {broker_count}")
    offset += 4
    
    # Continue parsing...
    print("\nThe issue: Chronik includes throttle_time_ms for v1, but Kafka doesn't!")
    print("Throttle time should only be included for v3+")

import struct

analyze_kafka_response()
analyze_chronik_response()