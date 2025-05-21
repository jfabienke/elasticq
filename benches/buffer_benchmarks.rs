use criterion::{black_box, criterion_group, criterion_main, BatchSize, Criterion};
use elasticq::{BufferError, Config, DynamicCircularBuffer};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread; // For tracking items

fn bench_push_pop_single(c: &mut Criterion) {
    let buffer = DynamicCircularBuffer::<i32>::new(Config::default()).unwrap();
    c.bench_function("single_push_pop", |b| {
        b.iter(|| {
            buffer.push(black_box(42)).unwrap();
            black_box(buffer.pop()).unwrap();
        })
    });
}

fn bench_push_pop_batch(c: &mut Criterion) {
    let mut group = c.benchmark_group("batch_operations_amortized_setup"); // Renamed for clarity

    for batch_size in [10, 100, 1000].iter() {
        // Config for this test: initial capacity is set to avoid resizes *during the batch operations themselves*.
        // The goal is to measure the batch call overhead.
        let config = Config::default()
            .with_initial_capacity(*batch_size * 2) // Enough for one batch push + some headroom
            .with_min_capacity(*batch_size * 2); // Prevent shrinking during this specific test

        let buffer = DynamicCircularBuffer::<i32>::new(config.clone()).unwrap();
        let items_to_push: Vec<i32> = (0..*batch_size as i32).collect();

        group.bench_function(format!("push_pop_batch_{}", batch_size), |b| {
            b.iter_batched(
                || items_to_push.clone(), // Setup: clone the data for each batch push
                |data_to_push| {
                    // Routine: the actual operations to benchmark
                    buffer.push_batch(black_box(data_to_push)).unwrap();
                    black_box(buffer.pop_batch(*batch_size)).unwrap();
                },
                BatchSize::SmallInput,
            );
        });
    }
    group.finish();
}

fn bench_steady_state_batch(c: &mut Criterion) {
    let mut group = c.benchmark_group("batch_operations_steady_state");

    for batch_size in [10, 100, 1000].iter() {
        // Config for steady state: large enough initial capacity that no resizing/shrinking
        // should occur during the benchmark loop itself, after an initial fill.
        let steady_state_capacity = batch_size * 10; // e.g., 10x the batch size
        let config = Config::default()
            .with_initial_capacity(steady_state_capacity)
            .with_min_capacity(steady_state_capacity) // Prevent shrinking
            .with_max_capacity(steady_state_capacity * 2); // Allow some room if needed, but shouldn't be hit

        let buffer = DynamicCircularBuffer::<i32>::new(config.clone()).unwrap();
        let items_for_batch: Vec<i32> = (0..*batch_size as i32).collect();

        // Pre-fill the buffer to a certain level (e.g., half of one batch size)
        // This is to ensure pop doesn't immediately return empty in the loop.
        let initial_fill_count = *batch_size / 2;
        if initial_fill_count > 0 {
            buffer
                .push_batch((0..initial_fill_count as i32).collect())
                .unwrap();
        }

        group.bench_function(format!("steady_push_pop_batch_{}", batch_size), |b| {
            b.iter_batched(
                || items_for_batch.clone(),
                |data_to_push| {
                    buffer.push_batch(black_box(data_to_push.clone())).unwrap(); // Push a batch
                    black_box(buffer.pop_batch(*batch_size)).unwrap(); // Pop a batch
                },
                BatchSize::SmallInput,
            );
        });
    }
    group.finish();
}

fn bench_push_causing_resize(c: &mut Criterion) {
    let initial_cap = 16;
    let items_to_push_count = 128; // Push enough to trigger a few resizes

    let config = Config::default()
        .with_initial_capacity(initial_cap)
        .with_min_capacity(initial_cap)
        .with_max_capacity(1024 * 1024)
        .with_growth_factor(2.0);

    c.bench_function("push_causing_resize", |b| {
        b.iter_batched(
            || DynamicCircularBuffer::<i32>::new(config.clone()).unwrap(),
            |buffer| {
                for i in 0..items_to_push_count {
                    buffer.push(black_box(i)).unwrap();
                }
            },
            BatchSize::SmallInput,
        );
    });
}

fn bench_pop_causing_shrink(c: &mut Criterion) {
    let initial_fill_items = 128;
    // Config that allows shrinking
    // Initial capacity will be set by the number of items pushed.
    // We want it to grow to initial_fill_items, then shrink.
    let config = Config::default()
        .with_initial_capacity(initial_fill_items) // Start at the peak
        .with_min_capacity(16) // Allow shrinking down to 16
        .with_max_capacity(initial_fill_items * 2) // Ample max capacity
        .with_shrink_threshold(0.25) // Standard shrink threshold
        .with_growth_factor(2.0); // Standard growth factor

    c.bench_function("pop_causing_shrink", |b| {
        b.iter_batched(
            || {
                // Setup for each iteration
                let buffer = DynamicCircularBuffer::<i32>::new(config.clone()).unwrap();
                // Fill the buffer to its initial_capacity (which is initial_fill_items)
                let items_to_fill: Vec<i32> = (0..initial_fill_items as i32).collect();
                buffer.push_batch(items_to_fill).unwrap();
                // Sanity check: capacity should be initial_fill_items
                assert_eq!(buffer.capacity(), initial_fill_items);
                assert_eq!(buffer.len(), initial_fill_items);
                buffer
            },
            |buffer| {
                // Routine: pop all items, causing shrinks
                for _i in 0..initial_fill_items {
                    black_box(buffer.pop()).unwrap();
                }
            },
            BatchSize::SmallInput, // Buffer creation and filling is the setup
        );
    });
}

