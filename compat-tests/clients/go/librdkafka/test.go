package main

import (
	"encoding/json"
	"fmt"
	"os"
	"time"

	"github.com/confluentinc/confluent-kafka-go/v2/kafka"
	"github.com/google/uuid"
)

type TestResult struct {
	Test   string `json:"test"`
	API    string `json:"api"`
	Passed bool   `json:"passed"`
	Client string `json:"client"`
}

type TestSummary struct {
	Client    string       `json:"client"`
	Version   string       `json:"version"`
	Timestamp string       `json:"timestamp"`
	Tests     []TestResult `json:"tests"`
	Summary   struct {
		Total  int `json:"total"`
		Passed int `json:"passed"`
		Failed int `json:"failed"`
	} `json:"summary"`
}

func logMessage(level, message string) {
	entry := map[string]interface{}{
		"timestamp": time.Now().UTC().Format(time.RFC3339),
		"level":     level,
		"client":    "confluent-kafka-go",
		"message":   message,
	}
	data, _ := json.Marshal(entry)
	fmt.Println(string(data))
}

func testApiVersions(brokers string) (bool, string) {
	config := &kafka.ConfigMap{
		"bootstrap.servers":            brokers,
		"socket.timeout.ms":            5000,
		"api.version.request":          true,
		"api.version.request.timeout.ms": 5000,
		"debug":                        "broker",
	}
	
	producer, err := kafka.NewProducer(config)
	if err != nil {
		// Try with fallback
		logMessage("WARNING", fmt.Sprintf("ApiVersions v3 failed, trying v0 fallback: %v", err))
		config["api.version.fallback.ms"] = 0
		producer, err = kafka.NewProducer(config)
		if err != nil {
			logMessage("ERROR", fmt.Sprintf("ApiVersions test FAILED: %v", err))
			return false, "ApiVersions"
		}
	}
	defer producer.Close()
	
	// Get metadata
	metadata, err := producer.GetMetadata(nil, false, 5000)
	if err != nil {
		logMessage("ERROR", fmt.Sprintf("ApiVersions test FAILED: %v", err))
		return false, "ApiVersions"
	}
	
	if len(metadata.Brokers) > 0 {
		logMessage("INFO", fmt.Sprintf("ApiVersions test PASSED - connected to %d brokers", len(metadata.Brokers)))
		return true, "ApiVersions"
	}
	
	logMessage("ERROR", "ApiVersions test FAILED - no brokers found")
	return false, "ApiVersions"
}

func testMetadata(brokers string) (bool, string) {
	producer, err := kafka.NewProducer(&kafka.ConfigMap{
		"bootstrap.servers": brokers,
	})
	if err != nil {
		logMessage("ERROR", fmt.Sprintf("Metadata test FAILED: %v", err))
		return false, "Metadata"
	}
	defer producer.Close()
	
	metadata, err := producer.GetMetadata(nil, true, 5000)
	if err != nil {
		logMessage("ERROR", fmt.Sprintf("Metadata test FAILED: %v", err))
		return false, "Metadata"
	}
	
	logMessage("INFO", fmt.Sprintf("Metadata test PASSED - %d topics, %d brokers", 
		len(metadata.Topics), len(metadata.Brokers)))
	return true, "Metadata"
}

func testProduce(brokers string) (bool, string) {
	producer, err := kafka.NewProducer(&kafka.ConfigMap{
		"bootstrap.servers": brokers,
		"client.id":         "confluent-go-test",
		"acks":              "all",
	})
	if err != nil {
		logMessage("ERROR", fmt.Sprintf("Produce test FAILED: %v", err))
		return false, "Produce"
	}
	defer producer.Close()
	
	topic := fmt.Sprintf("test-%s", uuid.New().String()[:8])
	value := fmt.Sprintf(`{"client":"confluent-kafka-go","timestamp":%d,"test":"produce"}`, 
		time.Now().Unix())
	
	deliveryChan := make(chan kafka.Event, 1)
	err = producer.Produce(&kafka.Message{
		TopicPartition: kafka.TopicPartition{Topic: &topic, Partition: kafka.PartitionAny},
		Value:          []byte(value),
	}, deliveryChan)
	
	if err != nil {
		logMessage("ERROR", fmt.Sprintf("Produce test FAILED: %v", err))
		return false, "Produce"
	}
	
	// Wait for delivery
	select {
	case e := <-deliveryChan:
		m := e.(*kafka.Message)
		if m.TopicPartition.Error != nil {
			logMessage("ERROR", fmt.Sprintf("Produce test FAILED: %v", m.TopicPartition.Error))
			return false, "Produce"
		}
		logMessage("INFO", fmt.Sprintf("Produce test PASSED - offset: %v", m.TopicPartition.Offset))
		return true, "Produce"
	case <-time.After(10 * time.Second):
		logMessage("ERROR", "Produce test FAILED - timeout")
		return false, "Produce"
	}
}

