package main

import (
	"fmt"
	"github.com/confluentinc/confluent-kafka-go/v2/kafka"
)

func main() {
	fmt.Println("Testing librdkafka v2.11.1 with Chronik Stream...")
	
	config := kafka.ConfigMap{
		"bootstrap.servers": "localhost:9092",
		"client.id":         "test-librdkafka",
	}
	
	producer, err := kafka.NewProducer(&config)
	if err != nil {
		fmt.Printf("Failed to create producer: %v\n", err)
		return
	}
	defer producer.Close()
	
	fmt.Println("Producer created successfully!")
	
	// Get metadata to verify connection
	metadata, err := producer.GetMetadata(nil, false, 1000)
	if err != nil {
		fmt.Printf("GetMetadata failed: %v\n", err)
	} else {
		fmt.Printf("Metadata retrieved successfully!\n")
		fmt.Printf("  Brokers: %d\n", len(metadata.Brokers))
		fmt.Printf("  Topics: %d\n", len(metadata.Topics))
		for _, topic := range metadata.Topics {
			fmt.Printf("    - %s (%d partitions)\n", topic.Topic, len(topic.Partitions))
		}
	}
	
	// Try to produce a message
	topic := "test-topic"
	msg := &kafka.Message{
		TopicPartition: kafka.TopicPartition{Topic: &topic, Partition: kafka.PartitionAny},
		Value:          []byte("Hello from librdkafka v2.11.1!"),
	}
	
	err = producer.Produce(msg, nil)
	if err != nil {
		fmt.Printf("Failed to produce message: %v\n", err)
	} else {
		fmt.Println("Message produced successfully!")
		producer.Flush(1000)
	}
}