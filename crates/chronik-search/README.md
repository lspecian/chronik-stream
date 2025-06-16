# Chronik Search

Elasticsearch-compatible REST API for searching Kafka messages in Chronik Stream.

## Features

- **Elasticsearch-compatible API**: Provides familiar REST endpoints that work with existing Elasticsearch clients
- **Tantivy-based indexing**: High-performance full-text search using the Tantivy search engine
- **Real-time indexing**: Index Kafka messages as they arrive
- **Multiple query types**: Support for match, term, range, and bool queries
- **Index management**: Create, delete, and manage indices with custom mappings
- **Production-ready**: Includes health checks, metrics, and proper error handling

## Architecture

The search module consists of several components:

1. **TantivyIndexer**: Core indexing engine that stores Kafka messages
2. **SearchApi**: REST API server providing Elasticsearch-compatible endpoints
3. **SearchIntegration**: Bridge between the indexer and API for Kafka message processing
4. **Handlers**: HTTP request handlers implementing the API endpoints

## API Endpoints

### Health & Monitoring
- `GET /health` - Health check endpoint
- `GET /metrics` - Prometheus metrics

### Search Operations
- `GET|POST /_search` - Search across all indices
- `GET|POST /{index}/_search` - Search within a specific index

### Document Operations
- `POST|PUT /{index}/_doc/{id}` - Index or update a document
- `GET /{index}/_doc/{id}` - Retrieve a document by ID
- `DELETE /{index}/_doc/{id}` - Delete a document

### Index Management
- `PUT /{index}` - Create an index with optional mapping
- `DELETE /{index}` - Delete an index
- `GET /{index}/_mapping` - Get index mapping
- `GET /_cat/indices` - List all indices

## Usage

### Starting the Server

```bash
cargo run --bin search-server
```

The server will start on port 9200 by default (configurable).

### Creating an Index

```bash
curl -X PUT "localhost:9200/messages" -H 'Content-Type: application/json' -d'
{
  "mappings": {
    "properties": {
      "topic": { "type": "keyword" },
      "value": { "type": "text" },
      "timestamp": { "type": "long" }
    }
  }
}'
```

### Indexing Documents

```bash
curl -X POST "localhost:9200/messages/_doc/1" -H 'Content-Type: application/json' -d'
{
  "topic": "orders",
  "value": "Order created for customer John Doe",
  "timestamp": 1640000000000
}'
```

### Searching

```bash
# Match query
curl -X POST "localhost:9200/messages/_search" -H 'Content-Type: application/json' -d'
{
  "query": {
    "match": {
      "value": "customer"
    }
  }
}'

# Bool query
curl -X POST "localhost:9200/messages/_search" -H 'Content-Type: application/json' -d'
{
  "query": {
    "bool": {
      "must": [
        {"term": {"topic": "orders"}},
        {"match": {"value": "created"}}
      ]
    }
  }
}'
```

## Query Types

### Match Query
Analyzes text and finds documents containing the terms:
```json
{
  "query": {
    "match": {
      "field_name": "search text"
    }
  }
}
```

### Term Query
Exact term matching (not analyzed):
```json
{
  "query": {
    "term": {
      "field_name": "exact_value"
    }
  }
}
```

### Range Query
Numeric range queries:
```json
{
  "query": {
    "range": {
      "field_name": {
        "gte": 100,
        "lte": 200
      }
    }
  }
}
```

### Bool Query
Combine multiple queries with boolean logic:
```json
{
  "query": {
    "bool": {
      "must": [...],      // AND - must match all
      "should": [...],    // OR - should match at least one
      "must_not": [...],  // NOT - must not match any
      "filter": [...]     // AND - must match but doesn't affect score
    }
  }
}
```

## Integration with Kafka

The search module can automatically index Kafka messages:

```rust
use chronik_search::{SearchApi, SearchIntegration, IndexerConfig};
use std::sync::Arc;

#[tokio::main]
async fn main() {
    // Create search API
    let api = Arc::new(SearchApi::new().unwrap());
    
    // Create integration
    let mut integration = SearchIntegration::new(api.clone()).await.unwrap();
    
    // Initialize Kafka indexer
    let config = IndexerConfig {
        index_path: "/data/search/index".to_string(),
        heap_size_bytes: 512 * 1024 * 1024,
        num_threads: 4,
        commit_interval_secs: 5,
    };
    
    integration.init_kafka_indexer(config).await.unwrap();
    
    // Index a batch of messages
    let batch = get_kafka_batch(); // Your Kafka consumer
    integration.index_kafka_batch("my-topic", 0, &batch).await.unwrap();
}
```

## Field Types

Supported field types in mappings:

- `text` - Analyzed text for full-text search
- `keyword` - Exact string matching (not analyzed)
- `long`/`integer` - Numeric fields
- More types coming soon (date, boolean, etc.)

## Configuration

The indexer can be configured with:

- `index_path` - Directory to store index files
- `heap_size_bytes` - Memory budget for indexing
- `num_threads` - Number of indexing threads
- `commit_interval_secs` - How often to commit changes

## Performance Considerations

1. **Batch indexing**: Index multiple documents in a single request for better performance
2. **Commit interval**: Adjust based on your durability vs performance needs
3. **Memory allocation**: Increase heap size for larger indexing workloads
4. **Thread count**: Set based on available CPU cores

## Error Handling

The API returns Elasticsearch-compatible error responses:

```json
{
  "error": {
    "type": "index_not_found_exception",
    "reason": "no such index [test]"
  },
  "status": 404
}
```

Common error types:
- `index_not_found_exception` - Index doesn't exist
- `mapper_parsing_exception` - Invalid document or mapping
- `search_phase_execution_exception` - Search query error
- `version_conflict_engine_exception` - Concurrent modification

## Metrics

Prometheus metrics are available at `/metrics`:

- `search_api_requests_total` - Total API requests by method, endpoint, and status
- `search_api_request_duration_seconds` - Request duration histogram
- `search_api_errors_total` - Error counts by type

## Testing

Run the test suite:

```bash
cargo test -p chronik-search
```

Run the example client:

```bash
# Start the server first
cargo run --bin search-server

# In another terminal
cargo run --example search_api_client
```

## Future Enhancements

- Additional query types (wildcard, fuzzy, prefix)
- Aggregations support
- Index aliases
- Bulk operations
- Scroll API for large result sets
- Index templates
- More field types (date, boolean, geo_point)
- Query DSL validation
- Index settings management