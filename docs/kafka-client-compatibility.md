# Kafka Client Compatibility Guide

This document describes Chronik Stream's compatibility with various Kafka clients and how to test it.

## Supported Clients

Chronik Stream aims to be compatible with all major Kafka clients. The following clients have been tested:

### Officially Tested Clients

| Client | Language | Version | Status | Notes |
|--------|----------|---------|--------|-------|
| kafkactl | CLI | Latest | ✅ Supported | Full support for basic operations |
| Sarama | Go | v1.34+ | ✅ Supported | Producer, consumer, admin operations |
| confluent-kafka-python | Python | 2.0+ | ✅ Supported | Full client functionality |
| librdkafka | C/C++ | 1.9+ | ✅ Supported | Via language bindings |
| kafka-python | Python | 2.0+ | 🟡 Partial | Basic operations work |
| node-rdkafka | Node.js | 2.0+ | ✅ Supported | Via librdkafka |
| Java client | Java | 3.0+ | ✅ Supported | Official Apache Kafka client |

### Supported Operations

| Operation | Description | Support Level |
|-----------|-------------|---------------|
| Metadata | Fetch cluster metadata | ✅ Full |
| Produce | Send messages to topics | ✅ Full |
| Fetch | Consume messages from topics | ✅ Full |
| ListOffsets | Get offset information | ✅ Full |
| CreateTopics | Create new topics | ✅ Full |
| DeleteTopics | Delete existing topics | ✅ Full |
| DescribeConfigs | Get topic/broker configurations | ✅ Full |
| AlterConfigs | Modify configurations | 🟡 Partial |
| Consumer Groups | Group coordination | ✅ Full |
| Transactions | Transactional messaging | 🟡 Partial |
| SASL/SSL | Security features | 🔴 Not yet |

## Testing Client Compatibility

### Prerequisites

Install the clients you want to test:

```bash
# Install kafkactl
curl -L https://github.com/deviceinsight/kafkactl/releases/latest/download/kafkactl-linux-amd64.tar.gz \
  -o kafkactl.tar.gz
tar xzf kafkactl.tar.gz
sudo mv kafkactl /usr/local/bin/

# Install Python client
pip install confluent-kafka

# Install kcat (formerly kafkacat)
sudo apt-get install kafkacat

# Install Go (for Sarama)
sudo apt-get install golang-go
go install github.com/Shopify/sarama/tools/kafka-console-consumer@latest
go install github.com/Shopify/sarama/tools/kafka-console-producer@latest
```

### Running Tests

1. **Start Chronik Stream**:
   ```bash
   docker-compose up -d
   ```

2. **Run automated tests**:
   ```bash
   cd tests
   python3 test_real_clients.py
   ```

3. **Generate compatibility report**:
   ```bash
   python3 generate-compatibility-report.py --output report
   ```

### Manual Testing Examples

#### kafkactl

```bash
# List brokers
kafkactl get brokers --brokers localhost:9092

# Create topic
kafkactl create topic test-topic --partitions 3 --brokers localhost:9092

# Produce message
echo "Hello Chronik" | kafkactl produce test-topic --brokers localhost:9092

# Consume messages
kafkactl consume test-topic --from-beginning --brokers localhost:9092

# List consumer groups
kafkactl get consumer-groups --brokers localhost:9092
```

#### Python (confluent-kafka)

```python
from confluent_kafka import Producer, Consumer
from confluent_kafka.admin import AdminClient, NewTopic

# Admin operations
admin = AdminClient({'bootstrap.servers': 'localhost:9092'})
topic = NewTopic('python-topic', num_partitions=3, replication_factor=1)
admin.create_topics([topic])

# Producer
producer = Producer({'bootstrap.servers': 'localhost:9092'})
producer.produce('python-topic', key='key1', value='Hello from Python')
producer.flush()

# Consumer
consumer = Consumer({
    'bootstrap.servers': 'localhost:9092',
    'group.id': 'python-group',
    'auto.offset.reset': 'earliest'
})
consumer.subscribe(['python-topic'])

msg = consumer.poll(timeout=5.0)
if msg and not msg.error():
    print(f"Received: {msg.value().decode('utf-8')}")

consumer.close()
```

#### Go (Sarama)

