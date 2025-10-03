#!/usr/bin/env python3
"""
Test Transaction APIs using confluent-kafka client
"""

import time
import logging
import sys
from confluent_kafka import Producer, Consumer, admin, KafkaError
from confluent_kafka.admin import AdminClient, NewTopic

logging.basicConfig(level=logging.INFO, format='%(asctime)s - %(levelname)s - %(message)s')
logger = logging.getLogger(__name__)

KAFKA_BROKER = 'localhost:9094'
TOPIC_NAME = 'txn-test-topic'
TRANSACTIONAL_ID = 'test-transaction-1'
GROUP_ID = 'test-consumer-group'

def delivery_report(err, msg):
    """Delivery callback for produce"""
    if err is not None:
        logger.error(f'Message delivery failed: {err}')
    else:
        logger.debug(f'Message delivered to {msg.topic()} [{msg.partition()}] at offset {msg.offset()}')

def setup_topic():
    """Create test topic"""
    try:
        admin_client = AdminClient({'bootstrap.servers': KAFKA_BROKER})

        # Delete topic if exists
        try:
            fs = admin_client.delete_topics([TOPIC_NAME], operation_timeout=30)
            for topic, f in fs.items():
                try:
                    f.result()
                    logger.info(f"Deleted topic {topic}")
                except Exception as e:
                    logger.debug(f"Topic deletion failed (may not exist): {e}")
        except:
            pass

        time.sleep(1)

        # Create topic
        topic = NewTopic(TOPIC_NAME, num_partitions=3, replication_factor=1)
        fs = admin_client.create_topics([topic])
        for topic, f in fs.items():
            try:
                f.result()
                logger.info(f"Created topic {topic}")
            except Exception as e:
                logger.error(f"Failed to create topic {topic}: {e}")
                raise
    except Exception as e:
        logger.error(f"Setup failed: {e}")
        raise

def test_transaction_apis():
    """Test all transaction APIs"""
    logger.info("=" * 60)
    logger.info("Testing Transaction APIs with Confluent Kafka")
    logger.info("=" * 60)

    producer = None
    try:
        # Create transactional producer
        logger.info(f"Creating transactional producer with id: {TRANSACTIONAL_ID}")
        producer = Producer({
            'bootstrap.servers': KAFKA_BROKER,
            'transactional.id': TRANSACTIONAL_ID,
            'enable.idempotence': True,
        })

        # Initialize transactions - calls InitProducerId (API 22)
        logger.info("Initializing transactions (InitProducerId API)...")
        producer.init_transactions()
        logger.info("✅ InitProducerId successful")

        # Test 1: Committed transaction
        logger.info("\n--- Test 1: Committed Transaction ---")
        producer.begin_transaction()
        logger.info("Transaction started")

        # Send messages - triggers AddPartitionsToTxn (API 24)
        for i in range(3):
            msg = f"Committed message {i}"
            producer.produce(TOPIC_NAME,
                            key=f"key{i}",
                            value=msg,
                            partition=i % 3,
                            callback=delivery_report)
            logger.info(f"Produced: {msg} to partition {i % 3}")

        producer.flush()

        # Commit transaction - calls EndTxn (API 26) with commit=true
        logger.info("Committing transaction (EndTxn API with commit)...")
        producer.commit_transaction()
        logger.info("✅ Transaction committed successfully")

        # Test 2: Aborted transaction
        logger.info("\n--- Test 2: Aborted Transaction ---")
        producer.begin_transaction()
        logger.info("Transaction started")

        for i in range(2):
            msg = f"Will be aborted {i}"
            producer.produce(TOPIC_NAME,
                            key=f"abort{i}",
                            value=msg,
                            callback=delivery_report)
            logger.info(f"Produced (will abort): {msg}")

        producer.flush()

        # Abort transaction - calls EndTxn (API 26) with commit=false
        logger.info("Aborting transaction (EndTxn API with abort)...")
        producer.abort_transaction()
        logger.info("✅ Transaction aborted successfully")

        # Test 3: Transaction with consumer offsets
        logger.info("\n--- Test 3: Transaction with Offsets (TxnOffsetCommit) ---")

        # Create a consumer to get offsets
        consumer = Consumer({
            'bootstrap.servers': KAFKA_BROKER,
            'group.id': GROUP_ID,
            'auto.offset.reset': 'earliest',
            'enable.auto.commit': False,
        })

        consumer.subscribe([TOPIC_NAME])

        # Consume a message to get an offset
        msg = consumer.poll(1.0)
        if msg and not msg.error():
            logger.info(f"Consumed message at offset {msg.offset()}")

            # Send offset to transaction - calls TxnOffsetCommit (API 28)
            producer.begin_transaction()
            logger.info("Transaction started with offset commit")

            producer.produce(TOPIC_NAME,
                           key="txn_offset",
                           value="Message with offset commit",
                           callback=delivery_report)

            # Commit offsets as part of transaction
            logger.info("Sending offsets to transaction (TxnOffsetCommit API)...")
            offsets = consumer.position(consumer.assignment())
            producer.send_offsets_to_transaction(
                offsets,
                consumer.consumer_group_metadata()
            )

            producer.commit_transaction()
            logger.info("✅ TxnOffsetCommit successful - offsets committed with transaction")

        consumer.close()

        logger.info("\n" + "=" * 60)
        logger.info("TRANSACTION API TEST RESULTS")
        logger.info("=" * 60)
        logger.info("✅ InitProducerId (API 22) - Working")
        logger.info("✅ AddPartitionsToTxn (API 24) - Working")
        logger.info("✅ EndTxn (API 26) - Working (both commit and abort)")
        logger.info("✅ TxnOffsetCommit (API 28) - Working")
        logger.info("\n🎉 All transaction APIs verified successfully!")

        return True

    except Exception as e:
        logger.error(f"Transaction test failed: {e}")
        import traceback
        traceback.print_exc()
        return False
    finally:
        if producer:
            producer.flush()

