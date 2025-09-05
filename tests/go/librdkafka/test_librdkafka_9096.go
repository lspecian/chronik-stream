package main

import (
	"fmt"
	"github.com/confluentinc/confluent-kafka-go/v2/kafka"
)

func main() {
	fmt.Println("Testing librdkafka with capture server on port 9096")
	
	config := kafka.ConfigMap{
		"bootstrap.servers": "localhost:9096",
		"client.id":         "test-librdkafka",
	}
	
	producer, err := kafka.NewProducer(&config)
	if err != nil {
		fmt.Printf("Failed: %v\n", err)
		return
	}
	defer producer.Close()
	
	fmt.Println("Producer created, getting metadata...")
	
	metadata, err := producer.GetMetadata(nil, false, 1000)
	if err != nil {
		fmt.Printf("GetMetadata failed: %v\n", err)
	} else {
		fmt.Printf("Got metadata: %d brokers\n", len(metadata.Brokers))
	}
}