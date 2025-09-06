package main

import (
	"fmt"
	"log"

	"github.com/confluentinc/confluent-kafka-go/v2/kafka"
)

func main() {
	// Create producer config
	config := &kafka.ConfigMap{
		"bootstrap.servers": "localhost:29092",
		"client.id":        "go-test-client",
		"debug":           "protocol", // Enable protocol debugging
	}

	// Create producer
	producer, err := kafka.NewProducer(config)
	if err != nil {
		log.Fatalf("Failed to create producer: %v", err)
	}
	defer producer.Close()

	fmt.Println("Producer created successfully")

	// Test topic
	topic := "test-topic"
	
	// Produce a test message
	message := &kafka.Message{
		TopicPartition: kafka.TopicPartition{Topic: &topic, Partition: kafka.PartitionAny},
		Key:            []byte("test-key"),
		Value:          []byte("test-value"),
	}

	// Send the message
	err = producer.Produce(message, nil)
	if err != nil {
		log.Fatalf("Failed to produce message: %v", err)
	}

	fmt.Println("Message queued for delivery")

	// Wait for delivery report
	e := <-producer.Events()
	switch ev := e.(type) {
	case *kafka.Message:
		if ev.TopicPartition.Error != nil {
			fmt.Printf("Delivery failed: %v\n", ev.TopicPartition.Error)
		} else {
			fmt.Printf("Message delivered to %v\n", ev.TopicPartition)
		}
	default:
		fmt.Printf("Unexpected event: %v\n", e)
	}

	// Flush any remaining messages
	producer.Flush(5000)
	
	fmt.Println("Test completed successfully")
}