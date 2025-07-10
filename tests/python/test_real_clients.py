#!/usr/bin/env python3
"""
Real Kafka client compatibility tests for Chronik Stream.

This script tests that various Kafka clients can successfully interact with Chronik Stream.
"""

import subprocess
import sys
import time
import os
import tempfile
import json

def run_command(cmd, shell=False, check=True, capture_output=True):
    """Run a shell command and return output."""
    try:
        result = subprocess.run(
            cmd,
            shell=shell,
            check=check,
            capture_output=capture_output,
            text=True
        )
        return result.stdout, result.stderr, result.returncode
    except subprocess.CalledProcessError as e:
        return e.stdout or "", e.stderr or "", e.returncode

def check_prerequisite(command, package_name):
    """Check if a prerequisite command is available."""
    _, _, returncode = run_command(["which", command], check=False)
    if returncode != 0:
        print(f"⚠️  {package_name} not found. Install it to run these tests.")
        return False
    return True

def test_kafkactl(bootstrap_servers):
    """Test kafkactl client operations."""
    print("\n🧪 Testing kafkactl client...")
    
    if not check_prerequisite("kafkactl", "kafkactl"):
        return False
    
    topic = "kafkactl-test-topic"
    
    # Test 1: Get brokers
    print("  ✓ Testing broker discovery...")
    stdout, stderr, _ = run_command([
        "kafkactl", "get", "brokers",
        "--brokers", bootstrap_servers
    ])
    assert "ID" in stdout or "id" in stdout, f"Broker info not found: {stdout}"
    
    # Test 2: Create topic
    print("  ✓ Testing topic creation...")
    stdout, stderr, returncode = run_command([
        "kafkactl", "create", "topic", topic,
        "--partitions", "3",
        "--replication-factor", "1",
        "--brokers", bootstrap_servers
    ], check=False)
    
    # Topic might already exist, which is fine
    if returncode != 0 and "already exists" not in stderr:
        raise Exception(f"Failed to create topic: {stderr}")
    
    # Test 3: List topics
    print("  ✓ Testing topic listing...")
    stdout, stderr, _ = run_command([
        "kafkactl", "get", "topics",
        "--brokers", bootstrap_servers
    ])
    assert topic in stdout, f"Topic {topic} not found in list"
    
    # Test 4: Produce message
    print("  ✓ Testing message production...")
    test_message = "Hello from kafkactl!"
    stdout, stderr, _ = run_command([
        "kafkactl", "produce", topic,
        "--value", test_message,
        "--brokers", bootstrap_servers
    ])
    
    # Test 5: Consume message
    print("  ✓ Testing message consumption...")
    stdout, stderr, _ = run_command([
        "kafkactl", "consume", topic,
        "--from-beginning",
        "--max-messages", "1",
        "--print-values",
        "--brokers", bootstrap_servers
    ])
    assert test_message in stdout, f"Message not found in output: {stdout}"
    
    print("  ✅ kafkactl tests passed!")
    return True

