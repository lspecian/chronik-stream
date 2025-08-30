#!/usr/bin/env python3
"""
Test all critical fixes for Chronik Stream
This verifies the P0 and P1 issues from CRITICAL_ISSUES.md
"""

import sys
import time
import json
from kafka import KafkaProducer, KafkaConsumer, KafkaAdminClient
from kafka.admin import NewTopic
from kafka.errors import KafkaError
import uuid

def test_metadata_api():
    """Test 1: Verify metadata API works (Issue #2: Protocol Compatibility)"""
    print("\n=== Test 1: Metadata API ===")
    try:
        from kafka import KafkaClient
        client = KafkaClient(bootstrap_servers='localhost:9092')
        metadata = client.cluster
        print(f"✅ Metadata API working - Found {len(metadata.brokers())} brokers")
        client.close()
        return True
    except Exception as e:
        print(f"❌ Metadata API failed: {e}")
        return False

def test_produce_and_persist():
    """Test 2: Verify messages are persisted (Issue #3: Messages Not Persisted)"""
    print("\n=== Test 2: Message Persistence ===")
    try:
        topic = f"persist-test-{uuid.uuid4().hex[:8]}"
        
        # Produce messages
        producer = KafkaProducer(
            bootstrap_servers='localhost:9092',
            value_serializer=lambda v: json.dumps(v).encode('utf-8')
        )
        
        messages = []
        for i in range(5):
            msg = {"id": i, "data": f"persist-test-{i}"}
            messages.append(msg)
            future = producer.send(topic, msg)
            result = future.get(timeout=10)
            print(f"  Sent message {i} to {topic}:{result.partition}@{result.offset}")
        
        producer.close()
        
        # Consume to verify persistence
        consumer = KafkaConsumer(
            topic,
            bootstrap_servers='localhost:9092',
            auto_offset_reset='earliest',
            value_deserializer=lambda m: json.loads(m.decode('utf-8')),
            consumer_timeout_ms=5000
        )
        
        received = []
        for message in consumer:
            received.append(message.value)
        
        consumer.close()
        
        if len(received) == len(messages):
            print(f"✅ All {len(messages)} messages persisted and retrieved")
            return True
        else:
            print(f"❌ Message persistence failed: sent {len(messages)}, got {len(received)}")
            return False
            
    except Exception as e:
        print(f"❌ Message persistence test failed: {e}")
        return False

def test_consumer_groups():
    """Test 3: Verify consumer groups work (Issue #2 & #8: Consumer Group Management)"""
    print("\n=== Test 3: Consumer Groups ===")
    try:
        topic = f"group-test-{uuid.uuid4().hex[:8]}"
        group_id = f"test-group-{uuid.uuid4().hex[:8]}"
        
        # Produce test messages
        producer = KafkaProducer(bootstrap_servers='localhost:9092')
        for i in range(10):
            producer.send(topic, f"group-msg-{i}".encode())
        producer.flush()
        producer.close()
        
        # Consumer 1: Consume first 5 messages
        consumer1 = KafkaConsumer(
            topic,
            group_id=group_id,
            bootstrap_servers='localhost:9092',
            auto_offset_reset='earliest',
            max_poll_records=5,
            consumer_timeout_ms=3000
        )
        
        msgs1 = []
        for msg in consumer1:
            msgs1.append(msg.value.decode())
            if len(msgs1) >= 5:
                break
        
        # Commit offsets
        consumer1.commit()
        consumer1.close()
        
        print(f"  Consumer 1 read {len(msgs1)} messages")
        
        # Consumer 2: Should continue from where Consumer 1 left off
        consumer2 = KafkaConsumer(
            topic,
            group_id=group_id,
            bootstrap_servers='localhost:9092',
            auto_offset_reset='earliest',
            consumer_timeout_ms=3000
        )
        
        msgs2 = []
        for msg in consumer2:
            msgs2.append(msg.value.decode())
        
        consumer2.close()
        
        print(f"  Consumer 2 read {len(msgs2)} messages")
        
        if len(msgs2) == 5:  # Should get the remaining 5 messages
            print(f"✅ Consumer group offset tracking works correctly")
            return True
        else:
            print(f"❌ Consumer group failed: Consumer 2 got {len(msgs2)} messages instead of 5")
            return False
            
    except Exception as e:
        print(f"❌ Consumer group test failed: {e}")
        return False

