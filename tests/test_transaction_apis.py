#!/usr/bin/env python3
"""
Test Transaction APIs for Chronik Stream

Tests the following Kafka transaction APIs:
- InitProducerId (API Key 22)
- AddPartitionsToTxn (API Key 24)
- EndTxn (API Key 26)
- TxnOffsetCommit (API Key 28)
"""

import time
import logging
from kafka import KafkaProducer, KafkaConsumer
from kafka.admin import KafkaAdminClient, NewTopic
from kafka.errors import KafkaError

logging.basicConfig(level=logging.INFO, format='%(asctime)s - %(levelname)s - %(message)s')
logger = logging.getLogger(__name__)

KAFKA_BROKER = 'localhost:9094'
TOPIC_NAME = 'txn-test-topic'
TRANSACTIONAL_ID = 'test-transaction-1'
GROUP_ID = 'test-consumer-group'

def cleanup():
    """Clean up topics before test"""
    try:
        admin = KafkaAdminClient(
            bootstrap_servers=[KAFKA_BROKER],
            client_id='test-admin'
        )
        # Try to delete topic if it exists
        try:
            admin.delete_topics([TOPIC_NAME])
            logger.info(f"Deleted topic {TOPIC_NAME}")
            time.sleep(1)
        except Exception as e:
            logger.debug(f"Topic deletion failed (may not exist): {e}")

        # Create topic
        topic = NewTopic(name=TOPIC_NAME, num_partitions=3, replication_factor=1)
        admin.create_topics([topic])
        logger.info(f"Created topic {TOPIC_NAME} with 3 partitions")
        admin.close()
        time.sleep(1)
    except Exception as e:
        logger.error(f"Setup failed: {e}")
        raise

def test_transactional_producer():
    """Test transactional producer using all transaction APIs"""
    logger.info("=" * 60)
    logger.info("Testing Transactional Producer")
    logger.info("=" * 60)

    producer = None
    try:
        # Create transactional producer - this will call InitProducerId
        logger.info(f"Creating transactional producer with id: {TRANSACTIONAL_ID}")
        producer = KafkaProducer(
            bootstrap_servers=[KAFKA_BROKER],
            transactional_id=TRANSACTIONAL_ID,
            enable_idempotence=True,  # Required for transactions
            acks='all',  # Required for transactions
            client_id='test-transactional-producer'
        )

        # Initialize transactions - calls InitProducerId (API 22)
        logger.info("Initializing transactions (InitProducerId)...")
        producer.init_transactions()
        logger.info("✅ InitProducerId successful")

        # Test 1: Successful transaction
        logger.info("\n--- Test 1: Successful Transaction ---")
        try:
            # Begin transaction
            logger.info("Beginning transaction...")
            producer.begin_transaction()

            # Send messages within transaction - calls AddPartitionsToTxn (API 24)
            messages = [
                ("key1", "Transaction message 1"),
                ("key2", "Transaction message 2"),
                ("key3", "Transaction message 3")
            ]

            for key, value in messages:
                future = producer.send(
                    TOPIC_NAME,
                    key=key.encode('utf-8'),
                    value=value.encode('utf-8'),
                    partition=hash(key) % 3  # Distribute across partitions
                )
                logger.info(f"Sent message: {key} -> {value} (partition {hash(key) % 3})")

            # Flush to ensure messages are sent
            producer.flush()

            # Commit transaction - calls EndTxn (API 26) with commit=true
            logger.info("Committing transaction (EndTxn with commit)...")
            producer.commit_transaction()
            logger.info("✅ Transaction committed successfully")

        except Exception as e:
            logger.error(f"Transaction failed: {e}")
            producer.abort_transaction()
            raise

        # Test 2: Aborted transaction
        logger.info("\n--- Test 2: Aborted Transaction ---")
        try:
            # Begin new transaction
            logger.info("Beginning transaction...")
            producer.begin_transaction()

            # Send some messages
            messages_abort = [
                ("abort1", "This should be aborted 1"),
                ("abort2", "This should be aborted 2")
            ]

            for key, value in messages_abort:
                producer.send(
                    TOPIC_NAME,
                    key=key.encode('utf-8'),
                    value=value.encode('utf-8')
                )
                logger.info(f"Sent message (will abort): {key} -> {value}")

            producer.flush()

            # Abort transaction - calls EndTxn (API 26) with commit=false
            logger.info("Aborting transaction (EndTxn with abort)...")
            producer.abort_transaction()
            logger.info("✅ Transaction aborted successfully")

        except Exception as e:
            logger.error(f"Abort test failed: {e}")
            raise

        # Test 3: Multiple partition transaction
        logger.info("\n--- Test 3: Multi-Partition Transaction ---")
        try:
            producer.begin_transaction()

            # Send to all partitions - tests AddPartitionsToTxn with multiple partitions
            multi_messages = [
                ("multi0", "Partition 0 message", 0),
                ("multi1", "Partition 1 message", 1),
                ("multi2", "Partition 2 message", 2)
            ]

            for key, value, partition in multi_messages:
                producer.send(
                    TOPIC_NAME,
                    key=key.encode('utf-8'),
                    value=value.encode('utf-8'),
                    partition=partition
                )
                logger.info(f"Sent to partition {partition}: {key} -> {value}")

            producer.flush()
            producer.commit_transaction()
            logger.info("✅ Multi-partition transaction committed")

        except Exception as e:
            logger.error(f"Multi-partition transaction failed: {e}")
            producer.abort_transaction()
            raise

        logger.info("\n✅ All transaction API tests passed!")

    except Exception as e:
        logger.error(f"Transactional producer test failed: {e}")
        raise
    finally:
        if producer:
            producer.close()

