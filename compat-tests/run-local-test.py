#!/usr/bin/env python3
"""
Direct local test runner for Chronik compatibility testing
Runs test scripts directly against local Chronik instance
"""

import subprocess
import json
import sys
import os
from datetime import datetime

def run_python_test(script_path, client_name):
    """Run a Python test script"""
    print(f"\n[INFO] Running {client_name} tests...")
    
    # Set environment variable for local Chronik
    env = os.environ.copy()
    env['BOOTSTRAP_SERVERS'] = 'localhost:9092'
    
    try:
        result = subprocess.run(
            [sys.executable, script_path],
            capture_output=True,
            text=True,
            env=env,
            timeout=30
        )
        
        print(f"[INFO] {client_name} output:")
        print(result.stdout)
        if result.stderr:
            print(f"[WARN] {client_name} stderr:")
            print(result.stderr)
            
        return result.returncode == 0
    except subprocess.TimeoutExpired:
        print(f"[ERROR] {client_name} test timed out")
        return False
    except Exception as e:
        print(f"[ERROR] Failed to run {client_name}: {e}")
        return False

def main():
    print("="*60)
    print("Chronik Local Compatibility Test Runner")
    print("="*60)
    
    # Check if Chronik is running
    import socket
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    result = sock.connect_ex(('localhost', 9092))
    sock.close()
    
    if result != 0:
        print("[ERROR] Chronik is not running on localhost:9092")
        print("Please start Chronik with: ./target/release/chronik-server")
        sys.exit(1)
    
    print("[INFO] Chronik is running on localhost:9092")
    
    # Test results
    results = {
        "timestamp": datetime.utcnow().isoformat(),
        "clients": {}
    }
    
    # Run kafka-python test
    kafka_python_path = "clients/python/kafka-python/test.py"
    if os.path.exists(kafka_python_path):
        success = run_python_test(kafka_python_path, "kafka-python")
        results["clients"]["kafka-python"] = {
            "passed": success,
            "timestamp": datetime.utcnow().isoformat()
        }
        
        # Try to load the JSON results if generated
        json_path = "/tmp/kafka-python-results.json"
        if os.path.exists(json_path):
            with open(json_path, 'r') as f:
                results["clients"]["kafka-python"]["details"] = json.load(f)
    
    # Run confluent-kafka test
    confluent_path = "clients/python/confluent-kafka/test.py"
    if os.path.exists(confluent_path):
        success = run_python_test(confluent_path, "confluent-kafka")
        results["clients"]["confluent-kafka"] = {
            "passed": success,
            "timestamp": datetime.utcnow().isoformat()
        }
        
        # Try to load the JSON results if generated
        json_path = "/tmp/confluent-kafka-results.json"
        if os.path.exists(json_path):
            with open(json_path, 'r') as f:
                results["clients"]["confluent-kafka"]["details"] = json.load(f)
    
    # Summary
    print("\n" + "="*60)
    print("TEST SUMMARY")
    print("="*60)
    
    total_clients = len(results["clients"])
    passed_clients = sum(1 for c in results["clients"].values() if c.get("passed", False))
    
    print(f"Total clients tested: {total_clients}")
    print(f"Passed: {passed_clients}")
    print(f"Failed: {total_clients - passed_clients}")
    
    for client, result in results["clients"].items():
        status = "✅ PASSED" if result.get("passed", False) else "❌ FAILED"
        print(f"  {client}: {status}")
    
    # Save results
    results_dir = "results"
    os.makedirs(results_dir, exist_ok=True)
    
    results_file = os.path.join(results_dir, "local-test-results.json")
    with open(results_file, 'w') as f:
        json.dump(results, f, indent=2)
    
    print(f"\n[INFO] Results saved to {results_file}")
    
    return 0 if passed_clients == total_clients else 1

if __name__ == "__main__":
    sys.exit(main())