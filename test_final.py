#!/usr/bin/env python3
"""Final test for kafkactl compatibility"""

import subprocess
import time

def test_kafkactl():
    """Test various kafkactl commands"""
    
    print("Testing kafkactl with Chronik Stream...")
    print("=" * 50)
    
    # Test 1: List topics
    print("\n1. List topics:")
    result = subprocess.run(['kafkactl', '--context', 'local', 'get', 'topics'], 
                          capture_output=True, text=True)
    print(f"   Exit code: {result.returncode}")
    print(f"   Output: {result.stdout}")
    if result.stderr:
        print(f"   Error: {result.stderr}")
    
    # Test 2: Create topic
    print("\n2. Create topic 'test-topic':")
    result = subprocess.run(['kafkactl', '--context', 'local', 'create', 'topic', 
                           'test-topic', '--partitions', '3', '--replicas', '1'], 
                          capture_output=True, text=True)
    print(f"   Exit code: {result.returncode}")
    print(f"   Output: {result.stdout}")
    if result.stderr:
        print(f"   Error: {result.stderr}")
    
    # Test 3: List topics again
    print("\n3. List topics after creation:")
    result = subprocess.run(['kafkactl', '--context', 'local', 'get', 'topics'], 
                          capture_output=True, text=True)
    print(f"   Exit code: {result.returncode}")
    print(f"   Output: {result.stdout}")
    if result.stderr:
        print(f"   Error: {result.stderr}")
    
    # Test 4: Produce a message
    print("\n4. Produce a message:")
    process = subprocess.Popen(['kafkactl', '--context', 'local', 'produce', 
                              'test-topic', '--value', 'Hello from kafkactl!'],
                             stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    stdout, stderr = process.communicate(timeout=5)
    print(f"   Exit code: {process.returncode}")
    print(f"   Output: {stdout}")
    if stderr:
        print(f"   Error: {stderr}")
    
    # Test 5: Consume messages
    print("\n5. Consume messages (5 second timeout):")
    process = subprocess.Popen(['kafkactl', '--context', 'local', 'consume', 
                              'test-topic', '--from-beginning'],
                             stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    try:
        stdout, stderr = process.communicate(timeout=5)
    except subprocess.TimeoutExpired:
        process.terminate()
        stdout, stderr = process.communicate()
        print("   (Timed out after 5 seconds)")
    
    print(f"   Output: {stdout}")
    if stderr:
        print(f"   Error: {stderr}")
    
    print("\n" + "=" * 50)
    print("Test completed!")

if __name__ == "__main__":
    test_kafkactl()