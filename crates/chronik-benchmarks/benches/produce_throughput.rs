use criterion::{black_box, criterion_group, criterion_main, Criterion, BenchmarkId, Throughput};
use chronik_benchmarks::{TestDataGenerator, LatencyTracker};
use std::time::Duration;

fn bench_message_generation(c: &mut Criterion) {
    let mut group = c.benchmark_group("message_generation");
    
    for size in [128, 512, 1024, 4096, 16384].iter() {
        group.throughput(Throughput::Bytes(*size as u64));
        group.bench_with_input(BenchmarkId::new("generate", size), size, |b, &size| {
            let mut generator = TestDataGenerator::new(12345);
            b.iter(|| {
                black_box(generator.generate_message(size));
            });
        });
    }
    
    group.finish();
}

fn bench_batch_generation(c: &mut Criterion) {
    let mut group = c.benchmark_group("batch_generation");
    
    for batch_size in [10, 50, 100, 500].iter() {
        group.bench_with_input(BenchmarkId::new("batch", batch_size), batch_size, |b, &batch_size| {
            let mut generator = TestDataGenerator::new(12345);
            b.iter(|| {
                black_box(generator.generate_batch(batch_size, 1024));
            });
        });
    }
    
    group.finish();
}

fn bench_latency_tracking(c: &mut Criterion) {
    c.bench_function("latency_tracking", |b| {
        b.iter(|| {
            let mut tracker = LatencyTracker::new();
            for _ in 0..100 {
                tracker.start();
                // Simulate work
                std::thread::sleep(Duration::from_micros(10));
                tracker.end();
            }
            black_box(tracker.measurements());
        });
    });
}

criterion_group!(benches, bench_message_generation, bench_batch_generation, bench_latency_tracking);
criterion_main!(benches);