package main

import (
    "fmt"
    "github.com/confluentinc/confluent-kafka-go/v2/kafka"
)

func testKafka(name string, port int) {
    fmt.Printf("\n=== Testing %s on port %d ===\n", name, port)
    
    config := kafka.ConfigMap{
        "bootstrap.servers": fmt.Sprintf("localhost:%d", port),
        "client.id":         "minimal-test",
    }
    
    fmt.Println("Creating producer...")
    producer, err := kafka.NewProducer(&config)
    if err != nil {
        fmt.Printf("❌ Failed to create producer: %v\n", err)
        return
    }
    defer producer.Close()
    
    fmt.Println("✓ Producer created")
    
    fmt.Println("Getting metadata (triggers ApiVersions)...")
    _, err = producer.GetMetadata(nil, false, 1000)
    if err != nil {
        fmt.Printf("❌ Failed to get metadata: %v\n", err)
    } else {
        fmt.Println("✓ Metadata retrieved")
    }
    
    fmt.Println("✅ Test completed without crash")
}

func main() {
    fmt.Println("confluent-kafka-go minimal crash test")
    fmt.Println("======================================")
    
    // Test real Kafka
    testKafka("Real Kafka", 19092)
    
    // Test Chronik
    testKafka("Chronik", 9092)
}