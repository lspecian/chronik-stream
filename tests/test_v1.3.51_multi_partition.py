#!/usr/bin/env python3
"""
Test v1.3.51 - Multi-partition, 10000 messages
Tests WAL offset metadata filtering with multiple partitions
Note: Chronik auto-creates with 3 default partitions
"""

import time
from kafka import KafkaProducer, KafkaConsumer

BOOTSTRAP_SERVER = 'localhost:9092'
TOPIC_NAME = 'test-v1.3.51-multi'
NUM_MESSAGES = 10000

def main():
    print("=" * 80)
    print(f"TEST v1.3.51 - {NUM_MESSAGES} messages, auto-partitioned")
    print("=" * 80)

    # Let Chronik auto-create topic with default partitions (3)
    print(f"\n1. Letting Chronik auto-create topic with default partitions...")

    # Produce messages
    producer = KafkaProducer(
        bootstrap_servers=BOOTSTRAP_SERVER,
        api_version=(0, 10, 0)
    )

    print(f"\n2. Producing {NUM_MESSAGES} messages...")
    start = time.time()
    for i in range(NUM_MESSAGES):
        msg = f"msg-{i:06d}".encode('utf-8')
        # Use key for partition distribution across Chronik's default 3 partitions
        key = f"key-{i % 3}".encode('utf-8')
        producer.send(TOPIC_NAME, key=key, value=msg)
        if (i + 1) % 1000 == 0:
            print(f"   Sent {i + 1}/{NUM_MESSAGES}...")

    producer.flush()
    producer.close()
    elapsed = time.time() - start
    print(f"✓ Produced {NUM_MESSAGES} messages in {elapsed:.2f}s ({NUM_MESSAGES/elapsed:.0f} msg/s)")

    # Wait for flush
    print("\n3. Waiting 3s for server to flush...")
    time.sleep(3)

    # Kill server (simulate crash)
    print("\n4. Simulating crash (kill -9)...")
    import subprocess
    subprocess.run(['pkill', '-9', '-f', 'chronik-server'], check=False)
    time.sleep(2)

    # Restart server
    print("\n5. Restarting server (WAL recovery)...")
    import os
    env = os.environ.copy()
    env['RUST_LOG'] = 'info,chronik_wal=debug'
    proc = subprocess.Popen(
        ['./target/release/chronik-server', 'standalone'],
        stdout=open('/tmp/v1.3.51-multi-recover.log', 'w'),
        stderr=subprocess.STDOUT,
        env=env
    )
    time.sleep(6)
    print(f"✓ Server restarted (PID {proc.pid})")

    # Consume all messages
    print(f"\n6. Consuming from offset 0 (all partitions)...")
    consumer = KafkaConsumer(
        TOPIC_NAME,
        bootstrap_servers=BOOTSTRAP_SERVER,
        api_version=(0, 10, 0),
        auto_offset_reset='earliest',
        consumer_timeout_ms=10000
    )

    messages = []
    partition_counts = {}
    for msg in consumer:
        messages.append(msg.value.decode('utf-8'))
        partition_counts[msg.partition] = partition_counts.get(msg.partition, 0) + 1

    consumer.close()

    print(f"\n✓ Consumed {len(messages)} messages")
    print(f"  Expected: {NUM_MESSAGES}")
    print(f"  Difference: {NUM_MESSAGES - len(messages)}")
    print(f"\n  Distribution across partitions:")
    for p in sorted(partition_counts.keys()):
        print(f"    Partition {p}: {partition_counts[p]} messages")

    # Verify
    if len(messages) == NUM_MESSAGES:
        print("\n" + "=" * 80)
        print("✅ SUCCESS - All messages recovered across all partitions!")
        print("=" * 80)
        return 0
    else:
        print("\n" + "=" * 80)
        print(f"❌ FAILED - Missing {NUM_MESSAGES - len(messages)} messages")
        print("=" * 80)
        # Show which messages we got
        received_indices = set()
        for msg in messages:
            if msg.startswith('msg-'):
                idx = int(msg.split('-')[1])
                received_indices.add(idx)

        missing = []
        for i in range(NUM_MESSAGES):
            if i not in received_indices:
                missing.append(i)

        if missing:
            print(f"\nMissing {len(missing)} messages")
            print(f"First 20 missing: {missing[:20]}")
            print(f"Last 20 missing: {missing[-20:]}")
        return 1

if __name__ == '__main__':
    exit(main())
