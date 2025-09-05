package main

import (
	"fmt"
	"log"
	"time"

	"github.com/confluentinc/confluent-kafka-go/v2/kafka"
)

func main() {
	fmt.Println("=== Testing librdkafka Produce with Fixed Server ===")

	// Create producer configuration
	config := &kafka.ConfigMap{
		"bootstrap.servers": "localhost:9092",
		"client.id":         "test-producer",
		"debug":             "broker,protocol", // Enable debug for detailed output
	}

	// Create producer
	producer, err := kafka.NewProducer(config)
	if err != nil {
		log.Fatalf("Failed to create producer: %v", err)
	}
	defer producer.Close()

	// Wait for metadata
	time.Sleep(1 * time.Second)

	// Get metadata
	metadata, err := producer.GetMetadata(nil, false, 5000)
	if err != nil {
		log.Printf("Failed to get metadata: %v", err)
	} else {
		fmt.Printf("Connected to cluster with %d broker(s):\n", len(metadata.Brokers))
		for _, broker := range metadata.Brokers {
			fmt.Printf("  - %s (id: %d)\n", broker.Host, broker.ID)
		}
		fmt.Printf("Topics available: %d\n", len(metadata.Topics))
		for topic, _ := range metadata.Topics {
			partitions := metadata.Topics[topic]
			fmt.Printf("  - %s (%d partitions)\n", topic, len(partitions.Partitions))
		}
	}

	// Produce a message
	topic := "test-topic"
	fmt.Printf("\nProducing message to topic '%s'...\n", topic)

	deliveryChan := make(chan kafka.Event)
	message := &kafka.Message{
		TopicPartition: kafka.TopicPartition{Topic: &topic, Partition: kafka.PartitionAny},
		Key:            []byte("test-key"),
		Value:          []byte("Hello from librdkafka with fixed server!"),
	}

	err = producer.Produce(message, deliveryChan)
	if err != nil {
		log.Fatalf("Failed to produce message: %v", err)
	}

	// Wait for delivery report
	select {
	case e := <-deliveryChan:
		m := e.(*kafka.Message)
		if m.TopicPartition.Error != nil {
			fmt.Printf("❌ Delivery failed: %v\n", m.TopicPartition.Error)
		} else {
			fmt.Printf("✅ Message delivered successfully to %s [%d] at offset %v\n",
				*m.TopicPartition.Topic, m.TopicPartition.Partition, m.TopicPartition.Offset)
		}
	case <-time.After(10 * time.Second):
		fmt.Println("❌ Timeout waiting for delivery report")
	}

	// Flush any remaining messages
	remaining := producer.Flush(5000)
	if remaining > 0 {
		fmt.Printf("Warning: %d messages still in queue after flush\n", remaining)
	}

	fmt.Println("\nTest complete!")
}