```go
package main

import (
    "fmt"
    "log"
    "github.com/Shopify/sarama"
)

func main() {
    config := sarama.NewConfig()
    config.Version = sarama.V2_6_0_0
    config.Producer.Return.Successes = true

    // Create client
    client, err := sarama.NewClient([]string{"localhost:9092"}, config)
    if err != nil {
        log.Fatal(err)
    }
    defer client.Close()

    // Producer
    producer, err := sarama.NewSyncProducerFromClient(client)
    if err != nil {
        log.Fatal(err)
    }
    defer producer.Close()

    msg := &sarama.ProducerMessage{
        Topic: "go-topic",
        Value: sarama.StringEncoder("Hello from Go"),
    }

    partition, offset, err := producer.SendMessage(msg)
    if err != nil {
        log.Fatal(err)
    }

    fmt.Printf("Message sent to partition %d at offset %d\n", partition, offset)
}
```

## Protocol Compatibility

Chronik Stream implements the Kafka wire protocol with the following version support:

| API | Min Version | Max Version | Notes |
|-----|-------------|-------------|-------|
| Produce | 0 | 9 | Full support |
| Fetch | 0 | 13 | Full support |
| ListOffsets | 0 | 7 | Full support |
| Metadata | 0 | 12 | Full support |
| LeaderAndIsr | 0 | 5 | Broker-only API |
| StopReplica | 0 | 3 | Broker-only API |
| UpdateMetadata | 0 | 7 | Broker-only API |
| ControlledShutdown | 0 | 3 | Broker-only API |
| OffsetCommit | 0 | 8 | Full support |
| OffsetFetch | 0 | 8 | Full support |
| FindCoordinator | 0 | 4 | Full support |
| JoinGroup | 0 | 7 | Full support |
| Heartbeat | 0 | 4 | Full support |
| LeaveGroup | 0 | 4 | Full support |
| SyncGroup | 0 | 5 | Full support |
| DescribeGroups | 0 | 5 | Full support |
| ListGroups | 0 | 4 | Full support |
| SaslHandshake | 0 | 1 | Basic support |
| ApiVersions | 0 | 3 | Full support |
| CreateTopics | 0 | 7 | Full support |
| DeleteTopics | 0 | 6 | Full support |
| DeleteRecords | 0 | 2 | Not implemented |
| InitProducerId | 0 | 4 | Partial support |
| OffsetForLeaderEpoch | 0 | 4 | Not implemented |
| AddPartitionsToTxn | 0 | 3 | Not implemented |
| AddOffsetsToTxn | 0 | 3 | Not implemented |
| EndTxn | 0 | 3 | Not implemented |
| WriteTxnMarkers | 0 | 1 | Broker-only API |
| TxnOffsetCommit | 0 | 3 | Not implemented |
| DescribeAcls | 0 | 2 | Not implemented |
| CreateAcls | 0 | 2 | Not implemented |
| DeleteAcls | 0 | 2 | Not implemented |
| DescribeConfigs | 0 | 4 | Full support |
| AlterConfigs | 0 | 2 | Partial support |

## Known Limitations

1. **Security**: SASL and SSL are not yet implemented
2. **Transactions**: Transaction support is partial
3. **Exactly-once semantics**: Not yet supported
4. **ACLs**: Access control lists are not implemented
5. **Quotas**: Client quotas are not enforced

## Troubleshooting

### Connection Issues

If clients cannot connect:

1. Check that Chronik Stream is running:
   ```bash
   docker-compose ps
   ```

2. Verify the port is accessible:
   ```bash
   nc -zv localhost 9092
   ```

3. Check logs for errors:
   ```bash
   docker-compose logs chronik-ingest
   ```

### Protocol Errors

If you see protocol errors:

1. Check the client version - older clients may not support newer protocol features
2. Enable debug logging:
   ```bash
   export KAFKA_LOG4J_OPTS="-Dlog4j.configuration=file:log4j.properties"
   ```

3. Use tcpdump to capture network traffic:
   ```bash
   sudo tcpdump -i lo -w kafka.pcap port 9092
   ```

### Performance Issues

For performance problems:

1. Check resource usage:
   ```bash
   docker stats
   ```

2. Monitor metrics (if Prometheus is enabled):
   ```
   http://localhost:9090/graph
   ```

3. Adjust client configurations:
   - Increase batch size for producers
   - Adjust fetch sizes for consumers
   - Tune connection pool settings

## Contributing

To add support for a new client:

1. Add tests in `tests/integration/real_kafka_clients_test.rs`
2. Update the compatibility matrix in this document
3. Run the full test suite
4. Submit a PR with the test results

## References

- [Kafka Protocol Documentation](https://kafka.apache.org/protocol)
- [kafkactl Documentation](https://github.com/deviceinsight/kafkactl)
- [Sarama Documentation](https://github.com/Shopify/sarama)
- [confluent-kafka-python Documentation](https://docs.confluent.io/kafka-clients/python/current/overview.html)