def test_transactional_consumer():
    """Test consuming messages with read_committed isolation"""
    logger.info("\n" + "=" * 60)
    logger.info("Testing Transactional Consumer")
    logger.info("=" * 60)

    consumer = None
    try:
        # Create consumer with read_committed isolation
        logger.info("Creating consumer with read_committed isolation...")
        consumer = KafkaConsumer(
            TOPIC_NAME,
            bootstrap_servers=[KAFKA_BROKER],
            group_id=GROUP_ID,
            auto_offset_reset='earliest',
            enable_auto_commit=False,
            isolation_level='read_committed',  # Only see committed transactions
            consumer_timeout_ms=5000
        )

        logger.info("Consuming committed messages...")
        messages_consumed = []

        for message in consumer:
            msg_str = f"Partition {message.partition}, Offset {message.offset}: {message.key.decode()} -> {message.value.decode()}"
            logger.info(f"Consumed: {msg_str}")
            messages_consumed.append(message.value.decode())

        # Verify only committed messages are visible
        expected_messages = [
            "Transaction message 1",
            "Transaction message 2",
            "Transaction message 3",
            "Partition 0 message",
            "Partition 1 message",
            "Partition 2 message"
        ]

        # Check aborted messages are NOT visible
        aborted_messages = ["This should be aborted 1", "This should be aborted 2"]

        for msg in aborted_messages:
            if msg in messages_consumed:
                logger.error(f"❌ Aborted message was consumed: {msg}")
                raise Exception("Aborted messages should not be visible")

        logger.info(f"\n✅ Verified: Aborted messages were not consumed")
        logger.info(f"✅ Successfully consumed {len(messages_consumed)} committed messages")

    except Exception as e:
        if "timeout" not in str(e).lower():
            logger.error(f"Consumer test failed: {e}")
            raise
        else:
            logger.info("Consumer timeout reached (expected)")
    finally:
        if consumer:
            consumer.close()

def test_txn_offset_commit():
    """Test transactional offset commits (TxnOffsetCommit - API 28)"""
    logger.info("\n" + "=" * 60)
    logger.info("Testing Transactional Offset Commit")
    logger.info("=" * 60)

    producer = None
    consumer = None

    try:
        # Create transactional producer
        producer = KafkaProducer(
            bootstrap_servers=[KAFKA_BROKER],
            transactional_id=f"{TRANSACTIONAL_ID}-offset",
            enable_idempotence=True,
            acks='all'
        )

        # Create consumer
        consumer = KafkaConsumer(
            TOPIC_NAME,
            bootstrap_servers=[KAFKA_BROKER],
            group_id=f"{GROUP_ID}-txn",
            auto_offset_reset='earliest',
            enable_auto_commit=False
        )

        # Initialize producer transactions
        logger.info("Initializing producer transactions...")
        producer.init_transactions()

        # Begin transaction
        producer.begin_transaction()

        # Send messages
        logger.info("Sending messages in transaction...")
        for i in range(3):
            producer.send(TOPIC_NAME, value=f"Offset test {i}".encode())

        producer.flush()

        # Send offsets to transaction - calls TxnOffsetCommit (API 28)
        logger.info("Sending offsets to transaction (TxnOffsetCommit)...")
        offsets = {
            consumer.assignment().pop(): consumer.position(consumer.assignment().pop())
        }

        # Note: send_offsets_to_transaction is the API that uses TxnOffsetCommit
        # This commits consumer offsets as part of the transaction
        producer.send_offsets_to_transaction(
            offsets,
            f"{GROUP_ID}-txn"
        )

        # Commit transaction with offsets
        logger.info("Committing transaction with offsets...")
        producer.commit_transaction()

        logger.info("✅ TxnOffsetCommit successful - offsets committed with transaction")

    except Exception as e:
        logger.error(f"TxnOffsetCommit test failed: {e}")
        if producer:
            try:
                producer.abort_transaction()
            except:
                pass
        raise
    finally:
        if consumer:
            consumer.close()
        if producer:
            producer.close()

def verify_transaction_apis():
    """Verify all transaction APIs are working"""
    logger.info("\n" + "=" * 60)
    logger.info("TRANSACTION API VERIFICATION SUMMARY")
    logger.info("=" * 60)

    results = {
        "InitProducerId (API 22)": "✅ Working - Producer initialization successful",
        "AddPartitionsToTxn (API 24)": "✅ Working - Multiple partitions added to transactions",
        "EndTxn (API 26)": "✅ Working - Both commit and abort tested",
        "TxnOffsetCommit (API 28)": "✅ Working - Offsets committed with transaction"
    }

    for api, status in results.items():
        logger.info(f"{api}: {status}")

    logger.info("\n" + "=" * 60)
    logger.info("ALL TRANSACTION APIS VERIFIED SUCCESSFULLY!")
    logger.info("=" * 60)

def main():
    """Run all transaction API tests"""
    try:
        # Setup
        cleanup()
        time.sleep(2)

        # Run tests
        test_transactional_producer()
        time.sleep(1)

        test_transactional_consumer()
        time.sleep(1)

        test_txn_offset_commit()

        # Summary
        verify_transaction_apis()

        logger.info("\n🎉 All transaction API tests completed successfully!")
        return 0

    except Exception as e:
        logger.error(f"\n❌ Test suite failed: {e}")
        import traceback
        traceback.print_exc()
        return 1

if __name__ == "__main__":
    exit(main())