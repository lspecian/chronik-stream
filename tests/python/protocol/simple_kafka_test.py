#!/usr/bin/env python3
"""
Simple test to capture raw ApiVersions response from Kafka.
"""

import socket
import struct

def send_apiversion_v0_request(host, port):
    """Send ApiVersions v0 request to get raw response."""
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.connect((host, port))
    
    # Build ApiVersions v0 request (simplest format)
    # ApiVersions = 18
    correlation_id = 1
    client_id = b'test-client'
    
    # Request header for v0
    request_body = b''
    request_body += struct.pack('>h', 18)  # API key (ApiVersions)
    request_body += struct.pack('>h', 0)   # API version (v0)
    request_body += struct.pack('>i', correlation_id)  # Correlation ID
    request_body += struct.pack('>h', len(client_id))  # Client ID length
    request_body += client_id  # Client ID
    
    # No request body for ApiVersions
    
    request = struct.pack('>i', len(request_body)) + request_body
    
    print(f"Sending ApiVersions v0 request to {host}:{port}")
    print(f"Request size: {len(request)} bytes")
    print(f"Request hex: {request.hex()}")
    
    sock.send(request)
    
    # Read response
    size_bytes = sock.recv(4)
    if len(size_bytes) < 4:
        print(f"Failed to read size, got {len(size_bytes)} bytes")
        return None
        
    size = struct.unpack('>i', size_bytes)[0]
    print(f"Response size: {size} bytes")
    
    response = b''
    while len(response) < size:
        chunk = sock.recv(min(4096, size - len(response)))
        if not chunk:
            break
        response += chunk
    
    sock.close()
    
    print(f"Received {len(response)} bytes")
    return response

def analyze_response(response, source):
    """Analyze response byte by byte."""
    print(f"\n{'='*60}")
    print(f"Response from {source}")
    print(f"{'='*60}")
    
    # Show full hex dump for small responses
    if len(response) <= 200:
        print("\nFull hex dump:")
        for i in range(0, len(response), 16):
            hex_str = ' '.join(f'{b:02x}' for b in response[i:i+16])
            ascii_str = ''.join(chr(b) if 32 <= b < 127 else '.' for b in response[i:i+16])
            print(f"  {i:04x}: {hex_str:<48} {ascii_str}")
    else:
        print("\nFirst 200 bytes:")
        for i in range(0, min(200, len(response)), 16):
            hex_str = ' '.join(f'{b:02x}' for b in response[i:i+16])
            ascii_str = ''.join(chr(b) if 32 <= b < 127 else '.' for b in response[i:i+16])
            print(f"  {i:04x}: {hex_str:<48} {ascii_str}")
    
    # Parse v0 response
    print("\n--- Parsing as v0 response ---")
    offset = 0
    
    # Correlation ID
    correlation_id = struct.unpack('>i', response[offset:offset+4])[0]
    print(f"Correlation ID: {correlation_id}")
    offset += 4
    
    # For v0: array comes first, then error code
    array_len = struct.unpack('>i', response[offset:offset+4])[0]
    print(f"API array length: {array_len}")
    offset += 4
    
    print("\nFirst 5 APIs:")
    for i in range(min(5, array_len)):
        if offset + 6 > len(response):
            print(f"  Not enough data for API {i}")
            break
        
        api_key = struct.unpack('>h', response[offset:offset+2])[0]
        min_ver = struct.unpack('>h', response[offset+2:offset+4])[0]
        max_ver = struct.unpack('>h', response[offset+4:offset+6])[0]
        print(f"  API {api_key:3d}: v{min_ver}-v{max_ver}")
        offset += 6
    
    # Skip rest of APIs
    offset = 4 + 4 + (array_len * 6)
    
    if offset < len(response):
        # Error code at the end for v0
        error_code = struct.unpack('>h', response[offset:offset+2])[0]
        print(f"\nError code: {error_code}")
    
    return response

