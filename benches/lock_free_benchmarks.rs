use criterion::{black_box, criterion_group, criterion_main, Criterion, BenchmarkId, Throughput};
use elasticq::{Config, DynamicCircularBuffer, LockFreeMPSCQueue};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

fn benchmark_enqueue_dequeue(c: &mut Criterion) {
    let mut group = c.benchmark_group("enqueue_dequeue");
    
    // Test different message counts
    for &msg_count in &[1000, 10000, 100000] {
        group.throughput(Throughput::Elements(msg_count));
        
        // Lock-based implementation
        group.bench_with_input(
            BenchmarkId::new("lock_based", msg_count),
            &msg_count,
            |b, &msg_count| {
                let config = Config::default()
                    .with_initial_capacity(1024)
                    .with_min_capacity(512)
                    .with_max_capacity(1048576);
                let buffer = DynamicCircularBuffer::new(config).unwrap();
                
                b.iter(|| {
                    for i in 0..msg_count {
                        buffer.push(black_box(i)).unwrap();
                    }
                    for _ in 0..msg_count {
                        black_box(buffer.pop().unwrap());
                    }
                });
            },
        );
        
        // Lock-free implementation
        group.bench_with_input(
            BenchmarkId::new("lock_free", msg_count),
            &msg_count,
            |b, &msg_count| {
                let config = Config::default()
                    .with_initial_capacity(1024)
                    .with_min_capacity(512)
                    .with_max_capacity(1048576);
                let queue = LockFreeMPSCQueue::new(config).unwrap();
                
                b.iter(|| {
                    for i in 0..msg_count {
                        while queue.try_enqueue(black_box(i)).is_err() {
                            // Retry on failure (e.g., during resize)
                            thread::yield_now();
                        }
                    }
                    for _ in 0..msg_count {
                        while let Ok(None) = queue.try_dequeue() {
                            thread::yield_now();
                        }
                    }
                });
            },
        );
    }
    
    group.finish();
}

fn benchmark_mpsc_contention(c: &mut Criterion) {
    let mut group = c.benchmark_group("mpsc_contention");
    
    // Test different producer counts
    for &producer_count in &[1, 2, 4, 8] {
        let msg_per_producer = 10000;
        let total_messages = producer_count * msg_per_producer;
        group.throughput(Throughput::Elements(total_messages));
        
        // Lock-based implementation
        group.bench_with_input(
            BenchmarkId::new("lock_based", producer_count),
            &producer_count,
            |b, &producer_count| {
                b.iter(|| {
                    let config = Config::default()
                        .with_initial_capacity(1024)
                        .with_min_capacity(512)
                        .with_max_capacity(1048576);
                    let buffer = Arc::new(DynamicCircularBuffer::new(config).unwrap());
                    
                    // Spawn producers
                    let mut handles = vec![];
                    for producer_id in 0..producer_count {
                        let buffer_clone = Arc::clone(&buffer);
                        let handle = thread::spawn(move || {
                            for i in 0..msg_per_producer {
                                let msg = producer_id * 1000000 + i;
                                buffer_clone.push(black_box(msg)).unwrap();
                            }
                        });
                        handles.push(handle);
                    }
                    
                    // Consumer
                    let buffer_clone = Arc::clone(&buffer);
                    let consumer_handle = thread::spawn(move || {
                        let mut received = 0;
                        while received < total_messages {
                            if buffer_clone.pop().is_ok() {
                                received += 1;
                            } else {
                                thread::yield_now();
                            }
                        }
                    });
                    
                    // Wait for completion
                    for handle in handles {
                        handle.join().unwrap();
                    }
                    consumer_handle.join().unwrap();
                });
            },
        );
        
        // Lock-free implementation
        group.bench_with_input(
            BenchmarkId::new("lock_free", producer_count),
            &producer_count,
            |b, &producer_count| {
                b.iter(|| {
                    let config = Config::default()
                        .with_initial_capacity(1024)
                        .with_min_capacity(512)
                        .with_max_capacity(1048576);
                    let queue = Arc::new(LockFreeMPSCQueue::new(config).unwrap());
                    
                    // Spawn producers
                    let mut handles = vec![];
                    for producer_id in 0..producer_count {
                        let queue_clone = Arc::clone(&queue);
                        let handle = thread::spawn(move || {
                            for i in 0..msg_per_producer {
                                let msg = producer_id * 1000000 + i;
                                while queue_clone.try_enqueue(black_box(msg)).is_err() {
                                    thread::yield_now();
                                }
                            }
                        });
                        handles.push(handle);
                    }
                    
                    // Consumer
                    let queue_clone = Arc::clone(&queue);
                    let consumer_handle = thread::spawn(move || {
                        let mut received = 0;
                        while received < total_messages {
                            match queue_clone.try_dequeue() {
                                Ok(Some(_)) => received += 1,
                                Ok(None) => thread::yield_now(),
                                Err(_) => thread::yield_now(),
                            }
                        }
                    });
                    
                    // Wait for completion
                    for handle in handles {
                        handle.join().unwrap();
                    }
                    consumer_handle.join().unwrap();
                });
            },
        );
    }
    
    group.finish();
}

fn benchmark_memory_usage(c: &mut Criterion) {
    let mut group = c.benchmark_group("memory_efficiency");
    
    // Test memory efficiency with resize operations
    group.bench_function("lock_based_resize", |b| {
        let config = Config::default()
            .with_initial_capacity(64)
            .with_min_capacity(32)
            .with_max_capacity(65536)
            .with_growth_factor(2.0);
        let buffer = DynamicCircularBuffer::new(config).unwrap();
        
        b.iter(|| {
            // Fill beyond capacity to trigger multiple resizes
            for i in 0..10000 {
                buffer.push(black_box(i)).unwrap();
            }
            // Drain to trigger shrinking
            for _ in 0..9000 {
                black_box(buffer.pop().unwrap());
            }
            // Fill again
            for i in 10000..15000 {
                buffer.push(black_box(i)).unwrap();
            }
            // Final drain
            while buffer.pop().is_ok() {}
        });
    });
    
    group.bench_function("lock_free_resize", |b| {
        let config = Config::default()
            .with_initial_capacity(64)
            .with_min_capacity(32)
            .with_max_capacity(65536)
            .with_growth_factor(2.0);
        let queue = LockFreeMPSCQueue::new(config).unwrap();
        
        b.iter(|| {
            // Fill beyond capacity to trigger multiple resizes
            for i in 0..10000 {
                while queue.try_enqueue(black_box(i)).is_err() {
                    thread::yield_now();
                }
            }
            // Drain to trigger potential shrinking
            for _ in 0..9000 {
                while let Ok(None) = queue.try_dequeue() {
                    thread::yield_now();
                }
            }
            // Fill again
            for i in 10000..15000 {
                while queue.try_enqueue(black_box(i)).is_err() {
                    thread::yield_now();
                }
            }
            // Final drain
            loop {
                match queue.try_dequeue() {
                    Ok(Some(_)) => continue,
                    Ok(None) => break,
                    Err(_) => break,
                }
            }
        });
    });
    
    group.finish();
}

criterion_group!(
    benches,
    benchmark_enqueue_dequeue,
    benchmark_mpsc_contention,
    benchmark_memory_usage
);
criterion_main!(benches);