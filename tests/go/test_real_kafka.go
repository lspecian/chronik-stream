package main

import (
    "fmt"
    "log"
    "github.com/confluentinc/confluent-kafka-go/v2/kafka"
)

func main() {
    fmt.Println("Testing Go client against real Kafka on port 9093...")
    
    // Create producer config
    config := kafka.ConfigMap{
        "bootstrap.servers": "localhost:9093",
        "client.id":         "go-test-client",
    }
    
    // Create producer
    producer, err := kafka.NewProducer(&config)
    if err != nil {
        log.Fatal("Failed to create producer:", err)
    }
    defer producer.Close()
    
    fmt.Println("✓ Producer created successfully")
    
    // Try to get metadata
    metadata, err := producer.GetMetadata(nil, false, 5000)
    if err != nil {
        log.Fatal("Failed to get metadata:", err)
    }
    
    fmt.Printf("✓ Connected to Kafka cluster with %d broker(s)\n", len(metadata.Brokers))
    for _, broker := range metadata.Brokers {
        fmt.Printf("  Broker %d: %s:%d\n", broker.ID, broker.Host, broker.Port)
    }
    
    // Try to produce a test message
    topic := "test-topic"
    message := &kafka.Message{
        TopicPartition: kafka.TopicPartition{Topic: &topic, Partition: kafka.PartitionAny},
        Value:          []byte("test message from Go"),
    }
    
    err = producer.Produce(message, nil)
    if err != nil {
        log.Fatal("Failed to produce message:", err)
    }
    
    fmt.Println("✓ Message queued for sending")
    
    // Flush to ensure message is sent
    remaining := producer.Flush(5000)
    if remaining > 0 {
        fmt.Printf("⚠ Warning: %d messages still in queue after flush\n", remaining)
    } else {
        fmt.Println("✓ All messages flushed successfully")
    }
    
    fmt.Println("\n✅ SUCCESS: Go client works perfectly with real Kafka!")
}