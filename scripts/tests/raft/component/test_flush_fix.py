#!/usr/bin/env python3
"""Simple test to verify flush fix for data consistency."""

import subprocess
import time
import os
import signal
from kafka import KafkaProducer, KafkaConsumer
from kafka.admin import KafkaAdminClient, NewTopic

def main():
    print("=" * 80)
    print("TEST: Data Consistency with Flush Fix")
    print("=" * 80)

    # Clean test directory
    test_dir = "/tmp/chronik_simple_test"
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

        # Give server a moment to process
        time.sleep(2)

        # Graceful shutdown (this should flush pending batches)
        print("\n4. Gracefully shutting down server...")
        server.send_signal(signal.SIGTERM)
        server.wait(timeout=10)
        print("   Server shut down")

        # Wait a bit
        time.sleep(2)

        # Restart server
        print("\n5. Restarting server...")
        server_log = open(f"{test_dir}/server.log", "a")
        server = subprocess.Popen(server_cmd, stdout=server_log, stderr=server_log)
        print(f"   Server restarted with PID {server.pid}")
        time.sleep(5)

        # Consume messages
        print("\n6. Consuming messages...")
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
        print("\n7. Verification:")
        print(f"   Produced: 10 messages")
        print(f"   Consumed: {len(messages)} messages")

        if len(messages) == 10:
            print("\n✅ TEST PASSED: All messages recovered!")
            return 0
        else:
            print(f"\n❌ TEST FAILED: Expected 10 messages, got {len(messages)}")
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
