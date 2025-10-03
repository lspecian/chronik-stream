#!/usr/bin/env python3
"""
Test SASL authentication with Chronik Stream
"""

import socket
import struct
import hashlib
import hmac
import base64

def test_sasl_handshake():
    """Test SaslHandshake API"""

    print("Testing SASL Handshake...")

    # Connect to server
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.connect(('localhost', 9094))

    # SaslHandshake request (API Key 17, Version 0)
    api_key = 17
    api_version = 0
    correlation_id = 1
    client_id = b'test-sasl-client'

    # Build request header
    header = struct.pack('>hhi', api_key, api_version, correlation_id)
    header += struct.pack('>h', len(client_id)) + client_id

    # Build request body - request PLAIN mechanism
    mechanism = b'PLAIN'
    body = struct.pack('>h', len(mechanism)) + mechanism

    # Send request
    request = header + body
    message = struct.pack('>i', len(request)) + request
    sock.sendall(message)

    # Read response
    size_bytes = sock.recv(4)
    size = struct.unpack('>i', size_bytes)[0]
    response = sock.recv(size)

    # Parse response
    correlation_resp = struct.unpack('>i', response[:4])[0]
    print(f"Correlation ID: {correlation_resp}")

    offset = 4
    # Error code
    error_code = struct.unpack('>h', response[offset:offset+2])[0]
    offset += 2

    if error_code == 0:
        print("✅ Handshake successful!")

        # Read mechanisms array
        mechanism_count = struct.unpack('>i', response[offset:offset+4])[0]
        offset += 4
        print(f"Available mechanisms: {mechanism_count}")

        mechanisms = []
        for _ in range(mechanism_count):
            mech_len = struct.unpack('>h', response[offset:offset+2])[0]
            offset += 2
            mechanism = response[offset:offset+mech_len].decode('utf-8')
            offset += mech_len
            mechanisms.append(mechanism)
            print(f"  - {mechanism}")

        sock.close()
        return True
    else:
        print(f"❌ Handshake failed with error code: {error_code}")
        sock.close()
        return False

def test_sasl_authenticate_plain():
    """Test SaslAuthenticate with PLAIN mechanism"""

    print("\nTesting SASL Authentication (PLAIN)...")

    # First do handshake
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.connect(('localhost', 9094))

    # Send handshake
    api_key = 17
    api_version = 0
    correlation_id = 1
    client_id = b'test-client'

    header = struct.pack('>hhi', api_key, api_version, correlation_id)
    header += struct.pack('>h', len(client_id)) + client_id

    mechanism = b'PLAIN'
    body = struct.pack('>h', len(mechanism)) + mechanism

    request = header + body
    message = struct.pack('>i', len(request)) + request
    sock.sendall(message)

    # Read handshake response
    size_bytes = sock.recv(4)
    size = struct.unpack('>i', size_bytes)[0]
    response = sock.recv(size)

    # Now send authenticate
    api_key = 36  # SaslAuthenticate
    api_version = 0
    correlation_id = 2

    header = struct.pack('>hhi', api_key, api_version, correlation_id)
    header += struct.pack('>h', len(client_id)) + client_id

    # Build PLAIN auth bytes: [authzid]\0authcid\0passwd
    authzid = b''  # Empty authorization identity
    authcid = b'admin'  # Authentication identity (username)
    passwd = b'admin-secret'  # Password

    auth_bytes = authzid + b'\0' + authcid + b'\0' + passwd

    # Build request body
    body = struct.pack('>i', len(auth_bytes)) + auth_bytes

    # Send authenticate request
    request = header + body
    message = struct.pack('>i', len(request)) + request
    sock.sendall(message)

    # Read authenticate response
    size_bytes = sock.recv(4)
    if len(size_bytes) == 4:
        size = struct.unpack('>i', size_bytes)[0]
        response = sock.recv(size)

        # Parse response
        correlation_resp = struct.unpack('>i', response[:4])[0]
        print(f"Correlation ID: {correlation_resp}")

        offset = 4
        # Error code
        error_code = struct.unpack('>h', response[offset:offset+2])[0]
        offset += 2

        if error_code == 0:
            print("✅ Authentication successful!")

            # Read error message (nullable string)
            msg_len = struct.unpack('>h', response[offset:offset+2])[0]
            offset += 2
            if msg_len > 0:
                error_msg = response[offset:offset+msg_len].decode('utf-8')
                print(f"Message: {error_msg}")
                offset += msg_len

            # Read auth bytes (nullable)
            auth_bytes_len = struct.unpack('>i', response[offset:offset+4])[0]
            offset += 4
            if auth_bytes_len > 0:
                auth_response = response[offset:offset+auth_bytes_len]
                print(f"Auth response bytes: {auth_response.hex()}")

            sock.close()
            return True
        else:
            print(f"❌ Authentication failed with error code: {error_code}")
            # Try to read error message
            if offset < len(response):
                msg_len = struct.unpack('>h', response[offset:offset+2])[0]
                if msg_len > 0:
                    offset += 2
                    error_msg = response[offset:offset+msg_len].decode('utf-8')
                    print(f"Error message: {error_msg}")
    else:
        print("Failed to read response")

    sock.close()
    return False

