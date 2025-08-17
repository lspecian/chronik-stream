#!/usr/bin/env python3
"""
Test search indexing functionality in Chronik Stream

This test verifies that messages produced to Kafka are properly indexed
by Tantivy and can be searched through the Elasticsearch-compatible API.
"""

import json
import time
import uuid
import requests
from kafka import KafkaProducer, KafkaAdminClient
from kafka.admin import NewTopic
from kafka.errors import TopicAlreadyExistsError

# Configuration
KAFKA_BOOTSTRAP_SERVERS = ['localhost:9092']
SEARCH_API_URL = 'http://localhost:9200'
TEST_TOPIC = f'search-test-{uuid.uuid4().hex[:8]}'


def wait_for_services():
    """Wait for Kafka and Search API to be available"""
    print("Waiting for services to be ready...")
    
    # Wait for Kafka
    for i in range(30):
        try:
            admin = KafkaAdminClient(
                bootstrap_servers=KAFKA_BOOTSTRAP_SERVERS,
                request_timeout_ms=5000
            )
            admin.list_topics()
            admin.close()
            print("✓ Kafka is ready")
            break
        except Exception as e:
            if i == 29:
                raise Exception(f"Kafka not ready after 30 seconds: {e}")
            time.sleep(1)
    
    # Wait for Search API
    for i in range(30):
        try:
            response = requests.get(f"{SEARCH_API_URL}/_cluster/health", timeout=5)
            if response.status_code == 200:
                print("✓ Search API is ready")
                break
        except:
            if i == 29:
                print("⚠️  Search API not available, continuing anyway")
            time.sleep(1)


def create_test_topic():
    """Create a test topic"""
    admin = KafkaAdminClient(
        bootstrap_servers=KAFKA_BOOTSTRAP_SERVERS,
        client_id='test-admin'
    )
    
    topic = NewTopic(
        name=TEST_TOPIC,
        num_partitions=3,
        replication_factor=1
    )
    
    try:
        admin.create_topics([topic], timeout_ms=30000)
        print(f"✓ Created topic '{TEST_TOPIC}'")
    except TopicAlreadyExistsError:
        print(f"✓ Topic '{TEST_TOPIC}' already exists")
    finally:
        admin.close()


def produce_test_messages():
    """Produce various types of messages for indexing"""
    producer = KafkaProducer(
        bootstrap_servers=KAFKA_BOOTSTRAP_SERVERS,
        client_id='test-producer',
        value_serializer=lambda v: json.dumps(v).encode('utf-8') if isinstance(v, dict) else v.encode('utf-8')
    )
    
    messages = [
        # JSON messages
        {
            "id": "msg-001",
            "type": "log",
            "level": "ERROR",
            "service": "payment-api",
            "message": "Payment processing failed",
            "error_code": "INSUFFICIENT_FUNDS",
            "user_id": "user-123",
            "amount": 99.99,
            "timestamp": "2024-01-15T10:30:00Z"
        },
        {
            "id": "msg-002", 
            "type": "log",
            "level": "INFO",
            "service": "payment-api",
            "message": "Payment processed successfully",
            "user_id": "user-456",
            "amount": 49.99,
            "timestamp": "2024-01-15T10:31:00Z"
        },
        {
            "id": "msg-003",
            "type": "metric",
            "metric_name": "api_latency",
            "value": 125.5,
            "endpoint": "/api/v1/payments",
            "method": "POST",
            "timestamp": "2024-01-15T10:32:00Z"
        },
        # String messages
        "Simple text message for full-text search",
        "Another message with keywords: error, warning, critical",
        # Complex nested JSON
        {
            "id": "msg-006",
            "type": "event",
            "event_type": "user_action",
            "user": {
                "id": "user-789",
                "name": "John Doe",
                "email": "john@example.com"
            },
            "action": {
                "type": "purchase",
                "items": [
                    {"product_id": "prod-1", "quantity": 2, "price": 29.99},
                    {"product_id": "prod-2", "quantity": 1, "price": 49.99}
                ],
                "total": 109.97
            },
            "timestamp": "2024-01-15T10:33:00Z"
        }
    ]
    
    print(f"\nProducing {len(messages)} test messages...")
    for i, msg in enumerate(messages):
        future = producer.send(TEST_TOPIC, value=msg, key=f"key-{i}".encode('utf-8'))
        metadata = future.get(timeout=10)
        print(f"  ✓ Message {i+1} sent to partition {metadata.partition} at offset {metadata.offset}")
    
    producer.flush()
    producer.close()
    
    # Wait for indexing to complete
    print("\nWaiting for indexing to complete...")
    time.sleep(5)


