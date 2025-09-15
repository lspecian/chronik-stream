#!/usr/bin/env python3
"""Decode what we're actually sending"""

response = bytes.fromhex('000000010000003c00000000000900010000000d')

offset = 0

# Correlation ID
correlation_id = struct.unpack('>i', response[offset:offset+4])[0]
offset += 4
print(f"Correlation ID: {correlation_id}")

# The next bytes are: 0000003c
# As int32, this is 60 (which is our API count)
# So we're still sending api_versions array first!

next_int32 = struct.unpack('>i', response[offset:offset+4])[0]
offset += 4
print(f"Next int32: {next_int32} - This is the API count")

# So our code change didn't work. Let's verify what we actually changed:
print("\nConclusion: We're still sending api_versions array first, not error_code")
print("The fix didn't apply properly")