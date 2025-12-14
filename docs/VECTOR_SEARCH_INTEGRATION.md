# Vector Search Integration - Missing Components

**Status**: Library implemented, NOT integrated with Kafka protocol
**Current Version**: v2.2.20

---

## Executive Summary

The HNSW vector search library exists (`chronik-storage/src/vector_search.rs`) but is **completely disconnected** from the Kafka protocol. It cannot be used by Kafka clients in any way.

### What We Have
- ✅ HNSW index with O(log n) search (instant-distance crate)
- ✅ Multiple distance metrics (Euclidean, Cosine, InnerProduct)
- ✅ Serialization/deserialization
- ✅ Benchmarks

### What We DON'T Have
- ❌ No embedding model integration
- ❌ No topic-level configuration
- ❌ No produce-time indexing
- ❌ No query API
- ❌ No persistence to object store
- ❌ No cluster replication of indexes

---

## Missing Components (Detailed)

### 1. Embedding Model Integration

**Problem**: We can store/search vectors, but we can't CREATE vectors from text.

**Required**:
```rust
// New crate: chronik-embeddings/

pub trait EmbeddingModel: Send + Sync {
    fn encode(&self, text: &str) -> Result<Vec<f32>>;
    fn encode_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>>;
    fn dimensions(&self) -> usize;
}

pub struct OpenAIEmbeddings {
    api_key: String,
    model: String,  // "text-embedding-3-small"
    dimensions: usize,
}

pub struct LocalEmbeddings {
    model: SentenceTransformer,  // via candle or ort
}
```

**Dependencies**:
```toml
# Option 1: OpenAI API
reqwest = { version = "0.11", features = ["json"] }

# Option 2: Local models (larger, no API costs)
candle-core = "0.4"
candle-transformers = "0.4"
tokenizers = "0.15"
```

**Effort**: 2-3 days

---

### 2. Topic-Level Configuration

**Problem**: No way to enable vector search for a topic.

**Required changes to TopicConfig**:
```rust
// In chronik-common/src/metadata/types.rs

pub struct TopicConfig {
    pub partition_count: i32,
    pub replication_factor: i32,
    pub retention_ms: Option<i64>,
    // ... existing fields

    // NEW: Vector search configuration
    pub vector_search: Option<VectorSearchConfig>,
}

pub struct VectorSearchConfig {
    pub enabled: bool,
    pub embedding_model: EmbeddingModelConfig,
    pub field: String,              // Which message field to embed ("value", "key", or JSON path)
    pub index_config: HnswConfig,   // M, ef_construction, ef_search
}

pub enum EmbeddingModelConfig {
    OpenAI { model: String },       // "text-embedding-3-small"
    Local { model_path: String },   // Path to ONNX model
    External { endpoint: String },  // Custom embedding service
}
```

**CLI usage**:
```bash
# Create topic with vector search enabled
chronik-server topic create logs \
  --partitions 3 \
  --vector-search \
  --embedding-model openai:text-embedding-3-small \
  --vector-field value
```

**Effort**: 1 day

---

### 3. Produce-Time Indexing

**Problem**: Messages are stored but never indexed for vector search.

**Required changes to ProduceHandler**:
```rust
// In chronik-server/src/produce_handler.rs

impl ProduceHandler {
    async fn handle_produce(&self, request: ProduceRequest) -> Result<ProduceResponse> {
        // 1. Existing: Store message to WAL + segments
        let offset = self.store_message(&record).await?;

        // 2. NEW: If topic has vector search enabled
        if let Some(vs_config) = self.get_vector_search_config(&topic).await? {
            // Extract text from message
            let text = self.extract_text(&record, &vs_config.field)?;

            // Generate embedding (async, potentially slow)
            let embedding = self.embedding_service.encode(&text).await?;

            // Add to index
            self.vector_index_manager
                .add_vector(&topic, partition, offset, &embedding)
                .await?;
        }

        Ok(response)
    }
}
```

**Considerations**:
- Embedding is slow (10-100ms per message)
- Should be async/background, not blocking produce
- Need batching for efficiency
- Need error handling (what if embedding fails?)

**Effort**: 2-3 days

---

### 4. Query API

**Problem**: No way to search. Kafka protocol doesn't have a "search" API.

