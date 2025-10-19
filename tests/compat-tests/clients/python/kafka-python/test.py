#!/usr/bin/env python3
"""
Kafka-python compatibility test for Chronik Stream
Tests basic Kafka operations and reports results
"""

import json
import os
import sys
import time
import uuid
from datetime import datetime

def log(level, message):
    """Structured logging for result parsing"""
    timestamp = datetime.utcnow().isoformat()
    print(json.dumps({
        "timestamp": timestamp,
        "level": level,
        "client": "kafka-python",
        "message": message
    }))
    sys.stdout.flush()

def test_connection(bootstrap_servers):
    """Test basic connection and ApiVersions"""
    try:
        from kafka import KafkaProducer, KafkaConsumer
        from kafka.errors import KafkaError
        
        # Test ApiVersions by creating a producer
        producer = KafkaProducer(
            bootstrap_servers=bootstrap_servers,
            request_timeout_ms=5000,
            api_version_auto_timeout_ms=5000
        )
        
        # Force API version negotiation
        producer.bootstrap_connected()
        producer.close()
        
        log("INFO", "ApiVersions test PASSED")
        return True, "ApiVersions"
        
    except Exception as e:
        log("ERROR", f"ApiVersions test FAILED: {str(e)}")
        return False, "ApiVersions"

def test_metadata(bootstrap_servers):
    """Test Metadata request"""
    try:
        from kafka import KafkaConsumer
        
        consumer = KafkaConsumer(
            bootstrap_servers=bootstrap_servers,
            consumer_timeout_ms=5000
        )
        
        # Get cluster metadata
        metadata = consumer.bootstrap_connected()
        topics = consumer.topics()
        
        consumer.close()
        
        log("INFO", f"Metadata test PASSED - found {len(topics) if topics else 0} topics")
        return True, "Metadata"
        
    except Exception as e:
        log("ERROR", f"Metadata test FAILED: {str(e)}")
        return False, "Metadata"

def test_produce(bootstrap_servers):
    """Test Produce operation"""
    try:
        from kafka import KafkaProducer
        
        topic = f"test-{uuid.uuid4().hex[:8]}"
        
        producer = KafkaProducer(
            bootstrap_servers=bootstrap_servers,
            request_timeout_ms=10000,
            api_version=(0, 10, 1),  # Force v2 for ProduceRequest
            value_serializer=lambda v: json.dumps(v).encode('utf-8')
        )
        
        # Send test message
        test_data = {
            "client": "kafka-python",
            "timestamp": time.time(),
            "test": "produce"
        }
        
        future = producer.send(topic, test_data)
        
        # Wait for send to complete
        try:
            metadata = future.get(timeout=10)
            log("INFO", f"Produce test PASSED - offset: {metadata.offset}, partition: {metadata.partition}")
            producer.close()
            return True, "Produce"
        except Exception as send_error:
            log("ERROR", f"Produce send failed: {str(send_error)}")
            producer.close()
            return False, "Produce"
            
    except Exception as e:
        log("ERROR", f"Produce test FAILED: {str(e)}")
        return False, "Produce"

def test_fetch(bootstrap_servers):
    """Test Fetch operation (produce then consume)"""
    try:
        from kafka import KafkaProducer, KafkaConsumer
        
        topic = f"test-fetch-{uuid.uuid4().hex[:8]}"
        
        # First produce a message
        producer = KafkaProducer(
            bootstrap_servers=bootstrap_servers,
            value_serializer=lambda v: json.dumps(v).encode('utf-8')
        )
        
        test_data = {"test": "fetch", "id": str(uuid.uuid4())}
        future = producer.send(topic, test_data)
        metadata = future.get(timeout=5)
        producer.close()
        
        # Now try to fetch it
        consumer = KafkaConsumer(
            topic,
            bootstrap_servers=bootstrap_servers,
            auto_offset_reset='earliest',
            consumer_timeout_ms=5000,
            value_deserializer=lambda m: json.loads(m.decode('utf-8'))
        )
        
        messages_found = False
        for message in consumer:
            if message.value.get("id") == test_data["id"]:
                messages_found = True
                break
        
        consumer.close()
        
        if messages_found:
            log("INFO", "Fetch test PASSED")
            return True, "Fetch"
        else:
            log("ERROR", "Fetch test FAILED - message not found")
            return False, "Fetch"
            
    except Exception as e:
        log("ERROR", f"Fetch test FAILED: {str(e)}")
        return False, "Fetch"

