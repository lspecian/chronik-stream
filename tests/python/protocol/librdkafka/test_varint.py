#!/usr/bin/env python3
"""Test varint encoding"""

def write_unsigned_varint(value):
    """Write an unsigned varint"""
    result = []
    while True:
        byte = value & 0x7F
        value >>= 7
        if value != 0:
            byte |= 0x80
        result.append(byte)
        if value == 0:
            break
    return bytes(result)

def write_compact_array_len(length):
    """Write a compact array length (length + 1)"""
    return write_unsigned_varint(length + 1)

# Test encoding 1 broker
encoded = write_compact_array_len(1)
print(f"Encoded 1 broker as: {encoded.hex()}")
print(f"Bytes: {list(encoded)}")

# Should be 0x02 (1 + 1 = 2)
assert encoded == b'\x02', f"Expected 0x02, got {encoded.hex()}"

# Test encoding 0 brokers
encoded = write_compact_array_len(0)
print(f"Encoded 0 brokers as: {encoded.hex()}")
assert encoded == b'\x01', f"Expected 0x01, got {encoded.hex()}"

print("\nAll tests passed! Compact encoding is correct.")
print("\nSo the issue is NOT with the varint encoding itself.")