#!/usr/bin/env python3
"""Debug Metadata v12 parsing issue."""

import socket
import struct

def test_metadata_v12():
    """Send just a Metadata v12 request to debug parsing."""

    # Create Metadata v12 request
    header = b''
    header += struct.pack('>h', 3)   # api_key = Metadata
    header += struct.pack('>h', 12)  # api_version = 12
    header += struct.pack('>i', 1)   # correlation_id = 1

    # client_id as normal string (non-flexible header)
    client_id = b'adminclient-2'
    header += struct.pack('>h', len(client_id))
    header += client_id

    # Body for Metadata v12
    body = b''
    # topics array - null means all topics (compact array encoding)
    body += bytes([0])  # Compact array with 0 elements (null)
    # allow_auto_topic_creation (boolean)
    body += bytes([1])  # true
    # include_topic_authorized_operations (boolean) - v12+
    body += bytes([1])  # true
    # Tagged fields (empty)
    body += bytes([0])

    # Combine header and body
    message = header + body

    print("Metadata v12 Request Analysis:")
    print("-" * 50)
    print(f"Header (23 bytes): {header.hex()}")
    print(f"  API key: {struct.unpack('>h', header[0:2])[0]}")
    print(f"  API version: {struct.unpack('>h', header[2:4])[0]}")
    print(f"  Correlation ID: {struct.unpack('>i', header[4:8])[0]}")
    print(f"  Client ID len: {struct.unpack('>h', header[8:10])[0]}")
    print(f"  Client ID: {header[10:23]}")
    print(f"\nBody (4 bytes): {body.hex()}")
    print(f"  Topics: 0x{body[0]:02x} (null array)")
    print(f"  Auto-create: 0x{body[1]:02x} (true)")
    print(f"  Include auth ops: 0x{body[2]:02x} (true)")
    print(f"  Tagged fields: 0x{body[3]:02x} (empty)")

    print(f"\nTotal message: {len(message)} bytes")
    print(f"Full message hex: {message.hex()}")

    # Add length prefix
    full_request = struct.pack('>i', len(message)) + message
    print(f"\nWith size prefix: {len(full_request)} bytes")
    print(f"Size prefix: {struct.pack('>i', len(message)).hex()}")

    # Send to server
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.connect(('localhost', 9092))
    print("\n" + "=" * 50)
    print("Connected to Chronik on port 9092")

    print(f"Sending {len(full_request)} bytes...")
    sock.send(full_request)

    # Try to read response
    try:
        response_len_bytes = sock.recv(4)
        if len(response_len_bytes) == 4:
            response_len = struct.unpack('>i', response_len_bytes)[0]
            print(f"\nResponse size: {response_len} bytes")

            response = sock.recv(response_len)
            print(f"Received {len(response)} bytes")

            if len(response) >= 4:
                corr_id = struct.unpack('>i', response[:4])[0]
                print(f"Correlation ID in response: {corr_id}")

                if len(response) >= 6:
                    error_code = struct.unpack('>h', response[4:6])[0]
                    print(f"Error code: {error_code}")
        else:
            print(f"Error: Only got {len(response_len_bytes)} bytes for size header")
    except Exception as e:
        print(f"Error reading response: {e}")

    sock.close()

if __name__ == '__main__':
    test_metadata_v12()