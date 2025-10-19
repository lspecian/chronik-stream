package main

import (
	"encoding/json"
	"fmt"
	"log"
	"os"
	"time"

	"github.com/IBM/sarama"
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
		"client":    "sarama",
		"message":   message,
	}
	data, _ := json.Marshal(entry)
	fmt.Println(string(data))
}

func testApiVersions(brokers []string) (bool, string) {
	config := sarama.NewConfig()
	config.Version = sarama.V2_6_0_0
	config.ClientID = "sarama-test"
	
	client, err := sarama.NewClient(brokers, config)
	if err != nil {
		logMessage("ERROR", fmt.Sprintf("ApiVersions test FAILED: %v", err))
		return false, "ApiVersions"
	}
	defer client.Close()
	
	// Check if we can get broker info
	brokerList := client.Brokers()
	if len(brokerList) > 0 {
		logMessage("INFO", fmt.Sprintf("ApiVersions test PASSED - connected to %d brokers", len(brokerList)))
		return true, "ApiVersions"
	}
	
	logMessage("ERROR", "ApiVersions test FAILED - no brokers found")
	return false, "ApiVersions"
}

func testMetadata(brokers []string) (bool, string) {
	config := sarama.NewConfig()
	config.Version = sarama.V2_6_0_0
	
	client, err := sarama.NewClient(brokers, config)
	if err != nil {
		logMessage("ERROR", fmt.Sprintf("Metadata test FAILED: %v", err))
		return false, "Metadata"
	}
	defer client.Close()
	
	topics, err := client.Topics()
	if err != nil {
		logMessage("ERROR", fmt.Sprintf("Metadata test FAILED: %v", err))
		return false, "Metadata"
	}
	
	logMessage("INFO", fmt.Sprintf("Metadata test PASSED - found %d topics", len(topics)))
	return true, "Metadata"
}

func testProduce(brokers []string) (bool, string) {
	config := sarama.NewConfig()
	config.Version = sarama.V2_6_0_0
	config.Producer.Return.Successes = true
	config.Producer.RequiredAcks = sarama.WaitForAll
	config.Producer.Retry.Max = 5
	
	producer, err := sarama.NewSyncProducer(brokers, config)
	if err != nil {
		logMessage("ERROR", fmt.Sprintf("Produce test FAILED: %v", err))
		return false, "Produce"
	}
	defer producer.Close()
	
	topic := fmt.Sprintf("test-%s", uuid.New().String()[:8])
	message := &sarama.ProducerMessage{
		Topic: topic,
		Value: sarama.StringEncoder(fmt.Sprintf(`{"client":"sarama","timestamp":%d,"test":"produce"}`, time.Now().Unix())),
	}
	
	partition, offset, err := producer.SendMessage(message)
	if err != nil {
		logMessage("ERROR", fmt.Sprintf("Produce test FAILED: %v", err))
		return false, "Produce"
	}
	
	logMessage("INFO", fmt.Sprintf("Produce test PASSED - partition: %d, offset: %d", partition, offset))
	return true, "Produce"
}

func testFetch(brokers []string) (bool, string) {
	config := sarama.NewConfig()
	config.Version = sarama.V2_6_0_0
	config.Producer.Return.Successes = true
	config.Consumer.Return.Errors = true
	
	// First produce a message
	producer, err := sarama.NewSyncProducer(brokers, config)
	if err != nil {
		logMessage("ERROR", fmt.Sprintf("Fetch test FAILED (producer): %v", err))
		return false, "Fetch"
	}
	
	topic := fmt.Sprintf("test-fetch-%s", uuid.New().String()[:8])
	testID := uuid.New().String()
	
	message := &sarama.ProducerMessage{
		Topic: topic,
		Value: sarama.StringEncoder(fmt.Sprintf(`{"test":"fetch","id":"%s"}`, testID)),
	}
	
	_, _, err = producer.SendMessage(message)
	producer.Close()
	
	if err != nil {
		logMessage("ERROR", fmt.Sprintf("Fetch test FAILED (produce): %v", err))
		return false, "Fetch"
	}
	
	// Now consume it
	consumer, err := sarama.NewConsumer(brokers, config)
	if err != nil {
		logMessage("ERROR", fmt.Sprintf("Fetch test FAILED (consumer): %v", err))
		return false, "Fetch"
	}
	defer consumer.Close()
	
	partitionConsumer, err := consumer.ConsumePartition(topic, 0, sarama.OffsetOldest)
	if err != nil {
		logMessage("ERROR", fmt.Sprintf("Fetch test FAILED (partition): %v", err))
		return false, "Fetch"
	}
	defer partitionConsumer.Close()
	
	found := false
	timeout := time.After(10 * time.Second)
	
	for !found {
		select {
		case msg := <-partitionConsumer.Messages():
			var data map[string]interface{}
			if err := json.Unmarshal(msg.Value, &data); err == nil {
				if data["id"] == testID {
					found = true
				}
			}
		case <-timeout:
			logMessage("ERROR", "Fetch test FAILED - timeout")
			return false, "Fetch"
		}
	}
	
	logMessage("INFO", "Fetch test PASSED")
	return true, "Fetch"
}

