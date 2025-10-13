#!/usr/bin/env python3
"""Quick performance test for optimized GroupCommitWal"""

import subprocess
import time
from kafka import KafkaProducer

def test_throughput(message_count=10000):
    print(f"🚀 Testing throughput with {message_count:,} messages")

    # Start server
    print("Starting server...")
    server = subprocess.Popen(
        ['./target/release/chronik-server'],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL
    )
    time.sleep(3)

    try:
        # Create producer with batching
        producer = KafkaProducer(
            bootstrap_servers='localhost:9092',
            acks=1,
            linger_ms=10,
            batch_size=16384,
        )

        # Send messages
        start = time.time()
        futures = []

        for i in range(message_count):
            future = producer.send('perf-test', f'Message {i}'.encode())
            futures.append(future)

        # Wait for all acks
        for future in futures:
            future.get(timeout=10)

        producer.flush()
        duration = time.time() - start

        throughput = message_count / duration

        print(f"\n📊 Results:")
        print(f"   Messages: {message_count:,}")
        print(f"   Duration: {duration:.2f}s")
        print(f"   Throughput: {throughput:,.0f} msgs/sec")

        producer.close()

    finally:
        server.terminate()
        try:
            server.wait(timeout=5)
        except subprocess.TimeoutExpired:
            server.kill()
            server.wait()

if __name__ == '__main__':
    test_throughput(10000)
