#!/usr/bin/env python3
"""
Test script to verify the Raft prost bridge fix works correctly.

This tests:
1. Single-node Raft can start and commit entries
2. Messages can be produced and consumed
3. No CRC or deserialization errors occur
"""

import subprocess
import time
import sys
from kafka import KafkaProducer, KafkaConsumer
from kafka.errors import KafkaError

def start_server(advertised_addr="localhost"):
    """Start chronik-server in standalone mode"""
    print(f"Starting chronik-server (advertised: {advertised_addr})...")

    # Use cargo run for latest code (will rebuild if needed)
    proc = subprocess.Popen(
        ["cargo", "run", "--release", "--bin", "chronik-server", "--",
         "--advertised-addr", advertised_addr, "standalone"],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True
    )

    # Wait for server to start
    print("Waiting for server to start...")
    time.sleep(10)

    return proc

def test_produce_consume():
    """Test basic produce/consume to verify Raft works"""
    print("\n=== Testing Produce/Consume ===")

    topic = "test-raft-bridge"

    try:
        # Create producer
        print("Creating producer...")
        producer = KafkaProducer(
            bootstrap_servers='localhost:9092',
            api_version=(0, 10, 0),
            request_timeout_ms=30000
        )

        # Send test messages
        print(f"Sending 10 messages to topic '{topic}'...")
        for i in range(10):
            msg = f"Test message {i} - verifying Raft bridge fix"
            future = producer.send(topic, msg.encode('utf-8'))
            result = future.get(timeout=10)
            print(f"  Message {i} sent: partition={result.partition}, offset={result.offset}")

        producer.flush()
        producer.close()
        print("✓ All messages sent successfully")

        # Create consumer
        print("\nCreating consumer...")
        consumer = KafkaConsumer(
            topic,
            bootstrap_servers='localhost:9092',
            auto_offset_reset='earliest',
            consumer_timeout_ms=10000,
            api_version=(0, 10, 0)
        )

        # Consume messages
        print(f"Consuming messages from topic '{topic}'...")
        messages = []
        for msg in consumer:
            messages.append(msg.value.decode('utf-8'))
            print(f"  Received: {msg.value.decode('utf-8')[:50]}... (offset={msg.offset})")

        consumer.close()

        if len(messages) == 10:
            print(f"✓ Consumed all {len(messages)} messages successfully")
            return True
        else:
            print(f"✗ Expected 10 messages, got {len(messages)}")
            return False

    except Exception as e:
        print(f"✗ Test failed with error: {e}")
        import traceback
        traceback.print_exc()
        return False

def main():
    print("=" * 60)
    print("Raft Prost Bridge Fix Verification")
    print("=" * 60)

    server = None
    try:
        # Start server
        server = start_server()

        # Run test
        success = test_produce_consume()

        if success:
            print("\n" + "=" * 60)
            print("✓ ALL TESTS PASSED - Raft bridge fix verified!")
            print("=" * 60)
            return 0
        else:
            print("\n" + "=" * 60)
            print("✗ TEST FAILED")
            print("=" * 60)
            return 1

    except KeyboardInterrupt:
        print("\nTest interrupted by user")
        return 1
    except Exception as e:
        print(f"\n✗ Unexpected error: {e}")
        import traceback
        traceback.print_exc()
        return 1
    finally:
        if server:
            print("\nStopping server...")
            server.terminate()
            server.wait(timeout=5)

if __name__ == "__main__":
    sys.exit(main())
