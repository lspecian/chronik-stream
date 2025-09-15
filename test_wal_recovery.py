#!/usr/bin/env python3
"""
Integration tests for WAL (Write-Ahead Log) recovery scenarios.

Tests:
1. Server crash and recovery with WAL replay
2. WAL truncation after successful segment persistence
3. Partial write recovery
4. Multi-partition recovery
"""

import os
import sys
import time
import signal
import subprocess
import shutil
from kafka import KafkaProducer, KafkaConsumer
from kafka.errors import KafkaError

def cleanup_data():
    """Clean up data directory"""
    if os.path.exists("data"):
        shutil.rmtree("data")
    print("✅ Cleaned up data directory")

def start_server(port=9092):
    """Start the Chronik server"""
    print(f"Starting Chronik server on port {port}...")
    proc = subprocess.Popen(
        ["./target/release/chronik-server", "--kafka-port", str(port)],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True
    )
    time.sleep(3)  # Wait for server to start
    return proc

def stop_server(proc):
    """Stop the server gracefully"""
    print("Stopping server...")
    proc.terminate()
    try:
        proc.wait(timeout=5)
    except subprocess.TimeoutExpired:
        proc.kill()
        proc.wait()
    print("✅ Server stopped")

def crash_server(proc):
    """Simulate server crash with SIGKILL"""
    print("💥 Simulating server crash...")
    proc.kill()
    proc.wait()
    print("✅ Server crashed")

def send_messages(topic, count, port=9092):
    """Send messages to a topic"""
    print(f"Sending {count} messages to {topic}...")
    producer = KafkaProducer(
        bootstrap_servers=[f'localhost:{port}'],
        value_serializer=lambda x: x.encode('utf-8'),
        api_version=(0, 10, 0)
    )

    messages_sent = []
    for i in range(count):
        message = f'Message {i+1}'
        future = producer.send(topic, message)
        result = future.get(timeout=10)
        messages_sent.append((message, result.offset))
        print(f"  Sent: {message} at offset {result.offset}")

    producer.flush()
    producer.close()
    return messages_sent

def consume_messages(topic, expected_count, port=9092, timeout=10):
    """Consume messages from a topic"""
    print(f"Consuming messages from {topic}...")
    consumer = KafkaConsumer(
        topic,
        bootstrap_servers=[f'localhost:{port}'],
        auto_offset_reset='earliest',
        enable_auto_commit=True,
        group_id=f'test-group-{time.time()}',
        value_deserializer=lambda x: x.decode('utf-8'),
        consumer_timeout_ms=timeout * 1000,
        api_version=(0, 10, 0)
    )

    messages_received = []
    for message in consumer:
        messages_received.append((message.value, message.offset))
        print(f"  Received: {message.value} at offset {message.offset}")
        if len(messages_received) >= expected_count:
            break

    consumer.close()
    return messages_received

def test_wal_recovery():
    """Test 1: Basic WAL recovery after crash"""
    print("\n" + "="*60)
    print("TEST 1: WAL Recovery After Crash")
    print("="*60)

    cleanup_data()

    # Start server and send messages
    server = start_server()
    messages_sent = send_messages("test-recovery", 10)

    # Crash the server (simulating unexpected shutdown)
    crash_server(server)

    # Restart server - should recover from WAL
    print("\n🔄 Restarting server to test WAL recovery...")
    server = start_server()
    time.sleep(5)  # Give time for WAL recovery

    # Verify messages are still available
    messages_received = consume_messages("test-recovery", 10)

    # Check if all messages were recovered
    if len(messages_received) == len(messages_sent):
        print(f"✅ TEST 1 PASSED: All {len(messages_sent)} messages recovered from WAL")
        # Verify message content
        for sent, received in zip(messages_sent, messages_received):
            if sent[0] != received[0] or sent[1] != received[1]:
                print(f"❌ Message mismatch: sent {sent} != received {received}")
                stop_server(server)
                return False
    else:
        print(f"❌ TEST 1 FAILED: Expected {len(messages_sent)} messages, got {len(messages_received)}")
        stop_server(server)
        return False

    stop_server(server)
    return True

def test_partial_write_recovery():
    """Test 2: Recovery from partial writes"""
    print("\n" + "="*60)
    print("TEST 2: Partial Write Recovery")
    print("="*60)

    cleanup_data()

    # Start server
    server = start_server()

    # Send first batch
    print("\n📤 Sending first batch...")
    batch1 = send_messages("test-partial", 5)

    # Send second batch but crash during it
    print("\n📤 Starting second batch (will crash mid-way)...")
    producer = KafkaProducer(
        bootstrap_servers=['localhost:9092'],
        value_serializer=lambda x: x.encode('utf-8'),
        api_version=(0, 10, 0)
    )

    # Send a few messages
    batch2 = []
    for i in range(5, 8):  # Send only 3 messages before crash
        message = f'Message {i+1}'
        future = producer.send("test-partial", message)
        result = future.get(timeout=10)
        batch2.append((message, result.offset))
        print(f"  Sent: {message} at offset {result.offset}")

    # Crash before flush
    crash_server(server)

    # Restart server
    print("\n🔄 Restarting server after partial write...")
    server = start_server()
    time.sleep(5)

    # Verify we can read the messages that were acknowledged
    messages_received = consume_messages("test-partial", len(batch1) + len(batch2))

    print(f"\n📊 Recovery results:")
    print(f"  First batch: {len(batch1)} messages")
    print(f"  Partial batch: {len(batch2)} messages acknowledged")
    print(f"  Total recovered: {len(messages_received)} messages")

    if len(messages_received) >= len(batch1):
        print(f"✅ TEST 2 PASSED: Recovered at least the first batch ({len(batch1)} messages)")
    else:
        print(f"❌ TEST 2 FAILED: Lost messages from first batch")
        stop_server(server)
        return False

    stop_server(server)
    return True

