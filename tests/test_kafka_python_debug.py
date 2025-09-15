#!/usr/bin/env python3
"""Debug kafka-python ApiVersionsResponse parsing"""

from kafka.protocol.admin import ApiVersionResponse_v0
from kafka.protocol.types import Array, Int16, Int32, Schema
import io

# Check the schema
print("ApiVersionResponse_v0 SCHEMA:")
print(ApiVersionResponse_v0.SCHEMA)

# Our response bytes (without the 4-byte length prefix)
response_bytes = bytes.fromhex('000000010000003c00000000000900010000000d00020000000700030000000c00040000000000050000000000060000000000070000000000080000000800090000000800' +
    '0a00000004000b00000009000c00000004000d00000005000e00000005000f00000005001000000004001100000001001200000004001300000007001400000006' +
    '00150000000000160000000000170000000000180000000000190000000000' + '1a000000001b000000001c000000001d000000001e000000001f00000000' +
    '2000000004002100000002002200000000002300000000002400000000002500000000002600000000002700000000002800000000002900000000002a00000000' +
    '002b00000000002c00000000002d00000000002e00000000002f000000000030000000000031000000000032000000000033000000000038000000000039000000' +
    '00003a0000000000003c0000000000003d000000000041000000000042000000000043000000000000')

# Try to decode manually
buffer = io.BytesIO(response_bytes)

# Skip correlation_id (already read by kafka-python)
correlation_id = int.from_bytes(buffer.read(4), 'big', signed=True)
print(f"\nCorrelation ID: {correlation_id}")

# According to kafka-python ApiVersionResponse_v0 schema:
# The schema is: ('error_code', Int16), ('api_versions', Array(('api_key', Int16), ('min_version', Int16), ('max_version', Int16)))
# So it expects error_code FIRST, then api_versions array

# But we're sending api_versions array first, then error_code (which is correct for Kafka protocol v0)

print("\nWhat kafka-python expects for v0:")
print("1. error_code (Int16)")
print("2. api_versions (Array)")

print("\nWhat we're sending:")
print("1. api_versions (Array)")
print("2. error_code (Int16)")

print("\nThis is the issue! kafka-python has the wrong field order for ApiVersionResponse_v0.")