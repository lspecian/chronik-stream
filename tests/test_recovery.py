#!/usr/bin/env python3
"""Test GroupCommitWal recovery - produce messages, crash, recover, consume."""

from kafka import KafkaProducer, KafkaConsumer
import subprocess
import time
import signal
import os

def start_server():
    """Start chronik server in background."""
    proc = subprocess.Popen(
        ['/Users/lspecian/Development/chronik-stream/target/debug/chronik-server',
         '--advertised-addr', 'localhost', 'standalone'],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL
    )
    time.sleep(5)  # Wait for server to start
    return proc

def stop_server(proc):
    """Kill server immediately (simulate crash)."""
    proc.send_signal(signal.SIGKILL)
    proc.wait()
    time.sleep(2)

def test_recovery():
    print("🧪 Testing GroupCommitWal Recovery")

    # Start server
    print("1️⃣  Starting server...")
    proc = start_server()

    # Produce messages with acks=1 (GroupCommitWal)
    print("2️⃣  Producing 10 messages with acks=1...")
    producer = KafkaProducer(
        bootstrap_servers='localhost:9092',
        api_version=(0, 10, 0),
        acks=1  # GroupCommitWal
    )

    for i in range(10):
        future = producer.send('recovery-test', f'Message {i}'.encode())
        future.get(timeout=10)  # Wait for ack
        print(f"   ✅ Message {i} acked")

    producer.close()
    print("   All messages produced and acked")

    # Kill server (simulate crash)
    print("3️⃣  💥 Killing server (simulating crash)...")
    stop_server(proc)

    # Restart server (should recover from WAL)
    print("4️⃣  🔄 Restarting server (recovery should happen)...")
    proc = start_server()

    # Consume messages
    print("5️⃣  Consuming messages from beginning...")
    consumer = KafkaConsumer(
        'recovery-test',
        bootstrap_servers='localhost:9092',
        auto_offset_reset='earliest',
        consumer_timeout_ms=10000,
        api_version=(0, 10, 0)
    )

    messages = []
    for msg in consumer:
        messages.append(msg.value.decode())
        print(f"   📥 Consumed: {msg.value.decode()}")

    consumer.close()

    # Verify
    print(f"\n📊 Results:")
    print(f"   Produced: 10 messages")
    print(f"   Consumed: {len(messages)} messages")

    if len(messages) == 10:
        print("✅ SUCCESS: 100% message recovery (0% loss)")
        stop_server(proc)
        return True
    else:
        print(f"❌ FAILURE: {10 - len(messages)} messages lost ({(10 - len(messages)) * 10}% loss)")
        stop_server(proc)
        return False

if __name__ == '__main__':
    success = test_recovery()
    exit(0 if success else 1)
