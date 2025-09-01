package main

import (
    "fmt"
    "github.com/confluentinc/confluent-kafka-go/v2/kafka"
)

func testKafka(name string, port int) error {
    fmt.Printf("\n=== Testing %s on port %d ===\n", name, port)
    
    config := kafka.ConfigMap{
        "bootstrap.servers": fmt.Sprintf("localhost:%d", port),
        "client.id":         "cp-kafka-test",
        "debug":             "protocol", // Enable debug
    }
    
    fmt.Println("Creating producer...")
    producer, err := kafka.NewProducer(&config)
    if err != nil {
        return fmt.Errorf("Failed to create producer: %v", err)
    }
    defer producer.Close()
    
    fmt.Println("✓ Producer created")
    
    fmt.Println("Getting metadata (triggers ApiVersions)...")
    metadata, err := producer.GetMetadata(nil, false, 5000)
    if err != nil {
        return fmt.Errorf("Failed to get metadata: %v", err)
    }
    
    fmt.Printf("✓ Metadata retrieved - %d broker(s)\n", len(metadata.Brokers))
    for _, broker := range metadata.Brokers {
        fmt.Printf("  Broker %d: %s:%d\n", broker.ID, broker.Host, broker.Port)
    }
    
    // Try to produce
    topic := "test-topic"
    msg := &kafka.Message{
        TopicPartition: kafka.TopicPartition{Topic: &topic, Partition: kafka.PartitionAny},
        Value:          []byte("test message"),
    }
    
    fmt.Println("Producing message...")
    err = producer.Produce(msg, nil)
    if err != nil {
        return fmt.Errorf("Failed to produce: %v", err)
    }
    fmt.Println("✓ Message queued")
    
    fmt.Println("Flushing (where crashes happen)...")
    remaining := producer.Flush(5000)
    if remaining > 0 {
        fmt.Printf("⚠ %d messages still in queue\n", remaining)
    } else {
        fmt.Println("✓ All messages flushed")
    }
    
    return nil
}

func main() {
    fmt.Println("Testing CP Kafka (User's Setup) vs Chronik")
    fmt.Println("============================================")
    fmt.Println("Using confluent-kafka-go v2.11.1 (latest)")
    
    // Test CP Kafka (user's setup)
    fmt.Println("\n1. CP Kafka 7.5.0 (matching user's docker-compose)")
    err := testKafka("CP Kafka", 29092)
    if err != nil {
        fmt.Printf("❌ CP Kafka test failed: %v\n", err)
    } else {
        fmt.Println("✅ CP Kafka test passed!")
    }
    
    // Test Chronik
    fmt.Println("\n2. Chronik Stream")
    err = testKafka("Chronik", 9092)
    if err != nil {
        fmt.Printf("❌ Chronik test failed: %v\n", err)
    } else {
        fmt.Println("✅ Chronik test passed!")
    }
    
    fmt.Println("\n====================================================")
    fmt.Println("Test complete. If both pass, the issue is elsewhere.")
    fmt.Println("If CP Kafka crashes too, it's a librdkafka issue.")
}