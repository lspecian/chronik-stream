#!/usr/bin/env python3
"""Test FindCoordinator API implementation"""

import struct
import socket
import time
import threading
import hashlib

def encode_string(s):
    """Encode a string as Kafka protocol string (length + data)"""
    if s is None:
        return struct.pack('>h', -1)
    encoded = s.encode('utf-8')
    return struct.pack('>h', len(encoded)) + encoded

def murmur2(data, seed=0):
    """Simplified murmur2 hash for testing (not exact match to Kafka's implementation)"""
    # This is a placeholder - real implementation would need to match Kafka's murmur2
    import hashlib
    h = hashlib.md5(data + str(seed).encode()).digest()
    return struct.unpack('>I', h[:4])[0]

def send_find_coordinator(host, port, key, key_type=0, version=0):
    """Send FindCoordinator request"""
    body = b''
    
    # Coordinator key (e.g., consumer group ID)
    body += encode_string(key)
    
    # Key type (v1+): 0 = group, 1 = transaction
    if version >= 1:
        body += struct.pack('>b', key_type)
    
    header = b''
    header += struct.pack('>h', 10)  # API key (FindCoordinator)
    header += struct.pack('>h', version)  # API version
    header += struct.pack('>i', 456)  # Correlation ID
    header += encode_string('find-coordinator-test')  # Client ID
    
    request = header + body
    request_with_length = struct.pack('>i', len(request)) + request
    
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.connect((host, port))
    sock.send(request_with_length)
    
    sock.settimeout(5.0)
    response_length_data = sock.recv(4)
    response_length = struct.unpack('>i', response_length_data)[0]
    response_data = sock.recv(response_length)
    sock.close()
    
    # Parse response
    pos = 4  # Skip correlation ID
    
    # Throttle time (v1+)
    throttle_time = 0
    if version >= 1:
        throttle_time = struct.unpack('>i', response_data[pos:pos+4])[0]
        pos += 4
    
    # Error code
    error_code = struct.unpack('>h', response_data[pos:pos+2])[0]
    pos += 2
    
    # Error message (v1+)
    error_message = None
    if version >= 1:
        msg_len = struct.unpack('>h', response_data[pos:pos+2])[0]
        pos += 2
        if msg_len > 0:
            error_message = response_data[pos:pos+msg_len].decode('utf-8')
            pos += msg_len
    
    # Node ID
    node_id = struct.unpack('>i', response_data[pos:pos+4])[0]
    pos += 4
    
    # Host
    host_len = struct.unpack('>h', response_data[pos:pos+2])[0]
    pos += 2
    host = response_data[pos:pos+host_len].decode('utf-8')
    pos += host_len
    
    # Port
    port = struct.unpack('>i', response_data[pos:pos+4])[0]
    
    return {
        'throttle_time': throttle_time,
        'error_code': error_code,
        'error_message': error_message,
        'node_id': node_id,
        'host': host,
        'port': port
    }

def test_basic_find_coordinator():
    """Test basic FindCoordinator request"""
    print("=== Testing Basic FindCoordinator ===\n")
    
    # Test with different consumer groups
    groups = ['test-group-1', 'test-group-2', 'test-group-3']
    
    for group in groups:
        result = send_find_coordinator('localhost', 9092, group)
        print(f"Group: {group}")
        print(f"  Coordinator node: {result['node_id']} ({result['host']}:{result['port']})")
        print(f"  Error code: {result['error_code']}")
        
        if result['error_code'] != 0:
            print(f"  ✗ Error: {result['error_message']}")
        else:
            print(f"  ✓ Success")

def test_coordinator_consistency():
    """Test that same group always gets same coordinator"""
    print("\n=== Testing Coordinator Consistency ===\n")
    
    group_id = "consistency-test-group"
    coordinator_nodes = set()
    
    # Request coordinator multiple times
    for i in range(10):
        result = send_find_coordinator('localhost', 9092, group_id)
        if result['error_code'] == 0:
            coordinator_nodes.add(result['node_id'])
    
    if len(coordinator_nodes) == 1:
        print(f"✓ Consistent: Group '{group_id}' always assigned to node {coordinator_nodes.pop()}")
    else:
        print(f"✗ Inconsistent: Group assigned to multiple nodes: {coordinator_nodes}")

