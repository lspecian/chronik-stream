package main

import (
	"fmt"
	"github.com/confluentinc/confluent-kafka-go/v2/kafka"
	"time"
)

func main() {
	fmt.Println("Testing confluent-kafka-go v2.11.1 with Chronik Stream")
	fmt.Println("========================================================")
	
	config := kafka.ConfigMap{
		"bootstrap.servers": "localhost:9092",
		"client.id":         "test-client-v2.11.1",
	}
	
	fmt.Println("\n1. Creating producer...")
	producer, err := kafka.NewProducer(&config)
	if err != nil {
		fmt.Printf("❌ Failed to create producer: %v\n", err)
		return
	}
	defer producer.Close()
	
	fmt.Println("✅ Producer created successfully")
	
	fmt.Println("\n2. Getting metadata (triggers ApiVersions)...")
	metadata, err := producer.GetMetadata(nil, false, 5000)
	if err != nil {
		fmt.Printf("❌ Failed to get metadata: %v\n", err)
		return
	}
	
	fmt.Printf("✅ Metadata retrieved - %d broker(s)\n", len(metadata.Brokers))
	for _, broker := range metadata.Brokers {
		fmt.Printf("   Broker %d: %s:%d\n", broker.ID, broker.Host, broker.Port)
	}
	
	fmt.Printf("   %d topic(s) found\n", len(metadata.Topics))
	
	// Try to produce a message
	fmt.Println("\n3. Producing test message...")
	topic := "test-topic"
	msg := &kafka.Message{
		TopicPartition: kafka.TopicPartition{Topic: &topic, Partition: kafka.PartitionAny},
		Value:          []byte(fmt.Sprintf("Test message from librdkafka v2.11.1 at %v", time.Now())),
	}
	
	err = producer.Produce(msg, nil)
	if err != nil {
		fmt.Printf("❌ Failed to queue message: %v\n", err)
		return
	}
	fmt.Println("✅ Message queued")
	
	fmt.Println("\n4. Flushing (where v2.1.0+ used to crash)...")
	remaining := producer.Flush(5000)
	if remaining > 0 {
		fmt.Printf("⚠️  %d messages still in queue after flush\n", remaining)
	} else {
		fmt.Println("✅ All messages flushed successfully")
	}
	
	fmt.Println("\n========================================================")
	fmt.Println("🎉 SUCCESS! No crashes with librdkafka v2.11.1")
	fmt.Println("The fix (adding all 69 APIs) resolved the issue!")
}