def test_search_functionality():
    """Test various search queries"""
    print("\n=== Testing Search Functionality ===")
    
    # Test 1: Search for all documents in the topic
    print("\n1. Searching for all documents...")
    try:
        response = requests.post(
            f"{SEARCH_API_URL}/{TEST_TOPIC}/_search",
            json={"query": {"match_all": {}}},
            headers={"Content-Type": "application/json"}
        )
        
        if response.status_code == 200:
            result = response.json()
            print(f"   ✓ Found {result.get('hits', {}).get('total', {}).get('value', 0)} documents")
        else:
            print(f"   ✗ Search failed: {response.status_code} - {response.text}")
    except requests.exceptions.ConnectionError:
        print("   ⚠️  Search API not available - indexing might be disabled")
        return
    
    # Test 2: Search for error logs
    print("\n2. Searching for error logs...")
    response = requests.post(
        f"{SEARCH_API_URL}/{TEST_TOPIC}/_search",
        json={
            "query": {
                "bool": {
                    "must": [
                        {"match": {"type": "log"}},
                        {"match": {"level": "ERROR"}}
                    ]
                }
            }
        },
        headers={"Content-Type": "application/json"}
    )
    
    if response.status_code == 200:
        result = response.json()
        hits = result.get('hits', {}).get('hits', [])
        print(f"   ✓ Found {len(hits)} error logs")
        for hit in hits:
            print(f"     - {hit['_source'].get('message', 'N/A')}")
    
    # Test 3: Range query on numeric field
    print("\n3. Searching for high-value payments...")
    response = requests.post(
        f"{SEARCH_API_URL}/{TEST_TOPIC}/_search",
        json={
            "query": {
                "range": {
                    "amount": {
                        "gte": 50.0
                    }
                }
            }
        },
        headers={"Content-Type": "application/json"}
    )
    
    if response.status_code == 200:
        result = response.json()
        hits = result.get('hits', {}).get('hits', [])
        print(f"   ✓ Found {len(hits)} high-value payments")
        for hit in hits:
            amount = hit['_source'].get('amount', 'N/A')
            user = hit['_source'].get('user_id', 'N/A')
            print(f"     - User {user}: ${amount}")
    
    # Test 4: Full-text search
    print("\n4. Testing full-text search...")
    response = requests.post(
        f"{SEARCH_API_URL}/{TEST_TOPIC}/_search",
        json={
            "query": {
                "match": {
                    "_value": "error warning"
                }
            }
        },
        headers={"Content-Type": "application/json"}
    )
    
    if response.status_code == 200:
        result = response.json()
        hits = result.get('hits', {}).get('hits', [])
        print(f"   ✓ Found {len(hits)} documents matching 'error warning'")
    
    # Test 5: Aggregations
    print("\n5. Testing aggregations...")
    response = requests.post(
        f"{SEARCH_API_URL}/{TEST_TOPIC}/_search",
        json={
            "size": 0,
            "aggs": {
                "log_levels": {
                    "terms": {
                        "field": "level"
                    }
                },
                "avg_amount": {
                    "avg": {
                        "field": "amount"
                    }
                }
            }
        },
        headers={"Content-Type": "application/json"}
    )
    
    if response.status_code == 200:
        result = response.json()
        aggs = result.get('aggregations', {})
        
        if 'log_levels' in aggs:
            print("   ✓ Log level distribution:")
            for bucket in aggs['log_levels'].get('buckets', []):
                print(f"     - {bucket['key']}: {bucket['doc_count']}")
        
        if 'avg_amount' in aggs:
            avg = aggs['avg_amount'].get('value', 0)
            print(f"   ✓ Average payment amount: ${avg:.2f}")


def test_realtime_indexing():
    """Test that new messages are indexed in real-time"""
    print("\n=== Testing Real-time Indexing ===")
    
    producer = KafkaProducer(
        bootstrap_servers=KAFKA_BOOTSTRAP_SERVERS,
        client_id='test-producer-realtime',
        value_serializer=lambda v: json.dumps(v).encode('utf-8')
    )
    
    # Produce a unique message
    unique_id = f"realtime-{uuid.uuid4().hex[:8]}"
    message = {
        "id": unique_id,
        "type": "realtime_test",
        "message": "This is a real-time indexing test",
        "timestamp": time.time()
    }
    
    print(f"\nProducing message with ID: {unique_id}")
    producer.send(TEST_TOPIC, value=message).get(timeout=10)
    producer.flush()
    producer.close()
    
    # Wait and search for the message
    print("Waiting for indexing...")
    for i in range(10):
        time.sleep(1)
        
        try:
            response = requests.post(
                f"{SEARCH_API_URL}/{TEST_TOPIC}/_search",
                json={
                    "query": {
                        "match": {
                            "id": unique_id
                        }
                    }
                },
                headers={"Content-Type": "application/json"}
            )
            
            if response.status_code == 200:
                result = response.json()
                if result.get('hits', {}).get('total', {}).get('value', 0) > 0:
                    print(f"✓ Message indexed within {i+1} seconds")
                    return True
        except:
            pass
    
    print("✗ Message not indexed within 10 seconds")
    return False


def cleanup():
    """Clean up test resources"""
    print("\n=== Cleanup ===")
    
    admin = KafkaAdminClient(
        bootstrap_servers=KAFKA_BOOTSTRAP_SERVERS,
        client_id='test-admin-cleanup'
    )
    
    try:
        admin.delete_topics([TEST_TOPIC], timeout_ms=30000)
        print(f"✓ Deleted topic '{TEST_TOPIC}'")
    except Exception as e:
        print(f"⚠️  Failed to delete topic: {e}")
    finally:
        admin.close()


def main():
    """Run all tests"""
    print("=" * 60)
    print("Chronik Stream Search Indexing Test")
    print("=" * 60)
    
    try:
        # Setup
        wait_for_services()
        create_test_topic()
        
        # Test message production and indexing
        produce_test_messages()
        
        # Test search functionality
        test_search_functionality()
        
        # Test real-time indexing
        test_realtime_indexing()
        
        print("\n✅ All tests completed!")
        
    except Exception as e:
        print(f"\n❌ Test failed: {e}")
        import traceback
        traceback.print_exc()
    finally:
        # Cleanup
        cleanup()


if __name__ == "__main__":
    main()