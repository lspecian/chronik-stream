#!/usr/bin/env python3
"""Decode the full Metadata v12 response"""

import binascii

def decode_response():
    # Read the captured response
    with open('/tmp/metadata_v12_resp2.bin', 'rb') as f:
        data = f.read()
    
    print(f"Total response size: {len(data)} bytes")
    print(f"First 50 bytes: {binascii.hexlify(data[:50]).decode()}")
    
    pos = 0
    
    # Response header
    print("\n=== RESPONSE HEADER ===")
    correlation_id = int.from_bytes(data[pos:pos+4], 'big')
    pos += 4
    print(f"Correlation ID: {correlation_id}")
    
    # For v12, should have tagged fields in header
    # BUT the next byte is 0x00, which would be an empty tagged field
    # Let's check what comes next
    
    print(f"\nBytes at position {pos}: {binascii.hexlify(data[pos:pos+10]).decode()}")
    
    # Try to decode as throttle time (INT32)
    throttle_time = int.from_bytes(data[pos:pos+4], 'big')
    print(f"Next 4 bytes as INT32: {throttle_time} (likely throttle_time)")
    pos += 4
    
    print(f"\nBytes at position {pos}: {binascii.hexlify(data[pos:pos+10]).decode()}")
    
    # This should be broker array
    # For flexible (v12), should be compact varint
    # But we see 00 02...
    
    # Try as INT16
    as_int16 = int.from_bytes(data[pos:pos+2], 'big')
    print(f"Next 2 bytes as INT16: {as_int16}")
    
    # Try as varint
    byte1 = data[pos]
    print(f"First byte: 0x{byte1:02x} = {byte1}")
    if byte1 & 0x80 == 0:
        print(f"This is a complete varint: value = {byte1}")
    
    # If this was 0x02 (single byte), it would mean array length 1 (compact encoding)
    # But we have 0x00 0x02 which is INT16 = 2
    
    print("\nCONCLUSION:")
    print("The response is using INT16 for broker count (0x00 0x02 = 2)")
    print("But for v12 (flexible), it should use compact varint (0x02 for 1 broker)")
    print("This confirms the encoding is using non-flexible path!")

decode_response()