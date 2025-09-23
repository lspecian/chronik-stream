# KSQL Integration Guide for Chronik Stream

## Overview

This guide provides comprehensive instructions for integrating KSQL with Chronik Stream, including configuration, testing, and troubleshooting.

## Current Compatibility Status

### ✅ Fully Supported Features

1. **AdminClient Operations**
   - ApiVersions (all versions)
   - Metadata (v0-v12)
   - DescribeCluster (v0-v1)
   - DescribeConfigs (v0-v3)
   - CreateTopics/DeleteTopics

2. **Consumer Group Coordination**
   - FindCoordinator
   - JoinGroup/SyncGroup
   - Heartbeat/LeaveGroup
   - DescribeGroups/ListGroups

3. **Transactional Processing**
   - InitProducerId (with WAL persistence)
   - AddPartitionsToTxn
   - EndTxn (commit/abort)
   - TxnOffsetCommit

4. **Offset Management**
   - OffsetCommit/OffsetFetch
   - ListOffsets
   - Consumer offset persistence (with WAL)

### ⚠️ Partial Support

1. **Configuration Management**
   - AlterConfigs (returns stub error)
   - IncrementalAlterConfigs (returns stub error)
   - Config reading works, modification not implemented

2. **Advanced Features**
   - AddOffsetsToTxn (basic implementation)
   - Transaction markers in logs (not implemented)
   - Distributed coordination (single-node only)

## Quick Start

### 1. Start Chronik Stream

```bash
# Build and run Chronik Stream
cargo build --release
./target/release/chronik-server \
  --kafka-port 9092 \
  --metrics-port 9093 \
  --advertised-host localhost
```

### 2. Configure KSQL

Create a `ksql-server.properties` file:

```properties
# Kafka broker configuration
bootstrap.servers=localhost:9092
ksql.service.id=ksql-chronik-cluster

# Schema Registry (if needed)
ksql.schema.registry.url=http://localhost:8081

# Processing guarantees
processing.guarantee=exactly_once
ksql.streams.producer.transactional.id.prefix=ksql-txn

# Optimization settings for Chronik compatibility
ksql.streams.producer.acks=all
ksql.streams.producer.retries=3
ksql.streams.producer.max.in.flight.requests.per.connection=5

# Consumer settings
ksql.streams.consumer.auto.offset.reset=earliest
ksql.streams.consumer.isolation.level=read_committed

# State store settings
ksql.streams.state.dir=/tmp/kafka-streams
ksql.streams.num.stream.threads=4

# Monitoring
ksql.metrics.tags.custom=chronik-stream
```

### 3. Start KSQL Server

```bash
# Start KSQL server
ksql-server-start ksql-server.properties
```

### 4. Connect KSQL CLI

```bash
# Start KSQL CLI
ksql http://localhost:8088
```

## Testing KSQL Compatibility

### Run AdminClient Compatibility Tests

```bash
# Test all AdminClient APIs
./compat-tests/test_ksql_admin_compatibility.py

# Expected output:
# ✅ DescribeCluster v0 successful!
# ✅ DescribeCluster v1 successful!
# ✅ DescribeConfigs v0 successful!
# ✅ DescribeConfigs v2 successful!
# ✅ AlterConfigs returned expected error for stub
# ✅ IncrementalAlterConfigs returned expected error for stub
```

### Run Streaming Job Tests

```bash
# Test KSQL streaming operations
./compat-tests/test_ksql_streaming.py

# Expected output:
# ✅ Topic creation successful
# ✅ Producer initialized
# ✅ Consumer group joined
# ✅ Messages produced
# ✅ Offsets committed
```

### Test KSQL Startup

```bash
# Test the complete startup sequence
./compat-tests/test_ksql_startup_simulation.py

# Expected output:
# ✅ ApiVersions v0 validated
# ✅ Metadata v0 validated
# ✅ All 6 startup requests succeeded
```

## Sample KSQL Queries

### Create a Stream

```sql
CREATE STREAM user_events (
  user_id BIGINT KEY,
  event_type VARCHAR,
  timestamp BIGINT
) WITH (
  KAFKA_TOPIC='user-events',
  VALUE_FORMAT='JSON',
  PARTITIONS=3
);
```

### Create a Table

```sql
CREATE TABLE user_profiles (
  user_id BIGINT PRIMARY KEY,
  username VARCHAR,
  email VARCHAR,
  created_at BIGINT
) WITH (
  KAFKA_TOPIC='user-profiles',
  VALUE_FORMAT='JSON',
  PARTITIONS=3
);
```

### Stream-Table Join

```sql
CREATE STREAM enriched_events AS
  SELECT
    e.user_id,
    e.event_type,
    e.timestamp,
    p.username,
    p.email
  FROM user_events e
  INNER JOIN user_profiles p
    ON e.user_id = p.user_id
  EMIT CHANGES;
```

### Windowed Aggregation

```sql
CREATE TABLE event_counts AS
  SELECT
    user_id,
    COUNT(*) as event_count,
    WINDOWSTART as window_start,
    WINDOWEND as window_end
  FROM user_events
  WINDOW TUMBLING (SIZE 1 HOUR)
  GROUP BY user_id
  EMIT CHANGES;
```

