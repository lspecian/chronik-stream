package main

import (
	"fmt"
	"time"
	"github.com/confluentinc/confluent-kafka-go/v2/kafka"
)

func main() {
	fmt.Println("=== Simple Produce Test ===")
	
	// Create producer without debug output
	producer, err := kafka.NewProducer(&kafka.ConfigMap{
		"bootstrap.servers": "localhost:9092",
		"client.id":         "simple-producer",
	})
	if err != nil {
		fmt.Printf("Failed to create producer: %v\n", err)
		return
	}
	defer producer.Close()
	
	fmt.Println("Producer created successfully")
	
	// Wait for metadata to be fetched
	metadata, err := producer.GetMetadata(nil, true, 5000)
	if err != nil {
		fmt.Printf("Failed to get metadata: %v\n", err)
	} else {
		fmt.Printf("Connected to cluster with %d broker(s):\n", len(metadata.Brokers))
		for _, broker := range metadata.Brokers {
			fmt.Printf("  - %s (id: %d)\n", broker.Host, broker.ID)
		}
		fmt.Printf("Topics available: %d\n", len(metadata.Topics))
		for _, topic := range metadata.Topics {
			if topic.Error.Code() == kafka.ErrNoError {
				fmt.Printf("  - %s (%d partitions)\n", topic.Topic, len(topic.Partitions))
			}
		}
	}
	
	// Try to produce a message
	topic := "test-topic"
	deliveryChan := make(chan kafka.Event)
	
	message := &kafka.Message{
		TopicPartition: kafka.TopicPartition{Topic: &topic, Partition: kafka.PartitionAny},
		Key:            []byte("test-key"),
		Value:          []byte("Hello from librdkafka!"),
	}
	
	fmt.Printf("\nProducing message to topic '%s'...\n", topic)
	err = producer.Produce(message, deliveryChan)
	if err != nil {
		fmt.Printf("Failed to produce message: %v\n", err)
		return
	}
	
	// Wait for delivery report
	select {
	case e := <-deliveryChan:
		m := e.(*kafka.Message)
		if m.TopicPartition.Error != nil {
			fmt.Printf("Delivery failed: %v\n", m.TopicPartition.Error)
		} else {
			fmt.Printf("Message delivered to %s [%d] at offset %v\n",
				*m.TopicPartition.Topic, m.TopicPartition.Partition, m.TopicPartition.Offset)
		}
	case <-time.After(10 * time.Second):
		fmt.Println("Timeout waiting for delivery report")
	}
	
	// Flush any remaining messages
	remaining := producer.Flush(5 * 1000)
	if remaining > 0 {
		fmt.Printf("Warning: %d messages still in queue after flush\n", remaining)
	}
	
	fmt.Println("\nTest complete")
}