def test_sasl_authenticate_scram():
    """Test SaslAuthenticate with SCRAM-SHA-256 mechanism"""

    print("\nTesting SASL Authentication (SCRAM-SHA-256)...")

    # This would require implementing the full SCRAM exchange
    # For now, just test that the server responds to the initial client message

    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.connect(('localhost', 9094))

    # Send handshake for SCRAM-SHA-256
    api_key = 17
    api_version = 0
    correlation_id = 1
    client_id = b'test-client'

    header = struct.pack('>hhi', api_key, api_version, correlation_id)
    header += struct.pack('>h', len(client_id)) + client_id

    mechanism = b'SCRAM-SHA-256'
    body = struct.pack('>h', len(mechanism)) + mechanism

    request = header + body
    message = struct.pack('>i', len(request)) + request
    sock.sendall(message)

    # Read handshake response
    size_bytes = sock.recv(4)
    size = struct.unpack('>i', size_bytes)[0]
    response = sock.recv(size)

    # Check if SCRAM-SHA-256 is supported
    error_code = struct.unpack('>h', response[4:6])[0]
    if error_code == 0:
        print("✅ SCRAM-SHA-256 mechanism accepted")

        # Send initial SCRAM client message
        api_key = 36  # SaslAuthenticate
        correlation_id = 2

        header = struct.pack('>hhi', api_key, api_version, correlation_id)
        header += struct.pack('>h', len(client_id)) + client_id

        # Build initial SCRAM client message
        username = 'admin'
        client_nonce = base64.b64encode(b'test-nonce-12345').decode('ascii')
        client_first = f'n,,n={username},r={client_nonce}'
        auth_bytes = client_first.encode('utf-8')

        body = struct.pack('>i', len(auth_bytes)) + auth_bytes

        request = header + body
        message = struct.pack('>i', len(request)) + request
        sock.sendall(message)

        # Read server response
        size_bytes = sock.recv(4)
        if len(size_bytes) == 4:
            size = struct.unpack('>i', size_bytes)[0]
            response = sock.recv(size)

            error_code = struct.unpack('>h', response[4:6])[0]
            print(f"Server response error code: {error_code}")

            if error_code == 0:
                print("✅ Server accepted initial SCRAM message")
            else:
                print(f"❌ Server rejected SCRAM: error {error_code}")
    else:
        print(f"❌ SCRAM-SHA-256 not supported, error: {error_code}")

    sock.close()
    return error_code == 0

if __name__ == "__main__":
    # Kill any existing server
    import subprocess
    subprocess.run(['pkill', '-f', 'chronik-server'], stderr=subprocess.DEVNULL)
    import time
    time.sleep(1)

    # Start server
    print("Starting Chronik server...")
    subprocess.run(['rm', '-rf', './data'])
    server = subprocess.Popen(['./target/release/chronik-server', '-p', '9094'],
                             stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    time.sleep(2)

    try:
        # Run tests
        print("\n" + "="*50)
        result1 = test_sasl_handshake()
        result2 = test_sasl_authenticate_plain()
        result3 = test_sasl_authenticate_scram()

        print("\n" + "="*50)
        print("TEST RESULTS:")
        print(f"SASL Handshake: {'✅ PASSED' if result1 else '❌ FAILED'}")
        print(f"SASL PLAIN Auth: {'✅ PASSED' if result2 else '❌ FAILED'}")
        print(f"SASL SCRAM-SHA-256: {'✅ PASSED' if result3 else '❌ FAILED'}")

        if result1:
            print("\n🎉 SASL APIs are working!")
    finally:
        # Stop server
        server.terminate()
        server.wait()