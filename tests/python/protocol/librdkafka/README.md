# librdkafka Protocol Analysis Tools

This directory contains Python utilities for analyzing and testing the Kafka wire protocol, specifically for debugging librdkafka v2.11.1 compatibility issues.

## Tools

### capture_librdkafka.py
Captures raw requests from librdkafka clients to analyze the exact wire format being sent.

### intercept_proxy.py
TCP proxy that intercepts client-server communication on port 9093 and forwards to 9092. Useful for debugging protocol issues in real-time.

### test_produce_capture.py
Captures and analyzes complete Produce request/response cycles, showing detailed byte-level protocol analysis.

### decode_produce_response.py
Decodes and validates Produce v9 response structure to ensure compliance with Kafka protocol specification.

### test_raw_produce.py
Sends raw Produce v9 requests for testing server response handling.

### test_api_versions_v3.py
Tests ApiVersions v3 protocol implementation.

### test_metadata.py
Tests Metadata protocol implementation.

### test_varint.py
Tests compact/varint encoding and decoding.

### test_debug_response.py
Debug tool for response analysis.

### test_librdkafka_debug.py
Comprehensive librdkafka debugging suite.

## Usage

Run the intercept proxy to debug live traffic:
```bash
python3 intercept_proxy.py
# Then connect librdkafka to localhost:9093
```

Capture a Produce request/response:
```bash
python3 test_produce_capture.py
```

Test raw protocol handling:
```bash
python3 test_raw_produce.py
```