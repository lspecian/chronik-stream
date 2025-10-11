#!/usr/bin/env python3
"""
Comprehensive AdminClient API compatibility test for Chronik Stream
Tests the APIs that Kafka UI, Apache Flink, and other ecosystem tools require
"""

import socket
import struct
import sys

def send_kafka_request(api_key, api_version, correlation_id, client_id, body):
    """Send a Kafka protocol request and return the response"""
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.connect(('localhost', 9092))

    # Build request header
    header = struct.pack('>h', api_key)  # API key
    header += struct.pack('>h', api_version)  # API version
    header += struct.pack('>i', correlation_id)  # Correlation ID
    header += struct.pack('>h', len(client_id))
    header += client_id.encode('utf-8')

    # Send request
    message = header + body
    size_bytes = struct.pack('>i', len(message))
    sock.sendall(size_bytes + message)

    # Read response
    size_data = sock.recv(4)
    if len(size_data) < 4:
        sock.close()
        return None

    response_size = struct.unpack('>i', size_data)[0]
    response_data = sock.recv(response_size)
    sock.close()

    return response_data

def test_describe_acls_v0():
    """Test DescribeAcls v0 (standard encoding)"""
    print("\n=== Test 1: DescribeAcls v0 (standard encoding) ===")

    # Build request body
    body = b''
    body += struct.pack('>b', 1)  # Resource type (1 = Any)
    body += struct.pack('>h', -1)  # Resource name (null)
    body += struct.pack('>h', -1)  # Principal (null)
    body += struct.pack('>h', -1)  # Host (null)
    body += struct.pack('>b', 1)  # Operation (1 = Any)
    body += struct.pack('>b', 1)  # Permission type (1 = Any)

    response = send_kafka_request(29, 0, 1, 'test-client', body)
    if not response:
        print("❌ FAILED: No response")
        return False

    # Parse response
    corr_id = struct.unpack('>i', response[0:4])[0]
    throttle = struct.unpack('>i', response[4:8])[0]
    error_code = struct.unpack('>h', response[8:10])[0]

    print(f"Response size: {len(response)} bytes")
    print(f"Correlation ID: {corr_id}")
    print(f"Throttle time: {throttle}ms")
    print(f"Error code: {error_code}")

    if error_code == 0:
        print("✅ PASSED: DescribeAcls v0 works correctly")
        return True
    else:
        print(f"❌ FAILED: Error code {error_code}")
        return False

def test_describe_acls_v2():
    """Test DescribeAcls v2 (flexible/compact encoding)"""
    print("\n=== Test 2: DescribeAcls v2 (flexible encoding) ===")

    # Build request body with flexible encoding
    body = b''
    body += struct.pack('>b', 1)  # Resource type
    body += struct.pack('>h', -1)  # Resource name (null - old format for request)
    body += struct.pack('>b', 1)  # Pattern type (v1+)
    body += struct.pack('>h', -1)  # Principal (null)
    body += struct.pack('>h', -1)  # Host (null)
    body += struct.pack('>b', 1)  # Operation
    body += struct.pack('>b', 1)  # Permission
    body += struct.pack('>b', 0)  # Tagged fields

    response = send_kafka_request(29, 2, 2, 'test-client', body)
    if not response:
        print("❌ FAILED: No response")
        return False

    print(f"Response size: {len(response)} bytes")
    print(f"Response hex: {response.hex()}")

    if len(response) < 10:
        print(f"❌ FAILED: Response too short ({len(response)} bytes)")
        return False

    # Parse response
    corr_id = struct.unpack('>i', response[0:4])[0]
    # For flexible versions, header has tagged fields after correlation ID
    # Check if there's a tagged fields byte
    offset = 4
    if len(response) > 4 and response[4] == 0:  # Empty tagged fields in header
        print("Found tagged fields in header (flexible)")
        offset = 5

    throttle = struct.unpack('>i', response[offset:offset+4])[0]
    error_code = struct.unpack('>h', response[offset+4:offset+6])[0]

    print(f"Correlation ID: {corr_id}")
    print(f"Throttle time: {throttle}ms")
    print(f"Error code: {error_code}")

    # Check flexible encoding
    error_msg_byte = response[offset+6]  # Should be 0 for null compact string
    resources_byte = response[offset+7]  # Should be 1 for empty compact array

    print(f"Error message byte: 0x{error_msg_byte:02x} (should be 0x00 for null)")
    print(f"Resources array byte: 0x{resources_byte:02x} (should be 0x01 for empty)")

    if error_code == 0 and error_msg_byte == 0 and resources_byte == 1:
        print("✅ PASSED: DescribeAcls v2 uses correct flexible encoding")
        return True
    else:
        print(f"❌ FAILED: Incorrect encoding or error code {error_code}")
        return False

