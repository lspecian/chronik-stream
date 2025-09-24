#!/usr/bin/env python3
"""
Regression tests for Chronik Stream.
These tests ensure that previously fixed bugs don't reappear.
"""

import sys
import time
import socket
import struct
import logging

logging.basicConfig(level=logging.INFO)
logger = logging.getLogger(__name__)

CHRONIK_HOST = 'localhost'
CHRONIK_PORT = 9095

def send_kafka_request(request_bytes):
    """Send a raw Kafka request and get the response."""
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.connect((CHRONIK_HOST, CHRONIK_PORT))
    sock.settimeout(5)

    # Send the request
    sock.sendall(request_bytes)

    # Read the response length
    response_len_bytes = sock.recv(4)
    if len(response_len_bytes) < 4:
        raise Exception("Failed to read response length")

    response_len = struct.unpack('>I', response_len_bytes)[0]

    # Read the response
    response_data = b''
    while len(response_data) < response_len:
        chunk = sock.recv(min(4096, response_len - len(response_data)))
        if not chunk:
            break
        response_data += chunk

    sock.close()
    return response_data

def test_api_versions_v3_buffer_overflow():
    """
    Regression test for v1.3.0 buffer overflow bug.
    This request caused Chronik to panic with buffer overflow.
    """
    logger.info("Testing ApiVersions v3 (buffer overflow regression)...")

    # This is the exact request that crashed v1.3.0
    # ApiVersions request v3 with flexible encoding
    request = bytes.fromhex(
        '0000002e'  # Length: 46
        '0012'      # API Key: 18 (ApiVersions)
        '0003'      # API Version: 3
        '00000001'  # Correlation ID: 1
        '09'        # Client ID length (compact string): 8 + 1
        '6b61666b612d7079'  # Client ID: "kafka-py"
        '00'        # Tagged fields: 0 (none)
        '0a'        # Client Software Name length: 9 + 1
        '6b61666b612d707974686f6e'  # "kafka-python"
        '08'        # Client Software Version length: 7 + 1
        '322e302e322e646576'  # "2.0.2.dev"
        '00'        # Tagged fields: 0
    )

    try:
        response = send_kafka_request(request)
        logger.info(f"Received response: {len(response)} bytes")
        logger.info("✓ Buffer overflow regression test PASSED")
        return True
    except Exception as e:
        logger.error(f"✗ Buffer overflow regression test FAILED: {e}")
        return False

def test_metadata_response_format():
    """
    Regression test for v1.3.1 metadata response issues.
    Clients were rejecting the metadata response.
    """
    logger.info("Testing Metadata response format...")

    # Metadata request v9 (flexible)
    request = bytes.fromhex(
        '00000019'  # Length: 25
        '0003'      # API Key: 3 (Metadata)
        '0009'      # API Version: 9
        '00000002'  # Correlation ID: 2
        '09'        # Client ID length (compact): 8 + 1
        '746573745f6d657461'  # Client ID: "test_meta"
        '00'        # Tagged fields
        '01'        # Topics: null (all topics)
        '01'        # Allow auto topic creation: true
        '00'        # Tagged fields
    )

    try:
        response = send_kafka_request(request)
        logger.info(f"Received metadata response: {len(response)} bytes")

        # Parse response header
        if len(response) < 4:
            raise Exception(f"Response too short: {len(response)} bytes")

        correlation_id = struct.unpack('>I', response[0:4])[0]
        if correlation_id != 2:
            raise Exception(f"Wrong correlation ID: {correlation_id}")

        # Check for minimum expected size
        # v9 response should have at least:
        # - 4 bytes correlation ID
        # - 1 byte tagged fields
        # - 4 bytes throttle time
        # - 1+ byte brokers array
        # - more...
        if len(response) < 20:
            raise Exception(f"Response too small for valid metadata: {len(response)} bytes")

        logger.info("✓ Metadata response format test PASSED")
        return True
    except Exception as e:
        logger.error(f"✗ Metadata response format test FAILED: {e}")
        return False

def test_listoffsets_v7_tagged_fields():
    """
    Regression test for ListOffsets v7 tagged field parsing.
    This was causing "Failed to get topic offsets" in KSQL.
    """
    logger.info("Testing ListOffsets v7 with tagged fields...")

    # ListOffsets request v7 with flexible encoding and tagged fields
    request = bytes.fromhex(
        '00000023'  # Length: 35
        '0002'      # API Key: 2 (ListOffsets)
        '0007'      # API Version: 7
        '00000003'  # Correlation ID: 3
        '09'        # Client ID length: 8 + 1
        '746573745f6c697374'  # Client ID: "test_list"
        '00'        # Tagged fields in header: 0
        'ffffffff'  # Replica ID: -1 (consumer)
        '01'        # Isolation level: 1
        '01'        # Topics array: 0 + 1 = 1 topic
        '00'        # Tagged fields: 0
    )

    try:
        response = send_kafka_request(request)
        logger.info(f"Received ListOffsets response: {len(response)} bytes")

        # Basic validation
        if len(response) < 8:
            raise Exception(f"Response too short: {len(response)} bytes")

        correlation_id = struct.unpack('>I', response[0:4])[0]
        if correlation_id != 3:
            raise Exception(f"Wrong correlation ID: {correlation_id}")

        logger.info("✓ ListOffsets v7 tagged fields test PASSED")
        return True
    except Exception as e:
        logger.error(f"✗ ListOffsets v7 tagged fields test FAILED: {e}")
        return False

def run_regression_tests():
    """Run all regression tests."""
    logger.info("=" * 60)
    logger.info("Running Chronik Stream Regression Tests")
    logger.info("=" * 60)

    # Wait for Chronik to be ready
    logger.info(f"Connecting to Chronik at {CHRONIK_HOST}:{CHRONIK_PORT}...")
    for i in range(30):
        sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        sock.settimeout(1)
        result = sock.connect_ex((CHRONIK_HOST, CHRONIK_PORT))
        sock.close()
        if result == 0:
            logger.info("Chronik is ready")
            break
        time.sleep(1)
    else:
        logger.error("Chronik is not responding")
        return False

    # Run tests
    tests = [
        test_api_versions_v3_buffer_overflow,
        test_metadata_response_format,
        test_listoffsets_v7_tagged_fields,
    ]

    results = []
    for test in tests:
        try:
            result = test()
            results.append(result)
        except Exception as e:
            logger.error(f"Test {test.__name__} crashed: {e}")
            results.append(False)

    # Summary
    logger.info("=" * 60)
    passed = sum(results)
    total = len(results)
    logger.info(f"Regression Test Summary: {passed}/{total} tests passed")

    if passed == total:
        logger.info("✓ All regression tests PASSED")
    else:
        logger.error(f"✗ {total - passed} regression tests FAILED")

    return passed == total

if __name__ == '__main__':
    success = run_regression_tests()
    sys.exit(0 if success else 1)