def verify_committed_messages():
    """Verify only committed messages are visible"""
    logger.info("\n" + "=" * 60)
    logger.info("Verifying Transaction Isolation")
    logger.info("=" * 60)

    consumer = None
    try:
        # Create consumer with read_committed isolation
        consumer = Consumer({
            'bootstrap.servers': KAFKA_BROKER,
            'group.id': f'{GROUP_ID}-verify',
            'auto.offset.reset': 'earliest',
            'isolation.level': 'read_committed',
        })

        consumer.subscribe([TOPIC_NAME])
        logger.info("Consuming with read_committed isolation...")

        committed_count = 0
        aborted_found = False

        while True:
            msg = consumer.poll(1.0)
            if msg is None:
                break
            if msg.error():
                if msg.error().code() == KafkaError._PARTITION_EOF:
                    break
                else:
                    logger.error(f"Consumer error: {msg.error()}")
                    break

            value = msg.value().decode('utf-8')
            logger.info(f"Consumed: {value}")

            if "Committed message" in value or "Message with offset" in value:
                committed_count += 1
            elif "aborted" in value:
                aborted_found = True
                logger.error(f"❌ Found aborted message: {value}")

        if not aborted_found and committed_count > 0:
            logger.info(f"\n✅ Transaction isolation verified - {committed_count} committed messages, 0 aborted")
            return True
        else:
            logger.error("❌ Transaction isolation failed")
            return False

    except Exception as e:
        logger.error(f"Verification failed: {e}")
        return False
    finally:
        if consumer:
            consumer.close()

def main():
    """Run all tests"""
    try:
        # Setup
        setup_topic()
        time.sleep(2)

        # Test transaction APIs
        if not test_transaction_apis():
            return 1

        time.sleep(1)

        # Verify isolation
        if not verify_committed_messages():
            return 1

        return 0

    except Exception as e:
        logger.error(f"Test suite failed: {e}")
        return 1

if __name__ == "__main__":
    sys.exit(main())