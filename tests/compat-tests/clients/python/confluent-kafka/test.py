#!/usr/bin/env python3
"""
Confluent-kafka (librdkafka) compatibility test for Chronik Stream
Tests basic Kafka operations with librdkafka and reports results
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
        "client": "confluent-kafka",
        "message": message
    }))
    sys.stdout.flush()

def test_connection(bootstrap_servers):
    """Test basic connection and ApiVersions"""
    try:
        from confluent_kafka import Producer
        
        config = {
            'bootstrap.servers': bootstrap_servers,
            'socket.timeout.ms': 5000,
            'api.version.request': True,
            'api.version.request.timeout.ms': 5000,
            'debug': 'broker,protocol'  # Enable debug for librdkafka issues
        }
        
        producer = Producer(config)
        
        # Test metadata (which triggers ApiVersions)
        metadata = producer.list_topics(timeout=5)
        
        if metadata.brokers:
            log("INFO", f"ApiVersions test PASSED - connected to {len(metadata.brokers)} brokers")
            return True, "ApiVersions"
        else:
            log("ERROR", "ApiVersions test FAILED - no brokers found")
            return False, "ApiVersions"
            
    except Exception as e:
        log("ERROR", f"ApiVersions test FAILED: {str(e)}")
        # Try fallback to ApiVersions v0
        try:
            config['api.version.fallback.ms'] = 0
            producer = Producer(config)
            metadata = producer.list_topics(timeout=5)
            log("INFO", "ApiVersions test PASSED with v0 fallback")
            return True, "ApiVersions"
        except Exception as fallback_error:
            log("ERROR", f"ApiVersions v0 fallback also failed: {str(fallback_error)}")
            return False, "ApiVersions"

def test_metadata(bootstrap_servers):
    """Test Metadata request"""
    try:
        from confluent_kafka import Producer
        
        producer = Producer({
            'bootstrap.servers': bootstrap_servers,
            'socket.timeout.ms': 5000
        })
        
        metadata = producer.list_topics(timeout=5)
        
        log("INFO", f"Metadata test PASSED - {len(metadata.topics)} topics, {len(metadata.brokers)} brokers")
        
        # Log broker details
        for broker in metadata.brokers.values():
            log("DEBUG", f"Broker {broker.id}: {broker.host}:{broker.port}")
        
        return True, "Metadata"
        
    except Exception as e:
        log("ERROR", f"Metadata test FAILED: {str(e)}")
        return False, "Metadata"

def test_produce(bootstrap_servers):
    """Test Produce operation"""
    try:
        from confluent_kafka import Producer
        
        topic = f"test-{uuid.uuid4().hex[:8]}"
        
        delivered = []
        
        def delivery_report(err, msg):
            if err is not None:
                log("ERROR", f"Delivery failed: {err}")
            else:
                delivered.append({
                    "topic": msg.topic(),
                    "partition": msg.partition(),
                    "offset": msg.offset()
                })
        
        producer = Producer({
            'bootstrap.servers': bootstrap_servers,
            'socket.timeout.ms': 10000,
            'message.timeout.ms': 10000,
            'client.id': 'confluent-test',
            'api.version.request': True
        })
        
        # Send test message
        test_data = json.dumps({
            "client": "confluent-kafka",
            "timestamp": time.time(),
            "test": "produce"
        })
        
        producer.produce(
            topic,
            value=test_data.encode('utf-8'),
            callback=delivery_report
        )
        
        # Wait for delivery
        producer.flush(timeout=10)
        
        if delivered:
            msg = delivered[0]
            log("INFO", f"Produce test PASSED - offset: {msg['offset']}, partition: {msg['partition']}")
            return True, "Produce"
        else:
            log("ERROR", "Produce test FAILED - no delivery confirmation")
            return False, "Produce"
            
    except Exception as e:
        log("ERROR", f"Produce test FAILED: {str(e)}")
        return False, "Produce"

def test_fetch(bootstrap_servers):
    """Test Fetch operation (produce then consume)"""
    try:
        from confluent_kafka import Producer, Consumer
        
        topic = f"test-fetch-{uuid.uuid4().hex[:8]}"
        test_id = str(uuid.uuid4())
        
        # First produce a message
        producer = Producer({
            'bootstrap.servers': bootstrap_servers
        })
        
        test_data = json.dumps({"test": "fetch", "id": test_id})
        producer.produce(topic, value=test_data.encode('utf-8'))
        producer.flush(timeout=5)
        
        # Now try to fetch it
        consumer = Consumer({
            'bootstrap.servers': bootstrap_servers,
            'group.id': f'test-{uuid.uuid4().hex[:8]}',
            'auto.offset.reset': 'earliest',
            'enable.auto.commit': False
        })
        
        consumer.subscribe([topic])
        
        messages_found = False
        timeout = time.time() + 10
        
        while time.time() < timeout:
            msg = consumer.poll(1.0)
            
            if msg is None:
                continue
            if msg.error():
                log("ERROR", f"Consumer error: {msg.error()}")
                continue
            
            try:
                value = json.loads(msg.value().decode('utf-8'))
                if value.get("id") == test_id:
                    messages_found = True
                    break
            except:
                pass
        
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
        from confluent_kafka import Producer, Consumer
        
        topic = f"test-group-{uuid.uuid4().hex[:8]}"
        group_id = f"test-group-{uuid.uuid4().hex[:8]}"
        
        # Produce messages
        producer = Producer({
            'bootstrap.servers': bootstrap_servers
        })
        
        for i in range(5):
            producer.produce(topic, value=json.dumps({"msg": i}).encode('utf-8'))
        producer.flush()
        
        # Consume with group
        consumer = Consumer({
            'bootstrap.servers': bootstrap_servers,
            'group.id': group_id,
            'auto.offset.reset': 'earliest',
            'enable.auto.commit': True,
            'auto.commit.interval.ms': 1000
        })
        
        consumer.subscribe([topic])
        
        msg_count = 0
        timeout = time.time() + 10
        
        while time.time() < timeout and msg_count < 5:
            msg = consumer.poll(1.0)
            if msg and not msg.error():
                msg_count += 1
        
        # Commit offsets
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

def test_produce_v2_regression(bootstrap_servers):
    """Test ProduceResponse v2 with throttle_time_ms positioning"""
    try:
        from confluent_kafka import Producer
        
        topic = f"test-v2-{uuid.uuid4().hex[:8]}"
        
        delivered = []
        
        def delivery_report(err, msg):
            if err is not None:
                log("ERROR", f"V2 Delivery failed: {err}")
            else:
                delivered.append(True)
        
        # Force specific API version
        producer = Producer({
            'bootstrap.servers': bootstrap_servers,
            'socket.timeout.ms': 10000,
            'message.timeout.ms': 10000,
            'api.version.request': True,
            'broker.version.fallback': '0.10.0',  # Forces ProduceRequest v2
        })
        
        producer.produce(
            topic,
            value=b"test-v2-regression",
            callback=delivery_report
        )
        
        # This would timeout with the bug
        producer.flush(timeout=10)
        
        if delivered:
            log("INFO", "ProduceV2 regression test PASSED")
            return True, "ProduceV2"
        else:
            log("ERROR", "ProduceV2 regression test FAILED - timeout or error")
            return False, "ProduceV2"
            
    except Exception as e:
        log("ERROR", f"ProduceV2 regression test FAILED: {str(e)}")
        return False, "ProduceV2"

def main():
    """Run all tests and report results"""
    bootstrap_servers = os.getenv('BOOTSTRAP_SERVERS', 'chronik:9092')
    
    log("INFO", f"Starting confluent-kafka tests against {bootstrap_servers}")
    
    # Check if confluent-kafka is installed
    try:
        import confluent_kafka
        version = confluent_kafka.version()
        log("INFO", f"Using librdkafka version {version[0]} (confluent-kafka {version[1]})")
    except ImportError:
        log("ERROR", "confluent-kafka not installed")
        sys.exit(1)
    
    # Run tests
    tests = [
        ("ApiVersions", test_connection),
        ("Metadata", test_metadata),
        ("Produce", test_produce),
        ("Fetch", test_fetch),
        ("ConsumerGroup", test_consumer_group),
        ("ProduceV2Regression", test_produce_v2_regression)
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
                "client": "confluent-kafka"
            })
        except Exception as e:
            log("ERROR", f"Test {test_name} crashed: {str(e)}")
            results.append({
                "test": test_name,
                "api": test_name,
                "passed": False,
                "client": "confluent-kafka"
            })
    
    # Summary
    passed_count = sum(1 for r in results if r["passed"])
    total_count = len(results)
    
    log("INFO", f"Test summary: {passed_count}/{total_count} passed")
    
    # Output results in JSON format
    result_file = "/results/confluent-kafka-results.json"
    os.makedirs(os.path.dirname(result_file), exist_ok=True)
    
    with open(result_file, 'w') as f:
        json.dump({
            "client": "confluent-kafka",
            "version": version[0],
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