def test_transaction_coordinator():
    """Test FindCoordinator for transaction type"""
    print("\n=== Testing Transaction Coordinator ===\n")
    
    transaction_id = "test-transaction-1"
    
    # Request with key_type = 1 (transaction)
    result = send_find_coordinator('localhost', 9092, transaction_id, key_type=1, version=1)
    
    print(f"Transaction ID: {transaction_id}")
    print(f"  Coordinator node: {result['node_id']} ({result['host']}:{result['port']})")
    print(f"  Error code: {result['error_code']}")
    
    if result['error_code'] != 0:
        print(f"  ✗ Error: {result['error_message']}")
    else:
        print(f"  ✓ Success")

def test_distribution():
    """Test coordinator distribution across multiple groups"""
    print("\n=== Testing Coordinator Distribution ===\n")
    
    num_groups = 100
    coordinator_distribution = {}
    
    for i in range(num_groups):
        group_id = f"distribution-test-{i}"
        result = send_find_coordinator('localhost', 9092, group_id)
        
        if result['error_code'] == 0:
            node_id = result['node_id']
            coordinator_distribution[node_id] = coordinator_distribution.get(node_id, 0) + 1
    
    print(f"Distribution across {len(coordinator_distribution)} coordinator nodes:")
    for node_id, count in sorted(coordinator_distribution.items()):
        percentage = (count / num_groups) * 100
        print(f"  Node {node_id}: {count} groups ({percentage:.1f}%)")

def test_concurrent_requests():
    """Test concurrent FindCoordinator requests"""
    print("\n=== Testing Concurrent Requests ===\n")
    
    results = []
    errors = []
    
    def find_coordinator_thread(thread_id):
        group_id = f"concurrent-test-{thread_id}"
        try:
            result = send_find_coordinator('localhost', 9092, group_id)
            results.append((thread_id, group_id, result))
        except Exception as e:
            errors.append((thread_id, str(e)))
    
    # Start multiple threads
    threads = []
    for i in range(20):
        thread = threading.Thread(target=find_coordinator_thread, args=(i,))
        threads.append(thread)
        thread.start()
    
    # Wait for all threads
    for thread in threads:
        thread.join()
    
    print(f"Completed {len(results)} successful requests")
    print(f"Failed requests: {len(errors)}")
    
    if errors:
        print("\nErrors:")
        for thread_id, error in errors[:5]:  # Show first 5 errors
            print(f"  Thread {thread_id}: {error}")

def test_invalid_requests():
    """Test error handling for invalid requests"""
    print("\n=== Testing Invalid Requests ===\n")
    
    # Test 1: Empty group ID
    print("1. Testing empty group ID")
    try:
        result = send_find_coordinator('localhost', 9092, '')
        if result['error_code'] != 0:
            print(f"✓ Correctly rejected with error code {result['error_code']}")
        else:
            print(f"✗ Should have failed but succeeded")
    except Exception as e:
        print(f"✓ Correctly failed: {e}")
    
    # Test 2: Invalid key type
    print("\n2. Testing invalid key type (2)")
    try:
        result = send_find_coordinator('localhost', 9092, 'test-group', key_type=2, version=1)
        if result['error_code'] != 0:
            print(f"✓ Correctly rejected with error code {result['error_code']}")
        else:
            print(f"✗ Should have failed but succeeded")
    except Exception as e:
        print(f"✓ Correctly failed: {e}")

def test_version_compatibility():
    """Test different API versions"""
    print("\n=== Testing Version Compatibility ===\n")
    
    group_id = "version-test-group"
    
    # Test v0 (original version)
    print("Testing API version 0:")
    result = send_find_coordinator('localhost', 9092, group_id, version=0)
    print(f"  Result: Node {result['node_id']}, Error: {result['error_code']}")
    
    # Test v1 (with key type)
    print("\nTesting API version 1:")
    result = send_find_coordinator('localhost', 9092, group_id, version=1)
    print(f"  Result: Node {result['node_id']}, Error: {result['error_code']}")
    print(f"  Throttle time: {result['throttle_time']}ms")

def main():
    print("Starting FindCoordinator Tests\n")
    print("Make sure Chronik is running on localhost:9092\n")
    
    try:
        test_basic_find_coordinator()
        test_coordinator_consistency()
        test_transaction_coordinator()
        test_distribution()
        test_concurrent_requests()
        test_invalid_requests()
        test_version_compatibility()
        
        print("\n=== All Tests Completed ===")
        
    except Exception as e:
        print(f"\n✗ Test failed: {e}")
        print("Make sure Chronik is running on localhost:9092")

if __name__ == "__main__":
    main()