def test_consumer_group(bootstrap_servers):
    """Test consumer group operations"""
    try:
        from kafka import KafkaProducer, KafkaConsumer
        
        topic = f"test-group-{uuid.uuid4().hex[:8]}"
        group_id = f"test-group-{uuid.uuid4().hex[:8]}"
        
        # Produce messages
        producer = KafkaProducer(
            bootstrap_servers=bootstrap_servers,
            value_serializer=lambda v: json.dumps(v).encode('utf-8')
        )
        
        for i in range(5):
            producer.send(topic, {"msg": i})
        producer.flush()
        producer.close()
        
        # Consume with group
        consumer = KafkaConsumer(
            topic,
            bootstrap_servers=bootstrap_servers,
            group_id=group_id,
            auto_offset_reset='earliest',
            enable_auto_commit=True,
            consumer_timeout_ms=5000
        )
        
        msg_count = 0
        for message in consumer:
            msg_count += 1
            if msg_count >= 5:
                break
        
        consumer.commit()
        consumer.close()
        
        if msg_count >= 5:
            log("INFO", f"ConsumerGroup test PASSED - consumed {msg_count} messages")
            return True, "ConsumerGroup"
        else:
            log("ERROR", f"ConsumerGroup test FAILED - only got {msg_count} messages")
            return False, "ConsumerGroup"
            
    except Exception as e:
        log("ERROR", f"ConsumerGroup test FAILED: {str(e)}")
        return False, "ConsumerGroup"

def main():
    """Run all tests and report results"""
    bootstrap_servers = os.getenv('BOOTSTRAP_SERVERS', 'chronik:9092')
    
    log("INFO", f"Starting kafka-python tests against {bootstrap_servers}")
    
    # Check if kafka-python is installed
    try:
        import kafka
        version = kafka.__version__
        log("INFO", f"Using kafka-python version {version}")
    except ImportError:
        log("ERROR", "kafka-python not installed")
        sys.exit(1)
    
    # Run tests
    tests = [
        ("ApiVersions", test_connection),
        ("Metadata", test_metadata),
        ("Produce", test_produce),
        ("Fetch", test_fetch),
        ("ConsumerGroup", test_consumer_group)
    ]
    
    results = []
    for test_name, test_func in tests:
        log("INFO", f"Running {test_name} test...")
        try:
            passed, api = test_func(bootstrap_servers)
            results.append({
                "test": test_name,
                "api": api,
                "passed": passed,
                "client": "kafka-python"
            })
        except Exception as e:
            log("ERROR", f"Test {test_name} crashed: {str(e)}")
            results.append({
                "test": test_name,
                "api": test_name,
                "passed": False,
                "client": "kafka-python"
            })
    
    # Summary
    passed_count = sum(1 for r in results if r["passed"])
    total_count = len(results)
    
    log("INFO", f"Test summary: {passed_count}/{total_count} passed")
    
    # Output results in JSON format (use /tmp for local testing)
    if os.path.exists("/tmp") and os.access("/tmp", os.W_OK):
        result_file = "/tmp/kafka-python-results.json"
    else:
        result_file = "/results/kafka-python-results.json"
        os.makedirs(os.path.dirname(result_file), exist_ok=True)
    
    with open(result_file, 'w') as f:
        json.dump({
            "client": "kafka-python",
            "version": version,
            "timestamp": datetime.utcnow().isoformat(),
            "tests": results,
            "summary": {
                "total": total_count,
                "passed": passed_count,
                "failed": total_count - passed_count
            }
        }, f, indent=2)
    
    log("INFO", f"Results written to {result_file}")
    
    # Exit with error if any test failed
    sys.exit(0 if passed_count == total_count else 1)

if __name__ == "__main__":
    main()