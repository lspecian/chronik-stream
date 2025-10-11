#!/usr/bin/env python3
"""Detailed recovery test to trace duplicate messages."""

from kafka import KafkaProducer, KafkaConsumer
import subprocess
import time
import signal
import os
from collections import Counter

def start_server(log_file=None):
    """Start chronik server in background."""
    env = os.environ.copy()
    env['RUST_LOG'] = 'info,chronik_server::fetch_handler=debug'

    if log_file:
        logf = open(log_file, 'w')
        proc = subprocess.Popen(
            ['/Users/lspecian/Development/chronik-stream/target/debug/chronik-server',
             '--advertised-addr', 'localhost', 'standalone'],
            stdout=logf,
            stderr=logf,
            env=env
        )
    else:
        proc = subprocess.Popen(
            ['/Users/lspecian/Development/chronik-stream/target/debug/chronik-server',
             '--advertised-addr', 'localhost', 'standalone'],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            env=env
        )
    time.sleep(5)
    return proc

def stop_server(proc):
    """Kill server immediately."""
    proc.send_signal(signal.SIGKILL)
    proc.wait()
    time.sleep(2)

def test_recovery():
    print("🧪 Detailed GroupCommitWal Recovery Test\n")

    # Start server
    print("1️⃣  Starting server (first run)...")
    proc = start_server('/tmp/chronik_first.log')

    # Produce 10 unique messages
    print("2️⃣  Producing 10 unique messages with acks=1...")
    producer = KafkaProducer(
        bootstrap_servers='localhost:9092',
        api_version=(0, 10, 0),
        acks=1
    )

    produced = []
    for i in range(10):
        msg = f'UniqueMessage_{i:03d}'
        future = producer.send('test-dedup', msg.encode())
        future.get(timeout=10)
        produced.append(msg)
        print(f"   ✅ Produced: {msg}")

    producer.close()
    print(f"\n   Total produced: {len(produced)} unique messages")

    # Kill server
    print("\n3️⃣  💥 Killing server (simulating crash)...")
    stop_server(proc)

    # Restart server
    print("4️⃣  🔄 Restarting server (WAL recovery)...")
    proc = start_server('/tmp/chronik_recovery.log')

    # Consume all messages
    print("5️⃣  Consuming messages...\n")
    consumer = KafkaConsumer(
        'test-dedup',
        bootstrap_servers='localhost:9092',
        auto_offset_reset='earliest',
        consumer_timeout_ms=15000,
        api_version=(0, 10, 0)
    )

    consumed = []
    for msg in consumer:
        consumed.append(msg.value.decode())
        print(f"   📥 Consumed: {msg.value.decode()} (partition={msg.partition}, offset={msg.offset})")

    consumer.close()
    stop_server(proc)

    # Analysis
    print(f"\n📊 Analysis:")
    print(f"   Messages produced: {len(produced)}")
    print(f"   Messages consumed: {len(consumed)}")

    # Count duplicates
    counter = Counter(consumed)
    duplicates = {msg: count for msg, count in counter.items() if count > 1}

    if duplicates:
        print(f"\n❌ DUPLICATES DETECTED:")
        for msg, count in duplicates.items():
            print(f"      '{msg}' appeared {count} times")

    # Check for missing messages
    missing = set(produced) - set(consumed)
    if missing:
        print(f"\n❌ MISSING MESSAGES:")
        for msg in sorted(missing):
            print(f"      '{msg}'")

    # Check for unexpected messages
    unexpected = set(consumed) - set(produced)
    if unexpected:
        print(f"\n❌ UNEXPECTED MESSAGES:")
        for msg in sorted(unexpected):
            print(f"      '{msg}'")

    # Summary
    print(f"\n{'='*60}")
    if len(consumed) == len(produced) and not duplicates and not missing:
        print("✅ SUCCESS: Perfect recovery - no duplicates, no losses")
        print(f"{'='*60}")
        return True
    else:
        print("❌ FAILURE: Data integrity issues detected")
        print(f"   Expected: {len(produced)} unique messages")
        print(f"   Got: {len(consumed)} total messages ({len(set(consumed))} unique)")
        print(f"   Duplicates: {sum(count - 1 for count in counter.values() if count > 1)}")
        print(f"   Missing: {len(missing)}")
        print(f"{'='*60}")
        return False

if __name__ == '__main__':
    success = test_recovery()
    exit(0 if success else 1)
