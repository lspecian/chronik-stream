#!/usr/bin/env python3
"""
Test CreatePartitions API
"""

import time
from kafka.admin import KafkaAdminClient, NewTopic
from kafka import KafkaConsumer, KafkaProducer
import logging

logging.basicConfig(level=logging.INFO)
logger = logging.getLogger(__name__)

KAFKA_BROKER = 'localhost:9094'

def test_create_partitions():
    """Test the CreatePartitions API to expand partition count"""

    logger.info("Testing CreatePartitions API...")

    # Create admin client
    admin = KafkaAdminClient(
        bootstrap_servers=[KAFKA_BROKER],
        client_id='test-create-partitions'
    )

    # Create a topic with 1 partition initially
    topic_name = f'test-expand-{int(time.time())}'
    topic = NewTopic(name=topic_name, num_partitions=1, replication_factor=1)

    try:
        # Create the topic
        fs = admin.create_topics([topic])
        for t, future in fs.items():
            try:
                future.result()
                logger.info(f"✅ Created topic '{t}' with 1 partition")
            except Exception as e:
                logger.error(f"❌ Failed to create topic '{t}': {e}")
                return False
    except Exception as e:
        logger.error(f"❌ create_topics() failed: {e}")
        return False

    time.sleep(1)

    # Now try to expand it to 3 partitions using the CreatePartitions API
    logger.info(f"Attempting to expand {topic_name} from 1 to 3 partitions...")

    try:
        # Use the internal method to test CreatePartitions
        # Note: kafka-python doesn't have a public API for this, but the AdminClient
        # has the internal method _send_request_to_controller

        # First, let's check if the topic has 1 partition
        metadata = admin._metadata
        admin._refresh_controller_id()

        # Try using the describe_topics to see current state
        topic_metadata = admin.describe_topics([topic_name])
        if topic_metadata:
            for t in topic_metadata:
                logger.info(f"Current topic '{t['topic']}' has {len(t['partitions'])} partitions")

        # Unfortunately kafka-python doesn't expose CreatePartitions directly
        # We'd need to use confluent-kafka or implement the protocol directly
        logger.warning("kafka-python doesn't expose CreatePartitions API directly")

        # As a workaround, let's test by producing to multiple partitions
        # If CreatePartitions worked, we should be able to produce to partition 2
        producer = KafkaProducer(bootstrap_servers=[KAFKA_BROKER])

        # Try to produce to partition 0 (should work)
        try:
            future = producer.send(topic_name, b'test-p0', partition=0)
            metadata = future.get(timeout=5)
            logger.info(f"✅ Produced to partition 0 at offset {metadata.offset}")
        except Exception as e:
            logger.error(f"❌ Failed to produce to partition 0: {e}")

        # Try to produce to partition 2 (would only work if expanded)
        try:
            future = producer.send(topic_name, b'test-p2', partition=2)
            metadata = future.get(timeout=5)
            logger.info(f"✅ Produced to partition 2 at offset {metadata.offset} - partitions were expanded!")
            producer.close()
            admin.close()
            return True
        except Exception as e:
            logger.warning(f"Could not produce to partition 2: {e}")
            logger.info("This is expected since we couldn't call CreatePartitions with kafka-python")

        producer.close()

    except Exception as e:
        logger.error(f"❌ Test failed: {e}")
        import traceback
        traceback.print_exc()
    finally:
        admin.close()

    return False

def test_with_raw_protocol():
    """Test CreatePartitions by sending raw protocol message"""
    import socket
    import struct

    logger.info("\nTesting CreatePartitions with raw protocol...")

    # First create a topic
    admin = KafkaAdminClient(
        bootstrap_servers=[KAFKA_BROKER],
        client_id='test-raw-create-partitions'
    )

    topic_name = f'test-raw-expand-{int(time.time())}'
    topic = NewTopic(name=topic_name, num_partitions=1, replication_factor=1)

    fs = admin.create_topics([topic])
    for t, future in fs.items():
        future.result()
        logger.info(f"Created topic '{t}' with 1 partition")

    admin.close()
    time.sleep(1)

    # Now send CreatePartitions request directly
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.connect(('localhost', 9094))

    # Build CreatePartitions request (API Key 37, Version 0)
    api_key = 37
    api_version = 0
    correlation_id = 1
    client_id = b'test-client'

    # Request header
    header = struct.pack('>hhi', api_key, api_version, correlation_id)
    header += struct.pack('>h', len(client_id)) + client_id

    # Request body
    # topics array (1 topic)
    body = struct.pack('>i', 1)
    # topic name
    topic_name_bytes = topic_name.encode('utf-8')
    body += struct.pack('>h', len(topic_name_bytes)) + topic_name_bytes
    # new partition count
    body += struct.pack('>i', 3)  # Expand to 3 partitions
    # assignments (null)
    body += struct.pack('>i', -1)
    # timeout_ms
    body += struct.pack('>i', 5000)

    # Send request
    request = header + body
    message = struct.pack('>i', len(request)) + request
    sock.sendall(message)

    # Read response
    # First read the size
    size_bytes = sock.recv(4)
    if len(size_bytes) == 4:
        size = struct.unpack('>i', size_bytes)[0]
        logger.info(f"Response size: {size} bytes")

        # Read the response
        response = sock.recv(size)

        # Parse response header
        correlation_id_resp = struct.unpack('>i', response[:4])[0]
        logger.info(f"Correlation ID: {correlation_id_resp}")

        # Parse response body (simplified - just check error code)
        # Skip throttle_time_ms
        offset = 4 + 4
        # Read results array size
        results_count = struct.unpack('>i', response[offset:offset+4])[0]
        offset += 4

        if results_count > 0:
            # Read topic name length
            name_len = struct.unpack('>h', response[offset:offset+2])[0]
            offset += 2
            # Skip topic name
            offset += name_len
            # Read error code
            error_code = struct.unpack('>h', response[offset:offset+2])[0]

            if error_code == 0:
                logger.info(f"✅ CreatePartitions succeeded! Topic expanded to 3 partitions")
                sock.close()
                return True
            else:
                logger.error(f"❌ CreatePartitions failed with error code: {error_code}")
        else:
            logger.warning("No results in response")
    else:
        logger.error("Failed to read response size")

    sock.close()
    return False

if __name__ == "__main__":
    # Try the basic test first
    result1 = test_create_partitions()

    # Then try with raw protocol
    result2 = test_with_raw_protocol()

    print("\n" + "=" * 50)
    print("TEST RESULTS:")
    print(f"Basic test: {'✅ PASSED' if result1 else '⚠️ LIMITED (kafka-python limitation)'}")
    print(f"Raw protocol test: {'✅ PASSED' if result2 else '❌ FAILED'}")

    if result2:
        print("\n🎉 CreatePartitions API is working!")
    else:
        print("\n❌ CreatePartitions API needs more work")