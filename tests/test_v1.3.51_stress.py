#!/usr/bin/env python3
"""
Test v1.3.51 - Stress test with 50000 messages
Tests WAL offset metadata filtering under high load
Note: Chronik auto-creates with 3 default partitions
"""

import time
from kafka import KafkaProducer, KafkaConsumer

BOOTSTRAP_SERVER = 'localhost:9092'
TOPIC_NAME = 'test-v1.3.51-stress'
NUM_MESSAGES = 50000

def main():
    print("=" * 80)
    print(f"TEST v1.3.51 STRESS - {NUM_MESSAGES} messages, auto-partitioned (3 partitions)")
    print("=" * 80)

    # Produce messages
    producer = KafkaProducer(
        bootstrap_servers=BOOTSTRAP_SERVER,
        api_version=(0, 10, 0),
        batch_size=16384,
        linger_ms=10
    )

    print(f"\n1. Producing {NUM_MESSAGES} messages...")
    start = time.time()
    for i in range(NUM_MESSAGES):
        msg = f"stress-msg-{i:07d}".encode('utf-8')
        producer.send(TOPIC_NAME, value=msg)
        if (i + 1) % 5000 == 0:
            print(f"   Sent {i + 1}/{NUM_MESSAGES}...")

    producer.flush()
    producer.close()
    elapsed = time.time() - start
    print(f"✓ Produced {NUM_MESSAGES} messages in {elapsed:.2f}s ({NUM_MESSAGES/elapsed:.0f} msg/s)")

    # Wait for flush
    print("\n2. Waiting 5s for server to flush...")
    time.sleep(5)

    # Kill server (simulate crash)
    print("\n3. Simulating crash (kill -9)...")
    import subprocess
    subprocess.run(['pkill', '-9', '-f', 'chronik-server'], check=False)
    time.sleep(2)

    # Restart server
    print("\n4. Restarting server (WAL recovery)...")
    import os
    env = os.environ.copy()
    env['RUST_LOG'] = 'info,chronik_wal=info'  # Less verbose for stress test
    proc = subprocess.Popen(
        ['./target/release/chronik-server', 'standalone'],
        stdout=open('/tmp/v1.3.51-stress-recover.log', 'w'),
        stderr=subprocess.STDOUT,
        env=env
    )
    time.sleep(8)
    print(f"✓ Server restarted (PID {proc.pid})")

    # Consume all messages
    print(f"\n5. Consuming all messages...")
    consumer = KafkaConsumer(
        TOPIC_NAME,
        bootstrap_servers=BOOTSTRAP_SERVER,
        api_version=(0, 10, 0),
        auto_offset_reset='earliest',
        consumer_timeout_ms=15000,
        max_poll_records=1000
    )

    start_consume = time.time()
    messages = []
    for msg in consumer:
        messages.append(msg.value.decode('utf-8'))
        if len(messages) % 5000 == 0:
            print(f"   Consumed {len(messages)}...")

    consumer.close()
    consume_elapsed = time.time() - start_consume

    print(f"\n✓ Consumed {len(messages)} messages in {consume_elapsed:.2f}s ({len(messages)/consume_elapsed:.0f} msg/s)")
    print(f"  Expected: {NUM_MESSAGES}")
    print(f"  Difference: {NUM_MESSAGES - len(messages)}")

    # Verify
    if len(messages) == NUM_MESSAGES:
        print("\n" + "=" * 80)
        print("✅ SUCCESS - Stress test passed! All 50K messages recovered!")
        print("=" * 80)
        return 0
    else:
        print("\n" + "=" * 80)
        print(f"❌ FAILED - Missing {NUM_MESSAGES - len(messages)} messages")
        print("=" * 80)

        # Analyze missing messages
        received_indices = set()
        for msg in messages:
            if msg.startswith('stress-msg-'):
                idx = int(msg.split('-')[2])
                received_indices.add(idx)

        missing = []
        for i in range(NUM_MESSAGES):
            if i not in received_indices:
                missing.append(i)

        if missing:
            print(f"\nMissing {len(missing)} messages")
            print(f"First 20 missing: {missing[:20]}")
            print(f"Last 20 missing: {missing[-20:]}")

            # Check for patterns
            if len(missing) > 0:
                gaps = []
                start = missing[0]
                prev = missing[0]
                for m in missing[1:]:
                    if m != prev + 1:
                        gaps.append((start, prev, prev - start + 1))
                        start = m
                    prev = m
                gaps.append((start, prev, prev - start + 1))

                print(f"\nGap analysis (first 10 gaps):")
                for start, end, count in gaps[:10]:
                    print(f"  Gap: {start}-{end} ({count} messages)")

        return 1

if __name__ == '__main__':
    exit(main())
