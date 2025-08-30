#!/usr/bin/env python3
"""
Test Kafka protocol compatibility with multiple API versions
Verifies that all the wire protocol fixes are working correctly
"""

import struct
import socket
import time
import uuid
from kafka import KafkaProducer, KafkaConsumer, KafkaAdminClient
from kafka.admin import NewTopic
from kafka.errors import KafkaError
import sys

def test_raw_protocol(host='localhost', port=9092):
    """Test raw protocol handling with correlation IDs"""
    print("\n=== Testing Raw Protocol (Correlation IDs) ===")
    
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.connect((host, port))
    sock.settimeout(5.0)
    
    try:
        # Test ApiVersions request (API key 18)
        correlation_id = 12345
        client_id = "test-client"
        
        # Build ApiVersions request v0
        request = struct.pack('>h', 18)  # API key
        request += struct.pack('>h', 0)  # API version
        request += struct.pack('>i', correlation_id)  # Correlation ID
        # Client ID (nullable string)
        client_id_bytes = client_id.encode('utf-8')
        request += struct.pack('>h', len(client_id_bytes))
        request += client_id_bytes
        
        # Send request with length prefix
        full_request = struct.pack('>i', len(request)) + request
        sock.send(full_request)
        
        # Read response
        response_length_bytes = sock.recv(4)
        if len(response_length_bytes) == 4:
            response_length = struct.unpack('>i', response_length_bytes)[0]
            response = sock.recv(response_length)
            
            # Check correlation ID in response
            resp_correlation_id = struct.unpack('>i', response[:4])[0]
            if resp_correlation_id == correlation_id:
                print(f"✅ Correlation ID handling correct: {correlation_id}")
            else:
                print(f"❌ Correlation ID mismatch: expected {correlation_id}, got {resp_correlation_id}")
                return False
                
            # Parse API versions array
            # For v0: array comes first (before error_code)
            pos = 4  # Skip correlation ID
            api_count = struct.unpack('>i', response[pos:pos+4])[0]  # 4-byte array length
            print(f"  Server supports {api_count} APIs")
            
            if api_count > 0 and api_count < 100:  # Sanity check
                print("✅ ApiVersions response parsing successful")
                return True
            else:
                print(f"❌ Unexpected API count: {api_count}")
                return False
        else:
            print("❌ Failed to read response")
            return False
            
    except Exception as e:
        print(f"❌ Raw protocol test failed: {e}")
        return False
    finally:
        sock.close()

def test_metadata_versions(host='localhost', port=9092):
    """Test metadata request with different versions"""
    print("\n=== Testing Metadata API Versions ===")
    
    results = []
    
    for version in [0, 1, 5, 9]:  # Test different versions
        sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        sock.connect((host, port))
        sock.settimeout(5.0)
        
        try:
            correlation_id = 1000 + version
            
            # Build Metadata request
            request = struct.pack('>h', 3)  # API key for Metadata
            request += struct.pack('>h', version)  # API version
            request += struct.pack('>i', correlation_id)  # Correlation ID
            request += struct.pack('>h', 9)  # Client ID length
            request += b'test-meta'  # Client ID
            
            # For v9+, add tagged fields (empty)
            if version >= 9:
                request += b'\x00'  # Empty tagged fields
            
            # Topics array (null = all topics)
            request += struct.pack('>i', -1)  # Null array
            
            # Additional fields for newer versions
            if version >= 4:
                request += struct.pack('>b', 1)  # allow_auto_topic_creation
            if version >= 8:
                request += struct.pack('>b', 1)  # include_cluster_authorized_operations
                request += struct.pack('>b', 1)  # include_topic_authorized_operations
            
            # Send request
            full_request = struct.pack('>i', len(request)) + request
            sock.send(full_request)
            
            # Read response
            response_length_bytes = sock.recv(4)
            if len(response_length_bytes) == 4:
                response_length = struct.unpack('>i', response_length_bytes)[0]
                response = sock.recv(response_length)
                
                # Verify correlation ID
                resp_correlation_id = struct.unpack('>i', response[:4])[0]
                if resp_correlation_id == correlation_id:
                    print(f"  ✅ Metadata v{version} correlation ID correct")
                    results.append(True)
                else:
                    print(f"  ❌ Metadata v{version} correlation ID mismatch")
                    results.append(False)
            else:
                print(f"  ❌ Metadata v{version} failed to read response")
                results.append(False)
                
        except Exception as e:
            print(f"  ❌ Metadata v{version} failed: {e}")
            results.append(False)
        finally:
            sock.close()
    
    return all(results)

