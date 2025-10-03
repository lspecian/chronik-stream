#!/usr/bin/env python3
import struct
import sys

def find_kafka_messages(data, name):
    """Find Kafka protocol messages in packet capture data"""
    messages = []

    # Look for patterns that indicate Kafka requests
    # Pattern: length (4 bytes) + API key (2 bytes) + API version (2 bytes)
    i = 0
    while i < len(data) - 8:
        # Look for adminclient pattern first as a marker
        if b'adminclient' in data[i:i+100]:
            # Back up to find the start of the message
            for j in range(max(0, i-20), i):
                if j + 8 < len(data):
                    try:
                        # Try to parse as Kafka request
                        msg_len = struct.unpack('>I', data[j:j+4])[0]
                        if 10 < msg_len < 1000 and j + 4 + msg_len <= len(data):
                            api_key = struct.unpack('>H', data[j+4:j+6])[0]
                            api_version = struct.unpack('>H', data[j+6:j+8])[0]

                            # Check for ApiVersions (18) or DescribeCluster (32)
                            if api_key in [18, 32]:
                                correlation_id = struct.unpack('>I', data[j+8:j+12])[0]
                                msg_data = data[j:j+4+msg_len]
                                messages.append({
                                    'offset': j,
                                    'api_key': api_key,
                                    'api_version': api_version,
                                    'correlation_id': correlation_id,
                                    'length': msg_len,
                                    'data': msg_data,
                                    'hex': msg_data.hex()
                                })
                                print(f"{name}: Found {['ApiVersions' if api_key == 18 else 'DescribeCluster'][0]} v{api_version} at offset {j}, correlation_id={correlation_id}")
                                i = j + 4 + msg_len
                                break
                    except:
                        pass
        i += 1

    return messages

# Analyze Kafka capture
print("=== Analyzing Real Kafka Capture ===")
with open('/tmp/kafka_ksql.pcap', 'rb') as f:
    kafka_data = f.read()

kafka_messages = find_kafka_messages(kafka_data, "Kafka")

# Analyze Chronik capture
print("\n=== Analyzing Chronik Capture ===")
with open('/tmp/chronik_ksql.pcap', 'rb') as f:
    chronik_data = f.read()

chronik_messages = find_kafka_messages(chronik_data, "Chronik")

# Focus on DescribeCluster messages
print("\n=== DescribeCluster Comparison ===")

kafka_dc = [m for m in kafka_messages if m['api_key'] == 32]
chronik_dc = [m for m in chronik_messages if m['api_key'] == 32]

if kafka_dc:
    print(f"\nKafka DescribeCluster request (first one):")
    msg = kafka_dc[0]
    print(f"  Full hex: {msg['hex'][:200]}...")
    print(f"  API Version: {msg['api_version']}")
    print(f"  Correlation ID: {msg['correlation_id']}")

if chronik_dc:
    print(f"\nChronik DescribeCluster request (first one):")
    msg = chronik_dc[0]
    print(f"  Full hex: {msg['hex'][:200]}...")
    print(f"  API Version: {msg['api_version']}")
    print(f"  Correlation ID: {msg['correlation_id']}")

# Look for responses
print("\n=== Looking for DescribeCluster Responses ===")

# For Kafka - look for response pattern after DescribeCluster request
if kafka_dc:
    dc_offset = kafka_dc[0]['offset']
    # Look for response within next 1000 bytes
    response_search = kafka_data[dc_offset:dc_offset+1000]

    # Response should have the same correlation ID
    corr_id_bytes = struct.pack('>I', kafka_dc[0]['correlation_id'])
    corr_pos = response_search.find(corr_id_bytes)
    if corr_pos > 0:
        # Back up to find response start (length prefix)
        for i in range(max(0, corr_pos-10), corr_pos):
            try:
                resp_len = struct.unpack('>I', response_search[i:i+4])[0]
                if 10 < resp_len < 500:
                    resp_data = response_search[i:i+4+resp_len]
                    print(f"Kafka DescribeCluster response found:")
                    print(f"  Offset from request: {i}")
                    print(f"  Length: {resp_len}")
                    print(f"  Hex: {resp_data.hex()}")

                    # Parse response
                    # Skip length (4) to get correlation ID (4) and error code (2)
                    if len(resp_data) >= 10:
                        resp_corr_id = struct.unpack('>I', resp_data[4:8])[0]
                        error_code = struct.unpack('>h', resp_data[8:10])[0]
                        print(f"  Correlation ID: {resp_corr_id}")
                        print(f"  Error code: {error_code}")
                    break
            except:
                pass

# For Chronik - similar search
if chronik_dc:
    dc_offset = chronik_dc[0]['offset']
    response_search = chronik_data[dc_offset:dc_offset+1000]

    corr_id_bytes = struct.pack('>I', chronik_dc[0]['correlation_id'])
    corr_pos = response_search.find(corr_id_bytes)
    if corr_pos > 0:
        for i in range(max(0, corr_pos-10), corr_pos):
            try:
                resp_len = struct.unpack('>I', response_search[i:i+4])[0]
                if 10 < resp_len < 500:
                    resp_data = response_search[i:i+4+resp_len]
                    print(f"\nChronik DescribeCluster response found:")
                    print(f"  Offset from request: {i}")
                    print(f"  Length: {resp_len}")
                    print(f"  Hex: {resp_data.hex()}")

                    # Parse response
                    if len(resp_data) >= 10:
                        resp_corr_id = struct.unpack('>I', resp_data[4:8])[0]
                        error_code = struct.unpack('>h', resp_data[8:10])[0]
                        print(f"  Correlation ID: {resp_corr_id}")
                        print(f"  Error code: {error_code}")
                    break
            except:
                pass