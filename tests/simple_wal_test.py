#!/usr/bin/env python3
"""
Simple WAL test that sends messages one at a time with immediate flush.
This ensures messages are actually sent to the server without batching delays.
"""

import time
from kafka import KafkaProducer
from kafka.errors import KafkaError

BOOTSTRAP_SERVER = 'localhost:9092'
TOPIC = 'wal-test-simple'
TARGET_MB = 100  # Send 100MB of data
MESSAGE_SIZE = 1024  # 1KB per message
MESSAGE_COUNT = (TARGET_MB * 1024 * 1024) // MESSAGE_SIZE

print(f"Simple WAL Test")
print(f"=" * 60)
print(f"Target: {TARGET_MB}MB ({MESSAGE_COUNT:,} messages of {MESSAGE_SIZE} bytes)")
print(f"Server: {BOOTSTRAP_SERVER}")
print(f"Topic: {TOPIC}")
print()

# Create producer with acks='all' for durability
producer = KafkaProducer(
    bootstrap_servers=BOOTSTRAP_SERVER,
    api_version=(0, 10, 0),
    acks='all',  # Require full durability (acks=-1)
    compression_type=None,
    max_request_size=2 * 1024 * 1024,  # 2MB max request
    request_timeout_ms=30000,
    retries=3
)

print("Producer connected. Starting to send messages...")

# Generate a 1KB message payload
message = b'X' * MESSAGE_SIZE

start_time = time.time()
last_report = start_time
sent_count = 0
error_count = 0

try:
    for i in range(MESSAGE_COUNT):
        try:
            # Send message and wait for confirmation
            future = producer.send(TOPIC, value=message)
            record_metadata = future.get(timeout=10)
            sent_count += 1

            # Report progress every 1000 messages
            if sent_count % 1000 == 0:
                now = time.time()
                elapsed = now - start_time
                mb_sent = (sent_count * MESSAGE_SIZE) / (1024 * 1024)
                throughput = mb_sent / elapsed if elapsed > 0 else 0

                print(f"Sent {sent_count:,}/{MESSAGE_COUNT:,} messages "
                      f"({mb_sent:.1f}MB) - {throughput:.2f} MB/s")

        except KafkaError as e:
            error_count += 1
            print(f"Error sending message {i}: {e}")
            if error_count > 10:
                print("Too many errors, aborting")
                break

except KeyboardInterrupt:
    print("\nInterrupted by user")

finally:
    # Flush and close
    print("\nFlushing producer...")
    producer.flush()
    producer.close()

    end_time = time.time()
    duration = end_time - start_time
    mb_sent = (sent_count * MESSAGE_SIZE) / (1024 * 1024)
    avg_throughput = mb_sent / duration if duration > 0 else 0

    print()
    print(f"Test completed!")
    print(f"Total sent: {sent_count:,} messages ({mb_sent:.2f}MB)")
    print(f"Errors: {error_count}")
    print(f"Duration: {duration:.1f} seconds")
    print(f"Average throughput: {avg_throughput:.2f} MB/s")
    print()
    print(f"Check WAL data at: data/wal/{TOPIC}/0/")
