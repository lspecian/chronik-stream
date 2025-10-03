#!/usr/bin/env python3
"""Test script to reproduce and verify the v1.3.5 fix for the 5.0\0 error."""

import socket
import struct
import time
import sys

def send_valid_request(sock):
    """Send a valid Kafka Metadata request."""
    # Valid Kafka Metadata request (API key 3, version 0)
    request = bytearray()
    request.extend(struct.pack('>h', 3))      # API key (Metadata)
    request.extend(struct.pack('>h', 0))      # API version
    request.extend(struct.pack('>i', 1))      # Correlation ID
    request.extend(struct.pack('>h', -1))     # Client ID length (-1 = null)
    request.extend(struct.pack('>i', -1))     # Topics array (-1 = all topics)

    # Send with size header
    size = len(request)
    full_request = struct.pack('>i', size) + request
    sock.send(full_request)
    print(f"Sent valid Kafka request (size={size})")

    # Read response
    try:
        size_bytes = sock.recv(4)
        if size_bytes:
            response_size = struct.unpack('>i', size_bytes)[0]
            response = sock.recv(response_size)
            print(f"Got response: {response_size} bytes")
            return True
    except Exception as e:
        print(f"Error reading response: {e}")
        return False
    return False

def send_bad_request(sock):
    """Send the problematic 5.0\0 sequence that causes the error."""
    bad_data = b"5.0\0"
    sock.send(bad_data)
    print(f"Sent problematic data: {bad_data.hex()} ('{bad_data.decode('ascii', errors='replace')}')")

    # When interpreted as big-endian i32: 0x352e3000 = 892,219,392
    as_size = struct.unpack('>i', bad_data)[0]
    print(f"  If interpreted as request size: {as_size:,} bytes")

def test_chronik(port=9094):
    """Test that Chronik v1.3.5 handles the error gracefully."""
    print(f"\n=== Testing Chronik v1.3.5 on port {port} ===\n")

    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.settimeout(5)

    try:
        # Connect
        print(f"Connecting to localhost:{port}...")
        sock.connect(('localhost', port))
        print("Connected!")

        # Send a valid request first
        print("\n1. Sending valid Kafka request...")
        if send_valid_request(sock):
            print("   ✓ Valid request handled successfully")
        else:
            print("   ✗ Valid request failed")

        time.sleep(0.5)

        # Now send the problematic data that triggers the error
        print("\n2. Sending problematic '5.0\\0' data...")
        send_bad_request(sock)

        time.sleep(1)

        # The key test: Can we still send valid requests after the error?
        print("\n3. Testing recovery - sending another valid request...")
        if send_valid_request(sock):
            print("   ✓ RECOVERY SUCCESSFUL! Connection still works after error")
            print("\n✅ v1.3.5 FIX VERIFIED: Server recovered from protocol error")
            return True
        else:
            print("   ✗ Recovery failed - connection broken")
            print("\n❌ Fix not working: Connection broken after error")
            return False

    except socket.timeout:
        print("Connection timed out")
        return False
    except ConnectionResetError:
        print("Connection reset by server")
        return False
    except Exception as e:
        print(f"Error: {e}")
        return False
    finally:
        sock.close()

if __name__ == "__main__":
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 9094

    print("Testing Chronik Stream v1.3.5 error recovery")
    print("=" * 50)

    success = test_chronik(port)

    print("\n" + "=" * 50)
    if success:
        print("TEST PASSED: v1.3.5 successfully recovers from protocol errors")
        sys.exit(0)
    else:
        print("TEST FAILED: Server did not recover from protocol error")
        sys.exit(1)