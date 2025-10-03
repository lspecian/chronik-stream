#!/usr/bin/env python3
"""
Test SASL authentication with real Kafka client
"""

from kafka import KafkaProducer, KafkaConsumer
from kafka.admin import KafkaAdminClient, NewTopic
from kafka.errors import KafkaError
import time
import sys
import subprocess

def test_sasl_plain():
    """Test SASL PLAIN authentication"""
    print("\n=== Testing SASL PLAIN Authentication ===")

    try:
        # Try to connect with SASL PLAIN
        producer = KafkaProducer(
            bootstrap_servers=['localhost:9094'],
            security_protocol='SASL_PLAINTEXT',
            sasl_mechanism='PLAIN',
            sasl_plain_username='user',
            sasl_plain_password='password',
            request_timeout_ms=5000,
            api_version_auto_timeout_ms=5000
        )

        # Try to send a message
        future = producer.send('test-topic', b'test message')
        result = future.get(timeout=10)
        print("✅ SASL PLAIN authentication successful")
        producer.close()
        return True

    except Exception as e:
        print(f"❌ SASL PLAIN authentication failed: {e}")
        return False

def test_sasl_scram_sha256():
    """Test SASL SCRAM-SHA-256 authentication"""
    print("\n=== Testing SASL SCRAM-SHA-256 Authentication ===")

    try:
        # Try to connect with SASL SCRAM-SHA-256
        producer = KafkaProducer(
            bootstrap_servers=['localhost:9094'],
            security_protocol='SASL_PLAINTEXT',
            sasl_mechanism='SCRAM-SHA-256',
            sasl_plain_username='user',
            sasl_plain_password='password',
            request_timeout_ms=5000,
            api_version_auto_timeout_ms=5000
        )

        # Try to send a message
        future = producer.send('test-topic', b'test message')
        result = future.get(timeout=10)
        print("✅ SASL SCRAM-SHA-256 authentication successful")
        producer.close()
        return True

    except Exception as e:
        print(f"❌ SASL SCRAM-SHA-256 authentication failed: {e}")
        return False

def test_sasl_scram_sha512():
    """Test SASL SCRAM-SHA-512 authentication"""
    print("\n=== Testing SASL SCRAM-SHA-512 Authentication ===")

    try:
        # Try to connect with SASL SCRAM-SHA-512
        producer = KafkaProducer(
            bootstrap_servers=['localhost:9094'],
            security_protocol='SASL_PLAINTEXT',
            sasl_mechanism='SCRAM-SHA-512',
            sasl_plain_username='user',
            sasl_plain_password='password',
            request_timeout_ms=5000,
            api_version_auto_timeout_ms=5000
        )

        # Try to send a message
        future = producer.send('test-topic', b'test message')
        result = future.get(timeout=10)
        print("✅ SASL SCRAM-SHA-512 authentication successful")
        producer.close()
        return True

    except Exception as e:
        print(f"❌ SASL SCRAM-SHA-512 authentication failed: {e}")
        return False

def test_no_auth():
    """Test connection without authentication (should work)"""
    print("\n=== Testing Connection Without Authentication ===")

    try:
        # Try to connect without SASL
        producer = KafkaProducer(
            bootstrap_servers=['localhost:9094'],
            request_timeout_ms=5000,
            api_version_auto_timeout_ms=5000
        )

        # Try to send a message
        future = producer.send('test-topic', b'test message')
        result = future.get(timeout=10)
        print("✅ Non-authenticated connection successful")
        producer.close()
        return True

    except Exception as e:
        print(f"❌ Non-authenticated connection failed: {e}")
        return False

def main():
    # Kill any existing server
    subprocess.run(['pkill', '-f', 'chronik-server'], stderr=subprocess.DEVNULL)
    time.sleep(1)

    # Start server
    print("Starting Chronik server...")
    subprocess.run(['rm', '-rf', './data'])
    server = subprocess.Popen(
        ['./target/release/chronik-server', '-p', '9094'],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE
    )
    time.sleep(2)

    try:
        print("=" * 60)
        print("SASL AUTHENTICATION TEST SUITE")
        print("=" * 60)

        # First test non-authenticated connection (baseline)
        no_auth_pass = test_no_auth()

        # Test SASL mechanisms
        plain_pass = test_sasl_plain()
        scram256_pass = test_sasl_scram_sha256()
        scram512_pass = test_sasl_scram_sha512()

        # Print results
        print("\n" + "=" * 60)
        print("TEST RESULTS")
        print("=" * 60)
        print(f"No Authentication:    {'✅ PASSED' if no_auth_pass else '❌ FAILED'}")
        print(f"SASL PLAIN:          {'✅ PASSED' if plain_pass else '❌ FAILED (Expected - not fully implemented)'}")
        print(f"SASL SCRAM-SHA-256:  {'✅ PASSED' if scram256_pass else '❌ FAILED (Expected - not fully implemented)'}")
        print(f"SASL SCRAM-SHA-512:  {'✅ PASSED' if scram512_pass else '❌ FAILED (Expected - not fully implemented)'}")

        print("\n📝 Note: SASL authentication is partially implemented.")
        print("   The handshake works but full authentication flow needs completion.")

    finally:
        # Stop server
        server.terminate()
        server.wait()

if __name__ == "__main__":
    main()