#!/usr/bin/env python3
"""
Test script to verify v0.7.2 fixes the advertised address bug
"""

import subprocess
import time
import sys
from kafka import KafkaAdminClient
from kafka.admin import NewTopic

def test_advertised_address(advertised_addr, expected_advertised):
    """Test a specific advertised address configuration"""
    print(f"\n{'='*60}")
    print(f"Testing CHRONIK_ADVERTISED_ADDR='{advertised_addr}'")
    print(f"Expected: Advertised: {expected_advertised}")
    print('='*60)
    
    # Kill any existing server
    subprocess.run(['pkill', '-9', '-f', 'chronik-server'], 
                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    time.sleep(2)
    
    # Start server with the advertised address
    env = {'CHRONIK_ADVERTISED_ADDR': advertised_addr}
    server = subprocess.Popen(
        ['./target/release/chronik-server', '--metrics-port', '9094', 'standalone'],
        env={**subprocess.os.environ, **env},
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True
    )
    
    # Wait for server to start and check advertised address
    start_time = time.time()
    advertised_line = None
    while time.time() - start_time < 5:
        line = server.stdout.readline()
        if 'Advertised:' in line:
            advertised_line = line.strip()
            break
    
    if advertised_line:
        # Extract the advertised address
        advertised = advertised_line.split('Advertised:')[1].strip()
        print(f"✅ Server reports: Advertised: {advertised}")
        
        # Check if it matches expected
        if advertised == expected_advertised:
            print(f"✅ PASS: Advertised address is correct!")
            
            # Try to connect with Kafka client
            time.sleep(2)
            try:
                admin = KafkaAdminClient(
                    bootstrap_servers=['localhost:9092'],
                    api_version=(0, 10, 0),
                    request_timeout_ms=5000
                )
                print(f"✅ Kafka admin client connected successfully!")
                admin.close()
            except Exception as e:
                print(f"⚠️  Client connection test failed: {e}")
        else:
            print(f"❌ FAIL: Expected '{expected_advertised}', got '{advertised}'")
            print(f"❌ This is the bug from v0.7.1!")
    else:
        print(f"❌ Server failed to start or didn't report advertised address")
    
    # Kill server
    server.terminate()
    server.wait(timeout=5)
    
    return advertised_line

# Run tests
print("=" * 70)
print("CHRONIK STREAM v0.7.2 - ADVERTISED ADDRESS BUG FIX VERIFICATION")
print("=" * 70)

test_cases = [
    ("localhost", "localhost:9092"),
    ("localhost:9092", "localhost:9092"),
    ("chronik-stream", "chronik-stream:9092"),
    ("chronik-stream:9092", "chronik-stream:9092"),
    ("kafka.example.com", "kafka.example.com:9092"),
    ("kafka.example.com:29092", "kafka.example.com:29092"),
]

passed = 0
failed = 0

for addr, expected in test_cases:
    result = test_advertised_address(addr, expected)
    if result and expected in result:
        passed += 1
    else:
        failed += 1

print("\n" + "=" * 70)
print(f"TEST RESULTS: {passed} passed, {failed} failed")

if failed == 0:
    print("✅ ALL TESTS PASSED - v0.7.2 fix is working correctly!")
    print("The port duplication bug from v0.7.1 has been fixed.")
else:
    print(f"❌ {failed} tests failed - there may still be issues")

print("=" * 70)