**Options**:

**Option A: REST API (Recommended)**
```
POST /v1/topics/{topic}/search
{
  "query": "database connection errors",
  "k": 10,
  "filter": {
    "partition": 0,
    "min_offset": 1000,
    "max_offset": 5000
  }
}

Response:
{
  "results": [
    {
      "topic": "logs",
      "partition": 0,
      "offset": 1234,
      "score": 0.95,
      "value": "MySQL connection timeout after 30s"
    }
  ]
}
```

**Option B: Custom Kafka API**
```
// New API Key: 100 (custom, not standard Kafka)
VectorSearchRequest {
    topic: String,
    query: String,
    k: i32,
}

VectorSearchResponse {
    results: Vec<SearchResult>,
}
```

**Option C: gRPC Service**
```protobuf
service VectorSearch {
  rpc Search(SearchRequest) returns (SearchResponse);
  rpc BatchSearch(BatchSearchRequest) returns (BatchSearchResponse);
}
```

**Effort**: 2-3 days

---

### 5. Index Persistence

**Problem**: Index is lost on restart.

**Required**:
```rust
// In chronik-storage/src/vector_index_store.rs

pub struct VectorIndexStore {
    object_store: Arc<dyn ObjectStore>,
    local_cache: PathBuf,
}

impl VectorIndexStore {
    /// Save index to S3/GCS
    pub async fn save(&self, topic: &str, partition: i32, index: &RealHnswIndex) -> Result<()> {
        let key = format!("vector-indexes/{}/{}/index.hnsw", topic, partition);
        let data = index.serialize()?;
        self.object_store.put(&key, data).await?;
        Ok(())
    }

    /// Load index from S3/GCS
    pub async fn load(&self, topic: &str, partition: i32) -> Result<Option<RealHnswIndex>> {
        let key = format!("vector-indexes/{}/{}/index.hnsw", topic, partition);
        match self.object_store.get(&key).await {
            Ok(data) => Ok(Some(RealHnswIndex::deserialize(&data)?)),
            Err(e) if e.is_not_found() => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Periodic snapshot (every N vectors or T seconds)
    pub async fn snapshot_loop(&self) { ... }
}
```

**Effort**: 1-2 days

---

### 6. Cluster Replication

**Problem**: In a cluster, each node would have different indexes.

**Required**:
- Leader builds the index
- Followers replicate via Raft or dedicated replication
- Or: Each node builds its own index from messages (simpler but wasteful)

**Effort**: 3-5 days

---

## Total Effort Estimate

| Component | Days | Priority |
|-----------|------|----------|
| Embedding Model Integration | 2-3 | Critical |
| Topic Configuration | 1 | Critical |
| Produce-Time Indexing | 2-3 | Critical |
| Query API (REST) | 2-3 | Critical |
| Index Persistence | 1-2 | High |
| Cluster Replication | 3-5 | Medium |
| **Total** | **11-17 days** | - |

---

## Recommendation

### Option 1: Remove the Feature
If vector search isn't a priority, remove `vector_search.rs` entirely. It adds complexity without value.

### Option 2: External Integration
Keep the library but document it as "for external use only":
```python
# Users handle embedding + indexing externally
from kafka import KafkaConsumer
from sentence_transformers import SentenceTransformer
import faiss

model = SentenceTransformer('all-MiniLM-L6-v2')
index = faiss.IndexFlatL2(384)

for msg in KafkaConsumer('logs'):
    embedding = model.encode(msg.value)
    index.add(embedding)
```

### Option 3: Full Implementation
Implement all missing components (11-17 days of work).

---

## Why This Happened

The original `vector_search.rs` was a **stub** from before the codebase cleanup. It was listed in PENDING_IMPLEMENTATIONS.md as "needs HNSW upgrade."

The Phase 4 work upgraded the algorithm but didn't question whether the feature should exist or how it would integrate. This is a case of **implementing a solution before defining the problem**.

---

## Decision Needed

1. **Do we need vector search at all?** If not, delete `vector_search.rs`
2. **If yes, what's the MVP?** OpenAI embeddings + REST API would be fastest
3. **Who is the user?** Kafka clients can't use this - only REST/gRPC clients

Please advise on direction.
