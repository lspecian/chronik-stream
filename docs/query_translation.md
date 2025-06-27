# Tantivy Query Translation Layer

The Chronik Stream query translation layer provides a comprehensive interface for translating various query formats into Tantivy's native query objects. This enables users to query Kafka data using familiar syntax while leveraging Tantivy's powerful search capabilities.

## Supported Query Formats

### 1. Elasticsearch-Compatible JSON Query DSL

The translator supports most common Elasticsearch query types:

```json
{
  "bool": {
    "must": [
      {"term": {"topic": "events"}},
      {"range": {"timestamp": {"gte": 1000000, "lte": 2000000}}}
    ],
    "should": [
      {"match": {"value": "error warning"}},
      {"wildcard": {"key": "user:*"}}
    ],
    "must_not": [
      {"term": {"user": "system"}}
    ],
    "filter": [
      {"term": {"partition": 0}}
    ]
  }
}
```

Supported query types:
- `match` - Full-text search with analyzer support
- `match_all` - Match all documents
- `match_phrase` - Exact phrase matching
- `term` - Exact term matching (not analyzed)
- `terms` - Match any of multiple exact values
- `range` - Numeric/date range queries
- `bool` - Boolean combinations (must/should/must_not/filter)
- `wildcard` - Wildcard pattern matching (* and ?)
- `regexp` - Regular expression matching
- `fuzzy` - Fuzzy matching with edit distance
- `exists` - Check if field has any value

### 2. SQL-Like Queries

Basic SQL WHERE clause syntax is supported:

```sql
SELECT * FROM messages 
WHERE topic = 'events' 
  AND timestamp > 1000000 
  AND value LIKE '%error%'
  AND partition IN (0, 1, 2)
  AND offset BETWEEN 100 AND 200
```

Supported SQL features:
- Comparison operators: `=`, `!=`, `<>`, `>`, `>=`, `<`, `<=`
- `LIKE` with % and _ wildcards
- `IN` clause for multiple values
- `BETWEEN` for range queries
- `AND`/`OR` boolean logic
- String, numeric, and boolean literals

### 3. Kafka-Specific Query Format

A structured format designed specifically for querying Kafka data:

```rust
KafkaQuery {
    topic: Some("events"),
    partition: Some(0),
    start_offset: Some(1000),
    end_offset: Some(2000),
    start_timestamp: Some(1640995200000),
    end_timestamp: Some(1641081600000),
    key_pattern: Some("user:*"),
    value_query: Some(ValueQuery::Text("error OR warning")),
    headers: {
        "type": "error",
        "severity": "high"
    }
}
```

### 4. Simple Text Search

Basic text search across all text fields:

```
"error warning critical"
```

## Usage Example

```rust
use chronik_query::{QueryTranslator, QueryInput};
use tantivy::schema::Schema;

// Create translator with your schema
let translator = QueryTranslator::new(schema);

// Translate Elasticsearch query
let es_query = serde_json::json!({
    "match": {"value": "error"}
});
let tantivy_query = translator.translate_json_query(&es_query)?;

// Translate SQL query
let sql_query = QueryInput::Sql(
    "SELECT * FROM messages WHERE topic = 'events'".to_string()
);
let tantivy_query = translator.translate(&sql_query)?;

// Execute with Tantivy
let searcher = index_reader.searcher();
let top_docs = searcher.search(&*tantivy_query, &TopDocs::with_limit(10))?;
```

## Integration with Search API

The translation layer is integrated into the search API handlers:

```rust
// In your API handler
let translator = QueryTranslator::new(schema);
let tantivy_query = translator.translate_json_query(&request.query)?;
let results = searcher.search(&*tantivy_query, &collector)?;
```

## Query Optimization

The translator includes several optimizations:

1. **Wildcard to Regex Conversion**: Wildcard patterns are converted to efficient regex patterns
2. **Range Query Optimization**: Numeric ranges use Tantivy's efficient range query implementation
3. **Boolean Query Flattening**: Nested boolean queries are optimized where possible
4. **Term Query Caching**: Frequently used terms can be cached (implementation pending)

## Error Handling

The translator provides detailed error messages for:
- Unknown query types
- Invalid field names
- Type mismatches (e.g., range query on text field)
- Malformed query syntax
- Unsupported operations

## Extending the Translator

To add support for new query types:

1. Add the new query type to the match statement in `translate_json_query`
2. Implement the translation logic
3. Add tests for the new query type
4. Update documentation

## Performance Considerations

- Complex wildcard queries (e.g., `*error*`) can be slow on large datasets
- Prefer term queries over match queries when exact matching is needed
- Use filter context in bool queries for non-scoring filters
- Limit the use of fuzzy queries as they can be computationally expensive

## Testing

Run the comprehensive test suite:

```bash
cargo test -p chronik-query translator
```

Run the example:

```bash
cargo run --example query_translation
```

## Future Enhancements

- Support for aggregations
- Query result highlighting
- Query suggestions and auto-completion
- Query performance profiling
- Support for more Elasticsearch query types (geo queries, etc.)
- Query caching layer
- Query rewriting optimizations