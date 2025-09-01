package main

import (
	"fmt"
	"log"

	"github.com/confluentinc/confluent-kafka-go/v2/kafka"
)

func main() {
	fmt.Println("Testing Go client through proxy on port 19092...")

	// Create producer configuration - connect to proxy
	config := kafka.ConfigMap{
		"bootstrap.servers": "localhost:19092",
		"client.id":         "go-test-client",
		"debug":             "broker,protocol",
	}

	// Create producer
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
		Value:          []byte("Hello from Go!"),
	}

	err = producer.Produce(message, nil)
	if err != nil {
		log.Fatalf("Failed to produce message: %v", err)
	}
	fmt.Println("✓ Message produced successfully")

	// This is where it crashes - during flush
	fmt.Println("Testing flush operation (previously caused memory corruption)...")
	producer.Flush(5000)
	fmt.Println("✓ Flush completed successfully")
}