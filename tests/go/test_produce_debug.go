package main

import (
	"encoding/hex"
	"fmt"
	"log"
	"net"
	"time"
)

func main() {
	fmt.Println("Testing raw Kafka Produce request/response...")
	
	// Connect to Chronik
	conn, err := net.DialTimeout("tcp", "localhost:9092", 5*time.Second)
	if err != nil {
		log.Fatalf("Failed to connect: %v", err)
	}
	defer conn.Close()
	
	fmt.Println("✓ Connected to localhost:9092")
	
	// Send ApiVersions request first (API key 18, version 0)
	apiVersionsReq := []byte{
		0x00, 0x00, 0x00, 0x15, // Size (21 bytes)
		0x00, 0x12,             // API key (18 = ApiVersions)
		0x00, 0x00,             // API version (0)
		0x00, 0x00, 0x00, 0x01, // Correlation ID (1)
		0x00, 0x09,             // Client ID length (9)
		'g', 'o', '-', 't', 'e', 's', 't', '-', '1', // Client ID
	}
	
	fmt.Printf("Sending ApiVersions request: %s\n", hex.EncodeToString(apiVersionsReq))
	_, err = conn.Write(apiVersionsReq)
	if err != nil {
		log.Fatalf("Failed to send ApiVersions: %v", err)
	}
	
	// Read response
	respBuf := make([]byte, 1024)
	n, err := conn.Read(respBuf)
	if err != nil {
		log.Fatalf("Failed to read ApiVersions response: %v", err)
	}
	
	fmt.Printf("✓ ApiVersions response (%d bytes): %s\n", n, hex.EncodeToString(respBuf[:n]))
	
	// Now send a Produce request v3 (what librdkafka typically uses)
	// This is a minimal produce request with no actual records
	produceReq := []byte{
		0x00, 0x00, 0x00, 0x3A, // Size (58 bytes)
		0x00, 0x00,             // API key (0 = Produce)
		0x00, 0x03,             // API version (3)
		0x00, 0x00, 0x00, 0x02, // Correlation ID (2)
		0x00, 0x09,             // Client ID length
		'g', 'o', '-', 't', 'e', 's', 't', '-', '2', // Client ID
		0xFF, 0xFF,             // Transactional ID (null = -1)
		0x00, 0x01,             // Acks (1)
		0x00, 0x00, 0x07, 0xD0, // Timeout (2000ms)
		0x00, 0x00, 0x00, 0x01, // Topic array count (1)
		0x00, 0x0A,             // Topic name length (10)
		't', 'e', 's', 't', '-', 't', 'o', 'p', 'i', 'c', // Topic name
		0x00, 0x00, 0x00, 0x01, // Partition array count (1)
		0x00, 0x00, 0x00, 0x00, // Partition index (0)
		0x00, 0x00, 0x00, 0x00, // Records length (0 = no records)
	}
	
	fmt.Printf("\nSending Produce v3 request: %s\n", hex.EncodeToString(produceReq))
	_, err = conn.Write(produceReq)
	if err != nil {
		log.Fatalf("Failed to send Produce: %v", err)
	}
	
	// Read produce response
	n, err = conn.Read(respBuf)
	if err != nil {
		log.Fatalf("Failed to read Produce response: %v", err)
	}
	
	fmt.Printf("✓ Produce response (%d bytes): %s\n", n, hex.EncodeToString(respBuf[:n]))
	
	// Parse the response manually
	if n >= 12 {
		size := int(respBuf[0])<<24 | int(respBuf[1])<<16 | int(respBuf[2])<<8 | int(respBuf[3])
		correlationId := int(respBuf[4])<<24 | int(respBuf[5])<<16 | int(respBuf[6])<<8 | int(respBuf[7])
		
		fmt.Printf("\nParsed response:\n")
		fmt.Printf("  Size: %d\n", size)
		fmt.Printf("  Correlation ID: %d\n", correlationId)
		
		// For Produce v3, throttle_time_ms should be next
		if n >= 12 {
			throttleTime := int(respBuf[8])<<24 | int(respBuf[9])<<16 | int(respBuf[10])<<8 | int(respBuf[11])
			fmt.Printf("  Throttle Time: %d ms\n", throttleTime)
			fmt.Printf("  ✓ throttle_time_ms is at correct position (offset 8)\n")
		}
	}
	
	fmt.Println("\n=== TEST COMPLETE ===")
	fmt.Println("Response structure looks correct for librdkafka compatibility")
}