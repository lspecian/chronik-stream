package main

import (
	"encoding/binary"
	"encoding/hex"
	"fmt"
	"log"
	"net"
	"time"
)

func main() {
	fmt.Println("Testing raw Produce request/response to debug memory corruption...")
	
	// Connect to Chronik
	conn, err := net.DialTimeout("tcp", "localhost:9092", 5*time.Second)
	if err != nil {
		log.Fatalf("Failed to connect: %v", err)
	}
	defer conn.Close()
	
	fmt.Println("✓ Connected to localhost:9092")
	
	// Build a Produce v1 request (simplest version with throttle_time)
	// This is what older librdkafka versions use
	produceV1 := buildProduceV1Request()
	
	fmt.Printf("\nSending Produce v1 request (%d bytes):\n", len(produceV1))
	fmt.Printf("%s\n", hex.EncodeToString(produceV1))
	
	_, err = conn.Write(produceV1)
	if err != nil {
		log.Fatalf("Failed to send: %v", err)
	}
	
	// Read response
	respBuf := make([]byte, 4096)
	n, err := conn.Read(respBuf)
	if err != nil {
		log.Fatalf("Failed to read response: %v", err)
	}
	
	fmt.Printf("\n✓ Received response (%d bytes):\n", n)
	fmt.Printf("%s\n", hex.EncodeToString(respBuf[:n]))
	
	// Parse response
	parseProduceV1Response(respBuf[:n])
	
	// Now test Produce v3 (what newer librdkafka uses)
	fmt.Println("\n--- Testing Produce v3 ---")
	produceV3 := buildProduceV3Request()
	
	fmt.Printf("\nSending Produce v3 request (%d bytes):\n", len(produceV3))
	fmt.Printf("%s\n", hex.EncodeToString(produceV3))
	
	_, err = conn.Write(produceV3)
	if err != nil {
		log.Fatalf("Failed to send: %v", err)
	}
	
	// Read response
	n, err = conn.Read(respBuf)
	if err != nil {
		log.Fatalf("Failed to read response: %v", err)
	}
	
	fmt.Printf("\n✓ Received response (%d bytes):\n", n)
	fmt.Printf("%s\n", hex.EncodeToString(respBuf[:n]))
	
	// Parse response
	parseProduceV3Response(respBuf[:n])
}

func buildProduceV1Request() []byte {
	req := make([]byte, 0, 256)
	
	// Size placeholder
	req = append(req, 0, 0, 0, 0)
	
	// API key (0 = Produce)
	req = append(req, 0, 0)
	// API version (1)
	req = append(req, 0, 1)
	// Correlation ID
	req = append(req, 0, 0, 0, 42)
	// Client ID (null)
	req = append(req, 0xFF, 0xFF)
	// Acks (1)
	req = append(req, 0, 1)
	// Timeout (2000ms)
	req = append(req, 0, 0, 0x07, 0xD0)
	// Topic array count (1)
	req = append(req, 0, 0, 0, 1)
	// Topic name length (10)
	req = append(req, 0, 10)
	// Topic name "test-topic"
	req = append(req, []byte("test-topic")...)
	// Partition array count (1)
	req = append(req, 0, 0, 0, 1)
	// Partition index (0)
	req = append(req, 0, 0, 0, 0)
	// Records (null = -1)
	req = append(req, 0xFF, 0xFF, 0xFF, 0xFF)
	
	// Update size
	binary.BigEndian.PutUint32(req[0:4], uint32(len(req)-4))
	
	return req
}

