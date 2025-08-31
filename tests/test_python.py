from kafka import KafkaProducer, KafkaConsumer
import time

print("Testing Python client...")
try:
    producer = KafkaProducer(bootstrap_servers='localhost:9092')
    print("✓ Producer created")
    
    future = producer.send('test-topic', b'Hello from Python')
    result = future.get(timeout=5)
    print(f"✓ Message sent: {result}")
    
    producer.flush()
    print("✓ Flush successful")
    
except Exception as e:
    print(f"✗ Error: {e}")
