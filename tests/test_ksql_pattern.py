#!/usr/bin/env python3
"""Test KSQL's exact request pattern"""

import socket
import struct
import time

def send_raw_request(sock, request_bytes):
    """Send raw bytes as a Kafka request."""
    sock.sendall(request_bytes)

    # Read response size
    size_bytes = sock.recv(4)
    if len(size_bytes) < 4:
        print("Failed to read response size")
        return None

    response_size = struct.unpack('>i', size_bytes)[0]

    # Read response
    response = b''
    while len(response) < response_size:
        chunk = sock.recv(min(4096, response_size - len(response)))
        if not chunk:
            break
        response += chunk

    return response

def test_ksql_pattern():
    """Test KSQL's exact request pattern from pcap."""
    print("\n=== Testing KSQL Pattern (Connection 3) ===")

    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.connect(('localhost', 9092))

    try:
        # 1. Send exact ApiVersions v3 request from KSQL (from pcap)
        # This is the exact hex from the pcap
        api_versions_request = bytes.fromhex(
            '00000034' +  # Size: 52 bytes
            '0012' +      # API key: 18 (ApiVersions)
            '0003' +      # Version: 3
            '00000002' +  # Correlation ID: 2
            '000d' +      # Client ID length: 13
            '61646d696e636c69656e742d32' +  # Client ID: "adminclient-2"
            '00' +        # Client software name: empty compact string
            '12' +        # Client software version length
            '6170616368652d6b61666b612d6a617661' +  # "apache-kafka-java"
            '09' +        # Version string length
            '372e352e302d6365' +  # "7.5.0-ce"
            '00'          # Tagged fields: none
        )

        print(f"Sending ApiVersions v3 request ({len(api_versions_request)} bytes)")
        response = send_raw_request(sock, api_versions_request)

        if response:
            print(f"ApiVersions response: {len(response)} bytes")
            # Parse correlation ID
            corr_id = struct.unpack('>i', response[:4])[0]
            print(f"  Correlation ID: {corr_id}")

            # Check if it has error code
            if len(response) > 4:
                error_code = struct.unpack('>h', response[4:6])[0]
                print(f"  Error code: {error_code}")

        # 2. Send exact DescribeCluster v0 request from KSQL (from pcap)
        describe_cluster_request = bytes.fromhex(
            '0000001a' +  # Size: 26 bytes
            '003c' +      # API key: 60 (DescribeCluster)
            '0000' +      # Version: 0
            '00000003' +  # Correlation ID: 3
            '000d' +      # Client ID length: 13
            '61646d696e636c69656e742d32' +  # Client ID: "adminclient-2"
            '00'          # Include cluster authorized operations: false
        )

        print(f"\nSending DescribeCluster v0 request ({len(describe_cluster_request)} bytes)")
        response = send_raw_request(sock, describe_cluster_request)

        if response:
            print(f"DescribeCluster response: {len(response)} bytes")
            print(f"  Response hex: {response.hex()}")

            # Parse response
            offset = 0
            corr_id = struct.unpack('>i', response[offset:offset+4])[0]
            offset += 4
            print(f"  Correlation ID: {corr_id}")

            error_code = struct.unpack('>h', response[offset:offset+2])[0]
            offset += 2
            print(f"  Error code: {error_code}")

            if error_code == 0:
                print("  SUCCESS!")

                # Error message (nullable string)
                msg_len = struct.unpack('>h', response[offset:offset+2])[0]
                offset += 2
                if msg_len > 0:
                    error_msg = response[offset:offset+msg_len].decode('utf-8')
                    print(f"  Error message: {error_msg}")
                    offset += msg_len
                else:
                    print("  Error message: null")

                # Cluster ID (nullable string)
                id_len = struct.unpack('>h', response[offset:offset+2])[0]
                offset += 2
                if id_len > 0:
                    cluster_id = response[offset:offset+id_len].decode('utf-8')
                    print(f"  Cluster ID: {cluster_id}")
                    offset += id_len
                else:
                    print("  Cluster ID: null")

            elif error_code == -1:
                print("  ERROR: Unknown server error (-1)")
                print("  This is what KSQL is seeing!")
            else:
                print(f"  ERROR: Got error code {error_code}")

    finally:
        sock.close()

if __name__ == "__main__":
    test_ksql_pattern()