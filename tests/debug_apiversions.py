#!/usr/bin/env python3
import socket
import struct
import sys

def connect_and_send_api_versions():
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.connect(('localhost', 9092))

    # ApiVersions request with v3 and non-flexible encoding
    # Header format for v3 non-flexible:
    # - api_key (2 bytes): 18
    # - api_version (2 bytes): 3
    # - correlation_id (4 bytes): 0
    # - client_id (string with 2-byte length): "adminclient-2"

    header = b''
    header += struct.pack('>h', 18)  # api_key = ApiVersions
    header += struct.pack('>h', 3)   # api_version = 3
    header += struct.pack('>i', 0)   # correlation_id = 0

    # client_id as normal string (2-byte length prefix)
    client_id = b'adminclient-2'
    header += struct.pack('>h', len(client_id))
    header += client_id

    # Body for ApiVersions v3 (non-flexible):
    # - client_software_name (string with 2-byte length)
    # - client_software_version (string with 2-byte length)
    body = b''
    software_name = b'apache-kafka-java'
    body += struct.pack('>h', len(software_name))
    body += software_name

    software_version = b'7.5.0-140-ccs'
    body += struct.pack('>h', len(software_version))
    body += software_version

    # Combine header and body
    message = header + body

    # Send with length prefix
    full_message = struct.pack('>i', len(message)) + message

    print(f"Sending {len(full_message)} bytes total")
    print(f"Message length: {len(message)}")
    print(f"Header: {header.hex()}")
    print(f"Body: {body.hex()}")
    print(f"Full message hex: {full_message.hex()}")

    sock.send(full_message)

    # Read response
    response_len_bytes = sock.recv(4)
    response_len = struct.unpack('>i', response_len_bytes)[0]
    response = sock.recv(response_len)

    print(f"\nReceived {len(response)} byte response")
    print(f"Response hex: {response.hex()[:100]}...")

    sock.close()

if __name__ == '__main__':
    connect_and_send_api_versions()