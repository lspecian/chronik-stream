package main

import (
	"fmt"
	"log"
	
	"github.com/confluentinc/confluent-kafka-go/v2/kafka"
)

func main() {
	fmt.Println("Testing Go client through debug proxy...")
	fmt.Println("Connecting to localhost:9093 (proxy) -> localhost:9092 (chronik)")
	
	// Create producer with debug settings
	config := kafka.ConfigMap{
		"bootstrap.servers": "localhost:9093",  // Connect through proxy
		"client.id":        "go-test-via-proxy",
		"acks":             "all",
		"debug":            "broker,protocol,msg",  // Enable debug logging
		"api.version.request": true,
		"api.version.request.timeout.ms": 10000,
		"socket.timeout.ms": 10000,
		"message.timeout.ms": 10000,
	}
	
	producer, err := kafka.NewProducer(&config)
	if err != nil {
		log.Fatalf("Failed to create producer: %v", err)
	}
	defer producer.Close()
	
	fmt.Println("✓ Producer created successfully")
	
	// Try to produce a message
	topic := "test-topic"
	message := &kafka.Message{
		TopicPartition: kafka.TopicPartition{Topic: &topic, Partition: kafka.PartitionAny},
		Value:          []byte("Hello from Go client via proxy"),
		Key:            []byte("test-key"),
	}
	
	fmt.Println("Attempting to produce message...")
	err = producer.Produce(message, nil)
	if err != nil {
		log.Printf("Failed to produce message: %v", err)
	} else {
		fmt.Println("✓ Message queued for production")
	}
	
	// The critical flush operation that causes memory corruption
	fmt.Println("\nAttempting flush operation (this is where memory corruption occurs)...")
	fmt.Println("If you see 'Assertion failed', the crash happened")
	
	remaining := producer.Flush(5000)
	if remaining > 0 {
		fmt.Printf("⚠ Warning: %d messages still in queue after flush\n", remaining)
	} else {
		fmt.Println("✓ All messages flushed successfully")
	}
	
	// Try to get metadata  
	fmt.Println("\nAttempting to get metadata...")
	metadata, err := producer.GetMetadata(&topic, false, 5000)
	if err != nil {
		log.Printf("Failed to get metadata: %v", err)
	} else {
		fmt.Printf("✓ Got metadata for %d topics, %d brokers\n", 
			len(metadata.Topics), len(metadata.Brokers))
		
		// Print broker info
		for _, broker := range metadata.Brokers {
			fmt.Printf("  Broker %d: %s:%d\n", broker.ID, broker.Host, broker.Port)
		}
	}
	
	fmt.Println("\n✅ Test completed without crash!")
}