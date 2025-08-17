#!/usr/bin/env python3
"""Debug metadata response with correct structure."""

import struct

# Response from server
resp_hex = "000000010000000000000001000000010007302e302e302e3000002384ffff000e6368726f6e696b2d73747265616d0000000100000000"
response = bytes.fromhex(resp_hex)

print(f"Total response length: {len(response)} bytes")
print("\nExpected structure for Metadata v1 response:")
print("- Correlation ID (4 bytes)")
print("- Brokers array length (4 bytes)")  
print("- For each broker:")
print("  - Node ID (4 bytes)")
print("  - Host string (2 bytes length + string)")
print("  - Port (4 bytes)")
print("  - Rack string (2 bytes length + string)")
print("- Controller ID (4 bytes)")
print("- Topics array...")
print("\nActual decoding:\n")

offset = 0

# Correlation ID (4 bytes)
corr_id = struct.unpack('>I', response[offset:offset+4])[0]
print(f"[{offset:02d}-{offset+3:02d}] Correlation ID: {corr_id}")
offset += 4

# What if this extra 0 is something else?
mystery_val = struct.unpack('>i', response[offset:offset+4])[0]
print(f"[{offset:02d}-{offset+3:02d}] MYSTERY VALUE: {mystery_val} <- This shouldn't be here!")
offset += 4

# Brokers array length
broker_count = struct.unpack('>i', response[offset:offset+4])[0]
print(f"[{offset:02d}-{offset+3:02d}] Broker count: {broker_count}")
offset += 4

# Broker 0
node_id = struct.unpack('>i', response[offset:offset+4])[0]
print(f"[{offset:02d}-{offset+3:02d}] Broker[0].node_id: {node_id}")
offset += 4

# Host string
host_len = struct.unpack('>h', response[offset:offset+2])[0]
print(f"[{offset:02d}-{offset+1:02d}] Broker[0].host length: {host_len}")
offset += 2

host = response[offset:offset+host_len].decode('utf-8')
print(f"[{offset:02d}-{offset+host_len-1:02d}] Broker[0].host: '{host}'")
offset += host_len

# Port
port = struct.unpack('>i', response[offset:offset+4])[0]
print(f"[{offset:02d}-{offset+3:02d}] Broker[0].port: {port}")
offset += 4

# Rack
rack_len = struct.unpack('>h', response[offset:offset+2])[0]
print(f"[{offset:02d}-{offset+1:02d}] Broker[0].rack length: {rack_len} (-1 = null)")
offset += 2

print(f"\nProblem identified: There's an extra 4-byte value (0) after correlation ID!")
print("This is shifting all subsequent data by 4 bytes, causing Sarama to misparse the response.")