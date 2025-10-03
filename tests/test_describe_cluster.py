#!/usr/bin/env python3
"""Test DescribeCluster API directly"""

import socket
import struct
import time

def send_kafka_request(sock, api_key, api_version, correlation_id, client_id, body=b''):
    """Send a Kafka protocol request."""
    # Build request header based on version
    if api_key == 60 and api_version == 0:
        # DescribeCluster v0 uses non-flexible header
        # 2 bytes api_key + 2 bytes api_version + 4 bytes correlation_id + nullable string client_id
        header = struct.pack('>hhi', api_key, api_version, correlation_id)

        # Add client_id as nullable string (2 bytes length + string)
        if client_id:
            client_id_bytes = client_id.encode('utf-8')
            header += struct.pack('>h', len(client_id_bytes)) + client_id_bytes
        else:
            header += struct.pack('>h', -1)  # null string
    else:
        # Other APIs use their specific header formats
        header = struct.pack('>hhi', api_key, api_version, correlation_id)

        # Add client_id as nullable string (2 bytes length + string)
        if client_id:
            client_id_bytes = client_id.encode('utf-8')
            header += struct.pack('>h', len(client_id_bytes)) + client_id_bytes
        else:
            header += struct.pack('>h', -1)  # null string

    # Build full message
    message = header + body

    # Send with size prefix
    size = len(message)
    sock.sendall(struct.pack('>i', size) + message)

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

def test_describe_cluster():
    """Test DescribeCluster v0 API."""
    print("\n=== Testing DescribeCluster v0 ===")

    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.connect(('localhost', 9092))

    try:
        # Send DescribeCluster v0 request
        # Body: 1 byte include_cluster_authorized_operations (0 = false)
        body = struct.pack('>b', 0)
        response = send_kafka_request(sock, 60, 0, 1, 'test-client', body)

        if response:
            # Parse response
            correlation_id = struct.unpack('>i', response[:4])[0]
            print(f"Correlation ID: {correlation_id}")

            # Parse error code (next 2 bytes)
            error_code = struct.unpack('>h', response[4:6])[0]
            print(f"Error code: {error_code}")

            if error_code == 0:
                print("SUCCESS: DescribeCluster v0 worked!")
                # Parse rest of response
                offset = 6

                # Error message (nullable string)
                msg_len = struct.unpack('>h', response[offset:offset+2])[0]
                offset += 2
                if msg_len > 0:
                    error_msg = response[offset:offset+msg_len].decode('utf-8')
                    print(f"Error message: {error_msg}")
                    offset += msg_len
                else:
                    print("Error message: null")

                # Cluster ID (nullable string)
                id_len = struct.unpack('>h', response[offset:offset+2])[0]
                offset += 2
                if id_len > 0:
                    cluster_id = response[offset:offset+id_len].decode('utf-8')
                    print(f"Cluster ID: {cluster_id}")
                    offset += id_len
                else:
                    print("Cluster ID: null")

                # Controller ID (4 bytes)
                controller_id = struct.unpack('>i', response[offset:offset+4])[0]
                offset += 4
                print(f"Controller ID: {controller_id}")

                # Number of brokers (4 bytes)
                broker_count = struct.unpack('>i', response[offset:offset+4])[0]
                offset += 4
                print(f"Broker count: {broker_count}")

                # Parse each broker
                for i in range(broker_count):
                    broker_id = struct.unpack('>i', response[offset:offset+4])[0]
                    offset += 4

                    # Host (nullable string)
                    host_len = struct.unpack('>h', response[offset:offset+2])[0]
                    offset += 2
                    if host_len > 0:
                        host = response[offset:offset+host_len].decode('utf-8')
                        offset += host_len
                    else:
                        host = None

                    # Port (4 bytes)
                    port = struct.unpack('>i', response[offset:offset+4])[0]
                    offset += 4

                    # Rack (nullable string)
                    rack_len = struct.unpack('>h', response[offset:offset+2])[0]
                    offset += 2
                    if rack_len > 0:
                        rack = response[offset:offset+rack_len].decode('utf-8')
                        offset += rack_len
                    else:
                        rack = None

                    print(f"  Broker {i}: ID={broker_id}, Host={host}, Port={port}, Rack={rack}")

                # Authorized operations (4 bytes)
                auth_ops = struct.unpack('>i', response[offset:offset+4])[0]
                offset += 4
                print(f"Authorized operations: {auth_ops:08x}")

            elif error_code == -1:
                print("ERROR: Unknown server error (-1)")
                print("Response hex:", response.hex())
            elif error_code == 35:
                print("ERROR: Unsupported version (35)")
            else:
                print(f"ERROR: Got error code {error_code}")
        else:
            print("No response received")

    finally:
        sock.close()

if __name__ == "__main__":
    test_describe_cluster()