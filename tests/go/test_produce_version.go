package main

import (
	"fmt"
	"log"

	"github.com/confluentinc/confluent-kafka-go/v2/kafka"
)

func main() {
	// Create a producer with debug enabled
	producer, err := kafka.NewProducer(&kafka.ConfigMap{
		"bootstrap.servers": "localhost:9092",
		"client.id":         "go-version-test",
		"debug":             "protocol,broker", // Enable protocol debugging
		"api.version.request": true,  // Request API versions
	})
	
	if err != nil {
		log.Fatalf("Failed to create producer: %v", err)
	}
	defer producer.Close()
	
	fmt.Println("Producer created. Check debug output for API versions used...")
	
	// Produce a message to trigger protocol negotiation
	topic := "test-topic"
	err = producer.Produce(&kafka.Message{
		TopicPartition: kafka.TopicPartition{Topic: &topic, Partition: kafka.PartitionAny},
		Value:          []byte("test"),
	}, nil)
	
	if err != nil {
		log.Fatalf("Failed to produce: %v", err)
	}
	
	// Process events to see debug output
	for e := range producer.Events() {
		switch ev := e.(type) {
		case *kafka.Message:
			if ev.TopicPartition.Error != nil {
				fmt.Printf("Delivery failed: %v\n", ev.TopicPartition.Error)
			} else {
				fmt.Printf("Delivered message to %v\n", ev.TopicPartition)
			}
			return
		case kafka.Error:
			fmt.Printf("Error: %v\n", ev)
			if ev.IsFatal() {
				return
			}
		default:
			fmt.Printf("Event: %v\n", ev)
		}
	}
}