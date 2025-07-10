#!/usr/bin/env python3
"""Simple Kafka client test"""

from kafka import KafkaProducer, KafkaConsumer, KafkaAdminClient
from kafka.admin import NewTopic
import time

def test_kafka_python():
    """Test using kafka-python library"""
    print("Testing with kafka-python library...")
    
    try:
        # 1. Create admin client
        admin = KafkaAdminClient(
            bootstrap_servers=['localhost:9092'],
            client_id='test-admin'
        )
        print("✓ Connected to Kafka")
        
        # 2. Create topic
        topic = NewTopic(name='python-test', num_partitions=3, replication_factor=1)
        try:
            admin.create_topics([topic])
            print("✓ Created topic 'python-test'")
        except Exception as e:
            print(f"  Topic creation: {e}")
        
        # 3. List topics
        topics = admin.list_topics()
        print(f"✓ Topics: {topics}")
        
        # 4. Create producer
        producer = KafkaProducer(
            bootstrap_servers=['localhost:9092'],
            client_id='test-producer'
        )
        
        # 5. Send message
        future = producer.send('python-test', b'Hello from Python!')
        producer.flush()
        print("✓ Sent message")
        
        # 6. Create consumer
        consumer = KafkaConsumer(
            'python-test',
            bootstrap_servers=['localhost:9092'],
            client_id='test-consumer',
            auto_offset_reset='earliest',
            consumer_timeout_ms=5000
        )
        
        # 7. Consume messages
        print("✓ Consuming messages:")
        for message in consumer:
            print(f"  - {message.value.decode('utf-8')}")
        
        print("\n✅ All tests passed!")
        
    except Exception as e:
        print(f"\n❌ Error: {e}")
        import traceback
        traceback.print_exc()

if __name__ == "__main__":
    test_kafka_python()