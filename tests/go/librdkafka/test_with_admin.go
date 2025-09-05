package main

import (
	"context"
	"fmt"
	"time"
	"github.com/confluentinc/confluent-kafka-go/v2/kafka"
)

func main() {
	fmt.Println("Testing with Admin Client to create topic first...")
	
	// First create the topic using admin client
	adminClient, err := kafka.NewAdminClient(&kafka.ConfigMap{
		"bootstrap.servers": "localhost:9092",
		"client.id":         "test-admin",
	})
	if err != nil {
		fmt.Printf("Failed to create admin client: %v\n", err)
		return
	}
	defer adminClient.Close()
	
	fmt.Println("✓ Admin client created")
	
	// Create topic
	topicSpec := kafka.TopicSpecification{
		Topic:             "test-topic",
		NumPartitions:     3,
		ReplicationFactor: 1,
	}
	
	fmt.Println("Creating topic 'test-topic'...")
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	results, err := adminClient.CreateTopics(
		ctx,
		[]kafka.TopicSpecification{topicSpec},
		kafka.SetAdminOperationTimeout(5*time.Second),
	)
	
	if err != nil {
		fmt.Printf("✗ CreateTopics request failed: %v\n", err)
	} else {
		for _, result := range results {
			if result.Error.Code() != kafka.ErrNoError {
				// Check if it's already exists error
				if result.Error.Code() == kafka.ErrTopicAlreadyExists {
					fmt.Printf("ℹ Topic %s already exists\n", result.Topic)
				} else {
					fmt.Printf("✗ Failed to create topic %s: %v\n", result.Topic, result.Error)
				}
			} else {
				fmt.Printf("✓ Topic %s created successfully\n", result.Topic)
			}
		}
	}
	
	// Wait for topic to be available
	time.Sleep(2 * time.Second)
	
	// Now test produce
	fmt.Println("\n=== Testing Produce ===")
	
	producer, err := kafka.NewProducer(&kafka.ConfigMap{
		"bootstrap.servers": "localhost:9092",
		"client.id":         "test-producer",
		"debug":             "protocol,msg",
	})
	if err != nil {
		fmt.Printf("Failed to create producer: %v\n", err)
		return
	}
	defer producer.Close()
	
	fmt.Println("✓ Producer created")
	
	// Produce messages
	topic := "test-topic"
	deliveryChan := make(chan kafka.Event)
	
	for i := 0; i < 3; i++ {
		message := &kafka.Message{
			TopicPartition: kafka.TopicPartition{Topic: &topic, Partition: kafka.PartitionAny},
			Key:            []byte(fmt.Sprintf("key-%d", i)),
			Value:          []byte(fmt.Sprintf("Message %d from librdkafka", i)),
		}
		
		fmt.Printf("\nProducing message %d...\n", i)
		err = producer.Produce(message, deliveryChan)
		if err != nil {
			fmt.Printf("✗ Failed to produce message %d: %v\n", i, err)
			continue
		}
		
		// Wait for delivery report
		select {
		case e := <-deliveryChan:
			m := e.(*kafka.Message)
			
			if m.TopicPartition.Error != nil {
				fmt.Printf("✗ Delivery failed for message %d: %v\n", i, m.TopicPartition.Error)
			} else {
				fmt.Printf("✓ Message %d delivered to topic %s [%d] at offset %v\n",
					i, *m.TopicPartition.Topic, m.TopicPartition.Partition, m.TopicPartition.Offset)
			}
		case <-time.After(5 * time.Second):
			fmt.Printf("✗ Timeout waiting for delivery report for message %d\n", i)
		}
	}
	
	// Flush
	remaining := producer.Flush(5 * 1000)
	if remaining > 0 {
		fmt.Printf("\nWarning: %d messages still in queue after flush\n", remaining)
	}
	
	fmt.Println("\n=== Test Complete ===")
}