//! Test script demonstrating HNSW vector search functionality
//!
//! Run with: cargo test --test test_vector_search -- --nocapture

use chronik_storage::{RealHnswIndex, HnswConfig, DistanceMetric, VectorIndex};
use std::time::Instant;

/// Simulates word embeddings - in production these would come from an ML model
fn mock_embedding(word: &str) -> Vec<f32> {
    // Simple hash-based mock embedding (3 dimensions for demo)
    // Real embeddings would be 768-1536 dimensions from BERT/OpenAI
    let hash = word.bytes().fold(0u32, |acc, b| acc.wrapping_mul(31).wrapping_add(b as u32));
    vec![
        ((hash >> 0) & 0xFF) as f32 / 255.0,
        ((hash >> 8) & 0xFF) as f32 / 255.0,
        ((hash >> 16) & 0xFF) as f32 / 255.0,
    ]
}

#[test]
fn test_semantic_search_demo() {
    println!("\n=== HNSW Vector Search Demo ===\n");

    // Create index with 3 dimensions (real-world: 768-1536)
    let config = HnswConfig {
        dimensions: 3,
        metric: DistanceMetric::Euclidean,
        m: 16,
        ef_construction: 200,
        ef_search: 50,
    };
    let mut index = RealHnswIndex::new(config);

    // Simulate a message database with embeddings
    let messages = vec![
        (1, "database connection timeout"),
        (2, "MySQL socket closed unexpectedly"),
        (3, "PostgreSQL connection refused"),
        (4, "user login successful"),
        (5, "payment processed successfully"),
        (6, "Redis connection failed"),
        (7, "network timeout error"),
        (8, "authentication failed for user"),
    ];

    println!("Indexing {} messages...", messages.len());
    for (id, msg) in &messages {
        let embedding = mock_embedding(msg);
        index.add_vector(*id, &embedding).unwrap();
        println!("  [{:2}] \"{}\" -> [{:.2}, {:.2}, {:.2}]",
                 id, msg, embedding[0], embedding[1], embedding[2]);
    }

    // Build the HNSW graph
    println!("\nBuilding HNSW index...");
    let start = Instant::now();
    index.build().unwrap();
    println!("  Built in {:?}", start.elapsed());

    // Query: Find messages similar to "database error"
    let query = "connection error";
    let query_embedding = mock_embedding(query);
    println!("\nQuery: \"{}\" -> [{:.2}, {:.2}, {:.2}]",
             query, query_embedding[0], query_embedding[1], query_embedding[2]);

    let start = Instant::now();
    let results = index.search(&query_embedding, 3).unwrap();
    println!("\nTop 3 similar messages (in {:?}):", start.elapsed());
    for (id, distance) in results {
        let msg = messages.iter().find(|(i, _)| *i == id).map(|(_, m)| *m).unwrap();
        println!("  [{:2}] distance={:.4} \"{}\"", id, distance, msg);
    }

    println!("\n=== Demo Complete ===\n");
}

#[test]
fn test_distance_metrics() {
    println!("\n=== Distance Metric Comparison ===\n");

    let vectors = vec![
        (1, vec![1.0, 0.0, 0.0]),  // Point on X axis
        (2, vec![0.0, 1.0, 0.0]),  // Point on Y axis
        (3, vec![0.7, 0.7, 0.0]),  // 45 degrees between X and Y
        (4, vec![0.9, 0.1, 0.0]),  // Close to X axis
    ];

    let query = vec![1.0, 0.0, 0.0];  // Query on X axis

    for metric in [DistanceMetric::Euclidean, DistanceMetric::Cosine] {
        println!("{:?} distance from query [1.0, 0.0, 0.0]:", metric);

        let config = HnswConfig {
            dimensions: 3,
            metric,
            ..Default::default()
        };
        let mut index = RealHnswIndex::new(config);

        for (id, vec) in &vectors {
            index.add_vector(*id, vec).unwrap();
        }
        index.build().unwrap();

        let results = index.search(&query, 4).unwrap();
        for (id, dist) in results {
            let vec = &vectors.iter().find(|(i, _)| *i == id).unwrap().1;
            println!("  [{:2}] dist={:.4} {:?}", id, dist, vec);
        }
        println!();
    }
}

#[test]
fn test_performance_comparison() {
    println!("\n=== HNSW vs Brute Force Performance ===\n");

    for vector_count in [100, 1000, 5000] {
        let dimensions = 128;

        // Generate vectors
        let vectors: Vec<(u64, Vec<f32>)> = (0..vector_count)
            .map(|i| {
                let vec: Vec<f32> = (0..dimensions)
                    .map(|j| ((i * 7 + j) % 256) as f32 / 256.0)
                    .collect();
                (i as u64, vec)
            })
            .collect();

        let query: Vec<f32> = (0..dimensions).map(|i| (i % 128) as f32 / 128.0).collect();

        // HNSW (built)
        let config = HnswConfig {
            dimensions,
            metric: DistanceMetric::Euclidean,
            m: 16,
            ef_construction: 200,
            ef_search: 50,
        };
        let mut hnsw_index = RealHnswIndex::new(config.clone());
        for (id, vec) in &vectors {
            hnsw_index.add_vector(*id, vec).unwrap();
        }

        let build_start = Instant::now();
        hnsw_index.build().unwrap();
        let build_time = build_start.elapsed();

        // Benchmark HNSW search
        let iterations = 100;
        let start = Instant::now();
        for _ in 0..iterations {
            let _ = hnsw_index.search(&query, 10).unwrap();
        }
        let hnsw_time = start.elapsed() / iterations;

        // Brute force (not built)
        let mut brute_index = RealHnswIndex::new(config);
        for (id, vec) in &vectors {
            brute_index.add_vector(*id, vec).unwrap();
        }
        // Don't build - uses brute force fallback

        let start = Instant::now();
        for _ in 0..iterations {
            let _ = brute_index.search(&query, 10).unwrap();
        }
        let brute_time = start.elapsed() / iterations;

        let speedup = brute_time.as_nanos() as f64 / hnsw_time.as_nanos() as f64;

        println!("{} vectors ({} dimensions):", vector_count, dimensions);
        println!("  Build time:   {:?}", build_time);
        println!("  HNSW search:  {:?}", hnsw_time);
        println!("  Brute force:  {:?}", brute_time);
        println!("  Speedup:      {:.1}x", speedup);
        println!();
    }
}

#[test]
fn test_serialization() {
    println!("\n=== Index Serialization ===\n");

    let config = HnswConfig {
        dimensions: 3,
        metric: DistanceMetric::Euclidean,
        ..Default::default()
    };
    let mut index = RealHnswIndex::new(config);

    index.add_vector(1, &[1.0, 0.0, 0.0]).unwrap();
    index.add_vector(2, &[0.0, 1.0, 0.0]).unwrap();
    index.add_vector(3, &[0.0, 0.0, 1.0]).unwrap();
    index.build().unwrap();

    // Serialize
    let serialized = index.serialize().unwrap();
    println!("Serialized index size: {} bytes", serialized.len());

    // Deserialize
    let restored = RealHnswIndex::deserialize(&serialized).unwrap();
    println!("Restored index: {} vectors, {} dimensions",
             restored.count(), restored.dimensions());

    // Verify search works on restored index
    let results = restored.search(&[0.9, 0.1, 0.0], 1).unwrap();
    println!("Search on restored index: closest to [0.9, 0.1, 0.0] is vector {}", results[0].0);
    assert_eq!(results[0].0, 1);  // Should be vector 1 [1.0, 0.0, 0.0]

    println!("\nSerialization test passed!");
}
