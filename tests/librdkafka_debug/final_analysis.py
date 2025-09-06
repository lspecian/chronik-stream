#!/usr/bin/env python3
"""Final analysis of the librdkafka Metadata v12 format"""

# The captured bytes
data = bytes.fromhex("00 03 00 0c 00 00 00 02 00 0e 67 6f 2d 74 65 73 74 2d 63 6c 69 65 6e 74 00 02 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 0b 74 65 73 74 2d 74 6f 70 69 63 00 01 00 00".replace(" ", ""))

print("Total: {} bytes".format(len(data)))
print("\nByte-by-byte analysis:")
print("Offset | Hex  | Dec | ASCII | Field")
print("-------|------|-----|-------|------")

offset = 0
# Header
print(f"{offset:6d} | 0x{data[offset]:02x} | {data[offset]:3d} |   {chr(data[offset]) if 32 <= data[offset] < 127 else ' '}   | api_key high byte")
offset += 1
print(f"{offset:6d} | 0x{data[offset]:02x} | {data[offset]:3d} |   {chr(data[offset]) if 32 <= data[offset] < 127 else ' '}   | api_key low byte = 3 (Metadata)")
offset += 1
print(f"{offset:6d} | 0x{data[offset]:02x} | {data[offset]:3d} |   {chr(data[offset]) if 32 <= data[offset] < 127 else ' '}   | api_version high byte")
offset += 1
print(f"{offset:6d} | 0x{data[offset]:02x} | {data[offset]:3d} |   {chr(data[offset]) if 32 <= data[offset] < 127 else ' '}   | api_version low byte = 12")
offset += 1
print(f"{offset:6d} | 0x{data[offset]:02x} | {data[offset]:3d} |   {chr(data[offset]) if 32 <= data[offset] < 127 else ' '}   | correlation_id byte 1")
offset += 1
print(f"{offset:6d} | 0x{data[offset]:02x} | {data[offset]:3d} |   {chr(data[offset]) if 32 <= data[offset] < 127 else ' '}   | correlation_id byte 2")
offset += 1
print(f"{offset:6d} | 0x{data[offset]:02x} | {data[offset]:3d} |   {chr(data[offset]) if 32 <= data[offset] < 127 else ' '}   | correlation_id byte 3")
offset += 1
print(f"{offset:6d} | 0x{data[offset]:02x} | {data[offset]:3d} |   {chr(data[offset]) if 32 <= data[offset] < 127 else ' '}   | correlation_id byte 4 = 2")
offset += 1

print("\n--- ISSUE STARTS HERE ---")
print(f"{offset:6d} | 0x{data[offset]:02x} | {data[offset]:3d} |   {chr(data[offset]) if 32 <= data[offset] < 127 else ' '}   | client_id compact string length (0x00 = null)")
offset += 1

# THIS IS THE BUG! librdkafka sends the actual client_id string AFTER the null byte
print(f"{offset:6d} | 0x{data[offset]:02x} | {data[offset]:3d} |   {chr(data[offset]) if 32 <= data[offset] < 127 else ' '}   | SHOULD BE tagged fields, but it's 0x0e (14)")
offset += 1

# The next 14 bytes are "go-test-client"
for i in range(14):
    print(f"{offset:6d} | 0x{data[offset]:02x} | {data[offset]:3d} |   {chr(data[offset]) if 32 <= data[offset] < 127 else ' '}   | 'go-test-client'[{i}]")
    offset += 1

print(f"{offset:6d} | 0x{data[offset]:02x} | {data[offset]:3d} |   {chr(data[offset]) if 32 <= data[offset] < 127 else ' '}   | Tagged fields (should be here)")
offset += 1

print("\n--- Topics array starts ---")
print(f"{offset:6d} | 0x{data[offset]:02x} | {data[offset]:3d} |   {chr(data[offset]) if 32 <= data[offset] < 127 else ' '}   | Topics array: 0x02 = 1 topic")
offset += 1

print("\nTheory: librdkafka is sending:")
print("1. client_id as null (0x00)")
print("2. Then STILL sending the actual client_id string! (0x0e + 'go-test-client')")
print("3. This is a bug in librdkafka's encoding")

print("\nWhat librdkafka sends:")
print("  0x00 (null) + 0x0e + 'go-test-client' + 0x00 (tagged)")
print("\nWhat it SHOULD send:")
print("  0x0f + 'go-test-client' + 0x00 (tagged)")
print("  OR")
print("  0x00 (null) + 0x00 (tagged)")