func testFetch(brokers string) (bool, string) {
	// First produce
	producer, _ := kafka.NewProducer(&kafka.ConfigMap{
		"bootstrap.servers": brokers,
	})
	
	topic := fmt.Sprintf("test-fetch-%s", uuid.New().String()[:8])
	testID := uuid.New().String()
	value := fmt.Sprintf(`{"test":"fetch","id":"%s"}`, testID)
	
	producer.Produce(&kafka.Message{
		TopicPartition: kafka.TopicPartition{Topic: &topic, Partition: kafka.PartitionAny},
		Value:          []byte(value),
	}, nil)
	producer.Flush(5000)
	producer.Close()
	
	// Now consume
	consumer, err := kafka.NewConsumer(&kafka.ConfigMap{
		"bootstrap.servers": brokers,
		"group.id":          fmt.Sprintf("test-%s", uuid.New().String()[:8]),
		"auto.offset.reset": "earliest",
	})
	if err != nil {
		logMessage("ERROR", fmt.Sprintf("Fetch test FAILED: %v", err))
		return false, "Fetch"
	}
	defer consumer.Close()
	
	consumer.Subscribe(topic, nil)
	
	timeout := time.After(10 * time.Second)
	for {
		select {
		case <-timeout:
			logMessage("ERROR", "Fetch test FAILED - timeout")
			return false, "Fetch"
		default:
			msg, err := consumer.ReadMessage(100 * time.Millisecond)
			if err != nil {
				continue
			}
			
			var data map[string]interface{}
			if json.Unmarshal(msg.Value, &data) == nil {
				if data["id"] == testID {
					logMessage("INFO", "Fetch test PASSED")
					return true, "Fetch"
				}
			}
		}
	}
}

func testConsumerGroup(brokers string) (bool, string) {
	topic := fmt.Sprintf("test-group-%s", uuid.New().String()[:8])
	groupID := fmt.Sprintf("test-group-%s", uuid.New().String()[:8])
	
	// Produce messages
	producer, _ := kafka.NewProducer(&kafka.ConfigMap{
		"bootstrap.servers": brokers,
	})
	
	for i := 0; i < 5; i++ {
		value := fmt.Sprintf(`{"msg":%d}`, i)
		producer.Produce(&kafka.Message{
			TopicPartition: kafka.TopicPartition{Topic: &topic, Partition: kafka.PartitionAny},
			Value:          []byte(value),
		}, nil)
	}
	producer.Flush(5000)
	producer.Close()
	
	// Consumer group
	consumer, err := kafka.NewConsumer(&kafka.ConfigMap{
		"bootstrap.servers":  brokers,
		"group.id":           groupID,
		"auto.offset.reset":  "earliest",
		"enable.auto.commit": true,
	})
	if err != nil {
		logMessage("ERROR", fmt.Sprintf("ConsumerGroup test FAILED: %v", err))
		return false, "ConsumerGroup"
	}
	defer consumer.Close()
	
	consumer.Subscribe(topic, nil)
	
	msgCount := 0
	timeout := time.After(10 * time.Second)
	
	for msgCount < 5 {
		select {
		case <-timeout:
			logMessage("ERROR", fmt.Sprintf("ConsumerGroup test FAILED - only got %d messages", msgCount))
			return false, "ConsumerGroup"
		default:
			msg, err := consumer.ReadMessage(100 * time.Millisecond)
			if err == nil {
				msgCount++
			}
		}
	}
	
	consumer.CommitOffsets(nil)
	logMessage("INFO", fmt.Sprintf("ConsumerGroup test PASSED - consumed %d messages", msgCount))
	return true, "ConsumerGroup"
}

