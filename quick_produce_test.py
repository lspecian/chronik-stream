#!/usr/bin/env python3
from kafka import KafkaProducer
import sys

producer = KafkaProducer(
    bootstrap_servers=['localhost:9092'],
    acks=1,
    request_timeout_ms=5000
)

try:
    print("Sending test message...")
    future = producer.send('bench-test', b'test', partition=0)
    result = future.get(timeout=5)
    print(f"✓ SUCCESS: offset={result.offset}, partition={result.partition}")
    sys.exit(0)
except Exception as e:
    print(f"✗ FAILED: {e}")
    sys.exit(1)
finally:
    producer.close()
