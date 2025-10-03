#!/usr/bin/env python3
"""Test DescribeCluster v4 with Chronik"""

import socket
import struct
import sys

def send_describe_cluster_v4():
    """Send DescribeCluster v4 request like KSQL does"""
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.connect(('localhost', 9092))

    # Build DescribeCluster v4 request
    # Based on captured request from KSQL
    api_key = 32  # DescribeCluster
    api_version = 4
    correlation_id = 1
    client_id = "test-client"

    # Build request body
    request = bytearray()
    request.extend(struct.pack('>H', api_key))  # API key
    request.extend(struct.pack('>H', api_version))  # API version
    request.extend(struct.pack('>I', correlation_id))  # Correlation ID

    # Client ID (non-flexible string in hybrid mode)
    request.extend(struct.pack('>H', len(client_id)))  # Length as int16
    request.extend(client_id.encode('utf-8'))

    # Tagged fields header (empty)
    request.append(0)

    # includeClusterAuthorizedOperations (bool)
    request.append(1)  # true

    # endpointType (int8) - new in v2+
    request.append(0)  # DEFAULT

    # Tagged fields (empty)
    request.append(0)

    # Add length prefix
    full_request = struct.pack('>I', len(request)) + bytes(request)

    print(f"Sending DescribeCluster v4 request ({len(full_request)} bytes):")
    print(f"  Hex: {full_request.hex()}")

    sock.send(full_request)

    # Receive response
    response_length_bytes = sock.recv(4)
    if len(response_length_bytes) < 4:
        print("Failed to receive response length")
        return

    response_length = struct.unpack('>I', response_length_bytes)[0]
    print(f"\nResponse length: {response_length}")

    response = sock.recv(response_length)
    print(f"Response ({len(response)} bytes): {response.hex()}")

    # Parse response
    offset = 0
    correlation_id = struct.unpack('>I', response[offset:offset+4])[0]
    offset += 4
    print(f"  Correlation ID: {correlation_id}")

    # Tagged fields header (for flexible response)
    tagged_fields = response[offset]
    offset += 1
    print(f"  Tagged fields header: {tagged_fields:#x}")

    # Error code
    error_code = struct.unpack('>h', response[offset:offset+2])[0]
    offset += 2
    print(f"  Error code: {error_code}")

    if error_code == 0:
        print("  SUCCESS!")

        # Error message (compact string)
        msg_len = response[offset]
        offset += 1
        if msg_len > 0:
            actual_len = msg_len - 1
            error_msg = response[offset:offset+actual_len].decode('utf-8')
            offset += actual_len
            print(f"  Error message: '{error_msg}'")
        else:
            print(f"  Error message: null")

        # Cluster ID (compact string)
        cluster_id_len = response[offset]
        offset += 1
        if cluster_id_len > 0:
            actual_len = cluster_id_len - 1
            cluster_id = response[offset:offset+actual_len].decode('utf-8')
            offset += actual_len
            print(f"  Cluster ID: '{cluster_id}'")

        # Controller ID
        controller_id = struct.unpack('>I', response[offset:offset+4])[0]
        offset += 4
        print(f"  Controller ID: {controller_id}")

        # Cluster authorized operations
        auth_ops = struct.unpack('>i', response[offset:offset+4])[0]
        offset += 4
        print(f"  Cluster authorized operations: {auth_ops:#x}")

        # Brokers array (compact array)
        broker_count_encoded = response[offset]
        offset += 1
        broker_count = broker_count_encoded - 1 if broker_count_encoded > 0 else 0
        print(f"  Broker count: {broker_count}")

        for i in range(broker_count):
            # Broker ID
            broker_id = struct.unpack('>I', response[offset:offset+4])[0]
            offset += 4

            # Host (compact string)
            host_len = response[offset]
            offset += 1
            if host_len > 0:
                actual_len = host_len - 1
                host = response[offset:offset+actual_len].decode('utf-8')
                offset += actual_len
            else:
                host = None

            # Port
            port = struct.unpack('>I', response[offset:offset+4])[0]
            offset += 4

            # Rack (compact string)
            rack_len = response[offset]
            offset += 1
            if rack_len > 0:
                actual_len = rack_len - 1
                rack = response[offset:offset+actual_len].decode('utf-8')
                offset += actual_len
            else:
                rack = None

            # Tagged fields for broker
            broker_tags = response[offset]
            offset += 1

            print(f"    Broker {i}: id={broker_id}, host={host}, port={port}, rack={rack}")

        # Tagged fields for response
        final_tags = response[offset] if offset < len(response) else 0
        print(f"  Final tagged fields: {final_tags:#x}")

    else:
        print(f"  ERROR: Error code {error_code}")

    sock.close()

if __name__ == "__main__":
    send_describe_cluster_v4()