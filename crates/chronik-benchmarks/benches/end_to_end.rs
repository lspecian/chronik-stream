use criterion::{black_box, criterion_group, criterion_main, Criterion};
use chronik_benchmarks::{TestDataGenerator, LatencyTracker, RateLimiter};
use tokio::runtime::Runtime;

fn bench_rate_limiter(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    
    c.bench_function("rate_limiter_1000_ops_sec", |b| {
        b.iter(|| {
            rt.block_on(async {
                let mut limiter = RateLimiter::new(1000.0);
                for _ in 0..10 {
                    limiter.wait().await;
                }
            });
        });
    });
}

fn bench_progress_tracking(c: &mut Criterion) {
    c.bench_function("progress_tracking", |b| {
        b.iter(|| {
            let mut reporter = chronik_benchmarks::ProgressReporter::new(1000);
            for i in 0..100 {
                reporter.update(1);
            }
            black_box(reporter);
        });
    });
}

fn bench_e2e_simulation(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    
    c.bench_function("e2e_message_flow", |b| {
        b.iter(|| {
            rt.block_on(async {
                let mut generator = TestDataGenerator::new(12345);
                let mut tracker = LatencyTracker::new();
                
                // Simulate produce -> index -> search flow
                for _ in 0..10 {
                    let _message = generator.generate_message(1024);
                    
                    tracker.measure(async {
                        // Simulate processing pipeline
                        tokio::time::sleep(std::time::Duration::from_micros(100)).await;
                    }).await;
                }
                
                black_box(tracker.measurements());
            });
        });
    });
}

criterion_group!(benches, bench_rate_limiter, bench_progress_tracking, bench_e2e_simulation);
criterion_main!(benches);