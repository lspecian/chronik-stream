#!/usr/bin/env python3
"""Test that consumers can read from segments after WAL is deleted."""

import subprocess
import time
import os
import signal
import shutil
from kafka import KafkaProducer, KafkaConsumer
from kafka.admin import KafkaAdminClient, NewTopic

def main():
    print("=" * 80)
    print("TEST: Segment Fetch After WAL Deletion")
    print("=" * 80)

    # Clean test directory
    test_dir = "/tmp/chronik_segment_test"
    os.system(f"rm -rf {test_dir} && mkdir -p {test_dir}")

    # Start server
    print("\n1. Starting Chronik server...")
    server_cmd = [
        "./target/release/chronik-server",
        "--advertised-addr", "localhost",
        "--kafka-port", "9092",
        "--data-dir", test_dir,
        "standalone"
    ]

    server_log = open(f"{test_dir}/server.log", "w")
    server = subprocess.Popen(server_cmd, stdout=server_log, stderr=server_log)
    print(f"   Server started with PID {server.pid}")

    try:
        # Wait for server to start
        time.sleep(5)

        # Create topic
        print("\n2. Creating topic...")
        admin = KafkaAdminClient(bootstrap_servers='localhost:9092')
        topic = NewTopic(name='test-topic', num_partitions=1, replication_factor=1)
        admin.create_topics([topic])
        admin.close()
        print("   Topic created")
        time.sleep(1)

        # Produce messages
        print("\n3. Producing 10 messages...")
        producer = KafkaProducer(bootstrap_servers='localhost:9092')
        for i in range(10):
            msg = f"message-{i}".encode()
            producer.send('test-topic', msg)
            print(f"   Sent: message-{i}")
        producer.flush()
        producer.close()
        print("   All messages sent")

        # Manually flush to ensure data gets to segments
        print("\n4. Waiting for background indexer to run (30s)...")
        time.sleep(35)  # Wait for indexer interval + some buffer

        # Check WAL directory before shutdown
        print("\n4.5. Checking WAL directory before shutdown...")
        wal_dir = f"{test_dir}/wal"
        if os.path.exists(wal_dir):
            for root, dirs, files in os.walk(wal_dir):
                for file in files:
                    filepath = os.path.join(root, file)
                    size = os.path.getsize(filepath)
                    print(f"   Found WAL file: {filepath} (size={size} bytes)")
        else:
            print(f"   No WAL directory at {wal_dir}")

        # Gracefully shutdown to flush
        print("\n5. Shutting down server to flush pending batches...")
        server.send_signal(signal.SIGTERM)
        server.wait(timeout=10)
        print("   Server shut down")
        time.sleep(2)

        # Delete WAL to force segment-only reads
        print("\n6. Deleting WAL files...")
        wal_dir = f"{test_dir}/wal"
        if os.path.exists(wal_dir):
            shutil.rmtree(wal_dir)
            print(f"   Deleted {wal_dir}")
        else:
            print(f"   WAL directory not found: {wal_dir}")

        # Restart server
        print("\n7. Restarting server (without WAL)...")
        server_log = open(f"{test_dir}/server.log", "a")
        server = subprocess.Popen(server_cmd, stdout=server_log, stderr=server_log)
        print(f"   Server restarted with PID {server.pid}")
        time.sleep(5)

        # Consume messages (should come from segments now)
        print("\n8. Consuming messages (from segments)...")
        consumer = KafkaConsumer(
            'test-topic',
            bootstrap_servers='localhost:9092',
            auto_offset_reset='earliest',
            consumer_timeout_ms=10000
        )

        messages = []
        for msg in consumer:
            value = msg.value.decode()
            messages.append(value)
            print(f"   Received: {value}")

        consumer.close()

        # Check results
        print("\n9. Verification:")
        print(f"   Produced: 10 messages")
        print(f"   Consumed: {len(messages)} messages")

        if len(messages) == 10:
            print("\n✅ TEST PASSED: All messages fetched from segments!")
            return 0
        else:
            print(f"\n❌ TEST FAILED: Expected 10 messages, got {len(messages)}")
            print("   This means the flush didn't write to segments properly.")
            return 1

    except Exception as e:
        print(f"\n❌ TEST FAILED with exception: {e}")
        import traceback
        traceback.print_exc()
        return 1
    finally:
        if server.poll() is None:
            print("\nCleaning up: Stopping server...")
            server.send_signal(signal.SIGTERM)
            try:
                server.wait(timeout=5)
            except subprocess.TimeoutExpired:
                server.kill()
        server_log.close()

if __name__ == "__main__":
    exit(main())