## Monitoring & Observability

### Enable Enhanced Logging

Add to Chronik Stream configuration:

```rust
// In main.rs or configuration
tracing_subscriber::fmt()
    .with_max_level(tracing::Level::DEBUG)
    .with_target(true)
    .with_thread_ids(true)
    .with_thread_names(true)
    .init();
```

### Monitor API Requests

Chronik Stream logs all KSQL API requests:

```
INFO API Request: ApiVersions (key=18, v0) from 127.0.0.1:57929 [client: adminclient-1, correlation: 1]
INFO API Request: Metadata (key=3, v0) from 127.0.0.1:57929 [client: adminclient-1, correlation: 2]
INFO API Request: DescribeCluster (key=60, v0) from 127.0.0.1:57929 [client: adminclient-1, correlation: 3]
```

### Track Consumer Groups

```bash
# View consumer groups
./compat-tests/test_consumer_groups.py

# Monitor offset commits
grep "OffsetCommit" /var/log/chronik/server.log
```

## Troubleshooting

### Issue: AdminClient Thread Exits

**Symptom**: KSQL AdminClient thread exits prematurely

**Solution**:
1. Ensure DescribeCluster v0 doesn't include throttle_time_ms
2. Verify DescribeConfigs field ordering matches Kafka protocol
3. Check correlation ID matching in responses

### Issue: Transaction Failures

**Symptom**: Transactional operations fail

**Solution**:
1. Ensure InitProducerId returns valid producer ID and epoch
2. Verify WAL persistence is enabled
3. Check transaction timeout settings

### Issue: Offset Fetch Errors

**Symptom**: Consumer offsets not persisted across restarts

**Solution**:
1. Enable WAL persistence for offsets
2. Verify consumer group metadata is stored
3. Check offset commit acknowledgments

### Issue: Stream Processing Latency

**Symptom**: High latency in stream processing

**Solution**:
1. Adjust batch sizes in KSQL configuration
2. Increase stream threads: `ksql.streams.num.stream.threads`
3. Monitor network latency between KSQL and Chronik

## Performance Tuning

### KSQL Configuration

```properties
# Increase cache for better performance
ksql.streams.cache.max.bytes.buffering=10485760

# Optimize commit intervals
ksql.streams.commit.interval.ms=1000

# Adjust producer batching
ksql.streams.producer.batch.size=16384
ksql.streams.producer.linger.ms=100

# Consumer fetch settings
ksql.streams.consumer.fetch.min.bytes=1024
ksql.streams.consumer.max.poll.records=1000
```

### Chronik Stream Settings

```bash
# Start with optimized settings
./target/release/chronik-server \
  --kafka-port 9092 \
  --metrics-port 9093 \
  --wal-buffer-size 10485760 \
  --max-connections 1000 \
  --io-threads 8
```

## Integration with Other Tools

### Kafka Streams Applications

```java
Properties props = new Properties();
props.put(StreamsConfig.BOOTSTRAP_SERVERS_CONFIG, "localhost:9092");
props.put(StreamsConfig.APPLICATION_ID_CONFIG, "my-streams-app");
props.put(StreamsConfig.PROCESSING_GUARANTEE_CONFIG, StreamsConfig.EXACTLY_ONCE);

KafkaStreams streams = new KafkaStreams(topology, props);
streams.start();
```

### Flink SQL

```sql
CREATE TABLE user_events (
  user_id BIGINT,
  event_type STRING,
  event_time TIMESTAMP(3),
  WATERMARK FOR event_time AS event_time - INTERVAL '5' SECOND
) WITH (
  'connector' = 'kafka',
  'topic' = 'user-events',
  'properties.bootstrap.servers' = 'localhost:9092',
  'properties.group.id' = 'flink-consumer',
  'format' = 'json'
);
```

## Known Limitations

1. **Configuration Management**: AlterConfigs/IncrementalAlterConfigs return stub errors
2. **Distributed Coordination**: Currently single-node only
3. **Transaction Markers**: Not implemented in message logs
4. **Schema Registry**: External schema registry needed for AVRO support
5. **Quota Management**: Client/user quotas not implemented

## Future Enhancements

1. **Full Configuration Management**: Implement config persistence and modification
2. **Distributed Transaction Coordinator**: Multi-node transaction support
3. **Transaction Markers**: Add markers to message logs for exactly-once
4. **Native Schema Registry**: Built-in schema management
5. **Advanced Security**: SASL/SSL authentication and ACLs

## Support & Resources

- **GitHub Issues**: Report bugs and feature requests
- **Documentation**: See KSQL_COMPATIBILITY_PROGRESS.md for detailed API status
- **Test Suite**: Run all tests in `compat-tests/` directory
- **Logs**: Check `/var/log/chronik/` for detailed server logs

## Conclusion

Chronik Stream provides robust KSQL compatibility for stream processing workloads. While some advanced features are still in development, the core functionality required for KSQL operations is fully supported with WAL persistence for durability and exactly-once semantics for reliability.