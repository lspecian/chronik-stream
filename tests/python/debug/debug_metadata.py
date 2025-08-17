#!/usr/bin/env python3
"""Debug metadata response byte by byte."""

import struct

# Response from server
resp_hex = "000000010000000000000001000000010007302e302e302e3000002384ffff000e6368726f6e696b2d73747265616d0000000100000000"
response = bytes.fromhex(resp_hex)

print(f"Total response length: {len(response)} bytes")
print("Decoding byte by byte:\n")

offset = 0

# Correlation ID (4 bytes)
corr_id = struct.unpack('>I', response[offset:offset+4])[0]
print(f"[{offset:02d}-{offset+3:02d}] Correlation ID: {corr_id}")
offset += 4

# Next 4 bytes
next_val = struct.unpack('>i', response[offset:offset+4])[0]
print(f"[{offset:02d}-{offset+3:02d}] Next i32 value: {next_val}")
offset += 4

# Next 4 bytes
next_val2 = struct.unpack('>i', response[offset:offset+4])[0]
print(f"[{offset:02d}-{offset+3:02d}] Next i32 value: {next_val2}")
offset += 4

# Next 4 bytes
next_val3 = struct.unpack('>i', response[offset:offset+4])[0]
print(f"[{offset:02d}-{offset+3:02d}] Next i32 value: {next_val3}")
offset += 4

# Next 2 bytes as string length
str_len = struct.unpack('>h', response[offset:offset+2])[0]
print(f"[{offset:02d}-{offset+1:02d}] String length: {str_len}")
offset += 2

# String
if str_len > 0:
    string_val = response[offset:offset+str_len].decode('utf-8')
    print(f"[{offset:02d}-{offset+str_len-1:02d}] String value: '{string_val}'")
    offset += str_len

# Next 4 bytes
next_val4 = struct.unpack('>i', response[offset:offset+4])[0]
print(f"[{offset:02d}-{offset+3:02d}] Next i32 value: {next_val4}")
offset += 4

# Next 2 bytes
next_val5 = struct.unpack('>h', response[offset:offset+2])[0]
print(f"[{offset:02d}-{offset+1:02d}] Next i16 value: {next_val5}")
offset += 2

# Remaining bytes
remaining = response[offset:]
print(f"\nRemaining bytes ({len(remaining)}): {remaining.hex()}")