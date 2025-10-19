#!/usr/bin/env python3
"""Test that acks=0 producers don't lose data on shutdown.

This test verifies the fix for the critical data loss bug where:
1. Producer sends with acks=0 (fire-and-forget)
2. Data is buffered in WAL queue but not yet fsynced
3. Server receives SIGTERM
4. OLD BUG: flush_all_partitions() was a no-op, data lost
5. FIX: server.shutdown() seals WAL segments and fsyncs

Expected behavior: ALL messages should be recoverable after restart.
"""

import subprocess
import time
import os
import signal
import sys
from kafka import KafkaProducer, KafkaConsumer
from kafka.admin import KafkaAdminClient, NewTopic

def main():
    print("=" * 80)
    print("TEST: acks=0 Producer Data Loss Prevention")
    print("=" * 80)
    print("\nThis test verifies that messages sent with acks=0 are NOT lost")
    print("when the server is shut down gracefully with SIGTERM.\n")

    # Clean test directory
    test_dir = "/tmp/chronik_acks0_test"
    os.system(f"rm -rf {test_dir} && mkdir -p {test_dir}")

    # Start server
    print("1. Starting Chronik server...")
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
        print("\n2. Creating topic with 3 partitions...")
        admin = KafkaAdminClient(bootstrap_servers='localhost:9092')
        topic = NewTopic(name='acks0-test', num_partitions=3, replication_factor=1)
        admin.create_topics([topic])
        admin.close()
        print("   Topic created")
        time.sleep(1)

        # Produce messages with acks=0 (fire-and-forget)
        print("\n3. Producing 100 messages with acks=0 (fire-and-forget)...")
        producer = KafkaProducer(
            bootstrap_servers='localhost:9092',
            acks=0,  # CRITICAL: Fire-and-forget mode (no wait for fsync)
            linger_ms=0,  # Send immediately
            batch_size=1,  # One message per batch for testing
        )

        message_ids = []
        for i in range(100):
            message = f"acks0-message-{i}".encode('utf-8')
            future = producer.send('acks0-test', value=message)
            message_ids.append(i)
            if (i + 1) % 25 == 0:
                print(f"   Sent {i + 1} messages...")

        print("   All 100 messages sent (acks=0 - no fsync confirmation)")

        # Close producer
        producer.close()
        print("   Producer closed")

        # CRITICAL: Do NOT wait for background indexer
        # The point is to test that buffered WAL data is flushed on shutdown
        # even if it hasn't been indexed yet
        print("\n4. Immediately sending SIGTERM to server...")
        print("   (This tests that buffered WAL data is properly fsynced on shutdown)")

        # Send SIGTERM (graceful shutdown)
        os.kill(server.pid, signal.SIGTERM)

        # Wait for shutdown to complete
        print("   Waiting for graceful shutdown...")
        try:
            server.wait(timeout=15)
            print(f"   Server shut down cleanly with exit code {server.returncode}")
        except subprocess.TimeoutExpired:
            print("   ERROR: Server did not shutdown within 15 seconds")
            server.kill()
            server.wait()
            return False

        # Check the log for shutdown sequence
        print("\n5. Verifying shutdown sequence in logs...")
        with open(f"{test_dir}/server.log", "r") as f:
            log = f.read()

        shutdown_markers = [
            "Received SIGTERM",
            "Executing graceful shutdown sequence" if "raft_cluster" in log else "Flushing all partitions",
            "WAL segments sealed",
            "WalIndexer run complete"
        ]

        for marker in shutdown_markers:
            if marker in log:
                print(f"   ✓ Found: {marker}")
            else:
                print(f"   ✗ Missing: {marker}")

        # Check WAL directory
        print("\n6. Checking WAL directory...")
        wal_dir = f"{test_dir}/wal"
        if os.path.exists(wal_dir):
            wal_files = []
            for root, dirs, files in os.walk(wal_dir):
                for file in files:
                    if file.endswith('.log'):
                        filepath = os.path.join(root, file)
                        size = os.path.getsize(filepath)
                        wal_files.append((filepath, size))
                        print(f"   Found WAL file: {filepath} (size={size} bytes)")

            if not wal_files:
                print("   ⚠️  WARNING: No WAL files found (may have been cleaned up)")
        else:
            print("   ⚠️  WARNING: WAL directory doesn't exist")

        # Check for segment files in object store
        print("\n7. Checking for segments in object store...")
        segments_dir = f"{test_dir}/segments"
        if os.path.exists(segments_dir):
            segment_count = 0
            for root, dirs, files in os.walk(segments_dir):
                for file in files:
                    if file.endswith('.segment'):
                        filepath = os.path.join(root, file)
                        size = os.path.getsize(filepath)
                        segment_count += 1
                        print(f"   Found segment: {filepath} (size={size} bytes)")

            if segment_count > 0:
                print(f"   ✓ Total segments found: {segment_count}")
            else:
                print("   ⚠️  WARNING: No segment files found")
        else:
            print("   ⚠️  WARNING: Segments directory doesn't exist")

        # Restart server
        print("\n8. Restarting server...")
        server_log.close()
        server_log = open(f"{test_dir}/server.log", "a")
        server = subprocess.Popen(server_cmd, stdout=server_log, stderr=server_log)
        print(f"   Server restarted with PID {server.pid}")
        time.sleep(5)

        # Consume all messages
        print("\n9. Consuming messages to verify no data loss...")
        consumer = KafkaConsumer(
            'acks0-test',
            bootstrap_servers='localhost:9092',
            auto_offset_reset='earliest',
            consumer_timeout_ms=10000,
            group_id=f'test-consumer-acks0'
        )

        consumed_messages = []
        consumed_ids = []

        for message in consumer:
            value = message.value.decode('utf-8')
            consumed_messages.append(value)
            # Extract message ID
            if value.startswith('acks0-message-'):
                msg_id = int(value.split('-')[-1])
                consumed_ids.append(msg_id)
            if len(consumed_messages) % 25 == 0:
                print(f"   Consumed {len(consumed_messages)} messages...")

        consumer.close()
        print(f"   Total consumed: {len(consumed_messages)} messages")

        # Verify results
        print("\n10. Verification:")
        print(f"   Produced: {len(message_ids)} messages")
        print(f"   Consumed: {len(consumed_messages)} messages")

        # Check for missing messages
        missing_ids = set(message_ids) - set(consumed_ids)
        duplicate_ids = [x for x in consumed_ids if consumed_ids.count(x) > 1]

        if missing_ids:
            print(f"\n   ❌ MISSING {len(missing_ids)} messages: {sorted(list(missing_ids))[:20]}")

        if duplicate_ids:
            print(f"\n   ⚠️  WARNING: Found {len(duplicate_ids)} duplicate messages")

        # Final verdict
        print("\n" + "=" * 80)

        # The key test is: NO DATA LOSS (missing_ids should be empty)
        # Duplicates are acceptable (can happen when data is in both WAL and segments)
        # Consumers should use idempotency keys to deduplicate
        if not missing_ids:
            print("✅ TEST PASSED: All acks=0 messages survived graceful shutdown!")
            print("   Zero data loss confirmed - the fix works correctly.")
            if duplicate_ids:
                print(f"\n   Note: Found {len(set(duplicate_ids))} duplicate messages")
                print("   (This is acceptable - data exists in both WAL and segments)")
                print("   (Consumers should use idempotency keys for deduplication)")
            print("=" * 80)
            return True
        else:
            print("❌ TEST FAILED: Data loss detected with acks=0 producers!")
            print(f"   Expected: {len(message_ids)} messages")
            print(f"   Got: {len(consumed_messages)} unique messages")
            print(f"   Lost: {len(missing_ids)} messages")
            print(f"   Missing IDs: {sorted(list(missing_ids))[:20]}")
            print("=" * 80)
            return False

    except Exception as e:
        print(f"\n❌ TEST ERROR: {e}")
        import traceback
        traceback.print_exc()
        return False

    finally:
        print("\nCleaning up: Stopping server...")
        try:
            server.kill()
            server.wait(timeout=5)
        except:
            pass
        server_log.close()

if __name__ == '__main__':
    success = main()
    sys.exit(0 if success else 1)
