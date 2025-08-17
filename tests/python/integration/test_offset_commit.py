#!/usr/bin/env python3
"""Integration test for OffsetCommit and OffsetFetch APIs"""

import struct
import socket
import time
import subprocess
import threading

def encode_string(s):
    """Encode a string as Kafka protocol string (length + data)"""
    if s is None:
        return struct.pack('>h', -1)
    encoded = s.encode('utf-8')
    return struct.pack('>h', len(encoded)) + encoded

def encode_nullable_string(s):
    """Encode a nullable string (v1+ protocols)"""
    if s is None:
        return struct.pack('>h', -1)
    encoded = s.encode('utf-8')
    return struct.pack('>h', len(encoded)) + encoded

def send_offset_commit(host, port, group_id, generation_id, member_id, topics_offsets):
    """Send OffsetCommit request"""
    body = b''
    body += encode_string(group_id)
    body += struct.pack('>i', generation_id)  # generation ID
    body += encode_string(member_id)
    body += struct.pack('>i', len(topics_offsets))  # topic count
    
    for topic, partitions in topics_offsets.items():
        body += encode_string(topic)
        body += struct.pack('>i', len(partitions))  # partition count
        
        for partition, offset, metadata in partitions:
            body += struct.pack('>i', partition)  # partition index
            body += struct.pack('>q', offset)  # committed offset
            body += encode_nullable_string(metadata)  # metadata
    
    header = b''
    header += struct.pack('>h', 8)  # API key (OffsetCommit)
    header += struct.pack('>h', 0)  # API version 0
    header += struct.pack('>i', 123)  # Correlation ID
    header += encode_string('offset-test-client')  # Client ID
    
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
    topic_count = struct.unpack('>i', response_data[pos:pos+4])[0]
    pos += 4
    
    results = {}
    for _ in range(topic_count):
        topic_name_len = struct.unpack('>h', response_data[pos:pos+2])[0]
        pos += 2
        topic_name = response_data[pos:pos+topic_name_len].decode('utf-8')
        pos += topic_name_len
        
        partition_count = struct.unpack('>i', response_data[pos:pos+4])[0]
        pos += 4
        
        partitions = []
        for _ in range(partition_count):
            partition_index = struct.unpack('>i', response_data[pos:pos+4])[0]
            pos += 4
            error_code = struct.unpack('>h', response_data[pos:pos+2])[0]
            pos += 2
            partitions.append((partition_index, error_code))
        
        results[topic_name] = partitions
    
    return results

def send_offset_fetch(host, port, group_id, topics):
    """Send OffsetFetch request"""
    body = b''
    body += encode_string(group_id)
    
    if topics is None:
        body += struct.pack('>i', -1)  # Fetch all topics
    else:
        body += struct.pack('>i', len(topics))  # topic count
        for topic in topics:
            body += encode_string(topic)
    
    header = b''
    header += struct.pack('>h', 9)  # API key (OffsetFetch)
    header += struct.pack('>h', 0)  # API version 0
    header += struct.pack('>i', 456)  # Correlation ID
    header += encode_string('offset-test-client')  # Client ID
    
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
    topic_count = struct.unpack('>i', response_data[pos:pos+4])[0]
    pos += 4
    
    results = {}
    for _ in range(topic_count):
        topic_name_len = struct.unpack('>h', response_data[pos:pos+2])[0]
        pos += 2
        topic_name = response_data[pos:pos+topic_name_len].decode('utf-8')
        pos += topic_name_len
        
        partition_count = struct.unpack('>i', response_data[pos:pos+4])[0]
        pos += 4
        
        partitions = []
        for _ in range(partition_count):
            partition_index = struct.unpack('>i', response_data[pos:pos+4])[0]
            pos += 4
            committed_offset = struct.unpack('>q', response_data[pos:pos+8])[0]
            pos += 8
            
            # Read metadata (nullable string)
            metadata_len = struct.unpack('>h', response_data[pos:pos+2])[0]
            pos += 2
            if metadata_len >= 0:
                metadata = response_data[pos:pos+metadata_len].decode('utf-8')
                pos += metadata_len
            else:
                metadata = None
            
            error_code = struct.unpack('>h', response_data[pos:pos+2])[0]
            pos += 2
            
            partitions.append((partition_index, committed_offset, metadata, error_code))
        
        results[topic_name] = partitions
    
    return results

def test_basic_offset_commit_fetch():
    """Test basic offset commit and fetch"""
    print("=== Testing Basic Offset Commit/Fetch ===\n")
    
    group_id = f"test-group-{int(time.time())}"
    
    # Commit some offsets
    print(f"1. Committing offsets for group: {group_id}")
    offsets = {
        'test-topic': [
            (0, 100, "checkpoint-1"),
            (1, 200, "checkpoint-2"),
            (2, 300, None),
        ]
    }
    
    results = send_offset_commit('localhost', 9092, group_id, 1, 'member-1', offsets)
    
    print("Commit results:")
    for topic, partitions in results.items():
        for partition, error_code in partitions:
            status = "✓ Success" if error_code == 0 else f"✗ Error {error_code}"
            print(f"  {topic}:{partition} - {status}")
    
    # Fetch the offsets back
    print(f"\n2. Fetching offsets for group: {group_id}")
    fetched = send_offset_fetch('localhost', 9092, group_id, ['test-topic'])
    
    print("Fetched offsets:")
    for topic, partitions in fetched.items():
        for partition, offset, metadata, error_code in partitions:
            if error_code == 0:
                print(f"  {topic}:{partition} - offset={offset}, metadata={metadata}")
            else:
                print(f"  {topic}:{partition} - Error {error_code}")

