#!/usr/bin/env python3
"""Performance test for OffsetCommit API"""

import struct
import socket
import time
import statistics
import threading
import concurrent.futures

def encode_string(s):
    """Encode a string as Kafka protocol string (length + data)"""
    if s is None:
        return struct.pack('>h', -1)
    encoded = s.encode('utf-8')
    return struct.pack('>h', len(encoded)) + encoded

def encode_nullable_string(s):
    """Encode a nullable string"""
    if s is None:
        return struct.pack('>h', -1)
    encoded = s.encode('utf-8')
    return struct.pack('>h', len(encoded)) + encoded

def commit_offset_batch(host, port, group_id, start_offset, count):
    """Commit a batch of offsets and measure time"""
    start_time = time.time()
    
    # Build offset commit request
    body = b''
    body += encode_string(group_id)
    body += struct.pack('>i', 1)  # generation ID
    body += encode_string('perf-member')
    body += struct.pack('>i', 1)  # topic count
    
    # Add topic with multiple partitions
    body += encode_string('perf-topic')
    body += struct.pack('>i', count)  # partition count
    
    for i in range(count):
        body += struct.pack('>i', i)  # partition index
        body += struct.pack('>q', start_offset + i)  # offset
        body += encode_nullable_string(f"meta-{i}")  # metadata
    
    header = b''
    header += struct.pack('>h', 8)  # API key (OffsetCommit)
    header += struct.pack('>h', 0)  # API version
    header += struct.pack('>i', 123)  # Correlation ID
    header += encode_string('perf-test')  # Client ID
    
    request = header + body
    request_with_length = struct.pack('>i', len(request)) + request
    
    # Send request
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.connect((host, port))
    sock.send(request_with_length)
    
    # Receive response
    sock.settimeout(5.0)
    response_length_data = sock.recv(4)
    response_length = struct.unpack('>i', response_length_data)[0]
    response_data = sock.recv(response_length)
    sock.close()
    
    commit_time = time.time() - start_time
    
    # Count successes
    pos = 4  # Skip correlation ID
    topic_count = struct.unpack('>i', response_data[pos:pos+4])[0]
    pos += 4
    
    success_count = 0
    for _ in range(topic_count):
        topic_name_len = struct.unpack('>h', response_data[pos:pos+2])[0]
        pos += 2 + topic_name_len
        partition_count = struct.unpack('>i', response_data[pos:pos+4])[0]
        pos += 4
        
        for _ in range(partition_count):
            pos += 4  # partition index
            error_code = struct.unpack('>h', response_data[pos:pos+2])[0]
            pos += 2
            if error_code == 0:
                success_count += 1
    
    return commit_time, success_count

def test_throughput():
    """Test offset commit throughput"""
    print("=== Offset Commit Throughput Test ===\n")
    
    # Test different batch sizes
    batch_sizes = [1, 10, 50, 100, 200]
    
    for batch_size in batch_sizes:
        times = []
        total_commits = 0
        
        print(f"Testing batch size: {batch_size} partitions")
        
        # Run multiple iterations
        for i in range(10):
            group_id = f"perf-group-{batch_size}-{i}"
            commit_time, success_count = commit_offset_batch(
                'localhost', 9092, group_id, i * 1000, batch_size
            )
            
            if success_count == batch_size:
                times.append(commit_time)
                total_commits += success_count
        
        if times:
            avg_time = statistics.mean(times)
            min_time = min(times)
            max_time = max(times)
            commits_per_sec = batch_size / avg_time
            
            print(f"  Avg time: {avg_time*1000:.2f}ms")
            print(f"  Min/Max: {min_time*1000:.2f}ms / {max_time*1000:.2f}ms")
            print(f"  Throughput: {commits_per_sec:.0f} commits/sec")
            print(f"  Total successful: {total_commits}\n")