def test_produce_with_crc():
    """Test produce/fetch with CRC validation"""
    print("\n=== Testing Produce/Fetch with CRC ===")
    
    try:
        topic = f"crc-test-{uuid.uuid4().hex[:8]}"
        
        # Create producer with no compression first
        producer = KafkaProducer(
            bootstrap_servers='localhost:9092',
            compression_type=None,
            max_block_ms=5000
        )
        
        # Send a test message
        test_msg = b"CRC validation test message"
        future = producer.send(topic, test_msg, key=b"test-key")
        metadata = future.get(timeout=10)
        producer.flush()
        producer.close()
        
        print(f"  Produced to {topic}:{metadata.partition}@{metadata.offset}")
        
        # Consume and verify
        consumer = KafkaConsumer(
            topic,
            bootstrap_servers='localhost:9092',
            auto_offset_reset='earliest',
            consumer_timeout_ms=5000
        )
        
        messages_received = []
        for message in consumer:
            messages_received.append(message)
            if message.value == test_msg and message.key == b"test-key":
                print(f"  ✅ Message consumed successfully with correct content")
            else:
                print(f"  ❌ Message content mismatch")
                return False
        
        consumer.close()
        
        if messages_received:
            print("✅ CRC validation passed (no CRC errors reported)")
            return True
        else:
            print("❌ No messages received")
            return False
            
    except Exception as e:
        print(f"❌ CRC test failed: {e}")
        return False

def test_compression_types():
    """Test different compression types"""
    print("\n=== Testing Compression Types ===")
    
    compression_types = [None, 'gzip', 'snappy', 'lz4']
    results = []
    
    for comp_type in compression_types:
        try:
            topic = f"comp-{comp_type or 'none'}-{uuid.uuid4().hex[:8]}"
            
            producer = KafkaProducer(
                bootstrap_servers='localhost:9092',
                compression_type=comp_type,
                max_block_ms=5000
            )
            
            # Send multiple messages to trigger compression
            messages = [f"Message {i} with {comp_type or 'no'} compression" for i in range(10)]
            for msg in messages:
                producer.send(topic, msg.encode())
            producer.flush()
            producer.close()
            
            # Consume back
            consumer = KafkaConsumer(
                topic,
                bootstrap_servers='localhost:9092',
                auto_offset_reset='earliest',
                consumer_timeout_ms=3000
            )
            
            received = []
            for message in consumer:
                received.append(message.value.decode())
            
            consumer.close()
            
            if len(received) == len(messages):
                print(f"  ✅ {comp_type or 'none'}: All messages received")
                results.append(True)
            else:
                print(f"  ❌ {comp_type or 'none'}: Got {len(received)}/{len(messages)} messages")
                results.append(False)
                
        except Exception as e:
            print(f"  ❌ {comp_type or 'none'} failed: {e}")
            results.append(False)
    
    return all(results)

def test_consumer_group_protocol():
    """Test consumer group protocol (FindCoordinator, JoinGroup, etc.)"""
    print("\n=== Testing Consumer Group Protocol ===")
    
    try:
        topic = f"group-test-{uuid.uuid4().hex[:8]}"
        group_id = f"test-group-{uuid.uuid4().hex[:8]}"
        
        # Produce test messages
        producer = KafkaProducer(bootstrap_servers='localhost:9092')
        for i in range(10):
            producer.send(topic, f"msg-{i}".encode())
        producer.flush()
        producer.close()
        
        # Test consumer group operations
        consumer = KafkaConsumer(
            topic,
            group_id=group_id,
            bootstrap_servers='localhost:9092',
            auto_offset_reset='earliest',
            enable_auto_commit=True,
            consumer_timeout_ms=5000
        )
        
        messages = []
        for msg in consumer:
            messages.append(msg)
            if len(messages) >= 5:
                # Commit offsets after 5 messages
                consumer.commit()
                break
        
        # Check committed offsets
        from kafka import TopicPartition
        partitions = consumer.assignment()
        if partitions:
            tp = list(partitions)[0]
            committed = consumer.committed(tp)
            print(f"  Committed offset: {committed}")
        
        consumer.close()
        
        # Start new consumer in same group - should continue from offset
        consumer2 = KafkaConsumer(
            topic,
            group_id=group_id,
            bootstrap_servers='localhost:9092',
            auto_offset_reset='earliest',
            consumer_timeout_ms=3000
        )
        
        messages2 = []
        for msg in consumer2:
            messages2.append(msg)
        
        consumer2.close()
        
        total_messages = len(messages) + len(messages2)
        if total_messages == 10:
            print(f"✅ Consumer group protocol working: {len(messages)} + {len(messages2)} = 10 messages")
            return True
        else:
            print(f"❌ Consumer group issue: got {total_messages} messages total")
            return False
            
    except Exception as e:
        print(f"❌ Consumer group test failed: {e}")
        return False