// --- New Concurrent Benchmark ---
fn bench_concurrent_mpsc(c: &mut Criterion) {
    let mut group = c.benchmark_group("concurrent_mpsc");

    // Test with different numbers of producers and consumers
    // [(producers, consumers, items_per_producer_thread)]
    let scenarios = [
        (1, 1, 10000), // Baseline: 1P, 1C (similar to single_push_pop but with thread overhead)
        (2, 2, 5000),  // 2P, 2C
        (4, 4, 2500),  // 4P, 4C
        (8, 8, 1250),  // 8P, 8C (if you have enough cores)
        (4, 1, 2500),  // Many producers, one consumer
        (1, 4, 10000), // One producer, many consumers
    ];

    for (num_producers, num_consumers, items_per_producer) in scenarios.iter().cloned() {
        let total_items_to_produce = num_producers * items_per_producer;

        // Configure buffer - initial capacity can be important here
        // Let's give it a reasonable starting size, e.g., total_items / 4, but at least 1024
        let initial_buffer_capacity = (total_items_to_produce / 4).max(1024);
        let config = Config::default()
            .with_initial_capacity(initial_buffer_capacity)
            .with_min_capacity(initial_buffer_capacity / 2) // Allow some shrinking
            .with_max_capacity(initial_buffer_capacity * 4); // Allow growth

        group.bench_function(
            format!(
                "{}p_{}c_{}_items",
                num_producers, num_consumers, total_items_to_produce
            ),
            |b| {
                b.iter_custom(|iters| {
                    // Use iter_custom for manual timing loop
                    let mut total_duration = std::time::Duration::new(0, 0);

                    for _i in 0..iters {
                        // Criterion's outer loop for multiple samples
                        let buffer =
                            Arc::new(DynamicCircularBuffer::<usize>::new(config.clone()).unwrap());
                        let produced_count = Arc::new(AtomicUsize::new(0));
                        let consumed_count = Arc::new(AtomicUsize::new(0));
                        let mut handles = vec![];

                        let start_time = std::time::Instant::now();

                        // Spawn Producers
                        for p_idx in 0..num_producers {
                            let buffer_clone = Arc::clone(&buffer);
                            let produced_count_clone = Arc::clone(&produced_count);
                            handles.push(thread::spawn(move || {
                                for item_idx in 0..items_per_producer {
                                    let item_val = p_idx * items_per_producer + item_idx; // Unique item
                                    loop {
                                        // Retry loop for push
                                        match buffer_clone.push(black_box(item_val)) {
                                            Ok(_) => {
                                                produced_count_clone
                                                    .fetch_add(1, Ordering::Relaxed);
                                                break;
                                            }
                                            Err(BufferError::Full) => {
                                                // Simple retry if full
                                                thread::yield_now(); // Give consumers a chance
                                                continue;
                                            }
                                            Err(e) => panic!("Producer error: {:?}", e),
                                        }
                                    }
                                }
                            }));
                        }

                        // Spawn Consumers
                        // Consumers will try to consume roughly the total number of items.
                        // Each consumer thread aims for total_items_to_produce / num_consumers.
                        let items_per_consumer_target =
                            (total_items_to_produce + num_consumers - 1) / num_consumers; // Ceiling division

                        for _c_idx in 0..num_consumers {
                            let buffer_clone = Arc::clone(&buffer);
                            let consumed_count_clone = Arc::clone(&consumed_count);
                            let produced_count_clone_for_consumer = Arc::clone(&produced_count);

                            handles.push(thread::spawn(move || {
                                let mut local_consumed = 0;
                                while local_consumed < items_per_consumer_target {
                                    // Only try to pop if more items are expected to be produced or are already produced
                                    if consumed_count_clone.load(Ordering::Relaxed)
                                        >= total_items_to_produce
                                    {
                                        break; // All items consumed by all consumers
                                    }
                                    // A more robust check: have all producers finished AND buffer is empty?
                                    // For this benchmark, we let consumers spin a bit if buffer is empty but more items are coming.

                                    match buffer_clone.pop() {
                                        Ok(_item) => {
                                            consumed_count_clone.fetch_add(1, Ordering::Relaxed);
                                            local_consumed += 1;
                                        }
                                        Err(BufferError::Empty) => {
                                            // If all expected items already produced and buffer is empty, then done.
                                            if produced_count_clone_for_consumer
                                                .load(Ordering::Relaxed)
                                                >= total_items_to_produce
                                                && buffer_clone.is_empty()
                                            {
                                                break;
                                            }
                                            thread::yield_now(); // Buffer is empty, yield
                                            continue;
                                        }
                                        Err(e) => panic!("Consumer error: {:?}", e),
                                    }
                                }
                            }));
                        }

                        for handle in handles {
                            handle.join().expect("Thread panicked");
                        }
                        total_duration += start_time.elapsed();

                        // Sanity check (optional, can slow down benchmark if run every iter)
                        // assert_eq!(produced_count.load(Ordering::Relaxed), total_items_to_produce);
                        // assert_eq!(consumed_count.load(Ordering::Relaxed), total_items_to_produce);
                    }
                    total_duration // Criterion measures this total time over `iters`
                })
            },
        );
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_push_pop_single,
    bench_push_pop_batch,     // Measures batch call overhead primarily
    bench_steady_state_batch, // Measures batch ops in a stable capacity scenario
    bench_push_causing_resize,
    bench_pop_causing_shrink,
    bench_concurrent_mpsc
);
criterion_main!(benches);
