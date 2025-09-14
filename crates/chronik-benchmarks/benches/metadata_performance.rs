//! Performance benchmarks comparing WAL vs TiKV metadata stores

use criterion::{black_box, criterion_group, criterion_main, Criterion, BenchmarkId};
use tokio::runtime::Runtime;
use std::sync::Arc;
use tempfile::TempDir;
use uuid::Uuid;

use chronik_common::metadata::{
    MetadataStore, TopicConfig, TopicMetadata, BrokerMetadata, BrokerStatus,
    ConsumerGroupMetadata, ConsumerOffset, SegmentMetadata,
    metalog_store::ChronikMetaLogStore,
};
use chronik_storage::metadata_wal_adapter::WalMetadataAdapter;
use chronik_wal::config::WalConfig;

struct BenchmarkSetup {
    wal_store: Arc<ChronikMetaLogStore>,
    temp_dir: TempDir,
}

impl BenchmarkSetup {
    async fn new() -> Self {
        let temp_dir = TempDir::new().unwrap();
        let wal_config = WalConfig {
            enabled: true,
            data_dir: temp_dir.path().to_path_buf(),
            flush_interval_ms: 10, // Fast flush for benchmarks
            flush_threshold: 1024, // Small threshold
            ..Default::default()
        };

        let adapter = Arc::new(WalMetadataAdapter::new(wal_config).await.unwrap());
        let wal_store = Arc::new(ChronikMetaLogStore::new(adapter).await.unwrap());

        Self { wal_store, temp_dir }
    }
}

fn bench_topic_operations(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();

    let mut group = c.benchmark_group("topic_operations");

    // Test different topic counts
    for topic_count in [10, 100, 1000].iter() {
        group.bench_with_input(
            BenchmarkId::new("wal_create_topics", topic_count),
            topic_count,
            |b, &topic_count| {
                b.to_async(&rt).iter(|| async {
                    let setup = BenchmarkSetup::new().await;

                    for i in 0..topic_count {
                        let topic_name = format!("test-topic-{}", i);
                        let config = TopicConfig {
                            partition_count: 3,
                            replication_factor: 1,
                            ..Default::default()
                        };

                        black_box(
                            setup.wal_store.create_topic(&topic_name, config).await.unwrap()
                        );
                    }
                })
            },
        );

        group.bench_with_input(
            BenchmarkId::new("wal_read_topics", topic_count),
            topic_count,
            |b, &topic_count| {
                b.to_async(&rt).iter(|| async {
                    let setup = BenchmarkSetup::new().await;

                    // Pre-create topics
                    for i in 0..topic_count {
                        let topic_name = format!("test-topic-{}", i);
                        let config = TopicConfig::default();
                        setup.wal_store.create_topic(&topic_name, config).await.unwrap();
                    }

                    // Benchmark reading all topics
                    black_box(setup.wal_store.list_topics().await.unwrap());
                })
            },
        );

        group.bench_with_input(
            BenchmarkId::new("wal_get_single_topic", topic_count),
            topic_count,
            |b, &topic_count| {
                b.to_async(&rt).iter(|| async {
                    let setup = BenchmarkSetup::new().await;

                    // Pre-create topics
                    for i in 0..topic_count {
                        let topic_name = format!("test-topic-{}", i);
                        let config = TopicConfig::default();
                        setup.wal_store.create_topic(&topic_name, config).await.unwrap();
                    }

                    // Benchmark getting a specific topic
                    black_box(
                        setup.wal_store.get_topic("test-topic-50").await.unwrap()
                    );
                })
            },
        );
    }

    group.finish();
}

