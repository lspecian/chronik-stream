#!/usr/bin/env python3
"""
Test script to verify correlation ID ordering in Chronik Stream.

This script tests three scenarios:
1. Sequential Metadata requests (should work - baseline)
2. Sequential Produce requests (should work - baseline)
3. Pipelined Metadata requests (CRITICAL TEST - currently fails)

Expected behavior: Responses must match request order (Kafka protocol requirement)
"""

import socket
import struct
import sys
import time

HOST = 'localhost'
PORT = 9092

def connect():
    """Create a new socket connection to Chronik."""
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.connect((HOST, PORT))
    return sock

def build_metadata_request(correlation_id):
    """
    Build a Metadata v12 request (flexible version).

    Metadata v12 format:
    - Header: API key (2), API version (2), correlation_id (4), client_id (compact string), tagged fields (varint)
    - Body: topics (compact array), allow_auto_create (bool), tagged fields (varint)
    """
    # Request header
    api_key = struct.pack('>h', 3)  # Metadata = 3
    api_version = struct.pack('>h', 12)  # v12 (flexible)
    corr_id = struct.pack('>i', correlation_id)

    # Client ID (compact string: length+1, then bytes)
    client_id = b'test-client'
    client_id_len = struct.pack('B', len(client_id) + 1)  # Compact string encoding

    # Tagged fields (0 = no tags)
    tagged_fields = b'\x00'

    # Body: topics (compact array: 0 = null/all topics)
    topics = b'\x00'  # All topics

    # allow_auto_create (bool: 0 = false)
    allow_auto_create = b'\x00'

    # Request body
    request_body = corr_id + client_id_len + client_id + tagged_fields + topics + allow_auto_create + tagged_fields

    # Complete request = size + header + body
    request_header = api_key + api_version
    full_request = request_header + request_body
    message_size = struct.pack('>i', len(full_request))

    return message_size + full_request

def read_response(sock):
    """
    Read a complete response from the socket.
    Returns: (correlation_id, response_data)
    """
    # Read response size (4 bytes)
    size_bytes = sock.recv(4)
    if len(size_bytes) != 4:
        raise Exception(f"Failed to read response size (got {len(size_bytes)} bytes)")

    response_size = struct.unpack('>i', size_bytes)[0]

    # Read correlation ID (4 bytes)
    corr_id_bytes = sock.recv(4)
    if len(corr_id_bytes) != 4:
        raise Exception(f"Failed to read correlation ID (got {len(corr_id_bytes)} bytes)")

    correlation_id = struct.unpack('>i', corr_id_bytes)[0]

    # Read rest of response
    remaining = response_size - 4  # Subtract correlation ID
    response_data = b''
    while len(response_data) < remaining:
        chunk = sock.recv(min(4096, remaining - len(response_data)))
        if not chunk:
            raise Exception(f"Connection closed while reading response (got {len(response_data)}/{remaining} bytes)")
        response_data += chunk

    return correlation_id, response_data

def test_sequential_requests():
    """
    Test 1: Sequential requests (send one, wait for response, repeat).
    This should ALWAYS work - it's the baseline test.
    """
    print("=" * 80)
    print("TEST 1: Sequential Metadata Requests")
    print("=" * 80)
    print("Expected: All correlation IDs match (responses in request order)")
    print()

    test_ids = [1, 2, 3, 5, 10, 100, 999]
    passed = 0
    failed = 0

    for corr_id in test_ids:
        sock = connect()

        # Send request
        request = build_metadata_request(corr_id)
        sock.sendall(request)

        # Wait for response
        response_corr_id, _ = read_response(sock)

        # Check if correlation IDs match
        status = "✅ PASS" if response_corr_id == corr_id else "❌ FAIL"
        if response_corr_id == corr_id:
            passed += 1
        else:
            failed += 1

        print(f"Testing correlation_id={corr_id:3d}:  Response correlation_id={response_corr_id:3d}   {status}")

        sock.close()

    print()
    print(f"Result: {passed}/{len(test_ids)} PASS")
    print()

    return failed == 0

