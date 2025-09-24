#!/usr/bin/env python3
"""Test protocol detection in Chronik Stream v1.3.4"""

import socket
import sys

def test_version_string(host='localhost', port=9092):
    """Send '5.0\\0' to simulate Kafka UI behavior"""
    print(f"Testing protocol detection on {host}:{port}")

    try:
        # Create socket and connect
        s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        s.connect((host, port))

        # Send "5.0\0" (the problematic data from Kafka UI)
        data = b'5.0\x00'
        print(f"Sending bytes: {list(data)} = '{data.decode('ascii', errors='replace')}'")
        s.send(data)

        # Try to receive response
        response = s.recv(1024)
        print(f"\nServer response:\n{response.decode('utf-8', errors='replace')}")

        s.close()

        print("\n✅ Server handled non-Kafka protocol gracefully!")
        return True

    except Exception as e:
        print(f"\n❌ Error: {e}")
        return False

if __name__ == "__main__":
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 9092
    test_version_string(port=port)