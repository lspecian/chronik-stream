#!/usr/bin/env python3
"""
Debug test for API functionality
"""

import time
from kafka.admin import KafkaAdminClient, NewTopic

KAFKA_BROKER = 'localhost:9094'

def test_create_topics():
    """Test CreateTopics API behavior"""
    print("Testing CreateTopics API...")

    admin = KafkaAdminClient(
        bootstrap_servers=[KAFKA_BROKER],
        client_id='debug-admin'
    )

    # Test 1: Create a new topic
    topic_name = f'debug-topic-{int(time.time())}'
    topic = NewTopic(name=topic_name, num_partitions=1, replication_factor=1)

    try:
        fs = admin.create_topics([topic])
        for topic, future in fs.items():
            try:
                result = future.result()
                print(f"✅ Created topic '{topic}': {result}")
            except Exception as e:
                print(f"❌ Failed to create topic '{topic}': {e}")
                print(f"   Exception type: {type(e).__name__}")
                return False
    except Exception as e:
        print(f"❌ create_topics() failed: {e}")
        return False

    # Test 2: Try to create the same topic again (should fail)
    print("\nTrying to create same topic again...")
    try:
        fs = admin.create_topics([topic])
        for topic, future in fs.items():
            try:
                result = future.result()
                print(f"❌ Should have failed but succeeded: {result}")
                return False
            except Exception as e:
                print(f"✅ Expected failure: {e}")
    except Exception as e:
        print(f"❌ Unexpected exception: {e}")

    admin.close()
    return True

def test_produce_messages():
    """Test simple produce"""
    from kafka import KafkaProducer

    print("\n\nTesting Produce API...")

    producer = KafkaProducer(bootstrap_servers=[KAFKA_BROKER])

    topic_name = 'debug-produce-test'

    # Create topic first
    admin = KafkaAdminClient(
        bootstrap_servers=[KAFKA_BROKER],
        client_id='debug-admin-2'
    )

    topic = NewTopic(name=topic_name, num_partitions=1, replication_factor=1)
    try:
        admin.create_topics([topic])
        print(f"Created topic {topic_name}")
    except:
        pass

    admin.close()
    time.sleep(0.5)

    # Try to produce
    try:
        future = producer.send(topic_name, b'test-message')
        metadata = future.get(timeout=10)
        print(f"✅ Produced message to partition {metadata.partition} at offset {metadata.offset}")
        producer.close()
        return True
    except Exception as e:
        print(f"❌ Produce failed: {e}")
        producer.close()
        return False

def test_fetch_messages():
    """Test fetch after produce"""
    from kafka import KafkaProducer, KafkaConsumer

    print("\n\nTesting Fetch API...")

    topic_name = f'debug-fetch-{int(time.time())}'

    # Create topic
    admin = KafkaAdminClient(
        bootstrap_servers=[KAFKA_BROKER],
        client_id='debug-admin-3'
    )

    topic = NewTopic(name=topic_name, num_partitions=1, replication_factor=1)
    admin.create_topics([topic])
    admin.close()
    time.sleep(0.5)

    # Produce a message
    producer = KafkaProducer(bootstrap_servers=[KAFKA_BROKER])
    future = producer.send(topic_name, b'fetch-test-message')
    metadata = future.get(timeout=10)
    print(f"Produced to offset {metadata.offset}")
    producer.close()

    # Try to fetch
    consumer = KafkaConsumer(
        topic_name,
        bootstrap_servers=[KAFKA_BROKER],
        auto_offset_reset='earliest',
        consumer_timeout_ms=2000
    )

    messages_found = 0
    for message in consumer:
        print(f"✅ Fetched message: {message.value}")
        messages_found += 1

    consumer.close()

    if messages_found == 0:
        print("❌ No messages fetched!")
        return False
    return True

if __name__ == "__main__":
    results = {
        "CreateTopics": test_create_topics(),
        "Produce": test_produce_messages(),
        "Fetch": test_fetch_messages()
    }

    print("\n" + "=" * 50)
    print("DEBUG TEST RESULTS:")
    for api, result in results.items():
        status = "✅" if result else "❌"
        print(f"{status} {api}: {'PASSED' if result else 'FAILED'}")

    if all(results.values()):
        print("\n✅ All basic APIs working!")
    else:
        print("\n❌ Some APIs have issues")