def test_invalid_offsets():
    """Test error handling for invalid offsets"""
    print("\n=== Testing Invalid Offset Commits ===\n")
    
    # Test 1: Empty group ID
    print("1. Testing empty group ID")
    try:
        results = send_offset_commit('localhost', 9092, '', 1, 'member-1', 
                                   {'topic': [(0, 100, None)]})
        print(f"Result: {results}")
    except Exception as e:
        print(f"✓ Correctly failed: {e}")
    
    # Test 2: Invalid offset
    print("\n2. Testing invalid offset (-5)")
    results = send_offset_commit('localhost', 9092, 'test-invalid', 1, 'member-1',
                               {'topic': [(0, -5, None)]})
    for topic, partitions in results.items():
        for partition, error_code in partitions:
            if error_code != 0:
                print(f"✓ Correctly rejected with error code {error_code}")
            else:
                print(f"✗ Should have failed but succeeded")
    
    # Test 3: Large metadata
    print("\n3. Testing oversized metadata")
    large_metadata = "x" * 5000  # Over 4KB limit
    results = send_offset_commit('localhost', 9092, 'test-metadata', 1, 'member-1',
                               {'topic': [(0, 100, large_metadata)]})
    for topic, partitions in results.items():
        for partition, error_code in partitions:
            if error_code == 12:  # OFFSET_METADATA_TOO_LARGE
                print(f"✓ Correctly rejected with OFFSET_METADATA_TOO_LARGE")
            else:
                print(f"✗ Expected error 12 but got {error_code}")

def test_concurrent_commits():
    """Test concurrent offset commits"""
    print("\n=== Testing Concurrent Offset Commits ===\n")
    
    group_id = f"concurrent-group-{int(time.time())}"
    results = []
    
    def commit_offset(thread_id):
        offset = 100 + thread_id * 10
        try:
            result = send_offset_commit('localhost', 9092, group_id, 1, f'member-{thread_id}',
                                      {'test-topic': [(0, offset, f"thread-{thread_id}")]})
            results.append((thread_id, offset, result))
        except Exception as e:
            results.append((thread_id, offset, str(e)))
    
    # Start multiple threads
    threads = []
    for i in range(5):
        thread = threading.Thread(target=commit_offset, args=(i,))
        threads.append(thread)
        thread.start()
    
    for thread in threads:
        thread.join()
    
    print(f"Concurrent commit results for {len(results)} threads:")
    for thread_id, offset, result in results:
        print(f"  Thread {thread_id} (offset={offset}): {result}")
    
    # Fetch final offset
    time.sleep(1)
    fetched = send_offset_fetch('localhost', 9092, group_id, ['test-topic'])
    for topic, partitions in fetched.items():
        for partition, offset, metadata, error_code in partitions:
            if error_code == 0:
                print(f"\nFinal offset: {offset} (metadata={metadata})")

def test_offset_persistence():
    """Test that offsets persist across restarts"""
    print("\n=== Testing Offset Persistence ===\n")
    
    group_id = f"persist-group-{int(time.time())}"
    
    # Commit offset
    print("1. Committing offset")
    send_offset_commit('localhost', 9092, group_id, 1, 'member-1',
                      {'persist-topic': [(0, 12345, "persistent-data")]})
    
    # Wait a bit
    time.sleep(2)
    
    # Fetch with different member ID (simulating restart)
    print("2. Fetching with different member ID")
    fetched = send_offset_fetch('localhost', 9092, group_id, ['persist-topic'])
    
    for topic, partitions in fetched.items():
        for partition, offset, metadata, error_code in partitions:
            if error_code == 0 and offset == 12345:
                print(f"✓ Offset persisted correctly: {offset} (metadata={metadata})")
            else:
                print(f"✗ Persistence failed: offset={offset}, error={error_code}")

def test_multi_partition_commit():
    """Test committing offsets for multiple partitions"""
    print("\n=== Testing Multi-Partition Commits ===\n")
    
    group_id = f"multi-partition-{int(time.time())}"
    
    # Commit offsets for multiple topics and partitions
    offsets = {
        'topic-a': [
            (0, 100, "a-0"),
            (1, 200, "a-1"),
            (2, 300, "a-2"),
        ],
        'topic-b': [
            (0, 400, "b-0"),
            (1, 500, None),
        ],
        'topic-c': [
            (0, 600, "c-0"),
        ]
    }
    
    print("Committing offsets for 3 topics, 6 partitions total")
    results = send_offset_commit('localhost', 9092, group_id, 1, 'member-1', offsets)
    
    success_count = 0
    for topic, partitions in results.items():
        for partition, error_code in partitions:
            if error_code == 0:
                success_count += 1
    
    print(f"✓ Successfully committed {success_count} partition offsets")
    
    # Fetch all back
    fetched = send_offset_fetch('localhost', 9092, group_id, ['topic-a', 'topic-b', 'topic-c'])
    
    print("\nVerifying fetched offsets:")
    for topic, partitions in fetched.items():
        for partition, offset, metadata, error_code in partitions:
            if error_code == 0:
                print(f"  {topic}:{partition} = {offset} ({metadata})")

def main():
    print("Starting OffsetCommit/Fetch Integration Tests\n")
    
    # Ensure services are running
    print("Ensuring services are running...")
    subprocess.run(['docker-compose', 'up', '-d'], capture_output=True)
    time.sleep(5)
    
    try:
        test_basic_offset_commit_fetch()
        test_invalid_offsets()
        test_concurrent_commits()
        test_offset_persistence()
        test_multi_partition_commit()
        
        print("\n=== All Tests Completed ===")
        
    except Exception as e:
        print(f"\n✗ Test failed: {e}")
        raise

if __name__ == "__main__":
    main()