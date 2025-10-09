#!/usr/bin/env python3
"""
Fast WAL rotation test - Generate 500MB to trigger rotation and indexing
"""

import json
import time
from kafka import KafkaProducer

BOOTSTRAP_SERVER = 'localhost:9092'
TOPIC_NAME = 'wal-test-500mb'

def create_message(index: int, size_kb: int = 100) -> bytes:
    """Create a message of approximately size_kb."""
    # Create base message with padding to reach target size
    padding = "x" * (size_kb * 1024 - 100)  # Leave room for JSON overhead
    message = {
        "id": index,
        "timestamp": time.time(),
        "type": "bulk_test",
        "padding": padding
    }
    return json.dumps(message).encode('utf-8')

def main():
    print("=" * 80)
    print("FAST WAL ROTATION TEST - 500MB")
    print("=" * 80)

    # Calculate: Need 500MB total, with 100KB messages = 5000 messages
    # But we have 3 partitions, so ~1667 messages per partition = ~167MB per partition
    # Let's produce 6000 messages to be safe = 600MB total
    num_messages = 6000
    message_size_kb = 100
    total_mb = (num_messages * message_size_kb) / 1024

    print(f"\nTarget: Produce {total_mb:.0f}MB to trigger WAL rotation (need 128MB per partition)")
    print(f"Messages: {num_messages} × {message_size_kb}KB = {total_mb:.0f}MB")
    print(f"Topic: {TOPIC_NAME} (3 partitions)")
    print(f"Expected: ~{total_mb/3:.0f}MB per partition\n")

    producer = KafkaProducer(
        bootstrap_servers=BOOTSTRAP_SERVER,
        api_version=(0, 10, 0),
        acks='all',
        compression_type=None,  # No compression for speed
        batch_size=1024 * 1024,   # 1MB batches for speed
        linger_ms=100,
        buffer_memory=128 * 1024 * 1024,  # 128MB buffer
        max_in_flight_requests_per_connection=10
    )

    print("Starting production...")
    start_time = time.time()

    for i in range(num_messages):
        message = create_message(i, message_size_kb)
        producer.send(TOPIC_NAME, value=message)

        if (i + 1) % 500 == 0:
            elapsed = time.time() - start_time
            mb_sent = ((i + 1) * message_size_kb) / 1024
            throughput = mb_sent / elapsed if elapsed > 0 else 0
            progress = ((i + 1) / num_messages) * 100
            print(f"  [{progress:5.1f}%] Sent {i + 1:5d}/{num_messages} messages "
                  f"({mb_sent:6.1f}MB, {throughput:6.1f} MB/s)")

    print("\nFlushing producer...")
    producer.flush()
    producer.close()

    elapsed = time.time() - start_time
    print(f"\n✅ Completed in {elapsed:.1f}s")
    print(f"   Total: {total_mb:.1f}MB")
    print(f"   Throughput: {total_mb / elapsed:.1f} MB/s")

    print("\n⏳ Waiting 10 seconds for server to process...")
    time.sleep(10)

    # Check results
    import os
    print("\n" + "=" * 80)
    print("CHECKING WAL SEGMENTS")
    print("=" * 80)

    wal_dir = f"data/wal/{TOPIC_NAME}"
    if os.path.exists(wal_dir):
        for partition in sorted(os.listdir(wal_dir)):
            partition_path = os.path.join(wal_dir, partition)
            if os.path.isdir(partition_path):
                segments = [f for f in os.listdir(partition_path) if f.endswith('.log')]
                print(f"\nPartition {partition}: {len(segments)} segment(s)")
                total_size = 0
                for seg in sorted(segments):
                    seg_path = os.path.join(partition_path, seg)
                    size_mb = os.path.getsize(seg_path) / (1024 * 1024)
                    total_size += size_mb
                    sealed = " [SEALED]" if len(segments) > 1 else ""
                    print(f"  {seg}: {size_mb:7.2f} MB{sealed}")
                print(f"  Total: {total_size:7.2f} MB")

                if total_size > 128:
                    print(f"  ✅ Exceeded 128MB threshold - should trigger sealing!")
    else:
        print(f"❌ WAL directory not found: {wal_dir}")

    print("\n⏳ Wait 30 seconds for WAL indexer to run and check:")
    print("   1. Watch server logs for 'WAL indexing run complete'")
    print("   2. Check data/tantivy_indexes/ directory")
    print("   3. Verify sealed segments are deleted after indexing")
    print("=" * 80)

if __name__ == '__main__':
    main()
