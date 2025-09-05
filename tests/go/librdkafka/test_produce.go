package main

import (
	"fmt"
	"time"
	"github.com/confluentinc/confluent-kafka-go/v2/kafka"
)

func main() {
	fmt.Println("Testing Produce API v9 with librdkafka v2.11.1...")
	
	// Create producer with debug logging
	producer, err := kafka.NewProducer(&kafka.ConfigMap{
		"bootstrap.servers": "localhost:9092",
		"client.id":         "test-produce",
		"debug":             "protocol,msg", // Enable debug logging
		"api.version.request": true,
	})
	if err != nil {
		fmt.Printf("Failed to create producer: %v\n", err)
		return
	}
	defer producer.Close()
	
	fmt.Println("✓ Producer created successfully")
	
	// Get metadata first
	metadata, err := producer.GetMetadata(nil, false, 5000)
	if err != nil {
		fmt.Printf("✗ GetMetadata failed: %v\n", err)
	} else {
		fmt.Printf("✓ Metadata retrieved: %d brokers, %d topics\n", 
			len(metadata.Brokers), len(metadata.Topics))
	}
	
	// Produce a test message
	topic := "test-topic"
	deliveryChan := make(chan kafka.Event)
	
	message := &kafka.Message{
		TopicPartition: kafka.TopicPartition{Topic: &topic, Partition: kafka.PartitionAny},
		Value:          []byte("Test message from librdkafka v2.11.1"),
	}
	
	fmt.Println("\nProducing message...")
	err = producer.Produce(message, deliveryChan)
	if err != nil {
		fmt.Printf("✗ Failed to produce message: %v\n", err)
		return
	}
	
	// Wait for delivery report
	select {
	case e := <-deliveryChan:
		m := e.(*kafka.Message)
		
		if m.TopicPartition.Error != nil {
			fmt.Printf("✗ Delivery failed: %v\n", m.TopicPartition.Error)
		} else {
			fmt.Printf("✓ Message delivered to topic %s [%d] at offset %v\n",
				*m.TopicPartition.Topic, m.TopicPartition.Partition, m.TopicPartition.Offset)
		}
	case <-time.After(10 * time.Second):
		fmt.Println("✗ Timeout waiting for delivery report")
	}
	
	// Flush any remaining messages
	producer.Flush(5 * 1000)
	
	fmt.Println("\n=== Test Complete ===")
}