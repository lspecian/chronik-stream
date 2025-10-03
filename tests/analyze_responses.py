#!/usr/bin/env python3
import struct

def analyze_packet_capture(filename, name):
    """Analyze a packet capture file for Kafka protocol messages"""
    with open(filename, 'rb') as f:
        data = f.read()

    print(f"\n=== Analyzing {name} ===")

    # Find DescribeCluster patterns and their responses
    # DescribeCluster API key is 0x0020 (32)
    i = 0
    while i < len(data) - 100:
        # Look for DescribeCluster request pattern
        if data[i:i+2] == b'\x00\x20':  # API key 32
            # Check if this looks like a valid request
            # Should have length prefix before it
            if i >= 4:
                try:
                    msg_len = struct.unpack('>I', data[i-4:i])[0]
                    if 10 < msg_len < 200:
                        api_version = struct.unpack('>H', data[i+2:i+4])[0]
                        correlation_id = struct.unpack('>I', data[i+4:i+8])[0]

                        print(f"\nFound DescribeCluster request:")
                        print(f"  Position: {i}")
                        print(f"  API Version: {api_version}")
                        print(f"  Correlation ID: {correlation_id}")
                        print(f"  Request hex (first 100 bytes): {data[i-4:i+96].hex()}")

                        # Look for response (should have same correlation ID)
                        corr_id_bytes = struct.pack('>I', correlation_id)
                        resp_start = i + msg_len

                        # Search for response within next 1000 bytes
                        search_area = data[resp_start:resp_start+1000]
                        resp_pos = search_area.find(corr_id_bytes)

                        if resp_pos >= 0:
                            # Found correlation ID, check if it's a response
                            actual_pos = resp_start + resp_pos
                            if actual_pos >= 4:
                                resp_len = struct.unpack('>I', data[actual_pos-4:actual_pos])[0]
                                if 4 < resp_len < 500:
                                    print(f"  Found response at position {actual_pos-4}:")
                                    print(f"    Response length: {resp_len}")

                                    # Parse response header
                                    resp_data = data[actual_pos-4:actual_pos-4+resp_len+4]
                                    print(f"    Response hex: {resp_data.hex()}")

                                    # Parse error code (should be right after correlation ID)
                                    if len(resp_data) >= 10:
                                        error_code = struct.unpack('>h', resp_data[8:10])[0]
                                        print(f"    Error code: {error_code}")

                                        if error_code == 0:
                                            # Parse successful response
                                            print(f"    Success! Response body: {resp_data[10:].hex()}")
                except:
                    pass
        i += 1

    # Also look for ApiVersions responses to understand versions supported
    i = 0
    found_api_versions = False
    while i < len(data) - 500 and not found_api_versions:
        # ApiVersions API key is 0x0012 (18)
        if data[i:i+2] == b'\x00\x12':
            if i >= 4:
                try:
                    msg_len = struct.unpack('>I', data[i-4:i])[0]
                    if 10 < msg_len < 100:
                        # Found ApiVersions request
                        api_version = struct.unpack('>H', data[i+2:i+4])[0]
                        correlation_id = struct.unpack('>I', data[i+4:i+8])[0]

                        print(f"\nFound ApiVersions request v{api_version}, looking for response...")

                        # Look for response
                        resp_start = i + msg_len
                        search_area = data[resp_start:min(resp_start+2000, len(data))]
                        corr_id_bytes = struct.pack('>I', correlation_id)
                        resp_pos = search_area.find(corr_id_bytes)

                        if resp_pos >= 0:
                            actual_pos = resp_start + resp_pos
                            if actual_pos >= 4:
                                resp_len = struct.unpack('>I', data[actual_pos-4:actual_pos])[0]
                                if 100 < resp_len < 1000:
                                    print(f"  ApiVersions response found, length: {resp_len}")
                                    resp_data = data[actual_pos-4:actual_pos-4+min(resp_len+4, 500)]

                                    # Look for DescribeCluster in response (API key 32 = 0x20)
                                    dc_pattern = b'\x00\x20'  # DescribeCluster API key
                                    dc_pos = resp_data.find(dc_pattern)
                                    if dc_pos > 0:
                                        # Found DescribeCluster in ApiVersions response
                                        # Next bytes should be min version, max version
                                        if dc_pos + 6 < len(resp_data):
                                            min_ver = struct.unpack('>H', resp_data[dc_pos+2:dc_pos+4])[0]
                                            max_ver = struct.unpack('>H', resp_data[dc_pos+4:dc_pos+6])[0]
                                            print(f"  DescribeCluster versions supported: {min_ver}-{max_ver}")
                                            found_api_versions = True
                except:
                    pass
        i += 1

# Analyze both captures
analyze_packet_capture('/tmp/kafka_ksql.pcap', 'Kafka')
analyze_packet_capture('/tmp/chronik_ksql.pcap', 'Chronik')