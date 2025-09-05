package main

import (
	"fmt"
	"github.com/confluentinc/confluent-kafka-go/v2/kafka"
	"time"
)

func main() {
	fmt.Println("Testing librdkafka v2.11.1 against Chronik Stream")
	fmt.Println("==================================================")
	
	config := kafka.ConfigMap{
		"bootstrap.servers": "localhost:9092",
		"client.id":         "test-librdkafka",
		"debug":             "broker,protocol", // Enable debug logging
		"api.version.request": true,
		"socket.timeout.ms": 5000,
	}
	
	fmt.Println("\nCreating producer with debug enabled...")
	
	producer, err := kafka.NewProducer(&config)
	if err != nil {
		fmt.Printf("❌ Failed to create producer: %v\n", err)
		return
	}
	defer producer.Close()
	
	fmt.Println("✅ Producer created")
	
	// Give it time to connect
	time.Sleep(2 * time.Second)
	
	fmt.Println("\nGetting metadata (triggers ApiVersions)...")
	metadata, err := producer.GetMetadata(nil, false, 5000)
	if err != nil {
		fmt.Printf("❌ Failed to get metadata: %v\n", err)
		
		// Try to get events to see what happened
		for e := range producer.Events() {
			switch ev := e.(type) {
			case kafka.Error:
				fmt.Printf("Error: %v\n", ev)
			default:
				fmt.Printf("Event: %v\n", ev)
			}
			
			// Only read a few events
			if len(producer.Events()) == 0 {
				break
			}
		}
	} else {
		fmt.Printf("✅ Got metadata - %d broker(s)\n", len(metadata.Brokers))
	}
}