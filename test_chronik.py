from kafka import KafkaProducer, KafkaConsumer
import time

try:
    # Test producer
    print("Creating producer...")
    producer = KafkaProducer(
        bootstrap_servers='localhost:9110',
        api_version=(0, 10, 0)
    )
    
    print("Sending message...")
    future = producer.send('test-topic', b'Hello from v1.1.0!')
    result = future.get(timeout=10)
    print(f"Message sent successfully: {result}")
    
    producer.flush()
    producer.close()
    
    # Test consumer
    print("\nCreating consumer...")
    consumer = KafkaConsumer(
        'test-topic',
        bootstrap_servers='localhost:9110',
        api_version=(0, 10, 0),
        auto_offset_reset='earliest',
        consumer_timeout_ms=2000  # Stop after 2 seconds of no messages
    )
    
    print("Reading messages...")
    for message in consumer:
        print(f"Received: {message.value.decode('utf-8')}")
    
    consumer.close()
    print("\nTest completed successfully!")
    
except Exception as e:
    print(f"Error: {e}")
