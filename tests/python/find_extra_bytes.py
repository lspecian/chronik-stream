#!/usr/bin/env python3
"""Find where the extra bytes come from."""

import socket
import struct

# First, let's manually construct what the response SHOULD be for v1
def build_expected_v1_response():
    response = b''
    
    # Correlation ID
    response += struct.pack('>I', 201)  # 0x000000c9
    
    # NO throttle_time_ms for v1!
    
    # Brokers array
    response += struct.pack('>i', 1)  # broker count
    response += struct.pack('>i', 1)  # node_id
    response += struct.pack('>h', 7)  # host length
    response += b'0.0.0.0'            # host
    response += struct.pack('>i', 9092)  # port
    response += struct.pack('>h', -1)    # rack (null)
    
    # Controller ID (v1+)
    response += struct.pack('>i', 1)
    
    # Topics array
    response += struct.pack('>i', 0)  # empty topics
    
    return response

def compare_responses():
    # Expected v1 response
    expected = build_expected_v1_response()
    print(f"Expected v1 response ({len(expected)} bytes):")
    print(f"Hex: {expected.hex()}")
    
    # Actual response from our previous test
    actual_hex = "000000c90000000000000001000000010007302e302e302e3000002384ffff0000000100000000"
    actual = bytes.fromhex(actual_hex)
    print(f"\nActual v1 response ({len(actual)} bytes):")
    print(f"Hex: {actual_hex}")
    
    # Find differences
    print("\nByte-by-byte comparison:")
    for i in range(min(len(expected), len(actual))):
        if i < len(expected) and i < len(actual):
            if expected[i] != actual[i]:
                print(f"  Byte {i}: expected {expected[i]:02x}, got {actual[i]:02x} *** DIFFERENT ***")
            elif i % 4 == 0:  # Show every 4th byte for context
                print(f"  Byte {i}: {expected[i]:02x} (both match)")
    
    # Show where extra bytes appear
    print(f"\nExpected length: {len(expected)}")
    print(f"Actual length: {len(actual)}")
    print(f"Difference: {len(actual) - len(expected)} bytes")
    
    if len(actual) > len(expected):
        print("\nExtra bytes in actual response:")
        # The extra 4 bytes should be at position 4 (after correlation ID)
        print(f"Bytes 4-7 in actual: {actual[4:8].hex()} (these are the extra zeros!)")

if __name__ == "__main__":
    compare_responses()