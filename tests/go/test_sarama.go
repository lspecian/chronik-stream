package main

import (
    "fmt"
    "log"
    "github.com/IBM/sarama"
)

func testKafka(name string, brokers []string) {
    fmt.Printf("\n=== Testing %s ===\n", name)
    fmt.Printf("Brokers: %v\n", brokers)
    
    // Create config
    config := sarama.NewConfig()
    config.Version = sarama.V2_6_0_0
    config.Producer.Return.Successes = true
    
    // Create client
    fmt.Println("Creating Sarama client...")
    client, err := sarama.NewClient(brokers, config)
    if err != nil {
        log.Printf("❌ Failed to create client: %v", err)
        return
    }
    defer client.Close()
    
    fmt.Println("✓ Client created successfully")
    
    // Get broker info
    brokerList := client.Brokers()
    fmt.Printf("✓ Found %d broker(s)\n", len(brokerList))
    for _, broker := range brokerList {
        fmt.Printf("  - Broker %d: %s\n", broker.ID(), broker.Addr())
    }
    
    // Create producer
    fmt.Println("\nCreating producer...")
    producer, err := sarama.NewSyncProducerFromClient(client)
    if err != nil {
        log.Printf("❌ Failed to create producer: %v", err)
        return
    }
    defer producer.Close()
    
    fmt.Println("✓ Producer created successfully")
    
    // Send a test message
    msg := &sarama.ProducerMessage{
        Topic: "test-topic",
        Value: sarama.StringEncoder("test message from Sarama"),
    }
    
    fmt.Println("\nSending test message...")
    partition, offset, err := producer.SendMessage(msg)
    if err != nil {
        fmt.Printf("⚠ Failed to send message: %v\n", err)
    } else {
        fmt.Printf("✓ Message sent to partition %d at offset %d\n", partition, offset)
    }
    
    fmt.Println("\n✅ Test completed successfully!")
}

func main() {
    fmt.Println("Sarama (Pure Go) Kafka Client Test")
    fmt.Println("===================================")
    
    // Test real Kafka first
    testKafka("Real Kafka on port 19092", []string{"localhost:19092"})
    
    // Test Chronik
    testKafka("Chronik on port 9092", []string{"localhost:9092"})
    
    fmt.Println("\n" + "==================================================")
    fmt.Println("Summary:")
    fmt.Println("Sarama is a pure Go client that doesn't use librdkafka")
    fmt.Println("If Sarama works with both, it confirms the issue is with librdkafka")
}