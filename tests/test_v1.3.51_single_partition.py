#!/usr/bin/env python3
"""
Test v1.3.51 - Single partition, 5000 messages
Tests WAL offset metadata filtering fix
"""

import time
from kafka import KafkaProducer, KafkaConsumer

BOOTSTRAP_SERVER = 'localhost:9092'
TOPIC_NAME = 'test-v1.3.51-single'
NUM_MESSAGES = 5000

def main():
    print("=" * 80)
    print(f"TEST v1.3.51 - {NUM_MESSAGES} messages, 1 partition")
    print("=" * 80)

    # Produce messages
    producer = KafkaProducer(
        bootstrap_servers=BOOTSTRAP_SERVER,
        api_version=(0, 10, 0)
    )

    print(f"\n1. Producing {NUM_MESSAGES} messages...")
    start = time.time()
    for i in range(NUM_MESSAGES):
        msg = f"msg-{i:05d}".encode('utf-8')
        producer.send(TOPIC_NAME, value=msg)
        if (i + 1) % 500 == 0:
            print(f"   Sent {i + 1}/{NUM_MESSAGES}...")

    producer.flush()
    producer.close()
    elapsed = time.time() - start
    print(f"✓ Produced {NUM_MESSAGES} messages in {elapsed:.2f}s")

    # Wait for flush
    print("\n2. Waiting 2s for server to flush...")
    time.sleep(2)

    # Kill server (simulate crash)
    print("\n3. Simulating crash (kill -9)...")
    import subprocess
    subprocess.run(['pkill', '-9', '-f', 'chronik-server'], check=False)
    time.sleep(2)

    # Restart server
    print("\n4. Restarting server (WAL recovery)...")
    import os
    env = os.environ.copy()
    env['RUST_LOG'] = 'info,chronik_wal=debug'
    proc = subprocess.Popen(
        ['./target/release/chronik-server', 'standalone'],
        stdout=open('/tmp/v1.3.51-recover.log', 'w'),
        stderr=subprocess.STDOUT,
        env=env
    )
    time.sleep(5)
    print(f"✓ Server restarted (PID {proc.pid})")

    # Consume all messages
    print(f"\n5. Consuming from offset 0...")
    consumer = KafkaConsumer(
        TOPIC_NAME,
        bootstrap_servers=BOOTSTRAP_SERVER,
        api_version=(0, 10, 0),
        auto_offset_reset='earliest',
        consumer_timeout_ms=5000
    )

    messages = []
    for msg in consumer:
        messages.append(msg.value.decode('utf-8'))

    consumer.close()

    print(f"\n✓ Consumed {len(messages)} messages")
    print(f"  Expected: {NUM_MESSAGES}")
    print(f"  Difference: {NUM_MESSAGES - len(messages)}")

    # Verify
    if len(messages) == NUM_MESSAGES:
        print("\n" + "=" * 80)
        print("✅ SUCCESS - All messages recovered!")
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
            print(f"\nFirst 20 missing: {missing[:20]}")
        return 1

if __name__ == '__main__':
    exit(main())