func buildProduceV3Request() []byte {
	req := make([]byte, 0, 256)
	
	// Size placeholder
	req = append(req, 0, 0, 0, 0)
	
	// API key (0 = Produce)
	req = append(req, 0, 0)
	// API version (3)
	req = append(req, 0, 3)
	// Correlation ID
	req = append(req, 0, 0, 0, 43)
	// Client ID (null)
	req = append(req, 0xFF, 0xFF)
	// Transactional ID (null)
	req = append(req, 0xFF, 0xFF)
	// Acks (1)
	req = append(req, 0, 1)
	// Timeout (2000ms)
	req = append(req, 0, 0, 0x07, 0xD0)
	// Topic array count (1)
	req = append(req, 0, 0, 0, 1)
	// Topic name length (10)
	req = append(req, 0, 10)
	// Topic name "test-topic"
	req = append(req, []byte("test-topic")...)
	// Partition array count (1)
	req = append(req, 0, 0, 0, 1)
	// Partition index (0)
	req = append(req, 0, 0, 0, 0)
	// Records (null = -1)
	req = append(req, 0xFF, 0xFF, 0xFF, 0xFF)
	
	// Update size
	binary.BigEndian.PutUint32(req[0:4], uint32(len(req)-4))
	
	return req
}

func parseProduceV1Response(data []byte) {
	fmt.Println("\nParsing Produce v1 response:")
	
	if len(data) < 4 {
		fmt.Println("✗ Response too short")
		return
	}
	
	offset := 0
	
	// Size
	size := binary.BigEndian.Uint32(data[offset:offset+4])
	fmt.Printf("  Size: %d bytes\n", size)
	offset += 4
	
	if len(data) < int(size)+4 {
		fmt.Printf("✗ Expected %d bytes, got %d\n", size+4, len(data))
		return
	}
	
	// Correlation ID
	correlationId := binary.BigEndian.Uint32(data[offset:offset+4])
	fmt.Printf("  Correlation ID: %d\n", correlationId)
	offset += 4
	
	// For v1+, throttle_time_ms should be next
	if offset+4 <= len(data) {
		throttleTime := binary.BigEndian.Uint32(data[offset:offset+4])
		fmt.Printf("  ✓ Throttle Time: %d ms (at correct position)\n", throttleTime)
		offset += 4
	} else {
		fmt.Println("  ✗ Missing throttle_time_ms!")
		return
	}
	
	// Topics array
	if offset+4 <= len(data) {
		topicCount := binary.BigEndian.Uint32(data[offset:offset+4])
		fmt.Printf("  Topic count: %d\n", topicCount)
		
		if topicCount > 10 {
			fmt.Printf("  ✗ Suspicious topic count %d - possible memory corruption!\n", topicCount)
		}
	}
	
	fmt.Println("\nStructure looks correct for librdkafka")
}

func parseProduceV3Response(data []byte) {
	fmt.Println("\nParsing Produce v3 response:")
	
	if len(data) < 4 {
		fmt.Println("✗ Response too short")
		return
	}
	
	offset := 0
	
	// Size
	size := binary.BigEndian.Uint32(data[offset:offset+4])
	fmt.Printf("  Size: %d bytes\n", size)
	offset += 4
	
	if len(data) < int(size)+4 {
		fmt.Printf("✗ Expected %d bytes, got %d\n", size+4, len(data))
		return
	}
	
	// Correlation ID
	correlationId := binary.BigEndian.Uint32(data[offset:offset+4])
	fmt.Printf("  Correlation ID: %d\n", correlationId)
	offset += 4
	
	// For v3, throttle_time_ms should be next
	if offset+4 <= len(data) {
		throttleTime := binary.BigEndian.Uint32(data[offset:offset+4])
		fmt.Printf("  ✓ Throttle Time: %d ms (at correct position)\n", throttleTime)
		offset += 4
	} else {
		fmt.Println("  ✗ Missing throttle_time_ms!")
		return
	}
	
	// Topics array
	if offset+4 <= len(data) {
		topicCount := binary.BigEndian.Uint32(data[offset:offset+4])
		fmt.Printf("  Topic count: %d\n", topicCount)
		
		if topicCount > 10 {
			fmt.Printf("  ✗ Suspicious topic count %d - possible memory corruption!\n", topicCount)
		}
	}
	
	fmt.Println("\nStructure looks correct for librdkafka")
}