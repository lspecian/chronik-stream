package main

import (
	"fmt"
	"github.com/confluentinc/confluent-kafka-go/v2/kafka"
)

func main() {
	fmt.Println("Testing librdkafka WITHOUT ApiVersions negotiation")
	fmt.Println("==================================================")
	
	config := kafka.ConfigMap{
		"bootstrap.servers": "localhost:9092",
		"client.id":         "test-no-version",
		"api.version.request": false,  // DISABLE version negotiation
		"broker.version.fallback": "0.10.0",  // Use old protocol versions
	}
	
	fmt.Println("\nConfig: api.version.request=false (skip ApiVersions)")
	fmt.Println("Creating producer...")
	
	producer, err := kafka.NewProducer(&config)
	if err != nil {
		fmt.Printf("❌ Failed to create producer: %v\n", err)
		return
	}
	defer producer.Close()
	
	fmt.Println("✅ Producer created")
	
	fmt.Println("\nGetting metadata...")
	metadata, err := producer.GetMetadata(nil, false, 5000)
	if err != nil {
		fmt.Printf("❌ Failed to get metadata: %v\n", err)
		return
	}
	
	fmt.Printf("✅ Got metadata - %d broker(s), %d topic(s)\n", 
		len(metadata.Brokers), len(metadata.Topics))
	
	for _, broker := range metadata.Brokers {
		fmt.Printf("  Broker %d: %s:%d\n", broker.ID, broker.Host, broker.Port)
	}
}