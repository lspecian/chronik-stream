package main

import (
    "fmt"
    "log"
    "github.com/confluentinc/confluent-kafka-go/v2/kafka"
)

func main() {
    fmt.Println("Testing Go client against real Kafka on port 19092...")
    
    config := kafka.ConfigMap{
        "bootstrap.servers": "localhost:19092",
        "client.id":         "go-test-client",
        "debug":             "protocol",  // Enable debug output
    }
    
    producer, err := kafka.NewProducer(&config)
    if err != nil {
        log.Fatal("Failed to create producer:", err)
    }
    defer producer.Close()
    
    fmt.Println("✓ Producer created successfully")
    
    // Get metadata
    metadata, err := producer.GetMetadata(nil, false, 5000)
    if err != nil {
        log.Fatal("Failed to get metadata:", err)
    }
    
    fmt.Printf("✓ Connected to Kafka cluster with %d broker(s)\n", len(metadata.Brokers))
    
    // Try to produce
    topic := "test-topic"
    message := &kafka.Message{
        TopicPartition: kafka.TopicPartition{Topic: &topic, Partition: kafka.PartitionAny},
        Value:          []byte("test message"),
    }
    
    err = producer.Produce(message, nil)
    if err != nil {
        log.Fatal("Failed to produce:", err)
    }
    
    fmt.Println("✓ Message queued")
    
    // Flush - this is where Chronik crashes
    fmt.Println("Flushing (this is where Chronik crashes)...")
    remaining := producer.Flush(5000)
    
    if remaining > 0 {
        fmt.Printf("⚠ %d messages still in queue\n", remaining)
    } else {
        fmt.Println("✓ All messages flushed")
    }
    
    fmt.Println("\n✅ SUCCESS: Go client works with real Kafka!")
}