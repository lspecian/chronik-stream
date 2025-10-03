#!/usr/bin/env python3
"""
Simple SASL test for Chronik Stream
"""

import socket
import struct

def test_sasl():
    """Test basic SASL handshake"""

    print("Testing SASL Handshake API...")

    # Connect to server
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.connect(('localhost', 9094))

    # SaslHandshake request (API Key 17, Version 0)
    api_key = 17
    api_version = 0
    correlation_id = 1
    client_id = b'test-client'

    # Build request header
    header = struct.pack('>hhi', api_key, api_version, correlation_id)
    header += struct.pack('>h', len(client_id)) + client_id

    # Build request body - request PLAIN mechanism
    mechanism = b'PLAIN'
    body = struct.pack('>h', len(mechanism)) + mechanism

    # Send request
    request = header + body
    message = struct.pack('>i', len(request)) + request
    print(f"Sending SaslHandshake request for mechanism: {mechanism.decode()}")
    sock.sendall(message)

    # Read response
    size_bytes = sock.recv(4)
    if len(size_bytes) == 4:
        size = struct.unpack('>i', size_bytes)[0]
        print(f"Response size: {size} bytes")

        response = sock.recv(size)
        print(f"Response bytes (hex): {response.hex()}")

        # Parse response
        offset = 0

        # Correlation ID
        correlation_resp = struct.unpack('>i', response[offset:offset+4])[0]
        offset += 4
        print(f"Correlation ID: {correlation_resp}")

        # Error code
        error_code = struct.unpack('>h', response[offset:offset+2])[0]
        offset += 2
        print(f"Error code: {error_code}")

        if error_code == 0:
            print("✅ SaslHandshake SUCCESSFUL!")

            # The rest of the response contains the mechanisms array
            remaining = response[offset:]
            print(f"Remaining bytes for mechanisms: {remaining.hex()}")

            # Try to parse mechanisms if there's data
            if len(remaining) >= 4:
                mechanism_count = struct.unpack('>i', remaining[0:4])[0]
                print(f"Number of mechanisms: {mechanism_count}")

                offset = 4
                for i in range(mechanism_count):
                    if offset + 2 <= len(remaining):
                        mech_len = struct.unpack('>h', remaining[offset:offset+2])[0]
                        offset += 2
                        if offset + mech_len <= len(remaining):
                            mechanism = remaining[offset:offset+mech_len].decode('utf-8')
                            offset += mech_len
                            print(f"  - {mechanism}")

            sock.close()
            return True
        else:
            print(f"❌ SaslHandshake failed with error code: {error_code}")
            sock.close()
            return False
    else:
        print("Failed to read response size")
        sock.close()
        return False

if __name__ == "__main__":
    import subprocess
    import time

    # Kill any existing server
    subprocess.run(['pkill', '-f', 'chronik-server'], stderr=subprocess.DEVNULL)
    time.sleep(1)

    # Start server
    print("Starting Chronik server...")
    subprocess.run(['rm', '-rf', './data'])
    server = subprocess.Popen(['./target/release/chronik-server', '-p', '9094'],
                             stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    time.sleep(2)

    try:
        # Run test
        print("\n" + "="*50)
        result = test_sasl()

        print("\n" + "="*50)
        if result:
            print("🎉 SASL Handshake API is WORKING!")
        else:
            print("❌ SASL Handshake API test failed")
    finally:
        # Stop server
        server.terminate()
        server.wait()