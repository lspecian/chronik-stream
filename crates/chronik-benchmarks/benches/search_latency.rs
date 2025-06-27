use criterion::{black_box, criterion_group, criterion_main, Criterion, BenchmarkId};
use chronik_benchmarks::TestDataGenerator;

fn bench_json_generation(c: &mut Criterion) {
    let mut group = c.benchmark_group("json_generation");
    
    let field_sets = vec![
        vec!["user_id", "event_type"],
        vec!["user_id", "session_id", "event_type", "amount"],
        vec!["user_id", "session_id", "event_type", "amount", "status"],
    ];
    
    for (i, fields) in field_sets.iter().enumerate() {
        group.bench_with_input(BenchmarkId::new("fields", i), fields, |b, fields| {
            let mut generator = TestDataGenerator::new(12345);
            b.iter(|| {
                black_box(generator.generate_json_message(fields));
            });
        });
    }
    
    group.finish();
}

fn bench_search_query_parsing(c: &mut Criterion) {
    c.bench_function("search_query_parsing", |b| {
        let query = serde_json::json!({
            "bool": {
                "must": [
                    {"match": {"content": "test"}},
                    {"range": {"timestamp": {"gte": "2023-01-01"}}}
                ],
                "filter": [
                    {"term": {"status": "active"}}
                ]
            }
        });
        
        b.iter(|| {
            black_box(serde_json::to_string(&query).unwrap());
        });
    });
}

criterion_group!(benches, bench_json_generation, bench_search_query_parsing);
criterion_main!(benches);