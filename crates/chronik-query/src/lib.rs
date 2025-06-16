//! Query engine for Chronik Stream.

pub mod search;
pub mod api;
pub mod index;
pub mod aggregation;

pub use search::{SearchEngine, SearchQuery, SearchResult};
pub use api::{QueryApi, QueryConfig};
pub use index::{Indexer, IndexSchema};
pub use aggregation::{AggregationEngine, AggregationQuery, AggregationResult, WindowType, AggregationFunction};