func testConsumerGroup(brokers []string) (bool, string) {
	config := sarama.NewConfig()
	config.Version = sarama.V2_6_0_0
	config.Consumer.Group.Rebalance.Strategy = sarama.BalanceStrategyRoundRobin
	config.Consumer.Offsets.Initial = sarama.OffsetOldest
	
	topic := fmt.Sprintf("test-group-%s", uuid.New().String()[:8])
	groupID := fmt.Sprintf("test-group-%s", uuid.New().String()[:8])
	
	// First produce messages
	producer, err := sarama.NewSyncProducer(brokers, config)
	if err != nil {
		logMessage("ERROR", fmt.Sprintf("ConsumerGroup test FAILED (producer): %v", err))
		return false, "ConsumerGroup"
	}
	
	for i := 0; i < 5; i++ {
		message := &sarama.ProducerMessage{
			Topic: topic,
			Value: sarama.StringEncoder(fmt.Sprintf(`{"msg":%d}`, i)),
		}
		producer.SendMessage(message)
	}
	producer.Close()
	
	// Create consumer group
	group, err := sarama.NewConsumerGroup(brokers, groupID, config)
	if err != nil {
		logMessage("ERROR", fmt.Sprintf("ConsumerGroup test FAILED: %v", err))
		return false, "ConsumerGroup"
	}
	defer group.Close()
	
	// Simple consumer handler
	handler := &consumerGroupHandler{
		messageCount: 0,
		done:         make(chan bool),
	}
	
	go func() {
		for {
			err := group.Consume(nil, []string{topic}, handler)
			if err != nil {
				logMessage("ERROR", fmt.Sprintf("ConsumerGroup consume error: %v", err))
				return
			}
		}
	}()
	
	// Wait for messages or timeout
	select {
	case <-handler.done:
		if handler.messageCount >= 5 {
			logMessage("INFO", fmt.Sprintf("ConsumerGroup test PASSED - consumed %d messages", handler.messageCount))
			return true, "ConsumerGroup"
		}
	case <-time.After(10 * time.Second):
		logMessage("ERROR", fmt.Sprintf("ConsumerGroup test FAILED - only got %d messages", handler.messageCount))
		return false, "ConsumerGroup"
	}
	
	return false, "ConsumerGroup"
}

type consumerGroupHandler struct {
	messageCount int
	done         chan bool
}

func (h *consumerGroupHandler) Setup(sarama.ConsumerGroupSession) error   { return nil }
func (h *consumerGroupHandler) Cleanup(sarama.ConsumerGroupSession) error { return nil }
func (h *consumerGroupHandler) ConsumeClaim(session sarama.ConsumerGroupSession, claim sarama.ConsumerGroupClaim) error {
	for message := range claim.Messages() {
		h.messageCount++
		session.MarkMessage(message, "")
		if h.messageCount >= 5 {
			close(h.done)
			return nil
		}
	}
	return nil
}

func main() {
	brokers := []string{os.Getenv("BOOTSTRAP_SERVERS")}
	if brokers[0] == "" {
		brokers = []string{"chronik:9092"}
	}
	
	logMessage("INFO", fmt.Sprintf("Starting Sarama tests against %v", brokers))
	
	// Run tests
	tests := []struct {
		name string
		fn   func([]string) (bool, string)
	}{
		{"ApiVersions", testApiVersions},
		{"Metadata", testMetadata},
		{"Produce", testProduce},
		{"Fetch", testFetch},
		{"ConsumerGroup", testConsumerGroup},
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
			Client: "sarama",
		})
		if passed {
			passedCount++
		}
	}
	
	// Write results
	summary := TestSummary{
		Client:    "sarama",
		Version:   sarama.Version,
		Timestamp: time.Now().UTC().Format(time.RFC3339),
		Tests:     results,
	}
	summary.Summary.Total = len(results)
	summary.Summary.Passed = passedCount
	summary.Summary.Failed = len(results) - passedCount
	
	logMessage("INFO", fmt.Sprintf("Test summary: %d/%d passed", passedCount, len(results)))
	
	// Save to file
	resultFile := "/results/sarama-results.json"
	file, err := os.Create(resultFile)
	if err != nil {
		log.Printf("Failed to create result file: %v", err)
	} else {
		encoder := json.NewEncoder(file)
		encoder.SetIndent("", "  ")
		encoder.Encode(summary)
		file.Close()
		logMessage("INFO", fmt.Sprintf("Results written to %s", resultFile))
	}
	
	if passedCount < len(results) {
		os.Exit(1)
	}
}