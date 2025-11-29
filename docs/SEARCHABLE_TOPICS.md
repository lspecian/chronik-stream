# Searchable Topics

**Available since v2.2.16**

Searchable topics enable real-time full-text indexing of messages using Tantivy, allowing you to search message content immediately after production.

## Overview

By default, topics are **not searchable** for maximum throughput. When searchable is enabled:
- Messages are indexed in real-time during the produce path
- Full-text search is available immediately after message acknowledgment
- There is a small performance overhead (~3% standalone, ~33% cluster)

## Configuration

### Server-Wide Default

Enable searchable by default for all new topics:

```bash
# All new topics will be searchable
CHRONIK_DEFAULT_SEARCHABLE=true ./chronik-server start
```

### Per-Topic Configuration

Topics can be configured individually via the CreateTopics API or topic configuration:

```python
from kafka.admin import KafkaAdminClient, NewTopic

admin = KafkaAdminClient(bootstrap_servers='localhost:9092')

# Create a searchable topic
admin.create_topics([
    NewTopic(
        name='searchable-events',
        num_partitions=3,
        replication_factor=1,
        topic_configs={'searchable': 'true'}
    )
])
```

### Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `CHRONIK_DEFAULT_SEARCHABLE` | `false` | Default searchable setting for new topics |

## Performance Impact

Benchmarks at 128 concurrency, 256 byte messages, 30s duration:

### Standalone Mode

| Configuration | Throughput | p99 Latency | Overhead |
|--------------|------------|-------------|----------|
| Non-Searchable | 197,793 msg/s | 0.59 ms | - |
| Searchable | 192,064 msg/s | 2.15 ms | 2.9% |

**Standalone searchable overhead is minimal (only 3%)** - recommended for most use cases.

### Cluster Mode (3 nodes, acks=1)

| Configuration | Throughput | p99 Latency | Overhead |
|--------------|------------|-------------|----------|
| Non-Searchable | 183,133 msg/s | 2.85 ms | - |
| Searchable | 123,406 msg/s | 14.43 ms | 32.6% |

**Cluster searchable has higher overhead** due to combined indexing + replication costs.

### Latency Distribution

#### Standalone Searchable
```
p50:    0.59ms
p90:    1.03ms
p95:    1.27ms
p99:    2.15ms
p99.9:  2.98ms
max:    9.70ms
```

#### Cluster Searchable
```
p50:    0.59ms
p90:    1.09ms
p95:    1.64ms
p99:   14.43ms
p99.9: 17.38ms
max:   23.21ms
```

## How It Works

### Real-time Indexing

When a topic is searchable, messages are indexed during the produce path:

1. **Producer sends message** to Chronik
2. **Message written to WAL** (Write-Ahead Log)
3. **Real-time indexer** extracts and indexes message content
4. **Acknowledgment sent** to producer
5. **Message immediately searchable** via Search API

```
Producer                     Chronik Server
   |                              |
   |-- Produce Request ---------->|
   |                              |-- Write to WAL
   |                              |-- Index message (if searchable)
   |<--- Acknowledgment ----------|
   |                              |
   |                    Message now searchable!
```

### Indexer Configuration

The real-time indexer processes messages for searchable topics:

- **Tantivy** full-text search engine
- Indexes message keys and values
- Supports field-level search queries
- Automatic index commits for durability

## Use Cases

### When to Enable Searchable Topics

**Recommended:**
- Log aggregation with search requirements
- Event sourcing with content queries
- Audit trails with search capability
- Real-time analytics dashboards

**Not Recommended:**
- High-throughput data pipelines where search isn't needed
- Latency-sensitive applications requiring sub-1ms p99
- Cost-sensitive deployments (indexing uses additional CPU/memory)

### Query Examples

Once indexed, messages can be searched:

```bash
# Search for messages containing "error"
curl "http://localhost:9091/search?topic=logs&q=error"

# Search with field filters
curl "http://localhost:9091/search?topic=events&q=user:john+action:login"

# Time-range search
curl "http://localhost:9091/search?topic=metrics&q=cpu&from=2024-01-01&to=2024-01-02"
```

## Best Practices

### 1. Separate Searchable and Non-Searchable Topics

Don't enable searchable globally if only some topics need search:

```bash
# Create dedicated searchable topics
./chronik-server start  # Default: non-searchable

# Create specific searchable topic via API
kafka-topics.sh --create --topic audit-logs --config searchable=true
```

### 2. Consider Cluster Mode Carefully

In cluster mode, searchable topics have ~33% overhead. Consider:
- Using standalone mode for search-heavy workloads
- Separating search workloads from high-throughput pipelines
- Accepting the latency trade-off if search is critical

### 3. Monitor Performance

Key metrics to watch:
- `chronik_realtime_indexer_lag_seconds` - Indexing lag
- `chronik_search_query_latency_seconds` - Search latency
- `chronik_produce_latency_seconds` - Produce latency impact

### 4. Capacity Planning

Searchable topics require additional resources:
- **CPU**: ~10-20% additional for indexing
- **Memory**: Tantivy index cache
- **Disk**: Index storage (typically 10-30% of message size)

## Migration Guide

### Enabling Search on Existing Topics

Existing non-searchable topics can be made searchable, but historical messages won't be indexed automatically. Options:

1. **Create new searchable topic** and migrate data
2. **Run batch indexer** on historical WAL segments
3. **Accept that only new messages** will be searchable

### Disabling Search

To disable search on a topic:
- Update topic config: `searchable=false`
- New messages won't be indexed
- Existing indexes remain until cleaned up

## Troubleshooting

### Messages Not Appearing in Search

1. **Check topic is searchable:**
   ```bash
   kafka-topics.sh --describe --topic my-topic
   # Look for searchable=true in config
   ```

2. **Check indexer is running:**
   ```bash
   curl http://localhost:9091/metrics | grep indexer
   ```

3. **Check for indexing errors:**
   ```bash
   grep "indexer" /var/log/chronik/server.log
   ```

### High Latency on Searchable Topics

1. Reduce indexing batch size
2. Increase indexer thread count
3. Consider non-searchable for latency-critical topics

### Index Corruption

If the search index becomes corrupted:
```bash
# Stop server
./chronik-server stop

# Remove index directory
rm -rf /data/indexes/

# Restart - index will rebuild from WAL
./chronik-server start
```

## See Also

- [BASELINE_PERFORMANCE.md](../BASELINE_PERFORMANCE.md) - Full benchmark results
- [docs/realtime_indexing.md](realtime_indexing.md) - Indexer architecture
- [docs/WAL_AUTO_TUNING.md](WAL_AUTO_TUNING.md) - WAL performance tuning