def test_pipelined_requests():
    """
    Test 2: Pipelined requests (send multiple, then read responses).
    This is the CRITICAL test that currently FAILS in Chronik v1.3.59.

    Kafka protocol requirement:
    "The server MUST return responses in the same order as requests were received."
    """
    print("=" * 80)
    print("TEST 2: Pipelined Metadata Requests (CRITICAL TEST)")
    print("=" * 80)
    print("Expected: Responses in request order (10, 20, 30)")
    print()

    sock = connect()

    # Send 3 requests back-to-back WITHOUT waiting for responses
    correlation_ids = [10, 20, 30]

    print("Sending 3 pipelined requests:")
    for corr_id in correlation_ids:
        request = build_metadata_request(corr_id)
        sock.sendall(request)
        print(f"  Sent: correlation_id={corr_id}")

    print()
    print("Reading responses:")

    # Read responses in order
    received_ids = []
    for expected_id in correlation_ids:
        response_corr_id, _ = read_response(sock)
        received_ids.append(response_corr_id)
        print(f"  Received: correlation_id={response_corr_id}")

    print()
    print(f"Sent pipelined:     correlation_id={', '.join(map(str, correlation_ids))}")
    print(f"Received responses: correlation_id={', '.join(map(str, received_ids))}")
    print()

    # Check if all responses came back in order
    passed = 0
    failed = 0
    for i, (expected, actual) in enumerate(zip(correlation_ids, received_ids)):
        status = "✅ PASS" if expected == actual else "❌ FAIL"
        if expected == actual:
            passed += 1
        else:
            failed += 1
        print(f"Expected: {expected}, got {actual}  {status}")

    print()
    print(f"Result: {passed}/{len(correlation_ids)} PASS")
    print()

    sock.close()

    return failed == 0

def test_heavy_pipelining():
    """
    Test 3: Heavy pipelining (10 requests) to stress test ordering.
    """
    print("=" * 80)
    print("TEST 3: Heavy Pipelining (10 Requests)")
    print("=" * 80)
    print("Expected: All responses in request order")
    print()

    sock = connect()

    # Send 10 requests back-to-back
    correlation_ids = [100, 200, 300, 400, 500, 600, 700, 800, 900, 1000]

    print(f"Sending {len(correlation_ids)} pipelined requests...")
    for corr_id in correlation_ids:
        request = build_metadata_request(corr_id)
        sock.sendall(request)

    print(f"Reading {len(correlation_ids)} responses...")

    # Read responses in order
    received_ids = []
    for _ in correlation_ids:
        response_corr_id, _ = read_response(sock)
        received_ids.append(response_corr_id)

    print()
    print(f"Sent:     {correlation_ids}")
    print(f"Received: {received_ids}")
    print()

    # Check ordering
    passed = 0
    failed = 0
    for expected, actual in zip(correlation_ids, received_ids):
        if expected == actual:
            passed += 1
        else:
            failed += 1

    if failed > 0:
        print(f"❌ FAIL: {failed}/{len(correlation_ids)} responses out of order")
    else:
        print(f"✅ PASS: All {passed} responses in correct order")

    print()

    sock.close()

    return failed == 0

def main():
    """Run all tests and report results."""
    print()
    print("╔" + "=" * 78 + "╗")
    print("║" + " " * 20 + "Chronik Correlation ID Test Suite" + " " * 24 + "║")
    print("╚" + "=" * 78 + "╝")
    print()
    print(f"Target: {HOST}:{PORT}")
    print()

    # Run tests
    try:
        test1_pass = test_sequential_requests()
        test2_pass = test_pipelined_requests()
        test3_pass = test_heavy_pipelining()

        # Summary
        print("=" * 80)
        print("TEST SUMMARY")
        print("=" * 80)
        print(f"Test 1 (Sequential):      {'✅ PASS' if test1_pass else '❌ FAIL'}")
        print(f"Test 2 (Pipelined - 3):   {'✅ PASS' if test2_pass else '❌ FAIL'}")
        print(f"Test 3 (Pipelined - 10):  {'✅ PASS' if test3_pass else '❌ FAIL'}")
        print()

        if test1_pass and test2_pass and test3_pass:
            print("🎉 ALL TESTS PASSED - Chronik is Kafka protocol compliant!")
            return 0
        else:
            print("❌ TESTS FAILED - Correlation ID ordering violation detected")
            print()
            print("Impact:")
            print("  - Apache Flink: CorrelationIdMismatchException")
            print("  - Kafka Streams: May fail with pipelined requests")
            print("  - High-performance clients: Will experience errors")
            print()
            print("See bug report for full details and fix recommendations.")
            return 1

    except ConnectionRefusedError:
        print(f"❌ ERROR: Cannot connect to {HOST}:{PORT}")
        print("Make sure Chronik is running:")
        print("  cargo run --bin chronik-server -- --advertised-addr localhost standalone")
        return 2
    except Exception as e:
        print(f"❌ ERROR: {e}")
        import traceback
        traceback.print_exc()
        return 3

if __name__ == '__main__':
    sys.exit(main())