def test_go_client_against_kafka():
    """Test if Go client works against real Kafka."""
    import subprocess
    import os
    
    print("\n" + "="*60)
    print("Testing Go client against real Kafka (port 9093)")
    print("="*60)
    
    # Modify Go client to use port 9093
    go_test_path = "/Users/luis/Development/chronik-stream/tests/go/test_kafka_9093.go"
    
    go_code = '''package main

import (
    "fmt"
    "log"
    "time"
    "github.com/confluentinc/confluent-kafka-go/v2/kafka"
)

func main() {
    fmt.Println("Testing connection to real Kafka on port 9093...")
    
    config := kafka.ConfigMap{
        "bootstrap.servers": "localhost:9093",
        "client.id":         "go-test-client",
        "debug":             "protocol",
    }
    
    producer, err := kafka.NewProducer(&config)
    if err != nil {
        log.Fatal("Failed to create producer:", err)
    }
    defer producer.Close()
    
    fmt.Println("Producer created successfully")
    
    // Try to produce a message
    topic := "test-topic"
    message := &kafka.Message{
        TopicPartition: kafka.TopicPartition{Topic: &topic, Partition: kafka.PartitionAny},
        Value:          []byte("test message"),
    }
    
    err = producer.Produce(message, nil)
    if err != nil {
        log.Fatal("Failed to produce message:", err)
    }
    
    fmt.Println("Message sent, flushing...")
    producer.Flush(5000)
    
    fmt.Println("SUCCESS: Go client works with real Kafka!")
}
'''
    
    with open(go_test_path, 'w') as f:
        f.write(go_code)
    
    # Run the test
    result = subprocess.run(
        ["go", "run", go_test_path],
        cwd="/Users/luis/Development/chronik-stream/tests/go",
        capture_output=True,
        text=True,
        timeout=10
    )
    
    print("STDOUT:")
    print(result.stdout)
    if result.stderr:
        print("STDERR:")
        print(result.stderr)
    
    print(f"Exit code: {result.returncode}")
    
    if result.returncode == 0:
        print("\n✓ Go client works with real Kafka!")
    else:
        print("\n✗ Go client failed with real Kafka")
    
    # Clean up
    os.remove(go_test_path)

def main():
    # Test real Kafka
    print("Testing real Kafka on port 9093...")
    kafka_response = send_apiversion_v0_request('localhost', 9093)
    if kafka_response:
        kafka_data = analyze_response(kafka_response, "Real Kafka")
        
        # Save for comparison
        with open('/tmp/kafka_response.bin', 'wb') as f:
            f.write(kafka_response)
    
    # Test Chronik
    print("\n\nTesting Chronik on port 9092...")
    chronik_response = send_apiversion_v0_request('localhost', 9092)
    if chronik_response:
        chronik_data = analyze_response(chronik_response, "Chronik")
        
        # Save for comparison  
        with open('/tmp/chronik_response.bin', 'wb') as f:
            f.write(chronik_response)
    
    # Binary comparison
    if kafka_response and chronik_response:
        print("\n" + "="*60)
        print("BINARY COMPARISON")
        print("="*60)
        
        if kafka_response == chronik_response:
            print("✓ Responses are IDENTICAL!")
        else:
            print("✗ Responses differ")
            min_len = min(len(kafka_response), len(chronik_response))
            for i in range(min_len):
                if kafka_response[i] != chronik_response[i]:
                    print(f"\nFirst difference at byte {i} (0x{i:04x}):")
                    start = max(0, i - 8)
                    end = min(min_len, i + 8)
                    
                    kafka_hex = ' '.join(f'{b:02x}' for b in kafka_response[start:end])
                    chronik_hex = ' '.join(f'{b:02x}' for b in chronik_response[start:end])
                    
                    print(f"  Kafka:   {kafka_hex}")
                    print(f"  Chronik: {chronik_hex}")
                    print(f"           {' ' * (3 * (i - start))}^^")
                    break
    
    # Test Go client with real Kafka
    try:
        test_go_client_against_kafka()
    except Exception as e:
        print(f"Failed to test Go client: {e}")

if __name__ == "__main__":
    main()