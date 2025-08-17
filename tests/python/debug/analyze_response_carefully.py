#!/usr/bin/env python3
"""Analyze the response super carefully to find the issue."""

import struct

# Let's look at both v1 and v3 responses
v1_hex = "000000c90000000000000001000000010007302e302e302e3000002384ffff0000000100000000"
v3_hex = "000000cb0000000000000001000000010007302e302e302e3000002384ffff0000000100000000"

v1_bytes = bytes.fromhex(v1_hex)
v3_bytes = bytes.fromhex(v3_hex)

print("V1 and V3 responses are EXACTLY THE SAME except for correlation ID!")
print(f"V1 length: {len(v1_bytes)}")
print(f"V3 length: {len(v3_bytes)}")
print(f"Are they same length? {len(v1_bytes) == len(v3_bytes)}")

# Compare byte by byte
print("\nByte comparison (showing differences only):")
for i in range(len(v1_bytes)):
    if v1_bytes[i] != v3_bytes[i]:
        print(f"  Byte {i}: v1={v1_bytes[i]:02x}, v3={v3_bytes[i]:02x}")

print("\nThis means the SAME encoding is being used for both v1 and v3!")
print("The throttle_time_ms is ALWAYS being written as 0x00000000")
print("\nFor v3, this is correct (throttle_time_ms=0)")
print("For v1, this is WRONG (no throttle_time_ms field should exist)")

# Let's see what the response SHOULD look like for v1
print("\n\nWhat v1 SHOULD be:")
correct_v1 = bytearray()
correct_v1.extend(struct.pack('>I', 0xc9))  # correlation id

# NO throttle_time_ms here!

correct_v1.extend(struct.pack('>i', 1))  # broker count
correct_v1.extend(struct.pack('>i', 1))  # node id  
correct_v1.extend(struct.pack('>h', 7))  # host len
correct_v1.extend(b'0.0.0.0')
correct_v1.extend(struct.pack('>i', 9092))  # port
correct_v1.extend(struct.pack('>h', -1))  # rack null

correct_v1.extend(struct.pack('>i', 1))  # controller id
correct_v1.extend(struct.pack('>i', 0))  # topic count

print(f"Correct v1 would be {len(correct_v1)} bytes (actual is {len(v1_bytes)} bytes)")
print(f"Difference: {len(v1_bytes) - len(correct_v1)} bytes (the 4-byte throttle_time_ms!)")