def test_confluent_kafka_python(bootstrap_servers):
    """Test confluent-kafka-python client."""
    print("\n🧪 Testing confluent-kafka-python client...")
    
    try:
        import confluent_kafka
        from confluent_kafka import Producer, Consumer, KafkaError
        from confluent_kafka.admin import AdminClient, NewTopic
    except ImportError:
        print("  ⚠️  confluent-kafka not installed. Run: pip install confluent-kafka")
        return False
    
    topic = "python-test-topic"
    
    # Test 1: Admin client
    print("  ✓ Testing admin client...")
    admin = AdminClient({'bootstrap.servers': bootstrap_servers})
    metadata = admin.list_topics(timeout=10)
    print(f"    Found {len(metadata.brokers)} brokers")
    
    # Create topic
    new_topic = NewTopic(topic, num_partitions=3, replication_factor=1)
    fs = admin.create_topics([new_topic])
    
    for topic_name, f in fs.items():
        try:
            f.result()
            print(f"    Created topic: {topic_name}")
        except Exception as e:
            if 'already exists' not in str(e):
                raise
    
    # Test 2: Producer
    print("  ✓ Testing producer...")
    producer = Producer({
        'bootstrap.servers': bootstrap_servers,
        'client.id': 'python-test-producer'
    })
    
    delivered = []
    
    def delivery_report(err, msg):
        if err is not None:
            raise Exception(f'Message delivery failed: {err}')
        delivered.append(msg)
    
    for i in range(5):
        producer.produce(
            topic,
            key=f'key-{i}'.encode('utf-8'),
            value=f'Message {i} from Python'.encode('utf-8'),
            callback=delivery_report
        )
    
    producer.flush(timeout=10)
    assert len(delivered) == 5, f"Expected 5 delivered messages, got {len(delivered)}"
    
    # Test 3: Consumer
    print("  ✓ Testing consumer...")
    consumer = Consumer({
        'bootstrap.servers': bootstrap_servers,
        'group.id': 'python-test-group',
        'auto.offset.reset': 'earliest'
    })
    
    consumer.subscribe([topic])
    
    consumed = 0
    start_time = time.time()
    
    while consumed < 5 and time.time() - start_time < 10:
        msg = consumer.poll(timeout=1.0)
        
        if msg is None:
            continue
            
        if msg.error():
            if msg.error().code() == KafkaError._PARTITION_EOF:
                continue
            else:
                raise Exception(f'Consumer error: {msg.error()}')
        
        print(f"    Consumed: {msg.value().decode('utf-8')}")
        consumed += 1
    
    consumer.close()
    
    assert consumed == 5, f"Expected 5 messages, consumed {consumed}"
    
    print("  ✅ confluent-kafka-python tests passed!")
    return True

def test_kcat(bootstrap_servers):
    """Test kcat (kafkacat) client."""
    print("\n🧪 Testing kcat client...")
    
    if not check_prerequisite("kcat", "kcat (kafkacat)"):
        # Try kafkacat as fallback
        if not check_prerequisite("kafkacat", "kafkacat"):
            return False
        kcat_cmd = "kafkacat"
    else:
        kcat_cmd = "kcat"
    
    topic = "kcat-test-topic"
    
    # Test 1: List metadata
    print("  ✓ Testing metadata...")
    stdout, stderr, _ = run_command([
        kcat_cmd, "-b", bootstrap_servers, "-L"
    ])
    assert "broker" in stdout.lower(), f"No broker info in metadata: {stdout}"
    
    # Test 2: Produce message
    print("  ✓ Testing message production...")
    test_message = "Hello from kcat!"
    process = subprocess.Popen(
        [kcat_cmd, "-b", bootstrap_servers, "-t", topic, "-P"],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True
    )
    stdout, stderr = process.communicate(input=test_message + "\n")
    
    # Test 3: Consume message
    print("  ✓ Testing message consumption...")
    stdout, stderr, _ = run_command([
        kcat_cmd, "-b", bootstrap_servers, "-t", topic,
        "-C", "-c", "1", "-e"
    ])
    assert test_message in stdout, f"Message not found: {stdout}"
    
    print("  ✅ kcat tests passed!")
    return True

