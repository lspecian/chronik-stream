#!/usr/bin/env python3
"""Simple test for ACL APIs using raw protocol"""

import socket
import struct
import sys

def encode_request_header(api_key, api_version, correlation_id, client_id):
    """Encode Kafka request header"""
    header = struct.pack('>h', api_key)  # API key
    header += struct.pack('>h', api_version)  # API version
    header += struct.pack('>i', correlation_id)  # Correlation ID
    header += struct.pack('>h', len(client_id))  # Client ID length
    header += client_id.encode('utf-8')  # Client ID
    return header

def encode_describe_acls_request_v0():
    """Encode DescribeAcls request v0"""
    request = b''

    # Resource type (1 = Any)
    request += struct.pack('>b', 1)

    # Resource name (null = match all)
    request += struct.pack('>h', -1)

    # Principal (null = match all)
    request += struct.pack('>h', -1)

    # Host (null = match all)
    request += struct.pack('>h', -1)

    # Operation (1 = Any)
    request += struct.pack('>b', 1)

    # Permission type (1 = Any)
    request += struct.pack('>b', 1)

    return request

def test_describe_acls():
    """Test DescribeAcls API call"""
    try:
        # Connect to Kafka
        sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        sock.connect(('localhost', 9092))

        print("Connected to Kafka server at localhost:9092")

        # Create DescribeAcls request
        header = encode_request_header(
            api_key=29,  # DescribeAcls
            api_version=0,
            correlation_id=1,
            client_id='test-acl-client'
        )

        request_body = encode_describe_acls_request_v0()

        # Send request
        message = header + request_body
        size_bytes = struct.pack('>i', len(message))
        sock.sendall(size_bytes + message)

        print("Sent DescribeAcls request")

        # Read response
        size_data = sock.recv(4)
        if len(size_data) < 4:
            print("Failed to read response size")
            return False

        response_size = struct.unpack('>i', size_data)[0]
        print(f"Response size: {response_size} bytes")

        response_data = sock.recv(response_size)

        # Parse response header
        correlation_id = struct.unpack('>i', response_data[:4])[0]
        print(f"Response correlation ID: {correlation_id}")

        # Parse response body (v0)
        offset = 4

        # Throttle time
        throttle_time = struct.unpack('>i', response_data[offset:offset+4])[0]
        offset += 4
        print(f"Throttle time: {throttle_time}ms")

        # Error code
        error_code = struct.unpack('>h', response_data[offset:offset+2])[0]
        offset += 2
        print(f"Error code: {error_code}")

        if error_code == 0:
            # Error message (nullable string)
            error_msg_len = struct.unpack('>h', response_data[offset:offset+2])[0]
            offset += 2
            if error_msg_len > 0:
                error_msg = response_data[offset:offset+error_msg_len].decode('utf-8')
                offset += error_msg_len
                print(f"Error message: {error_msg}")

            # Resources array
            resources_count = struct.unpack('>i', response_data[offset:offset+4])[0]
            offset += 4
            print(f"Number of resources with ACLs: {resources_count}")

            for i in range(resources_count):
                # Resource type
                resource_type = struct.unpack('>b', response_data[offset:offset+1])[0]
                offset += 1

                # Resource name
                name_len = struct.unpack('>h', response_data[offset:offset+2])[0]
                offset += 2
                resource_name = response_data[offset:offset+name_len].decode('utf-8')
                offset += name_len

                print(f"\nResource {i+1}: type={resource_type}, name={resource_name}")

                # ACLs array for this resource
                acls_count = struct.unpack('>i', response_data[offset:offset+4])[0]
                offset += 4
                print(f"  ACLs count: {acls_count}")

                for j in range(acls_count):
                    # Principal
                    principal_len = struct.unpack('>h', response_data[offset:offset+2])[0]
                    offset += 2
                    principal = response_data[offset:offset+principal_len].decode('utf-8')
                    offset += principal_len

                    # Host
                    host_len = struct.unpack('>h', response_data[offset:offset+2])[0]
                    offset += 2
                    host = response_data[offset:offset+host_len].decode('utf-8')
                    offset += host_len

                    # Operation
                    operation = struct.unpack('>b', response_data[offset:offset+1])[0]
                    offset += 1

                    # Permission type
                    permission = struct.unpack('>b', response_data[offset:offset+1])[0]
                    offset += 1

                    print(f"    ACL {j+1}: principal={principal}, host={host}, op={operation}, perm={permission}")

            print("\n✅ DescribeAcls test PASSED!")
            return True
        else:
            print(f"❌ DescribeAcls failed with error code: {error_code}")
            return False

        sock.close()

    except Exception as e:
        print(f"Test failed: {e}")
        return False

if __name__ == "__main__":
    success = test_describe_acls()
    sys.exit(0 if success else 1)