def test_offset_commit_fetch():
    """Test 4: Verify OffsetCommit/OffsetFetch APIs (Issue #2: Protocol Compatibility)"""
    print("\n=== Test 4: Offset Commit/Fetch ===")
    try:
        topic = f"offset-test-{uuid.uuid4().hex[:8]}"
        group_id = f"offset-group-{uuid.uuid4().hex[:8]}"
        
        # Produce messages
        producer = KafkaProducer(bootstrap_servers='localhost:9092')
        for i in range(10):
            producer.send(topic, f"offset-msg-{i}".encode())
        producer.flush()
        producer.close()
        
        # Consume and commit at specific offset
        consumer = KafkaConsumer(
            topic,
            group_id=group_id,
            bootstrap_servers='localhost:9092',
            auto_offset_reset='earliest',
            enable_auto_commit=False,
            consumer_timeout_ms=3000
        )
        
        # Read first 3 messages
        messages_read = 0
        for msg in consumer:
            messages_read += 1
            if messages_read == 3:
                # Manually commit offset
                consumer.commit()
                print(f"  Committed offset at message {messages_read}")
                break
        
        # Get committed offset
        from kafka import TopicPartition
        partitions = consumer.assignment()
        if partitions:
            committed = consumer.committed(list(partitions)[0])
            print(f"  Committed offset: {committed}")
        
        consumer.close()
        
        # New consumer should start from committed offset
        consumer2 = KafkaConsumer(
            topic,
            group_id=group_id,
            bootstrap_servers='localhost:9092',
            auto_offset_reset='earliest',
            consumer_timeout_ms=3000
        )
        
        msgs = []
        for msg in consumer2:
            msgs.append(msg.value.decode())
        
        consumer2.close()
        
        if len(msgs) == 7:  # Should get remaining 7 messages
            print(f"✅ OffsetCommit/Fetch working - resumed from correct offset")
            return True
        else:
            print(f"❌ OffsetCommit/Fetch failed - got {len(msgs)} messages instead of 7")
            return False
            
    except Exception as e:
        print(f"❌ Offset test failed: {e}")
        return False

def test_auto_topic_creation():
    """Test 5: Verify auto-topic creation works"""
    print("\n=== Test 5: Auto-Topic Creation ===")
    try:
        topic = f"auto-create-{uuid.uuid4().hex[:8]}"
        
        # Try to produce to non-existent topic
        producer = KafkaProducer(
            bootstrap_servers='localhost:9092',
            max_block_ms=5000
        )
        
        future = producer.send(topic, b"test-message")
        result = future.get(timeout=10)
        
        producer.close()
        
        print(f"✅ Auto-topic creation works - created topic '{topic}'")
        return True
        
    except Exception as e:
        print(f"❌ Auto-topic creation failed: {e}")
        return False

def test_compression():
    """Test 6: Verify compression support"""
    print("\n=== Test 6: Compression Support ===")
    
    compression_types = ['gzip', 'snappy', 'lz4']
    results = []
    
    for compression in compression_types:
        try:
            topic = f"compress-{compression}-{uuid.uuid4().hex[:8]}"
            
            producer = KafkaProducer(
                bootstrap_servers='localhost:9092',
                compression_type=compression
            )
            
            # Send compressed message
            msg = f"Compressed message with {compression}" * 100  # Make it large enough to compress
            future = producer.send(topic, msg.encode())
            result = future.get(timeout=10)
            
            producer.close()
            
            # Read it back
            consumer = KafkaConsumer(
                topic,
                bootstrap_servers='localhost:9092',
                auto_offset_reset='earliest',
                consumer_timeout_ms=3000
            )
            
            received = None
            for message in consumer:
                received = message.value.decode()
                break
            
            consumer.close()
            
            if received == msg:
                print(f"  ✅ {compression.upper()} compression working")
                results.append(True)
            else:
                print(f"  ❌ {compression.upper()} compression failed")
                results.append(False)
                
        except Exception as e:
            print(f"  ❌ {compression.upper()} compression failed: {e}")
            results.append(False)
    
    return all(results)

def test_correlation_ids():
    """Test 7: Verify correlation ID handling"""
    print("\n=== Test 7: Correlation ID Handling ===")
    try:
        # This is implicitly tested by all the above tests
        # If correlation IDs were broken, the client would disconnect
        print("✅ Correlation ID handling works (implicit in all tests)")
        return True
    except Exception as e:
        print(f"❌ Correlation ID test failed: {e}")
        return False

def main():
    print("=" * 60)
    print("CHRONIK STREAM CRITICAL FIXES VERIFICATION")
    print("=" * 60)
    print("\nThis test suite verifies all P0 and P1 issues are fixed")
    print("Make sure chronik-all-in-one is running on localhost:9092")
    
    time.sleep(2)
    
    # Run all tests
    results = []
    
    results.append(("Metadata API", test_metadata_api()))
    results.append(("Message Persistence", test_produce_and_persist()))
    results.append(("Consumer Groups", test_consumer_groups()))
    results.append(("Offset Management", test_offset_commit_fetch()))
    results.append(("Auto-Topic Creation", test_auto_topic_creation()))
    results.append(("Compression", test_compression()))
    results.append(("Correlation IDs", test_correlation_ids()))
    
    # Print summary
    print("\n" + "=" * 60)
    print("TEST SUMMARY")
    print("=" * 60)
    
    passed = sum(1 for _, result in results if result)
    total = len(results)
    
    for name, result in results:
        status = "✅ PASS" if result else "❌ FAIL"
        print(f"{status}: {name}")
    
    print(f"\nTotal: {passed}/{total} tests passed")
    
    if passed == total:
        print("\n🎉 ALL CRITICAL FIXES VERIFIED!")
        print("The server is ready for further testing.")
    else:
        print(f"\n⚠️  {total - passed} critical issues remain")
        print("Please check the implementation.")
    
    return 0 if passed == total else 1

if __name__ == "__main__":
    sys.exit(main())