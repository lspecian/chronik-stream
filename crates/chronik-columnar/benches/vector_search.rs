//! Benchmarks for vector search latency
//!
//! Measures:
//! - HNSW index population time
//! - Vector search latency by index size
//! - Search with different k values (top-10, top-50, top-100)
//! - Different distance metrics

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use tempfile::tempdir;

use chronik_columnar::{
    VectorIndexManager, VectorIndexConfig, HnswIndexConfig, DistanceMetric, VectorEntry,
};

/// Generate random vectors for benchmarking
fn generate_vectors(count: usize, dimensions: usize) -> Vec<VectorEntry> {
    (0..count)
        .map(|i| {
            // Deterministic pseudo-random vectors for reproducibility
            let vector: Vec<f32> = (0..dimensions)
                .map(|j| {
                    let seed = (i * 7919 + j * 6983) % 65536;
                    (seed as f32 / 65536.0) * 2.0 - 1.0 // Range [-1, 1]
                })
                .collect();

            VectorEntry {
                offset: i as i64,
                vector,
                text_preview: Some(format!("Document {} content preview...", i)),
            }
        })
        .collect()
}

/// Generate a query vector
fn generate_query_vector(dimensions: usize, seed: usize) -> Vec<f32> {
    (0..dimensions)
        .map(|j| {
            let s = (seed * 8191 + j * 7393) % 65536;
            (s as f32 / 65536.0) * 2.0 - 1.0
        })
        .collect()
}

/// Create an index manager and populate it with vectors
fn setup_index(vector_count: usize, dimensions: usize, metric: DistanceMetric) -> (VectorIndexManager, tempfile::TempDir) {
    let dir = tempdir().unwrap();

    let config = VectorIndexConfig {
        base_path: dir.path().to_path_buf(),
        default_hnsw_config: HnswIndexConfig {
            dimensions,
            metric,
            m: 16,
            ef_construction: 100,
            ef_search: 50,
        },
        max_vectors_in_memory: 100_000,
        snapshot_interval_secs: 300,
    };

    let manager = VectorIndexManager::new(config.clone());
    let runtime = tokio::runtime::Runtime::new().unwrap();

    runtime.block_on(async {
        // Register topic with HNSW config
        manager.register_topic("bench-topic", config.default_hnsw_config.clone()).await;

        // Generate and add vectors
        let vectors = generate_vectors(vector_count, dimensions);

        // Add in batches to partition 0
        for chunk in vectors.chunks(1000) {
            manager.add_vectors("bench-topic", 0, chunk.to_vec()).await.unwrap();
        }
    });

    (manager, dir)
}

/// Benchmark index population time
fn bench_index_populate(c: &mut Criterion) {
    let mut group = c.benchmark_group("hnsw_populate");
    group.sample_size(10); // Populating is slow

    let dimensions = 384; // all-MiniLM-L6-v2

    for vector_count in [1000, 5000, 10000].iter() {
        let vectors = generate_vectors(*vector_count, dimensions);

        group.throughput(Throughput::Elements(*vector_count as u64));
        group.bench_with_input(
            BenchmarkId::new("vectors", vector_count),
            &vectors,
            |b, vectors| {
                b.iter(|| {
                    let dir = tempdir().unwrap();
                    let config = VectorIndexConfig {
                        base_path: dir.path().to_path_buf(),
                        default_hnsw_config: HnswIndexConfig {
                            dimensions,
                            metric: DistanceMetric::Cosine,
                            m: 16,
                            ef_construction: 100,
                            ef_search: 50,
                        },
                        max_vectors_in_memory: 100_000,
                        snapshot_interval_secs: 300,
                    };

                    let manager = VectorIndexManager::new(config.clone());
                    let runtime = tokio::runtime::Runtime::new().unwrap();

                    runtime.block_on(async {
                        manager.register_topic("bench-topic", config.default_hnsw_config).await;
                        manager.add_vectors("bench-topic", 0, vectors.clone()).await.unwrap();
                    });

                    black_box(manager)
                });
            },
        );
    }

    group.finish();
}

/// Benchmark vector search latency by index size
fn bench_search_by_size(c: &mut Criterion) {
    let mut group = c.benchmark_group("vector_search_by_size");

    let dimensions = 384;
    let k = 10;

    for vector_count in [1000, 10000, 50000].iter() {
        let (manager, _dir) = setup_index(*vector_count, dimensions, DistanceMetric::Cosine);
        let query = generate_query_vector(dimensions, 42);

        group.bench_with_input(
            BenchmarkId::new("vectors", vector_count),
            &query,
            |b, query| {
                let runtime = tokio::runtime::Runtime::new().unwrap();
                b.iter(|| {
                    runtime.block_on(async {
                        let results = manager
                            .search("bench-topic", 0, black_box(query), k)
                            .await
                            .unwrap();
                        black_box(results)
                    })
                });
            },
        );
    }

    group.finish();
}

/// Benchmark vector search with different k values
fn bench_search_by_k(c: &mut Criterion) {
    let mut group = c.benchmark_group("vector_search_by_k");

    let dimensions = 384;
    let vector_count = 10000;
    let (manager, _dir) = setup_index(vector_count, dimensions, DistanceMetric::Cosine);
    let query = generate_query_vector(dimensions, 42);

    for k in [10, 50, 100, 200].iter() {
        group.bench_with_input(BenchmarkId::new("k", k), k, |b, &k| {
            let runtime = tokio::runtime::Runtime::new().unwrap();
            b.iter(|| {
                runtime.block_on(async {
                    let results = manager
                        .search("bench-topic", 0, black_box(&query), k)
                        .await
                        .unwrap();
                    black_box(results)
                })
            });
        });
    }

    group.finish();
}