fn bench_segment_operations(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();

    let mut group = c.benchmark_group("segment_operations");

    for segment_count in [50, 500, 2000].iter() {
        group.bench_with_input(
            BenchmarkId::new("wal_persist_segments", segment_count),
            segment_count,
            |b, &segment_count| {
                b.to_async(&rt).iter(|| async {
                    let setup = BenchmarkSetup::new().await;

                    // Create a topic first
                    setup.wal_store.create_topic("test-topic", TopicConfig::default()).await.unwrap();

                    for i in 0..segment_count {
                        let segment = SegmentMetadata {
                            segment_id: format!("segment-{}", i),
                            topic: "test-topic".to_string(),
                            partition: i % 10, // 10 partitions
                            start_offset: (i * 1000) as i64,
                            end_offset: ((i + 1) * 1000) as i64,
                            size: 1024 * 1024, // 1MB
                            record_count: 1000,
                            path: format!("/data/segments/segment-{}", i),
                            created_at: chrono::Utc::now(),
                        };

                        black_box(
                            setup.wal_store.persist_segment_metadata(segment).await.unwrap()
                        );
                    }
                })
            },
        );

        group.bench_with_input(
            BenchmarkId::new("wal_list_segments", segment_count),
            segment_count,
            |b, &segment_count| {
                b.to_async(&rt).iter(|| async {
                    let setup = BenchmarkSetup::new().await;

                    // Create topic and segments
                    setup.wal_store.create_topic("test-topic", TopicConfig::default()).await.unwrap();

                    for i in 0..segment_count {
                        let segment = SegmentMetadata {
                            segment_id: format!("segment-{}", i),
                            topic: "test-topic".to_string(),
                            partition: 0,
                            start_offset: (i * 1000) as i64,
                            end_offset: ((i + 1) * 1000) as i64,
                            size: 1024 * 1024,
                            record_count: 1000,
                            path: format!("/data/segments/segment-{}", i),
                            created_at: chrono::Utc::now(),
                        };

                        setup.wal_store.persist_segment_metadata(segment).await.unwrap();
                    }

                    // Benchmark listing segments
                    black_box(
                        setup.wal_store.list_segments("test-topic", Some(0)).await.unwrap()
                    );
                })
            },
        );
    }

    group.finish();
}

fn bench_consumer_operations(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();

    let mut group = c.benchmark_group("consumer_operations");

    for consumer_count in [10, 100, 500].iter() {
        group.bench_with_input(
            BenchmarkId::new("wal_create_consumer_groups", consumer_count),
            consumer_count,
            |b, &consumer_count| {
                b.to_async(&rt).iter(|| async {
                    let setup = BenchmarkSetup::new().await;

                    for i in 0..consumer_count {
                        let group_metadata = ConsumerGroupMetadata {
                            group_id: format!("group-{}", i),
                            state: "Stable".to_string(),
                            protocol: "range".to_string(),
                            protocol_type: "consumer".to_string(),
                            generation_id: 1,
                            leader_id: Some(format!("consumer-{}-0", i)),
                            created_at: chrono::Utc::now(),
                            updated_at: chrono::Utc::now(),
                        };

                        black_box(
                            setup.wal_store.create_consumer_group(group_metadata).await.unwrap()
                        );
                    }
                })
            },
        );

        group.bench_with_input(
            BenchmarkId::new("wal_commit_offsets", consumer_count),
            consumer_count,
            |b, &consumer_count| {
                b.to_async(&rt).iter(|| async {
                    let setup = BenchmarkSetup::new().await;

                    // Pre-create topic and consumer group
                    setup.wal_store.create_topic("test-topic", TopicConfig::default()).await.unwrap();

                    let group_metadata = ConsumerGroupMetadata {
                        group_id: "test-group".to_string(),
                        state: "Stable".to_string(),
                        protocol: "range".to_string(),
                        protocol_type: "consumer".to_string(),
                        generation_id: 1,
                        leader_id: Some("consumer-0".to_string()),
                        created_at: chrono::Utc::now(),
                        updated_at: chrono::Utc::now(),
                    };
                    setup.wal_store.create_consumer_group(group_metadata).await.unwrap();

                    // Benchmark offset commits
                    for i in 0..consumer_count {
                        let offset = ConsumerOffset {
                            group_id: "test-group".to_string(),
                            topic: "test-topic".to_string(),
                            partition: i % 10,
                            offset: (i * 1000) as i64,
                            metadata: Some("test-metadata".to_string()),
                            commit_timestamp: chrono::Utc::now(),
                        };

                        black_box(
                            setup.wal_store.commit_offset(offset).await.unwrap()
                        );
                    }
                })
            },
        );
    }

    group.finish();
}

