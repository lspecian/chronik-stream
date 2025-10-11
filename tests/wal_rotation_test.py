#!/usr/bin/env python3
"""
WAL Rotation Test - Send 500MB to verify WAL segments rotate at 128MB threshold
Uses chunked sending without waiting for individual acks to avoid hanging
"""

import json
import time
import os
from kafka import KafkaProducer
from kafka.errors import KafkaError

BOOTSTRAP_SERVER = 'localhost:9092'
TOPIC_NAME = 'wal-test-500mb'

def create_message(index: int, size_kb: int = 100) -> bytes:
    """Create a message of approximately size_kb."""
    padding = "x" * (size_kb * 1024 - 100)
    message = {
        "id": index,
        "timestamp": time.time(),
        "type": "bulk_test",
        "padding": padding
    }
    return json.dumps(message).encode('utf-8')

def main():
    print("=" * 80)
    print("WAL ROTATION TEST - 500MB Target")
    print("=" * 80)

    # 6000 messages × 100KB = 600MB total ÷ 3 partitions = 200MB per partition
    # This should trigger rotation at 128MB threshold
    num_messages = 6000
    message_size_kb = 100
    total_mb = (num_messages * message_size_kb) / 1024

    print(f"\nConfiguration:")
    print(f"  Messages: {num_messages} × {message_size_kb}KB = {total_mb:.0f}MB total")
    print(f"  Topic: {TOPIC_NAME} (auto-create with 3 partitions)")
    print(f"  Expected: ~{total_mb/3:.0f}MB per partition")
    print(f"  Goal: Trigger rotation at 128MB per partition\n")

    # Create producer with fire-and-forget style
    producer = KafkaProducer(
        bootstrap_servers=BOOTSTRAP_SERVER,
        api_version=(0, 10, 0),
        acks=1,  # Leader ack only (faster, no hang)
        compression_type=None,
        batch_size=512 * 1024,  # 512KB batches
        linger_ms=50,  # Send batches every 50ms
        buffer_memory=64 * 1024 * 1024,  # 64MB buffer
        max_request_size=5 * 1024 * 1024,  # 5MB max request
        request_timeout_ms=30000,
        max_in_flight_requests_per_connection=5
    )

    print("Producer created. Starting to send messages...\n")
    start_time = time.time()
    sent_count = 0
    error_count = 0

    try:
        for i in range(num_messages):
            message = create_message(i, message_size_kb)

            # Fire and forget - don't wait for ack
            try:
                producer.send(TOPIC_NAME, value=message)
                sent_count += 1
            except KafkaError as e:
                error_count += 1
                if error_count < 5:
                    print(f"  ⚠️  Error sending message {i}: {e}")

            # Report progress
            if (sent_count) % 500 == 0:
                elapsed = time.time() - start_time
                mb_sent = (sent_count * message_size_kb) / 1024
                throughput = mb_sent / elapsed if elapsed > 0 else 0
                progress = (sent_count / num_messages) * 100
                print(f"  [{progress:5.1f}%] Sent {sent_count:5d}/{num_messages} messages "
                      f"({mb_sent:6.1f}MB, {throughput:6.1f} MB/s)")

        print(f"\n✅ All {sent_count} messages queued!")
        print(f"   Errors: {error_count}")

        # Flush to ensure all messages are sent
        print("\nFlushing producer (forcing send of all buffered messages)...")
        flush_start = time.time()
        producer.flush(timeout=60)
        flush_duration = time.time() - flush_start
        print(f"Flush completed in {flush_duration:.1f}s")

    except KeyboardInterrupt:
        print("\n⚠️  Interrupted by user")
    finally:
        producer.close()

    elapsed = time.time() - start_time
    mb_sent = (sent_count * message_size_kb) / 1024

    print(f"\n{'='*80}")
    print("PRODUCTION COMPLETE")
    print(f"{'='*80}")
    print(f"  Messages sent: {sent_count:,} / {num_messages:,}")
    print(f"  Total data: {mb_sent:.1f}MB")
    print(f"  Duration: {elapsed:.1f}s")
    print(f"  Throughput: {mb_sent / elapsed:.1f} MB/s")
    print(f"  Errors: {error_count}")

    # Wait for server to process
    print(f"\n⏳ Waiting 5 seconds for server to finish processing...")
    time.sleep(5)

    # Check WAL segments
    print(f"\n{'='*80}")
    print("CHECKING WAL SEGMENTS")
    print(f"{'='*80}")

    wal_dir = f"data/wal/{TOPIC_NAME}"
    if os.path.exists(wal_dir):
        print(f"\n📁 WAL Directory: {wal_dir}\n")

        partitions = sorted([p for p in os.listdir(wal_dir) if os.path.isdir(os.path.join(wal_dir, p))])

        for partition in partitions:
            partition_path = os.path.join(wal_dir, partition)
            segments = sorted([f for f in os.listdir(partition_path) if f.endswith('.log')])

            print(f"Partition {partition}:")
            print(f"  Segments: {len(segments)}")

            total_size_mb = 0
            for seg in segments:
                seg_path = os.path.join(partition_path, seg)
                size_mb = os.path.getsize(seg_path) / (1024 * 1024)
                total_size_mb += size_mb
                status = "SEALED" if len(segments) > 1 and seg != segments[-1] else "ACTIVE"
                print(f"    {seg}: {size_mb:7.2f} MB [{status}]")

            print(f"  Total: {total_size_mb:7.2f} MB")

            # Check if rotation happened
            if len(segments) > 1:
                print(f"  ✅ ROTATION DETECTED! {len(segments)} segments created")
            elif total_size_mb > 128:
                print(f"  ⚠️  Size > 128MB but no rotation yet (might be pending)")
            else:
                print(f"  ℹ️  Single active segment (no rotation needed yet)")
            print()

        # Summary
        all_segments = sum([len([f for f in os.listdir(os.path.join(wal_dir, p))
                                if f.endswith('.log')])
                           for p in partitions])
        print(f"Total across all partitions: {all_segments} segments")

        if all_segments > len(partitions):
            print(f"✅ WAL ROTATION WORKING! (expected {len(partitions)} if no rotation)")
        else:
            print(f"ℹ️  No rotation detected (may need more data or time)")

    else:
        print(f"❌ WAL directory not found: {wal_dir}")
        print(f"   This means data was NOT written to WAL!")

    print(f"\n{'='*80}\n")

if __name__ == '__main__':
    main()
