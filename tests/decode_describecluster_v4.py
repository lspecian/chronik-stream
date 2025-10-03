#!/usr/bin/env python3
import struct

# DescribeCluster v4 request from KSQL (from packet capture)
request_hex = "000000210020000400000004000d61646d696e636c69656e742d3400020402310000000000"
request_data = bytes.fromhex(request_hex)

# Debug: show the actual bytes
print("Raw bytes:")
for i in range(0, len(request_data), 16):
    hex_part = request_data[i:i+16].hex(' ')
    ascii_part = ''.join(chr(b) if 32 <= b < 127 else '.' for b in request_data[i:i+16])
    print(f"  {i:04x}: {hex_part:<48} {ascii_part}")
print()

print("DescribeCluster v4 Request Analysis:")
print("=" * 50)

# Parse the request
offset = 0
length = struct.unpack('>I', request_data[offset:offset+4])[0]
offset += 4
print(f"Message length: {length}")

api_key = struct.unpack('>H', request_data[offset:offset+2])[0]
offset += 2
print(f"API key: {api_key} (DescribeCluster)")

api_version = struct.unpack('>H', request_data[offset:offset+2])[0]
offset += 2
print(f"API version: {api_version}")

correlation_id = struct.unpack('>I', request_data[offset:offset+4])[0]
offset += 4
print(f"Correlation ID: {correlation_id}")

# Client ID - appears to be non-flexible string (2-byte length prefix)
client_id_len = struct.unpack('>H', request_data[offset:offset+2])[0]
offset += 2
if client_id_len > 0:
    client_id = request_data[offset:offset+client_id_len].decode('utf-8')
    offset += client_id_len
else:
    client_id = None
print(f"Client ID: '{client_id}' (length={client_id_len})")

# Tagged fields (empty = 0x00)
tagged_fields = request_data[offset]
offset += 1
print(f"Tagged fields: {tagged_fields:#x}")

# DescribeCluster v4 specific fields
# includeClusterAuthorizedOperations (bool)
include_ops = request_data[offset]
offset += 1
print(f"Include cluster authorized operations: {include_ops}")

# endpointType (int8) - new in v1+
endpoint_type = request_data[offset]
offset += 1
print(f"Endpoint type: {endpoint_type}")

# Tagged fields
tagged_fields2 = request_data[offset]
offset += 1
print(f"Tagged fields: {tagged_fields2:#x}")

print(f"\nRemaining bytes: {request_data[offset:].hex()}")
print(f"Total parsed: {offset} bytes out of {len(request_data)}")

print("\n" + "=" * 50)
print("What DescribeCluster v4 expects in response:")
print("- Correlation ID (int32)")
print("- Tagged fields header (compact - 0x00 for empty)")
print("- Error code (int16)")
print("- Error message (compact string)")
print("- Cluster ID (compact string)")
print("- Controller ID (int32)")
print("- Cluster authorized operations (int32) - if requested")
print("- Brokers array (compact array):")
print("  - Each broker:")
print("    - Node ID (int32)")
print("    - Host (compact string)")
print("    - Port (int32)")
print("    - Rack (compact string)")
print("    - Tagged fields (compact)")
print("- Tagged fields (compact)")