package main

import (
    "fmt"
    "github.com/confluentinc/confluent-kafka-go/v2/kafka"
)

func testKafka(name string, port int) {
    fmt.Printf("\n=== Testing %s on port %d ===\n", name, port)
    
    config := kafka.ConfigMap{
        "bootstrap.servers": fmt.Sprintf("localhost:%d", port),
        "client.id":         "downgraded-test",
    }
    
    fmt.Println("Creating producer with confluent-kafka-go v2.1.0...")
    producer, err := kafka.NewProducer(&config)
    if err != nil {
        fmt.Printf("❌ Failed to create producer: %v\n", err)
        return
    }
    defer producer.Close()
    
    fmt.Println("✓ Producer created")
    
    fmt.Println("Getting metadata (triggers ApiVersions)...")
    metadata, err := producer.GetMetadata(nil, false, 1000)
    if err != nil {
        fmt.Printf("❌ Failed to get metadata: %v\n", err)
    } else {
        fmt.Printf("✓ Metadata retrieved - %d broker(s)\n", len(metadata.Brokers))
    }
    
    // Try to produce a message
    topic := "test-topic"
    msg := &kafka.Message{
        TopicPartition: kafka.TopicPartition{Topic: &topic, Partition: kafka.PartitionAny},
        Value:          []byte("test from downgraded client"),
    }
    
    fmt.Println("Producing message...")
    err = producer.Produce(msg, nil)
    if err != nil {
        fmt.Printf("❌ Failed to produce: %v\n", err)
    } else {
        fmt.Println("✓ Message queued")
    }
    
    fmt.Println("Flushing (where v2.11.1 crashes)...")
    remaining := producer.Flush(5000)
    if remaining > 0 {
        fmt.Printf("⚠ %d messages still in queue\n", remaining)
    } else {
        fmt.Println("✓ All messages flushed")
    }
    
    fmt.Println("✅ Test completed without crash!")
}

func main() {
    // Check version
    version, _ := kafka.LibraryVersion()
    fmt.Printf("confluent-kafka-go library version: %s\n", version)
    fmt.Println("================================================")
    
    // Test real Kafka
    testKafka("Real Kafka", 19092)
    
    // Test Chronik
    testKafka("Chronik", 9092)
    
    fmt.Println("\n================================================")
    fmt.Println("SUCCESS: Downgraded version works without crashes!")
}