def test_api_versions():
    """Test ApiVersions to see what's supported"""
    print("\n=== Test 3: ApiVersions ===")

    # ApiVersions request (usually empty body for v0)
    body = b''

    response = send_kafka_request(18, 0, 3, 'test-client', body)
    if not response:
        print("❌ FAILED: No response")
        return False

    corr_id = struct.unpack('>i', response[0:4])[0]
    error_code = struct.unpack('>h', response[4:6])[0]

    print(f"Correlation ID: {corr_id}")
    print(f"Error code: {error_code}")

    if error_code == 0:
        # Parse API versions
        num_apis = struct.unpack('>i', response[6:10])[0]
        print(f"Number of supported APIs: {num_apis}")

        # Find DescribeAcls
        offset = 10
        for i in range(min(num_apis, 50)):  # Limit to prevent infinite loop
            if offset + 6 > len(response):
                break
            api_key = struct.unpack('>h', response[offset:offset+2])[0]
            min_ver = struct.unpack('>h', response[offset+2:offset+4])[0]
            max_ver = struct.unpack('>h', response[offset+4:offset+6])[0]
            offset += 6

            if api_key == 29:  # DescribeAcls
                print(f"  DescribeAcls (29): v{min_ver}-v{max_ver}")
            elif api_key == 10:  # FindCoordinator
                print(f"  FindCoordinator (10): v{min_ver}-v{max_ver}")
            elif api_key == 11:  # JoinGroup
                print(f"  JoinGroup (11): v{min_ver}-v{max_ver}")

        print("✅ PASSED: ApiVersions works")
        return True
    else:
        print(f"❌ FAILED: Error code {error_code}")
        return False

def test_metadata():
    """Test Metadata API"""
    print("\n=== Test 4: Metadata API ===")

    # Metadata v1 request for all topics
    body = b''
    body += struct.pack('>i', -1)  # null topics array = all topics

    response = send_kafka_request(3, 1, 4, 'test-client', body)
    if not response:
        print("❌ FAILED: No response")
        return False

    corr_id = struct.unpack('>i', response[0:4])[0]

    print(f"Correlation ID: {corr_id}")
    print(f"Response size: {len(response)} bytes")

    # Basic check - correlation ID matches
    if corr_id == 4:
        print("✅ PASSED: Metadata API works")
        return True
    else:
        print(f"❌ FAILED: Correlation ID mismatch")
        return False

def test_find_coordinator():
    """Test FindCoordinator API (critical for consumer groups)"""
    print("\n=== Test 5: FindCoordinator API ===")

    # FindCoordinator v0 request
    group_id = 'test-group'
    body = struct.pack('>h', len(group_id))
    body += group_id.encode('utf-8')

    response = send_kafka_request(10, 0, 5, 'test-client', body)
    if not response:
        print("❌ FAILED: No response")
        return False

    corr_id = struct.unpack('>i', response[0:4])[0]
    error_code = struct.unpack('>h', response[4:6])[0]

    print(f"Correlation ID: {corr_id}")
    print(f"Error code: {error_code}")

    if error_code == 0:
        # Parse coordinator info
        node_id = struct.unpack('>i', response[6:10])[0]
        host_len = struct.unpack('>h', response[10:12])[0]
        host = response[12:12+host_len].decode('utf-8')
        port = struct.unpack('>i', response[12+host_len:16+host_len])[0]

        print(f"Coordinator: node_id={node_id}, host={host}, port={port}")
        print("✅ PASSED: FindCoordinator works")
        return True
    else:
        print(f"❌ FAILED: Error code {error_code}")
        return False

def main():
    """Run all tests"""
    print("="*70)
    print("Chronik Stream AdminClient API Compatibility Test Suite")
    print("="*70)

    results = []

    # Run all tests
    results.append(("DescribeAcls v0", test_describe_acls_v0()))
    results.append(("DescribeAcls v2", test_describe_acls_v2()))
    results.append(("ApiVersions", test_api_versions()))
    results.append(("Metadata", test_metadata()))
    results.append(("FindCoordinator", test_find_coordinator()))

    # Summary
    print("\n" + "="*70)
    print("SUMMARY")
    print("="*70)

    passed = sum(1 for _, result in results if result)
    total = len(results)

    for name, result in results:
        status = "✅ PASSED" if result else "❌ FAILED"
        print(f"{name:30s} {status}")

    print(f"\nTotal: {passed}/{total} tests passed")

    if passed == total:
        print("\n🎉 All tests PASSED! AdminClient APIs are compatible!")
        return 0
    else:
        print(f"\n⚠️  {total - passed} test(s) FAILED. AdminClient has compatibility issues.")
        return 1

if __name__ == "__main__":
    try:
        sys.exit(main())
    except KeyboardInterrupt:
        print("\n\nTest interrupted by user")
        sys.exit(1)
    except Exception as e:
        print(f"\n\n❌ Test suite failed with exception: {e}")
        import traceback
        traceback.print_exc()
        sys.exit(1)