fn bench_mixed_workload(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();

    let mut group = c.benchmark_group("mixed_workload");

    group.bench_function("wal_typical_workload", |b| {
        b.to_async(&rt).iter(|| async {
            let setup = BenchmarkSetup::new().await;

            // Create some topics
            for i in 0..10 {
                let topic_name = format!("topic-{}", i);
                let config = TopicConfig {
                    partition_count: 3,
                    replication_factor: 1,
                    ..Default::default()
                };
                setup.wal_store.create_topic(&topic_name, config).await.unwrap();
            }

            // Create some segments
            for i in 0..50 {
                let segment = SegmentMetadata {
                    segment_id: format!("segment-{}", i),
                    topic: format!("topic-{}", i % 10),
                    partition: i % 3,
                    start_offset: (i * 1000) as i64,
                    end_offset: ((i + 1) * 1000) as i64,
                    size: 1024 * 1024,
                    record_count: 1000,
                    path: format!("/data/segments/segment-{}", i),
                    created_at: chrono::Utc::now(),
                };
                setup.wal_store.persist_segment_metadata(segment).await.unwrap();
            }

            // Create consumer groups and commit offsets
            for i in 0..5 {
                let group_metadata = ConsumerGroupMetadata {
                    group_id: format!("group-{}", i),
                    state: "Stable".to_string(),
                    protocol: "range".to_string(),
                    protocol_type: "consumer".to_string(),
                    generation_id: 1,
                    leader_id: Some(format!("consumer-{}-0", i)),
                    created_at: chrono::Utc::now(),
                    updated_at: chrono::Utc::now(),
                };
                setup.wal_store.create_consumer_group(group_metadata).await.unwrap();

                // Commit some offsets
                for j in 0..10 {
                    let offset = ConsumerOffset {
                        group_id: format!("group-{}", i),
                        topic: format!("topic-{}", j),
                        partition: 0,
                        offset: (j * 1000) as i64,
                        metadata: None,
                        commit_timestamp: chrono::Utc::now(),
                    };
                    setup.wal_store.commit_offset(offset).await.unwrap();
                }
            }

            // Do some reads
            black_box(setup.wal_store.list_topics().await.unwrap());
            black_box(setup.wal_store.list_segments("topic-0", None).await.unwrap());
            black_box(setup.wal_store.get_consumer_group("group-0").await.unwrap());
        })
    });

    group.finish();
}

fn bench_recovery_performance(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();

    let mut group = c.benchmark_group("recovery_performance");

    for event_count in [100, 1000, 5000].iter() {
        group.bench_with_input(
            BenchmarkId::new("wal_recovery", event_count),
            event_count,
            |b, &event_count| {
                b.to_async(&rt).iter(|| async {
                    let temp_dir = TempDir::new().unwrap();
                    let wal_config = WalConfig {
                        enabled: true,
                        data_dir: temp_dir.path().to_path_buf(),
                        flush_interval_ms: 1, // Force immediate flush
                        flush_threshold: 1,   // Force flush every record
                        ..Default::default()
                    };

                    // Create initial store and populate with data
                    {
                        let adapter = Arc::new(WalMetadataAdapter::new(wal_config.clone()).await.unwrap());
                        let store = Arc::new(ChronikMetaLogStore::new(adapter).await.unwrap());

                        // Create events
                        for i in 0..event_count {
                            if i % 3 == 0 {
                                let topic_name = format!("topic-{}", i / 3);
                                let config = TopicConfig::default();
                                store.create_topic(&topic_name, config).await.unwrap();
                            } else if i % 3 == 1 {
                                let group_metadata = ConsumerGroupMetadata {
                                    group_id: format!("group-{}", i / 3),
                                    state: "Stable".to_string(),
                                    protocol: "range".to_string(),
                                    protocol_type: "consumer".to_string(),
                                    generation_id: 1,
                                    leader_id: Some(format!("consumer-{}", i)),
                                    created_at: chrono::Utc::now(),
                                    updated_at: chrono::Utc::now(),
                                };
                                store.create_consumer_group(group_metadata).await.unwrap();
                            } else {
                                let offset = ConsumerOffset {
                                    group_id: format!("group-{}", (i / 3).max(0)),
                                    topic: format!("topic-{}", (i / 3).max(0)),
                                    partition: 0,
                                    offset: i as i64 * 1000,
                                    metadata: None,
                                    commit_timestamp: chrono::Utc::now(),
                                };
                                store.commit_offset(offset).await.unwrap();
                            }
                        }
                    }

                    // Benchmark recovery
                    let start = std::time::Instant::now();
                    let adapter = Arc::new(WalMetadataAdapter::new(wal_config).await.unwrap());
                    let _store = Arc::new(ChronikMetaLogStore::new(adapter).await.unwrap());
                    let recovery_time = start.elapsed();

                    black_box(recovery_time);
                })
            },
        );
    }

    group.finish();
}

criterion_group!(
    benches,
    bench_topic_operations,
    bench_segment_operations,
    bench_consumer_operations,
    bench_mixed_workload,
    bench_recovery_performance
);

criterion_main!(benches);