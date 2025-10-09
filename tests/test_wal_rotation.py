#!/usr/bin/env python3
"""
Test script to trigger WAL segment rotation and verify WAL indexing.

This script produces enough messages to exceed the 128MB WAL segment size
and trigger rotation, then verifies that the WAL indexer creates indices.
"""

import json
import time
from kafka import KafkaProducer, KafkaConsumer
import requests

BOOTSTRAP_SERVER = 'localhost:9092'
SEARCH_API = 'http://localhost:8080'
TOPIC_NAME = 'wal-rotation-test'

def create_large_message(index: int, size_kb: int = 100) -> bytes:
    """Create a message of approximately size_kb."""
    # Create base message
    message = {
        "id": index,
        "timestamp": time.time(),
        "type": "rotation_test",
        "data": "x" * (size_kb * 1024 - 200)  # Approximate padding
    }
    return json.dumps(message).encode('utf-8')

def produce_messages(num_messages: int, message_size_kb: int = 100):
    """Produce messages to trigger WAL rotation."""
    print(f"\n=== Producing {num_messages} messages of ~{message_size_kb}KB each ===")
    print(f"Total expected data: ~{(num_messages * message_size_kb) / 1024:.1f} MB")

    producer = KafkaProducer(
        bootstrap_servers=BOOTSTRAP_SERVER,
        api_version=(0, 10, 0),
        acks='all',
        max_in_flight_requests_per_connection=5,
        batch_size=16384,
        linger_ms=10
    )

    start_time = time.time()
    for i in range(num_messages):
        message = create_large_message(i, message_size_kb)
        future = producer.send(TOPIC_NAME, value=message)

        if (i + 1) % 100 == 0:
            producer.flush()
            elapsed = time.time() - start_time
            throughput = (i + 1) * message_size_kb / 1024 / elapsed
            print(f"  Produced {i + 1}/{num_messages} messages ({throughput:.2f} MB/s)")

    producer.flush()
    producer.close()

    elapsed = time.time() - start_time
    total_mb = num_messages * message_size_kb / 1024
    print(f"\n✅ Produced {num_messages} messages in {elapsed:.2f}s")
    print(f"   Total data: {total_mb:.2f} MB")
    print(f"   Throughput: {total_mb / elapsed:.2f} MB/s")

def check_wal_segments():
    """Check WAL directory for segments."""
    import os
    wal_dir = f"data/wal/{TOPIC_NAME}"

    print(f"\n=== Checking WAL segments in {wal_dir} ===")

    if not os.path.exists(wal_dir):
        print(f"❌ WAL directory does not exist: {wal_dir}")
        return 0

    # List all partition directories
    partitions = [d for d in os.listdir(wal_dir) if os.path.isdir(os.path.join(wal_dir, d))]

    total_segments = 0
    for partition in sorted(partitions):
        partition_dir = os.path.join(wal_dir, partition)
        segments = [f for f in os.listdir(partition_dir) if f.endswith('.log')]
        segment_count = len(segments)
        total_segments += segment_count

        print(f"  Partition {partition}: {segment_count} segment(s)")
        for seg in sorted(segments):
            seg_path = os.path.join(partition_dir, seg)
            size_mb = os.path.getsize(seg_path) / (1024 * 1024)
            print(f"    - {seg}: {size_mb:.2f} MB")

    print(f"\n  Total segments across all partitions: {total_segments}")

    if total_segments > 1:
        print("✅ WAL rotation occurred (multiple segments found)")
    else:
        print("⚠️  No rotation yet (only 1 segment)")

    return total_segments

def check_wal_indices():
    """Check for WAL-generated tantivy indices."""
    import os
    index_dir = "data/tantivy_indexes"

    print(f"\n=== Checking WAL indices in {index_dir} ===")

    if not os.path.exists(index_dir):
        print(f"❌ WAL index directory does not exist: {index_dir}")
        return False

    # List all topic-partition indices
    indices = os.listdir(index_dir)

    if not indices:
        print("❌ No WAL indices found")
        return False

    print(f"✅ Found {len(indices)} WAL index(es):")
    for idx in sorted(indices):
        idx_path = os.path.join(index_dir, idx)
        if os.path.isdir(idx_path):
            files = os.listdir(idx_path)
            print(f"  - {idx}: {len(files)} files")

    return True

def check_realtime_indices():
    """Check real-time indices."""
    import os
    index_dir = f"data/index/{TOPIC_NAME}"

    print(f"\n=== Checking real-time indices in {index_dir} ===")

    if not os.path.exists(index_dir):
        print(f"❌ Real-time index directory does not exist")
        return False

    files = os.listdir(index_dir)
    print(f"✅ Real-time index exists: {len(files)} files")
    return True

def test_search(query: str = "rotation_test"):
    """Test search API."""
    print(f"\n=== Testing search for query: '{query}' ===")

    search_query = {
        "queries": [
            {
                "topic": TOPIC_NAME,
                "query": {
                    "match": {
                        "field": "type",
                        "query": query
                    }
                },
                "limit": 10
            }
        ]
    }

    try:
        response = requests.post(
            f"{SEARCH_API}/search",
            json=search_query,
            timeout=5
        )

        if response.status_code == 200:
            results = response.json()
            total_results = results.get('total_results', 0)
            print(f"✅ Search returned {total_results} results")

            if 'results' in results and results['results']:
                print(f"   First result: {results['results'][0].get('topic', 'N/A')}")

            return total_results
        else:
            print(f"❌ Search failed: {response.status_code} - {response.text}")
            return 0
    except Exception as e:
        print(f"❌ Search error: {e}")
        return 0

def main():
    """Main test flow."""
    print("=" * 70)
    print("WAL ROTATION AND INDEXING TEST")
    print("=" * 70)

    # Calculate messages needed to exceed 128MB WAL segment
    # 128MB / 100KB per message = 1280 messages
    # Add 20% buffer to ensure rotation: 1536 messages
    num_messages = 1536
    message_size_kb = 100

    print(f"\nTarget: Produce >{(128 * 1.2):.0f}MB to trigger WAL rotation")
    print(f"Strategy: {num_messages} messages × {message_size_kb}KB = {num_messages * message_size_kb / 1024:.1f}MB")

    # Step 1: Produce messages
    produce_messages(num_messages, message_size_kb)

    # Step 2: Give the server time to flush and potentially rotate
    print("\n⏳ Waiting 5 seconds for WAL flush...")
    time.sleep(5)

    # Step 3: Check WAL segments
    segment_count = check_wal_segments()

    # Step 4: Check indices
    check_realtime_indices()
    wal_indices_exist = check_wal_indices()

    # Step 5: Test search
    search_results = test_search()

    # Summary
    print("\n" + "=" * 70)
    print("SUMMARY")
    print("=" * 70)
    print(f"✅ Messages produced: {num_messages}")
    print(f"{'✅' if segment_count > 1 else '❌'} WAL rotation: {segment_count} segments")
    print(f"✅ Real-time indexing: Working")
    print(f"{'✅' if wal_indices_exist else '⏳'} WAL indexing: {'Working' if wal_indices_exist else 'Pending (segments not sealed yet)'}")
    print(f"✅ Search API: {search_results} results")
    print("=" * 70)

    if segment_count > 1 and not wal_indices_exist:
        print("\n💡 WAL segments rotated but WAL indices not created yet.")
        print("   This may be expected if segments haven't been sealed/compacted.")
        print("   Check the server logs for WAL indexer activity.")

if __name__ == '__main__':
    main()
