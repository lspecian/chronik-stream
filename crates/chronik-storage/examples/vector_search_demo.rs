//! Vector Search HNSW Demo
//!
//! This example demonstrates how to use HNSW for semantic similarity search.
//!
//! Run with: cargo run --example vector_search_demo

use chronik_storage::{RealHnswIndex, HnswConfig, DistanceMetric, VectorIndex};
use std::time::Instant;

fn main() {
    println!("╔═══════════════════════════════════════════════════════════╗");
    println!("║           HNSW Vector Search Demo                         ║");
    println!("╚═══════════════════════════════════════════════════════════╝\n");

    // =========================================================================
    // Part 1: Basic Concept - Finding Similar Points
    // =========================================================================
    println!("═══ Part 1: Basic Similarity Search ═══\n");

    // Create a 3D index (easy to visualize)
    let config = HnswConfig {
        dimensions: 3,
        metric: DistanceMetric::Euclidean,
        m: 16,
        ef_construction: 200,
        ef_search: 50,
    };
    let mut index = RealHnswIndex::new(config);

    // Add points representing colors in RGB space
    let colors = vec![
        (1, "Red",    vec![1.0, 0.0, 0.0]),
        (2, "Green",  vec![0.0, 1.0, 0.0]),
        (3, "Blue",   vec![0.0, 0.0, 1.0]),
        (4, "Yellow", vec![1.0, 1.0, 0.0]),
        (5, "Cyan",   vec![0.0, 1.0, 1.0]),
        (6, "Purple", vec![0.5, 0.0, 0.5]),
        (7, "Orange", vec![1.0, 0.5, 0.0]),
        (8, "Pink",   vec![1.0, 0.4, 0.7]),
    ];

    println!("Adding colors as 3D vectors (RGB values):");
    for (id, name, rgb) in &colors {
        index.add_vector(*id, rgb).unwrap();
        println!("  {:2}. {:8} → [{:.1}, {:.1}, {:.1}]", id, name, rgb[0], rgb[1], rgb[2]);
    }

    // Build the HNSW graph
    index.build().unwrap();
    println!("\n✓ HNSW index built\n");

    // Query: What's closest to a light red/pink?
    let query = vec![0.9, 0.3, 0.2];
    println!("Query: [{:.1}, {:.1}, {:.1}] (a pinkish-red)", query[0], query[1], query[2]);

    let results = index.search(&query, 3).unwrap();
    println!("\nTop 3 similar colors:");
    for (id, distance) in results {
        let (_, name, _) = colors.iter().find(|(i, _, _)| *i == id).unwrap();
        println!("  {:8} (distance: {:.4})", name, distance);
    }

    // =========================================================================
    // Part 2: Performance Comparison
    // =========================================================================
    println!("\n═══ Part 2: HNSW vs Brute Force Performance ═══\n");

    for vector_count in [1000, 10000] {
        let dimensions = 128;

        // Generate random vectors
        let vectors: Vec<(u64, Vec<f32>)> = (0..vector_count)
            .map(|i| {
                let vec: Vec<f32> = (0..dimensions)
                    .map(|j| ((i * 7 + j) % 256) as f32 / 256.0)
                    .collect();
                (i as u64, vec)
            })
            .collect();

        let query: Vec<f32> = (0..dimensions).map(|i| (i % 128) as f32 / 128.0).collect();

        // Build HNSW index
        let config = HnswConfig {
            dimensions,
            metric: DistanceMetric::Euclidean,
            m: 16,
            ef_construction: 200,
            ef_search: 50,
        };
        let mut hnsw = RealHnswIndex::new(config.clone());
        for (id, vec) in &vectors {
            hnsw.add_vector(*id, vec).unwrap();
        }

        let build_start = Instant::now();
        hnsw.build().unwrap();
        let build_time = build_start.elapsed();

        // Benchmark HNSW
        let iterations = 100;
        let start = Instant::now();
        for _ in 0..iterations {
            let _ = hnsw.search(&query, 10).unwrap();
        }
        let hnsw_avg = start.elapsed() / iterations;

        // Benchmark brute force (don't build the index)
        let mut brute = RealHnswIndex::new(config);
        for (id, vec) in &vectors {
            brute.add_vector(*id, vec).unwrap();
        }

        let start = Instant::now();
        for _ in 0..iterations {
            let _ = brute.search(&query, 10).unwrap();
        }
        let brute_avg = start.elapsed() / iterations;

        let speedup = brute_avg.as_nanos() as f64 / hnsw_avg.as_nanos() as f64;

        println!("{} vectors × {} dimensions:", vector_count, dimensions);
        println!("  ├─ Build time:     {:>10?}", build_time);
        println!("  ├─ HNSW search:    {:>10?} per query", hnsw_avg);
        println!("  ├─ Brute force:    {:>10?} per query", brute_avg);
        println!("  └─ Speedup:        {:>10.1}x faster\n", speedup);
    }

    // =========================================================================
    // Part 3: Different Distance Metrics
    // =========================================================================
    println!("═══ Part 3: Distance Metrics Comparison ═══\n");

    let test_vectors = vec![
        (1, vec![1.0, 0.0, 0.0]),      // Unit vector X
        (2, vec![0.0, 1.0, 0.0]),      // Unit vector Y
        (3, vec![0.707, 0.707, 0.0]),  // 45° between X and Y
        (4, vec![2.0, 0.0, 0.0]),      // Longer vector on X
    ];
    let query = vec![1.0, 0.0, 0.0];

    for metric in [DistanceMetric::Euclidean, DistanceMetric::Cosine, DistanceMetric::InnerProduct] {
        println!("{:?} from query [1.0, 0.0, 0.0]:", metric);

        let config = HnswConfig {
            dimensions: 3,
            metric,
            ..Default::default()
        };
        let mut index = RealHnswIndex::new(config);
        for (id, vec) in &test_vectors {
            index.add_vector(*id, vec).unwrap();
        }
        index.build().unwrap();

        let results = index.search(&query, 4).unwrap();
        for (id, score) in results {
            let vec = &test_vectors.iter().find(|(i, _)| *i == id).unwrap().1;
            println!("  {:?} → score: {:+.4}", vec, score);
        }
        println!();
    }

    println!("═══════════════════════════════════════════════════════════");
    println!("Key Takeaways:");
    println!("  • Euclidean: Measures straight-line distance");
    println!("  • Cosine: Measures angle (ignores magnitude)");
    println!("  • InnerProduct: Higher = more similar (for normalized vectors)");
    println!("═══════════════════════════════════════════════════════════\n");
}
