//! Query engine for Chronik Stream.
//!
//! This crate provides the unified query orchestration layer that ties together
//! all query backends (Tantivy, DataFusion, HNSW, WAL) into a single `/_query`
//! endpoint with parallel execution, RRF-based fusion, and pluggable ranking.
//!
//! ## Architecture
//!
//! ```text
//! POST /_query
//!     │
//!     ▼
//! QueryPlanner (build execution plan from request + capabilities)
//!     │
//!     ├─ TextNode      → Tantivy SearchApi
//!     ├─ VectorNode    → VectorSearchService
//!     ├─ SqlNode       → ColumnarQueryEngine
//!     └─ FetchNode     → WalManager
//!     │
//!     ▼ (parallel execution, per-topic timeout)
//! CandidateCollector (normalize into Vec<Candidate>)
//!     │
//!     ▼
//! RRF Merger (per-topic: merge text+vector+sql ranks)
//!     │
//!     ▼
//! RuleRanker (apply profile weights + freshness/boost)
//!     │
//!     ▼
//! Response (ranked results + explanations + stats)
//! ```

// Existing modules (production)
pub mod search;
pub mod api;
pub mod index;
pub mod aggregation;
pub mod query_executor;
pub mod translator;

// Phase 9: Unified query orchestration
pub mod types;
pub mod candidate;
pub mod capabilities;
pub mod plan;
pub mod rrf;
pub mod ranker;
pub mod profiles;
pub mod orchestrator;
pub mod features;

// Re-exports (existing)
pub use search::{SearchEngine, SearchQuery, SearchResult};
pub use api::{QueryApi, QueryConfig};
pub use index::{Indexer, IndexSchema};
pub use aggregation::{AggregationEngine, AggregationQuery, AggregationResult, WindowType, AggregationFunction};

// Re-exports (Phase 9)
pub use types::{QueryRequest, QueryResponse, QueryMode, SourceSpec, QuerySpec};
pub use candidate::{Candidate, CandidateSet, CandidateEntry};
pub use capabilities::{TopicCapabilities, CapabilityDetector, CapabilitiesCache};
pub use plan::{QueryPlan, ExecutionNode, QueryPlanner, PlanError};
pub use rrf::RrfMerger;
pub use ranker::{Ranker, RuleRanker};
pub use profiles::{RankingProfile, ProfileStore};
pub use orchestrator::QueryOrchestrator;
pub use features::FeatureLogger;