def test_multi_partition_recovery():
    """Test 3: Recovery across multiple partitions"""
    print("\n" + "="*60)
    print("TEST 3: Multi-Partition Recovery")
    print("="*60)

    cleanup_data()

    # Start server
    server = start_server()

    # Send messages to multiple topics (different partitions)
    print("\n📤 Sending messages to multiple topics...")
    topics_data = {}
    for topic_num in range(3):
        topic = f"test-topic-{topic_num}"
        messages = send_messages(topic, 5)
        topics_data[topic] = messages

    # Crash server
    crash_server(server)

    # Restart and verify recovery
    print("\n🔄 Restarting server for multi-partition recovery...")
    server = start_server()
    time.sleep(5)

    # Verify all topics recovered
    all_recovered = True
    for topic, expected_messages in topics_data.items():
        print(f"\n📥 Verifying recovery for {topic}...")
        recovered = consume_messages(topic, len(expected_messages))
        if len(recovered) == len(expected_messages):
            print(f"  ✅ {topic}: All {len(expected_messages)} messages recovered")
        else:
            print(f"  ❌ {topic}: Expected {len(expected_messages)}, got {len(recovered)}")
            all_recovered = False

    if all_recovered:
        print(f"\n✅ TEST 3 PASSED: All partitions recovered successfully")
    else:
        print(f"\n❌ TEST 3 FAILED: Some partitions failed to recover")

    stop_server(server)
    return all_recovered

def test_wal_truncation():
    """Test 4: WAL truncation after segment persistence"""
    print("\n" + "="*60)
    print("TEST 4: WAL Truncation")
    print("="*60)

    cleanup_data()

    # Start server
    server = start_server()

    # Send many messages to trigger segment rotation
    print("\n📤 Sending messages to trigger segment rotation...")
    messages_sent = send_messages("test-truncation", 100)

    # Wait for segments to be persisted
    print("\n⏳ Waiting for segment persistence and WAL truncation...")
    time.sleep(10)

    # Check WAL directory size (it should be smaller after truncation)
    wal_dir = "data/wal"
    if os.path.exists(wal_dir):
        # Count WAL files
        wal_files = []
        for root, dirs, files in os.walk(wal_dir):
            for file in files:
                if file.endswith('.wal'):
                    wal_files.append(os.path.join(root, file))

        print(f"\n📁 WAL Status:")
        print(f"  WAL files: {len(wal_files)}")
        for wal_file in wal_files:
            size = os.path.getsize(wal_file)
            print(f"    {os.path.basename(wal_file)}: {size} bytes")

    # Crash and recover to verify data integrity
    crash_server(server)

    print("\n🔄 Restarting to verify data after truncation...")
    server = start_server()
    time.sleep(5)

    # Verify all messages are still available
    messages_received = consume_messages("test-truncation", 100)

    if len(messages_received) == 100:
        print(f"✅ TEST 4 PASSED: All messages available after WAL truncation")
    else:
        print(f"❌ TEST 4 FAILED: Expected 100 messages, got {len(messages_received)}")
        stop_server(server)
        return False

    stop_server(server)
    return True

def main():
    """Run all WAL recovery tests"""
    print("🚀 Starting WAL Recovery Integration Tests")
    print("=" * 60)

    # Check if server binary exists
    if not os.path.exists("./target/release/chronik-server"):
        print("❌ Error: chronik-server binary not found")
        print("  Please build the project first: cargo build --release")
        return 1

    tests = [
        ("Basic WAL Recovery", test_wal_recovery),
        ("Partial Write Recovery", test_partial_write_recovery),
        ("Multi-Partition Recovery", test_multi_partition_recovery),
        ("WAL Truncation", test_wal_truncation),
    ]

    results = []
    for test_name, test_func in tests:
        try:
            success = test_func()
            results.append((test_name, success))
        except Exception as e:
            print(f"\n❌ {test_name} failed with exception: {e}")
            import traceback
            traceback.print_exc()
            results.append((test_name, False))

    # Print summary
    print("\n" + "=" * 60)
    print("TEST SUMMARY")
    print("=" * 60)

    passed = sum(1 for _, success in results if success)
    total = len(results)

    for test_name, success in results:
        status = "✅ PASSED" if success else "❌ FAILED"
        print(f"{test_name}: {status}")

    print(f"\nTotal: {passed}/{total} tests passed")

    if passed == total:
        print("\n🎉 All WAL recovery tests passed!")
        return 0
    else:
        print(f"\n❌ {total - passed} tests failed")
        return 1

if __name__ == "__main__":
    sys.exit(main())