def test_cross_client_compatibility(bootstrap_servers):
    """Test that messages produced by one client can be consumed by another."""
    print("\n🧪 Testing cross-client compatibility...")
    
    topic = "cross-client-test"
    
    # Check available clients
    has_kafkactl = check_prerequisite("kafkactl", "kafkactl")
    has_python = False
    
    try:
        import confluent_kafka
        has_python = True
    except ImportError:
        pass
    
    if not has_kafkactl or not has_python:
        print("  ⚠️  Need both kafkactl and confluent-kafka for cross-client test")
        return False
    
    # Create topic with kafkactl
    print("  ✓ Creating topic with kafkactl...")
    run_command([
        "kafkactl", "create", "topic", topic,
        "--brokers", bootstrap_servers
    ], check=False)
    
    # Produce with Python
    print("  ✓ Producing with Python client...")
    from confluent_kafka import Producer
    
    producer = Producer({'bootstrap.servers': bootstrap_servers})
    
    for i in range(3):
        producer.produce(
            topic,
            key=f'python-key-{i}'.encode(),
            value=f'Message from Python {i}'.encode()
        )
    
    producer.flush()
    
    # Consume with kafkactl
    print("  ✓ Consuming with kafkactl...")
    stdout, stderr, _ = run_command([
        "kafkactl", "consume", topic,
        "--from-beginning",
        "--max-messages", "3",
        "--print-keys",
        "--print-values",
        "--brokers", bootstrap_servers
    ])
    
    # Verify messages
    for i in range(3):
        assert f"Message from Python {i}" in stdout, f"Missing message {i}"
        assert f"python-key-{i}" in stdout, f"Missing key {i}"
    
    print("  ✅ Cross-client compatibility test passed!")
    return True

def test_error_handling(bootstrap_servers):
    """Test client error handling."""
    print("\n🧪 Testing error handling...")
    
    if not check_prerequisite("kafkactl", "kafkactl"):
        return False
    
    # Test 1: Invalid topic name
    print("  ✓ Testing invalid topic name...")
    stdout, stderr, returncode = run_command([
        "kafkactl", "create", "topic", "invalid..topic",
        "--brokers", bootstrap_servers
    ], check=False)
    
    assert returncode != 0, "Should fail with invalid topic name"
    assert "invalid" in stdout.lower() or "invalid" in stderr.lower()
    
    # Test 2: Non-existent broker
    print("  ✓ Testing connection to non-existent broker...")
    stdout, stderr, returncode = run_command([
        "kafkactl", "get", "brokers",
        "--brokers", "nonexistent:9999"
    ], check=False)
    
    assert returncode != 0, "Should fail with non-existent broker"
    
    print("  ✅ Error handling tests passed!")
    return True

def main():
    """Run all client compatibility tests."""
    # Default to localhost:9092 unless specified
    bootstrap_servers = os.environ.get("BOOTSTRAP_SERVERS", "localhost:9092")
    
    print(f"🚀 Running Kafka client compatibility tests")
    print(f"   Bootstrap servers: {bootstrap_servers}")
    
    # Check if Chronik Stream is running
    print("\n📡 Checking connection to Chronik Stream...")
    stdout, stderr, returncode = run_command([
        "nc", "-zv", "localhost", "9092"
    ], check=False, capture_output=True)
    
    if returncode != 0:
        print("❌ Cannot connect to Chronik Stream on localhost:9092")
        print("   Make sure Chronik Stream is running with:")
        print("   docker-compose up -d")
        sys.exit(1)
    
    print("✅ Connected to Chronik Stream")
    
    # Run tests
    tests = [
        ("kafkactl", test_kafkactl),
        ("confluent-kafka-python", test_confluent_kafka_python),
        ("kcat", test_kcat),
        ("cross-client", test_cross_client_compatibility),
        ("error-handling", test_error_handling),
    ]
    
    passed = 0
    failed = 0
    skipped = 0
    
    for test_name, test_func in tests:
        try:
            result = test_func(bootstrap_servers)
            if result:
                passed += 1
            else:
                skipped += 1
        except Exception as e:
            print(f"\n❌ {test_name} test failed: {e}")
            failed += 1
    
    # Summary
    print(f"\n📊 Test Summary:")
    print(f"   ✅ Passed:  {passed}")
    print(f"   ❌ Failed:  {failed}")
    print(f"   ⚠️  Skipped: {skipped}")
    
    if failed > 0:
        sys.exit(1)
    else:
        print("\n🎉 All tests completed successfully!")

if __name__ == "__main__":
    main()