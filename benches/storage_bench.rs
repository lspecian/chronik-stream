//! Benchmarks for storage operations.

use chronik_storage::{
    segment::{SegmentWriter, Segment},
    object_store::{LocalObjectStore, ObjectStore},
};
use criterion::{black_box, criterion_group, criterion_main, Criterion, Throughput};
use tempfile::TempDir;
use uuid::Uuid;

fn benchmark_segment_write(c: &mut Criterion) {
    let mut group = c.benchmark_group("segment_write");
    
    for message_size in [100, 1024, 10240] {
        group.throughput(Throughput::Bytes(message_size as u64 * 1000));
        group.bench_with_input(
            format!("{}_byte_messages", message_size),
            &message_size,
            |b, &size| {
                let message = vec![b'x'; size];
                
                b.iter(|| {
                    let mut writer = SegmentWriter::new(
                        Uuid::new_v4(),
                        "bench-topic".to_string(),
                        0,
                        0,
                    );
                    
                    for i in 0..1000 {
                        writer.append(i, &message).unwrap();
                    }
                    
                    black_box(writer.seal());
                });
            },
        );
    }
    
    group.finish();
}

fn benchmark_segment_read(c: &mut Criterion) {
    let mut group = c.benchmark_group("segment_read");
    
    for message_count in [100, 1000, 10000] {
        // Create a segment with test data
        let mut writer = SegmentWriter::new(
            Uuid::new_v4(),
            "bench-topic".to_string(),
            0,
            0,
        );
        
        let message = b"benchmark message data";
        for i in 0..message_count {
            writer.append(i as i64, message).unwrap();
        }
        
        let segment = writer.seal();
        let serialized = segment.serialize().unwrap();
        
        group.throughput(Throughput::Elements(message_count as u64));
        group.bench_with_input(
            format!("{}_messages", message_count),
            &serialized,
            |b, data| {
                b.iter(|| {
                    let segment = Segment::deserialize(data).unwrap();
                    
                    // Iterate through all messages
                    let mut count = 0;
                    for offset in 0..segment.message_count() {
                        if let Some(_msg) = segment.read_message(offset as i64) {
                            count += 1;
                        }
                    }
                    
                    black_box(count);
                });
            },
        );
    }
    
    group.finish();
}

fn benchmark_object_store(c: &mut Criterion) {
    let temp_dir = TempDir::new().unwrap();
    let store = LocalObjectStore::new(temp_dir.path()).unwrap();
    
    let mut group = c.benchmark_group("object_store");
    
    for data_size in [1024, 10240, 102400] {
        let data = vec![b'x'; data_size];
        
        group.throughput(Throughput::Bytes(data_size as u64));
        group.bench_with_input(
            format!("{}_bytes", data_size),
            &data,
            |b, data| {
                let runtime = tokio::runtime::Runtime::new().unwrap();
                
                b.iter(|| {
                    runtime.block_on(async {
                        let key = format!("bench/{}", Uuid::new_v4());
                        store.put(&key, data.clone()).await.unwrap();
                        let retrieved = store.get(&key).await.unwrap();
                        black_box(retrieved);
                    });
                });
            },
        );
    }
    
    group.finish();
}

fn benchmark_segment_serialization(c: &mut Criterion) {
    let mut group = c.benchmark_group("segment_serialization");
    
    for message_count in [100, 1000, 10000] {
        // Create a segment
        let mut writer = SegmentWriter::new(
            Uuid::new_v4(),
            "bench-topic".to_string(),
            0,
            0,
        );
        
        for i in 0..message_count {
            let message = format!("Message {}", i);
            writer.append(i as i64, message.as_bytes()).unwrap();
        }
        
        let segment = writer.seal();
        
        group.bench_with_input(
            format!("{}_messages", message_count),
            &segment,
            |b, segment| {
                b.iter(|| {
                    let serialized = segment.serialize().unwrap();
                    black_box(serialized);
                });
            },
        );
    }
    
    group.finish();
}

criterion_group!(
    storage_benches,
    benchmark_segment_write,
    benchmark_segment_read,
    benchmark_object_store,
    benchmark_segment_serialization
);
criterion_main!(storage_benches);