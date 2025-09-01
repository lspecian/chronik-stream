package main

import (
	"fmt"
	"log"
	"os"
	"time"
	"github.com/confluentinc/confluent-kafka-go/v2/kafka"
)

func main() {
	fmt.Println("Testing librdkafka debug mode with Chronik Stream...")
	
	// Enable debug logging
	config := kafka.ConfigMap{
		"bootstrap.servers": "localhost:9092",
		"client.id":        "go-debug-client",
		"debug":           "all", // Enable all debug logging
		"api.version.request": true,
		"api.version.request.timeout.ms": 10000,
		"socket.timeout.ms": 10000,
		"request.timeout.ms": 10000,
	}
	
	// Set librdkafka log level
	os.Setenv("KAFKA_DEBUG", "all")
	
	producer, err := kafka.NewProducer(&config)
	if err != nil {
		log.Fatalf("Failed to create producer: %v", err)
	}
	defer producer.Close()
	
	fmt.Println("Producer created, sending message...")
	
	// Try to produce a message
	topic := "test-topic"
	message := &kafka.Message{
		TopicPartition: kafka.TopicPartition{Topic: &topic, Partition: kafka.PartitionAny},
		Value:          []byte("Test message"),
	}
	
	err = producer.Produce(message, nil)
	if err != nil {
		log.Printf("Failed to produce message: %v", err)
	}
	
	fmt.Println("Waiting for events...")
	
	// Process events
	go func() {
		for e := range producer.Events() {
			switch ev := e.(type) {
			case *kafka.Message:
				if ev.TopicPartition.Error != nil {
					fmt.Printf("Delivery failed: %v\n", ev.TopicPartition.Error)
				} else {
					fmt.Printf("Delivered message to %v\n", ev.TopicPartition)
				}
			case kafka.Error:
				fmt.Printf("Error: %v\n", ev)
			default:
				fmt.Printf("Event: %v\n", ev)
			}
		}
	}()
	
	// Sleep a bit to see events
	time.Sleep(2 * time.Second)
	
	fmt.Println("Attempting flush (this is where it crashes)...")
	producer.Flush(5000)
	
	fmt.Println("✓ Test completed successfully!")
}