def test_concurrent_groups():
    """Test concurrent commits from different groups"""
    print("=== Concurrent Consumer Groups Test ===\n")
    
    num_groups = 10
    commits_per_group = 50
    
    def commit_for_group(group_num):
        group_id = f"concurrent-group-{group_num}"
        times = []
        
        for i in range(commits_per_group):
            start = time.time()
            commit_time, _ = commit_offset_batch(
                'localhost', 9092, group_id, i * 100, 1
            )
            times.append(commit_time)
        
        return group_num, times
    
    print(f"Running {num_groups} concurrent groups, {commits_per_group} commits each")
    
    start_time = time.time()
    
    with concurrent.futures.ThreadPoolExecutor(max_workers=num_groups) as executor:
        futures = [executor.submit(commit_for_group, i) for i in range(num_groups)]
        results = [f.result() for f in concurrent.futures.as_completed(futures)]
    
    total_time = time.time() - start_time
    
    # Analyze results
    all_times = []
    for group_num, times in results:
        all_times.extend(times)
    
    total_commits = len(all_times)
    avg_latency = statistics.mean(all_times)
    p50_latency = statistics.median(all_times)
    p99_latency = sorted(all_times)[int(len(all_times) * 0.99)]
    
    print(f"Total time: {total_time:.2f}s")
    print(f"Total commits: {total_commits}")
    print(f"Overall throughput: {total_commits/total_time:.0f} commits/sec")
    print(f"Average latency: {avg_latency*1000:.2f}ms")
    print(f"P50 latency: {p50_latency*1000:.2f}ms")
    print(f"P99 latency: {p99_latency*1000:.2f}ms")

def test_large_metadata():
    """Test performance with different metadata sizes"""
    print("\n=== Metadata Size Impact Test ===\n")
    
    metadata_sizes = [0, 100, 500, 1000, 2000, 4000]
    
    for size in metadata_sizes:
        metadata = "x" * size if size > 0 else None
        times = []
        
        print(f"Testing metadata size: {size} bytes")
        
        for i in range(5):
            group_id = f"metadata-test-{size}-{i}"
            
            # Build request with metadata
            body = b''
            body += encode_string(group_id)
            body += struct.pack('>i', 1)
            body += encode_string('member')
            body += struct.pack('>i', 1)
            body += encode_string('topic')
            body += struct.pack('>i', 1)
            body += struct.pack('>i', 0)
            body += struct.pack('>q', 100)
            body += encode_nullable_string(metadata)
            
            header = b''
            header += struct.pack('>h', 8)
            header += struct.pack('>h', 0)
            header += struct.pack('>i', 123)
            header += encode_string('test')
            
            request = header + body
            request_with_length = struct.pack('>i', len(request)) + request
            
            start = time.time()
            sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
            sock.connect(('localhost', 9092))
            sock.send(request_with_length)
            
            sock.settimeout(5.0)
            response_length_data = sock.recv(4)
            response_length = struct.unpack('>i', response_length_data)[0]
            response_data = sock.recv(response_length)
            sock.close()
            
            elapsed = time.time() - start
            
            # Check if successful
            if len(response_data) > 10:
                times.append(elapsed)
        
        if times:
            avg_time = statistics.mean(times)
            print(f"  Average time: {avg_time*1000:.2f}ms\n")

def test_cache_effectiveness():
    """Test cache hit rates"""
    print("=== Cache Effectiveness Test ===\n")
    
    group_id = "cache-test-group"
    
    # Commit initial offsets
    print("1. Committing initial offsets...")
    commit_time1, _ = commit_offset_batch('localhost', 9092, group_id, 1000, 10)
    print(f"   Initial commit: {commit_time1*1000:.2f}ms")
    
    # Immediate re-commit (should be faster due to cache)
    print("\n2. Re-committing same offsets (cache warm)...")
    times = []
    for i in range(5):
        commit_time, _ = commit_offset_batch('localhost', 9092, group_id, 1000, 10)
        times.append(commit_time)
    
    avg_cached = statistics.mean(times)
    print(f"   Average cached commit: {avg_cached*1000:.2f}ms")
    
    if avg_cached < commit_time1:
        speedup = commit_time1 / avg_cached
        print(f"   Cache speedup: {speedup:.2f}x")
    
    # Commit different offsets
    print("\n3. Committing different offsets...")
    commit_time2, _ = commit_offset_batch('localhost', 9092, group_id, 2000, 10)
    print(f"   New offset commit: {commit_time2*1000:.2f}ms")

def main():
    print("Starting Offset Commit Performance Tests\n")
    print("Make sure services are running with: docker-compose up -d\n")
    
    try:
        test_throughput()
        test_concurrent_groups()
        test_large_metadata()
        test_cache_effectiveness()
        
        print("\n=== Performance Tests Completed ===")
        
    except Exception as e:
        print(f"\n✗ Test failed: {e}")
        print("Make sure Chronik is running on localhost:9092")

if __name__ == "__main__":
    main()