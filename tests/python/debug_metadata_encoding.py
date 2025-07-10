#!/usr/bin/env python3
"""Debug the exact metadata encoding issue."""

import struct

# The actual response we got from chronik (fixed hex string)
chronik_hex = "0000002a0000000000000001000000010007302e302e302e3000002384ffff000e6368726f6e696b2d73747265616d0000000100000000"
chronik_response = bytes.fromhex(chronik_hex)

print("=== Chronik Response Byte-by-Byte ===")
print(f"Total: {len(chronik_response)} bytes\n")

offset = 0

# Correlation ID
corr_id = struct.unpack('>i', chronik_response[offset:offset+4])[0]
print(f"Offset {offset:02d}: Correlation ID = {corr_id} (0x{corr_id:08x})")
offset += 4

# Next 4 bytes
next_4 = struct.unpack('>i', chronik_response[offset:offset+4])[0]
print(f"Offset {offset:02d}: Next 4 bytes = {next_4} (0x{next_4:08x})")
offset += 4

# Next 4 bytes
next_4_2 = struct.unpack('>i', chronik_response[offset:offset+4])[0]
print(f"Offset {offset:02d}: Next 4 bytes = {next_4_2} (0x{next_4_2:08x})")
offset += 4

print("\nInterpretation:")
print("- If v1: Offset 4 should be broker count")
print("- If v3+: Offset 4 is throttle time, offset 8 is broker count")
print(f"\nWe see: offset 4 = {chronik_response[4:8].hex()} = {struct.unpack('>i', chronik_response[4:8])[0]}")
print(f"        offset 8 = {chronik_response[8:12].hex()} = {struct.unpack('>i', chronik_response[8:12])[0]}")
print("\nConclusion: The response includes throttle time (0) at offset 4!")

# Let's also check what the handler is actually seeing
print("\n=== What the handler sees ===")
print("The protocol handler gets the request and should see api_version=1")
print("But the response includes throttle_time_ms even though the check says version >= 3")
print("\nThis suggests either:")
print("1. The version check is being bypassed")
print("2. The response is being modified after encoding")
print("3. There's another encoding function being called")