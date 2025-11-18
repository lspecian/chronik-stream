#!/usr/bin/env python3
"""Manual benchmark - simpler than chronik-bench"""
from kafka import KafkaProducer
import time

producer = KafkaProducer(
    bootstrap_servers=['localhost:9092', 'localhost:9093', 'localhost:9094'],
    acks=1,
    linger_ms=0,
    batch_size=16384
)

topic = 'bench-test'
count = 1000
size = 1024
payload = b'x' * size

print(f"Producing {count} messages of {size} bytes...")
start = time.time()
failures = 0

for i in range(count):
    try:
        future = producer.send(topic, payload, partition=i % 3)
        if i % 100 == 0:
            result = future.get(timeout=5)
            print(f"  {i}: offset={result.offset}, partition={result.partition}")
    except Exception as e:
        failures += 1
        if failures < 5:
            print(f"  ERROR at {i}: {e}")

producer.flush()
producer.close()

elapsed = time.time() - start
rate = count / elapsed

print(f"\nResults:")
print(f"  Total: {count} messages")
print(f"  Failed: {failures}")
print(f"  Time: {elapsed:.2f}s")
print(f"  Rate: {rate:.0f} msg/s")
print(f"  Throughput: {rate * size / 1024 / 1024:.2f} MB/s")
