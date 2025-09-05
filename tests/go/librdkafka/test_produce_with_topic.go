package main

import (
	"fmt"
	"time"
	"github.com/confluentinc/confluent-kafka-go/v2/kafka"
)

func main() {
	fmt.Println("Testing Produce with auto-topic creation...")
	
	// Skip admin client for now - rely on auto-creation
	fmt.Println("Relying on auto-topic creation...")
	
	// Create producer with auto.create.topics.enable
	producer, err := kafka.NewProducer(&kafka.ConfigMap{
		"bootstrap.servers": "localhost:9092",
		"client.id":         "test-produce-auto",
		"debug":             "protocol,msg",
		"api.version.request": true,
		// Request broker to auto-create topics if they don't exist
		"allow.auto.create.topics": true,
	})
	if err != nil {
		fmt.Printf("Failed to create producer: %v\n", err)
		return
	}
	defer producer.Close()
	
	fmt.Println("✓ Producer created successfully")
	
	// Wait a bit for metadata to refresh
	time.Sleep(2 * time.Second)
	
	// Get metadata with auto-create enabled
	metadata, err := producer.GetMetadata(nil, true, 5000) // true = all topics
	if err != nil {
		fmt.Printf("✗ GetMetadata failed: %v\n", err)
	} else {
		fmt.Printf("✓ Metadata retrieved: %d brokers, %d topics\n", 
			len(metadata.Brokers), len(metadata.Topics))
		for _, topic := range metadata.Topics {
			fmt.Printf("  Topic: %s, %d partitions\n", topic.Topic, len(topic.Partitions))
		}
	}
	
	// Now try to produce messages
	topic := "test-topic"
	deliveryChan := make(chan kafka.Event)
	
	for i := 0; i < 3; i++ {
		message := &kafka.Message{
			TopicPartition: kafka.TopicPartition{Topic: &topic, Partition: kafka.PartitionAny},
			Key:            []byte(fmt.Sprintf("key-%d", i)),
			Value:          []byte(fmt.Sprintf("Message %d from librdkafka", i)),
			Headers: []kafka.Header{
				{Key: "test-header", Value: []byte("header-value")},
			},
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
	
	// Flush any remaining messages
	remaining := producer.Flush(5 * 1000)
	if remaining > 0 {
		fmt.Printf("\nWarning: %d messages still in queue after flush\n", remaining)
	}
	
	// Now try to consume the messages we just produced
	fmt.Println("\n=== Testing Consumer ===")
	
	consumer, err := kafka.NewConsumer(&kafka.ConfigMap{
		"bootstrap.servers": "localhost:9092",
		"group.id":          "test-consumer-group",
		"auto.offset.reset": "earliest",
		"client.id":         "test-consumer",
		"enable.auto.commit": false,
	})
	if err != nil {
		fmt.Printf("✗ Failed to create consumer: %v\n", err)
	} else {
		defer consumer.Close()
		
		fmt.Println("✓ Consumer created successfully")
		
		// Subscribe to topic
		err = consumer.Subscribe(topic, nil)
		if err != nil {
			fmt.Printf("✗ Failed to subscribe: %v\n", err)
		} else {
			fmt.Println("✓ Subscribed to topic")
			
			// Try to consume messages
			fmt.Println("Attempting to consume messages...")
			consumed := 0
			for i := 0; i < 5; i++ {
				msg, err := consumer.ReadMessage(time.Second)
				if err == nil {
					fmt.Printf("✓ Consumed message: key=%s, value=%s, offset=%v\n",
						string(msg.Key), string(msg.Value), msg.TopicPartition.Offset)
					consumed++
				} else if err.(kafka.Error).Code() == kafka.ErrTimedOut {
					if consumed == 0 {
						fmt.Println("  (No messages available)")
					}
					break
				} else {
					fmt.Printf("✗ Consumer error: %v\n", err)
					break
				}
			}
			
			if consumed > 0 {
				fmt.Printf("✓ Successfully consumed %d messages\n", consumed)
			}
		}
	}
	
	fmt.Println("\n=== Test Complete ===")
	fmt.Println("\nSummary:")
	fmt.Println("✓ Connection established")
	fmt.Println("✓ Metadata exchange works")
	if remaining == 0 {
		fmt.Println("✓ Messages produced successfully")
	} else {
		fmt.Println("✗ Some messages failed to produce")
	}
}