/// Benchmark search across multiple partitions
fn bench_search_multi_partition(c: &mut Criterion) {
    let mut group = c.benchmark_group("vector_search_multi_partition");

    let dimensions = 384;
    let vectors_per_partition = 5000;
    let num_partitions = 6;
    let k = 10;

    // Setup index with vectors across multiple partitions
    let dir = tempdir().unwrap();
    let config = VectorIndexConfig {
        base_path: dir.path().to_path_buf(),
        default_hnsw_config: HnswIndexConfig {
            dimensions,
            metric: DistanceMetric::Cosine,
            m: 16,
            ef_construction: 100,
            ef_search: 50,
        },
        max_vectors_in_memory: 100_000,
        snapshot_interval_secs: 300,
    };

    let manager = VectorIndexManager::new(config.clone());
    let runtime = tokio::runtime::Runtime::new().unwrap();

    runtime.block_on(async {
        manager.register_topic("bench-topic", config.default_hnsw_config.clone()).await;

        for partition in 0..num_partitions {
            let vectors = generate_vectors(vectors_per_partition, dimensions);
            manager.add_vectors("bench-topic", partition, vectors).await.unwrap();
        }
    });

    let query = generate_query_vector(dimensions, 42);

    // Single partition search
    group.bench_function("single_partition", |b| {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        b.iter(|| {
            runtime.block_on(async {
                let results = manager
                    .search("bench-topic", 0, black_box(&query), k)
                    .await
                    .unwrap();
                black_box(results)
            })
        });
    });

    // All partitions search (search_topic)
    group.bench_function("all_partitions", |b| {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        b.iter(|| {
            runtime.block_on(async {
                let results = manager
                    .search_topic("bench-topic", black_box(&query), k)
                    .await
                    .unwrap();
                black_box(results)
            })
        });
    });

    group.finish();
}

/// Benchmark different distance metrics
fn bench_distance_metrics(c: &mut Criterion) {
    let mut group = c.benchmark_group("vector_distance_metrics");

    let dimensions = 384;
    let vector_count = 10000;
    let k = 10;

    for metric in [DistanceMetric::Cosine, DistanceMetric::Euclidean, DistanceMetric::DotProduct] {
        let metric_name = format!("{:?}", metric);
        let (manager, _dir) = setup_index(vector_count, dimensions, metric);
        let query = generate_query_vector(dimensions, 42);

        group.bench_function(&metric_name, |b| {
            let runtime = tokio::runtime::Runtime::new().unwrap();
            b.iter(|| {
                runtime.block_on(async {
                    let results = manager
                        .search("bench-topic", 0, black_box(&query), k)
                        .await
                        .unwrap();
                    black_box(results)
                })
            });
        });
    }

    group.finish();
}

/// Benchmark different embedding dimensions
fn bench_dimensions(c: &mut Criterion) {
    let mut group = c.benchmark_group("vector_dimensions");

    let vector_count = 10000;
    let k = 10;

    for dimensions in [128, 384, 768, 1536].iter() {
        let (manager, _dir) = setup_index(vector_count, *dimensions, DistanceMetric::Cosine);
        let query = generate_query_vector(*dimensions, 42);

        group.bench_with_input(
            BenchmarkId::new("dims", dimensions),
            dimensions,
            |b, _| {
                let runtime = tokio::runtime::Runtime::new().unwrap();
                b.iter(|| {
                    runtime.block_on(async {
                        let results = manager
                            .search("bench-topic", 0, black_box(&query), k)
                            .await
                            .unwrap();
                        black_box(results)
                    })
                });
            },
        );
    }

    group.finish();
}

/// Benchmark HNSW parameter tuning (M parameter)
fn bench_hnsw_m_parameter(c: &mut Criterion) {
    let mut group = c.benchmark_group("hnsw_m_parameter");
    group.sample_size(10);

    let dimensions = 384;
    let vector_count = 10000;
    let k = 10;

    for m in [8, 16, 32, 64].iter() {
        let dir = tempdir().unwrap();
        let config = VectorIndexConfig {
            base_path: dir.path().to_path_buf(),
            default_hnsw_config: HnswIndexConfig {
                dimensions,
                metric: DistanceMetric::Cosine,
                m: *m,
                ef_construction: 100,
                ef_search: 50,
            },
            max_vectors_in_memory: 100_000,
            snapshot_interval_secs: 300,
        };

        let manager = VectorIndexManager::new(config.clone());
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let vectors = generate_vectors(vector_count, dimensions);

        runtime.block_on(async {
            manager.register_topic("bench-topic", config.default_hnsw_config).await;
            manager.add_vectors("bench-topic", 0, vectors).await.unwrap();
        });

        let query = generate_query_vector(dimensions, 42);

        group.bench_with_input(BenchmarkId::new("m", m), m, |b, _| {
            let runtime = tokio::runtime::Runtime::new().unwrap();
            b.iter(|| {
                runtime.block_on(async {
                    let results = manager
                        .search("bench-topic", 0, black_box(&query), k)
                        .await
                        .unwrap();
                    black_box(results)
                })
            });
        });
    }

    group.finish();
}

criterion_group!(
    benches,
    bench_index_populate,
    bench_search_by_size,
    bench_search_by_k,
    bench_search_multi_partition,
    bench_distance_metrics,
    bench_dimensions,
    bench_hnsw_m_parameter
);
criterion_main!(benches);
