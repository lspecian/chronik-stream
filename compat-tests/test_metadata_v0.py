#!/usr/bin/env python3
"""Test raw MetadataRequest v0 compatibility with Chronik Stream"""

import socket
import struct
import sys
import time
import subprocess

def encode_request_header(api_key, api_version, correlation_id, client_id):
    """Encode a Kafka request header"""
    header = struct.pack('>h', api_key)  # API key
    header += struct.pack('>h', api_version)  # API version
    header += struct.pack('>i', correlation_id)  # Correlation ID

    # Client ID (nullable string)
    if client_id:
        client_id_bytes = client_id.encode('utf-8')
        header += struct.pack('>h', len(client_id_bytes))
        header += client_id_bytes
    else:
        header += struct.pack('>h', -1)  # null string

    return header

def test_metadata_request_v0():
    """Test MetadataRequest v0 compatibility"""
    print("\n" + "="*60)
    print("TEST: MetadataRequest v0 Compatibility")
    print("="*60)

    # Start server
    print("\n📡 Starting Chronik server...")
    server = subprocess.Popen(
        ["../target/release/chronik-server", "--kafka-port", "9095"],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True
    )
    time.sleep(3)  # Wait for server to start

    try:
        sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        sock.settimeout(5)
        sock.connect(('localhost', 9095))

        # Build MetadataRequest v0
        header = encode_request_header(3, 0, 2, 'test-client')  # API 3 = Metadata, version 0

        # Request body for v0: just topic array
        # -1 = null array (all topics)
        body = struct.pack('>i', -1)

        request = header + body

        # Send with length prefix
        message = struct.pack('>i', len(request)) + request
        print(f"\n📤 Sending MetadataRequest v0 ({len(message)} bytes)")
        sock.send(message)

        # Read response
        response_len_bytes = sock.recv(4)
        if len(response_len_bytes) < 4:
            print(f"❌ Failed to read response length, got {len(response_len_bytes)} bytes")
            return False

        response_len = struct.unpack('>i', response_len_bytes)[0]
        print(f"📥 Response length: {response_len} bytes")

        response = sock.recv(response_len)

        # Parse response
        offset = 0

        # Correlation ID
        correlation_id = struct.unpack('>i', response[offset:offset+4])[0]
        offset += 4
        print(f"✅ Correlation ID: {correlation_id}")

        # Brokers array
        broker_count = struct.unpack('>i', response[offset:offset+4])[0]
        offset += 4
        print(f"✅ Broker count: {broker_count}")

        for i in range(broker_count):
            node_id = struct.unpack('>i', response[offset:offset+4])[0]
            offset += 4

            # Host string
            host_len = struct.unpack('>h', response[offset:offset+2])[0]
            offset += 2
            host = response[offset:offset+host_len].decode('utf-8')
            offset += host_len

            # Port
            port = struct.unpack('>i', response[offset:offset+4])[0]
            offset += 4

            print(f"  Broker {node_id}: {host}:{port}")

        # Topics array
        topic_count = struct.unpack('>i', response[offset:offset+4])[0]
        offset += 4
        print(f"✅ Topic count: {topic_count}")

        sock.close()

        print("\n✅ TEST PASSED: MetadataRequest v0 works correctly!")
        return True

    except Exception as e:
        print(f"\n❌ TEST FAILED: {e}")
        import traceback
        traceback.print_exc()
        return False
    finally:
        # Stop server
        server.terminate()
        try:
            server.wait(timeout=5)
        except subprocess.TimeoutExpired:
            server.kill()
            server.wait()

def main():
    """Run MetadataRequest v0 compatibility test"""
    print("🚀 MetadataRequest v0 Compatibility Test")

    # Check if server binary exists
    if not subprocess.call(["test", "-f", "../target/release/chronik-server"]) == 0:
        print("❌ Error: chronik-server binary not found")
        print("  Please build the project first: cargo build --release")
        return 1

    success = test_metadata_request_v0()

    if success:
        print("\n🎉 MetadataRequest v0 compatibility test passed!")
        return 0
    else:
        print("\n❌ MetadataRequest v0 compatibility test failed")
        return 1

if __name__ == "__main__":
    sys.exit(main())