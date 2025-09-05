package main

import (
	"fmt"
	"github.com/confluentinc/confluent-kafka-go/v2/kafka"
)

func main() {
	fmt.Println("Testing librdkafka with mock server on port 9094")
	fmt.Println("================================================")
	
	config := kafka.ConfigMap{
		"bootstrap.servers": "localhost:9094",
		"client.id":         "test-librdkafka-capture",
	}
	
	fmt.Println("\nCreating producer...")
	
	producer, err := kafka.NewProducer(&config)
	if err != nil {
		fmt.Printf("❌ Failed to create producer: %v\n", err)
		return
	}
	defer producer.Close()
	
	fmt.Println("✅ Producer created")
	
	fmt.Println("\nGetting metadata (triggers ApiVersions)...")
	metadata, err := producer.GetMetadata(nil, false, 5000)
	if err != nil {
		fmt.Printf("❌ Failed to get metadata: %v\n", err)
		return
	}
	
	fmt.Printf("✅ Got metadata - %d broker(s), %d topic(s)\n", 
		len(metadata.Brokers), len(metadata.Topics))
}