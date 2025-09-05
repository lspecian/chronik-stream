package main

import (
	"fmt"
	"github.com/confluentinc/confluent-kafka-go/v2/kafka"
)

func main() {
	fmt.Println("Testing librdkafka with capture server on port 9095")
	
	config := kafka.ConfigMap{
		"bootstrap.servers": "localhost:9095",
		"client.id":         "test-librdkafka",
	}
	
	producer, err := kafka.NewProducer(&config)
	if err != nil {
		fmt.Printf("Failed to create producer: %v\n", err)
		return
	}
	defer producer.Close()
	
	fmt.Println("Producer created, triggering ApiVersions...")
	
	metadata, err := producer.GetMetadata(nil, false, 1000)
	if err != nil {
		fmt.Printf("GetMetadata failed (expected): %v\n", err)
	} else {
		fmt.Printf("Got metadata: %d brokers\n", len(metadata.Brokers))
	}
}