def test_client_compatibility():
    """Test with different client configurations"""
    print("\n=== Testing Client Compatibility ===")
    
    try:
        # Test with admin client (uses newer protocol versions)
        admin = KafkaAdminClient(
            bootstrap_servers='localhost:9092',
            client_id='test-admin'
        )
        
        # List topics
        topics = admin.list_topics()
        print(f"  Admin client: Found {len(topics)} topics")
        
        # Create a topic using admin API
        test_topic = f"admin-created-{uuid.uuid4().hex[:8]}"
        new_topic = NewTopic(name=test_topic, num_partitions=1, replication_factor=1)
        
        try:
            admin.create_topics([new_topic], timeout_ms=5000)
            print(f"  ✅ Topic created via admin API: {test_topic}")
        except Exception as e:
            print(f"  ⚠️  Topic creation issue: {e}")
        
        admin.close()
        
        # Test with different client versions/settings
        configs = [
            {'api_version': (0, 10, 0)},  # Older client
            {'api_version': (2, 0, 0)},   # Newer client
            {'api_version': None},        # Auto-detect
        ]
        
        results = []
        for config in configs:
            try:
                topic = f"compat-test-{uuid.uuid4().hex[:8]}"
                
                # Create producer with specific API version
                producer = KafkaProducer(
                    bootstrap_servers='localhost:9092',
                    **config
                )
                
                future = producer.send(topic, b"compatibility test")
                metadata = future.get(timeout=5)
                producer.close()
                
                print(f"  ✅ Compatible with config: {config}")
                results.append(True)
                
            except Exception as e:
                print(f"  ❌ Failed with config {config}: {e}")
                results.append(False)
        
        return all(results) if results else False
        
    except Exception as e:
        print(f"❌ Client compatibility test failed: {e}")
        return False

def main():
    print("=" * 60)
    print("KAFKA PROTOCOL COMPATIBILITY TEST SUITE")
    print("=" * 60)
    print("\nThis test verifies all wire protocol fixes are working")
    print("Make sure chronik-ingest is running on localhost:9092")
    
    time.sleep(2)
    
    # Run all protocol tests
    results = []
    
    results.append(("Raw Protocol (Correlation IDs)", test_raw_protocol()))
    results.append(("Metadata API Versions", test_metadata_versions()))
    results.append(("Produce/Fetch with CRC", test_produce_with_crc()))
    results.append(("Compression Types", test_compression_types()))
    results.append(("Consumer Group Protocol", test_consumer_group_protocol()))
    results.append(("Client Compatibility", test_client_compatibility()))
    
    # Print summary
    print("\n" + "=" * 60)
    print("PROTOCOL COMPATIBILITY SUMMARY")
    print("=" * 60)
    
    passed = sum(1 for _, result in results if result)
    total = len(results)
    
    for name, result in results:
        status = "✅ PASS" if result else "❌ FAIL"
        print(f"{status}: {name}")
    
    print(f"\nTotal: {passed}/{total} tests passed")
    
    if passed == total:
        print("\n🎉 ALL PROTOCOL COMPATIBILITY TESTS PASSED!")
        print("The Kafka wire protocol implementation is working correctly.")
    else:
        print(f"\n⚠️  {total - passed} compatibility issues detected")
        print("Some protocol features may not work with all clients.")
    
    return 0 if passed == total else 1

if __name__ == "__main__":
    sys.exit(main())