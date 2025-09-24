#!/usr/bin/env python3
"""Test Metadata v1 response format to identify encoding issue."""

import socket
import struct
import sys

def test_metadata_v1(port=9094):
    """Send a Metadata v1 request and examine the response."""
    print(f"\n=== Testing Metadata v1 Response on port {port} ===\n")

    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.settimeout(5)

    try:
        # Connect
        print(f"Connecting to localhost:{port}...")
        sock.connect(('localhost', port))
        print("Connected!")

        # Build Metadata v1 request
        # API key 3, version 1
        request = bytearray()
        request.extend(struct.pack('>h', 3))      # API key (Metadata)
        request.extend(struct.pack('>h', 1))      # API version (v1)
        request.extend(struct.pack('>i', 123))    # Correlation ID
        request.extend(struct.pack('>h', -1))     # Client ID length (-1 = null)
        request.extend(struct.pack('>i', -1))     # Topics array (-1 = all topics)

        # Send with size header
        size = len(request)
        full_request = struct.pack('>i', size) + request
        print(f"Sending Metadata v1 request (size={size} bytes)")
        print(f"Request bytes: {full_request.hex()}")
        sock.send(full_request)

        # Read response size
        size_bytes = sock.recv(4)
        if not size_bytes or len(size_bytes) != 4:
            print("Failed to read response size")
            return False

        response_size = struct.unpack('>i', size_bytes)[0]
        print(f"\nResponse size: {response_size} bytes")

        # Read full response
        response = bytearray()
        while len(response) < response_size:
            chunk = sock.recv(min(4096, response_size - len(response)))
            if not chunk:
                break
            response.extend(chunk)

        print(f"Received {len(response)} bytes")

        # Parse response
        print("\n=== Response Analysis ===")
        print(f"Full response hex (first 64 bytes): {response[:64].hex()}")

        # Parse header
        offset = 0
        correlation_id = struct.unpack('>i', response[offset:offset+4])[0]
        offset += 4
        print(f"Correlation ID: {correlation_id}")

        # For v1, there should be NO throttle_time_ms field!
        # Next should be brokers array count

        # Check what's at offset 4 (should be broker count)
        next_4_bytes = response[offset:offset+4]
        print(f"\nNext 4 bytes after correlation ID: {next_4_bytes.hex()}")
        value_as_int = struct.unpack('>i', next_4_bytes)[0]
        print(f"  Interpreted as i32: {value_as_int}")

        # If this is 0 or 1, it could be throttle_time_ms (wrong!)
        # If it's a reasonable broker count, it's correct
        if value_as_int in [0, 1]:
            print(f"  WARNING: This looks like throttle_time_ms={value_as_int} (WRONG for v1!)")
            print(f"  v1 responses should NOT include throttle_time_ms")
            offset += 4  # Skip the incorrectly included throttle_time

            # Now try to read broker count
            broker_count = struct.unpack('>i', response[offset:offset+4])[0]
            offset += 4
            print(f"\nActual broker count (at offset 8): {broker_count}")
        else:
            broker_count = value_as_int
            offset += 4
            print(f"  This appears to be broker count: {broker_count} (CORRECT for v1)")

        # Try to decode with kafka-python
        print("\n=== Kafka-Python Decoding Test ===")
        try:
            from kafka.protocol.metadata import MetadataResponse_v1
            # kafka-python expects just the response body (no correlation ID)
            body_for_kafka = response[4:]  # Skip correlation ID
            decoded = MetadataResponse_v1.decode(body_for_kafka)
            print("SUCCESS: kafka-python decoded the response!")
            print(f"  Brokers: {len(decoded.brokers)}")
            print(f"  Topics: {len(decoded.topics)}")
            for broker in decoded.brokers:
                print(f"    Broker {broker[0]}: {broker[1]}:{broker[2]}")
        except ImportError:
            print("kafka-python not installed, skipping decode test")
        except Exception as e:
            print(f"FAILED: kafka-python could not decode: {e}")
            print(f"  This confirms the encoding issue")

        return True

    except Exception as e:
        print(f"Error: {e}")
        return False
    finally:
        sock.close()

if __name__ == "__main__":
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 9094

    print("Testing Metadata v1 Response Encoding")
    print("=" * 50)

    success = test_metadata_v1(port)

    print("\n" + "=" * 50)
    if success:
        print("Test completed")
    else:
        print("Test failed")

    sys.exit(0 if success else 1)