func testProduceV2Regression(brokers string) (bool, string) {
	// Test ProduceResponse v2 with specific version
	producer, err := kafka.NewProducer(&kafka.ConfigMap{
		"bootstrap.servers":       brokers,
		"broker.version.fallback": "0.10.0", // Forces ProduceRequest v2
		"api.version.request":     true,
	})
	if err != nil {
		logMessage("ERROR", fmt.Sprintf("ProduceV2 regression test FAILED: %v", err))
		return false, "ProduceV2"
	}
	defer producer.Close()
	
	topic := fmt.Sprintf("test-v2-%s", uuid.New().String()[:8])
	
	deliveryChan := make(chan kafka.Event, 1)
	err = producer.Produce(&kafka.Message{
		TopicPartition: kafka.TopicPartition{Topic: &topic, Partition: kafka.PartitionAny},
		Value:          []byte("test-v2-regression"),
	}, deliveryChan)
	
	if err != nil {
		logMessage("ERROR", fmt.Sprintf("ProduceV2 regression test FAILED: %v", err))
		return false, "ProduceV2"
	}
	
	// This would timeout with the bug
	select {
	case e := <-deliveryChan:
		m := e.(*kafka.Message)
		if m.TopicPartition.Error != nil {
			logMessage("ERROR", fmt.Sprintf("ProduceV2 regression test FAILED: %v", m.TopicPartition.Error))
			return false, "ProduceV2"
		}
		logMessage("INFO", "ProduceV2 regression test PASSED")
		return true, "ProduceV2"
	case <-time.After(10 * time.Second):
		logMessage("ERROR", "ProduceV2 regression test FAILED - timeout")
		return false, "ProduceV2"
	}
}

func main() {
	brokers := os.Getenv("BOOTSTRAP_SERVERS")
	if brokers == "" {
		brokers = "chronik:9092"
	}
	
	logMessage("INFO", fmt.Sprintf("Starting confluent-kafka-go tests against %s", brokers))
	
	// Get version
	version := fmt.Sprintf("librdkafka %s", kafka.LibraryVersion())
	logMessage("INFO", fmt.Sprintf("Using %s", version))
	
	// Run tests
	tests := []struct {
		name string
		fn   func(string) (bool, string)
	}{
		{"ApiVersions", testApiVersions},
		{"Metadata", testMetadata},
		{"Produce", testProduce},
		{"Fetch", testFetch},
		{"ConsumerGroup", testConsumerGroup},
		{"ProduceV2Regression", testProduceV2Regression},
	}
	
	var results []TestResult
	passedCount := 0
	
	for _, test := range tests {
		logMessage("INFO", fmt.Sprintf("Running %s test...", test.name))
		passed, api := test.fn(brokers)
		results = append(results, TestResult{
			Test:   test.name,
			API:    api,
			Passed: passed,
			Client: "confluent-kafka-go",
		})
		if passed {
			passedCount++
		}
	}
	
	// Write results
	summary := TestSummary{
		Client:    "confluent-kafka-go",
		Version:   version,
		Timestamp: time.Now().UTC().Format(time.RFC3339),
		Tests:     results,
	}
	summary.Summary.Total = len(results)
	summary.Summary.Passed = passedCount
	summary.Summary.Failed = len(results) - passedCount
	
	logMessage("INFO", fmt.Sprintf("Test summary: %d/%d passed", passedCount, len(results)))
	
	// Save to file
	resultFile := "/results/confluent-kafka-go-results.json"
	file, _ := os.Create(resultFile)
	encoder := json.NewEncoder(file)
	encoder.SetIndent("", "  ")
	encoder.Encode(summary)
	file.Close()
	logMessage("INFO", fmt.Sprintf("Results written to %s", resultFile))
	
	if passedCount < len(results) {
		os.Exit(1)
	}
}