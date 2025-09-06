#!/usr/bin/env python3
from kafka import KafkaProducer
import time

try:
    producer = KafkaProducer(bootstrap_servers=['localhost:29092'])
    future = producer.send('test-topic', b'test message')
    producer.flush(timeout=5)
    print("SUCCESS: Python client works")
except Exception as e:
    print(f"FAILED: {e}")
finally:
    producer.close()
