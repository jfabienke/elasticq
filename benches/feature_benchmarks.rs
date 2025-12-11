//! Benchmarks for v0.3.0 features: priority queues, persistence, and metrics
use criterion::{black_box, criterion_group, criterion_main, BatchSize, Criterion, Throughput};

// ============================================================================
// Priority Queue Benchmarks
// ============================================================================
#[cfg(feature = "priority")]
mod priority_benches {
    use super::*;
    use elasticq::priority::{PriorityCircularBuffer, PriorityConfig};
    use elasticq::Config;

    pub fn bench_priority_push_pop(c: &mut Criterion) {
        let mut group = c.benchmark_group("priority_queue");

        // Single push/pop at different priorities
        let config = PriorityConfig::default()
            .with_priority_levels(3)
            .with_fair_queuing(false);
        let buffer = PriorityCircularBuffer::<i32>::new(config).unwrap();

        group.bench_function("push_pop_high_priority", |b| {
            b.iter(|| {
                buffer.push_with_priority(black_box(42), 2).unwrap();
                black_box(buffer.pop()).unwrap();
            })
        });

        group.bench_function("push_pop_low_priority", |b| {
            b.iter(|| {
                buffer.push_with_priority(black_box(42), 0).unwrap();
                black_box(buffer.pop()).unwrap();
            })
        });

        group.finish();
    }

    pub fn bench_priority_fair_queuing(c: &mut Criterion) {
        let mut group = c.benchmark_group("priority_fair_queuing");

        // Compare fair vs non-fair queuing
        for fair in [false, true] {
            let config = PriorityConfig::default()
                .with_priority_levels(3)
                .with_fair_queuing(fair)
                .with_max_consecutive(5);
            let buffer = PriorityCircularBuffer::<i32>::new(config).unwrap();

            let label = if fair { "fair" } else { "strict" };
            group.bench_function(format!("mixed_priorities_{}", label), |b| {
                b.iter(|| {
                    // Push items at all priorities
                    buffer.push_with_priority(black_box(1), 0).unwrap();
                    buffer.push_with_priority(black_box(2), 1).unwrap();
                    buffer.push_with_priority(black_box(3), 2).unwrap();
                    // Pop all
                    black_box(buffer.pop()).unwrap();
                    black_box(buffer.pop()).unwrap();
                    black_box(buffer.pop()).unwrap();
                })
            });
        }

        group.finish();
    }

    pub fn bench_priority_batch(c: &mut Criterion) {
        let mut group = c.benchmark_group("priority_batch");

        for batch_size in [10, 100, 1000] {
            let base_config = Config::default()
                .with_initial_capacity(batch_size * 2)
                .with_min_capacity(batch_size * 2);
            let config = PriorityConfig::default()
                .with_priority_levels(3)
                .with_fair_queuing(false)
                .with_base_config(base_config);
            let buffer = PriorityCircularBuffer::<i32>::new(config).unwrap();
            let items: Vec<i32> = (0..batch_size as i32).collect();

            group.throughput(Throughput::Elements(batch_size as u64));
            group.bench_function(format!("batch_{}", batch_size), |b| {
                b.iter_batched(
                    || items.clone(),
                    |data| {
                        buffer.push_batch_with_priority(black_box(data), 1).unwrap();
                        for _ in 0..batch_size {
                            black_box(buffer.pop()).unwrap();
                        }
                    },
                    BatchSize::SmallInput,
                )
            });
        }

        group.finish();
    }

    pub fn bench_priority_throughput(c: &mut Criterion) {
        let mut group = c.benchmark_group("priority_throughput");
        group.throughput(Throughput::Elements(1000));

        let base_config = Config::default()
            .with_initial_capacity(2000)
            .with_min_capacity(2000);
        let config = PriorityConfig::default()
            .with_priority_levels(3)
            .with_fair_queuing(false)
            .with_base_config(base_config);
        let buffer = PriorityCircularBuffer::<i32>::new(config).unwrap();

        group.bench_function("1000_ops_mixed", |b| {
            b.iter(|| {
                for i in 0..1000i32 {
                    let priority = (i % 3) as u8;
                    buffer.push_with_priority(black_box(i), priority).unwrap();
                }
                for _ in 0..1000 {
                    black_box(buffer.pop()).unwrap();
                }
            })
        });

        group.finish();
    }
}

// ============================================================================
// Persistence Benchmarks
// ============================================================================
#[cfg(feature = "persistent")]
mod persistent_benches {
    use super::*;
    use elasticq::persistent::{PersistentCircularBuffer, PersistentConfig, SyncMode};
    use elasticq::Config;
    use std::time::Duration;
    use tempfile::tempdir;

    pub fn bench_persistent_push_pop(c: &mut Criterion) {
        let mut group = c.benchmark_group("persistent_queue");

        // NoSync mode (fastest)
        let dir = tempdir().unwrap();
        let config = PersistentConfig::new(dir.path().join("bench_nosync.dat"))
            .with_sync_mode(SyncMode::NoSync);
        let buffer = PersistentCircularBuffer::<i32>::new(config).unwrap();

        group.bench_function("push_pop_nosync", |b| {
            b.iter(|| {
                buffer.push(black_box(42)).unwrap();
                black_box(buffer.pop()).unwrap();
            })
        });

        // Periodic sync mode
        let dir2 = tempdir().unwrap();
        let config2 = PersistentConfig::new(dir2.path().join("bench_periodic.dat"))
            .with_sync_mode(SyncMode::Periodic(Duration::from_millis(100)));
        let buffer2 = PersistentCircularBuffer::<i32>::new(config2).unwrap();

        group.bench_function("push_pop_periodic_sync", |b| {
            b.iter(|| {
                buffer2.push(black_box(42)).unwrap();
                black_box(buffer2.pop()).unwrap();
            })
        });

        group.finish();
    }

    pub fn bench_persistent_batch(c: &mut Criterion) {
        let mut group = c.benchmark_group("persistent_batch");

        for batch_size in [10, 100] {
            let dir = tempdir().unwrap();
            let base_config = Config::default()
                .with_initial_capacity(batch_size * 2)
                .with_min_capacity(batch_size * 2);
            let config = PersistentConfig::new(dir.path().join(format!("bench_batch_{}.dat", batch_size)))
                .with_sync_mode(SyncMode::NoSync)
                .with_base_config(base_config);
            let buffer = PersistentCircularBuffer::<i32>::new(config).unwrap();
            let items: Vec<i32> = (0..batch_size as i32).collect();

            group.throughput(Throughput::Elements(batch_size as u64));
            group.bench_function(format!("batch_{}", batch_size), |b| {
                b.iter_batched(
                    || items.clone(),
                    |data| {
                        buffer.push_batch(black_box(data)).unwrap();
                        black_box(buffer.pop_batch(batch_size)).unwrap();
                    },
                    BatchSize::SmallInput,
                )
            });
        }

        group.finish();
    }

    pub fn bench_persistent_sync_modes(c: &mut Criterion) {
        let mut group = c.benchmark_group("persistent_sync_modes");
        group.sample_size(50); // Fewer samples due to I/O

        // Compare different sync modes
        let dir = tempdir().unwrap();

        // EveryWrite mode (slowest but safest)
        let config = PersistentConfig::new(dir.path().join("bench_everywrite.dat"))
            .with_sync_mode(SyncMode::EveryWrite);
        let buffer = PersistentCircularBuffer::<i32>::new(config).unwrap();

        group.bench_function("push_pop_every_write", |b| {
            b.iter(|| {
                buffer.push(black_box(42)).unwrap();
                black_box(buffer.pop()).unwrap();
            })
        });

        group.finish();
    }
}

// ============================================================================
// Metrics Benchmarks
// ============================================================================
#[cfg(feature = "metrics")]
mod metrics_benches {
    use super::*;
    use elasticq::metrics::MetricsRecorder;
    use elasticq::{Config, DynamicCircularBuffer};
    use std::sync::Arc;

    pub fn bench_metrics_overhead(c: &mut Criterion) {
        let mut group = c.benchmark_group("metrics_overhead");

        // Baseline: no metrics
        let buffer = DynamicCircularBuffer::<i32>::new(Config::default()).unwrap();
        group.bench_function("baseline_no_metrics", |b| {
            b.iter(|| {
                buffer.push(black_box(42)).unwrap();
                black_box(buffer.pop()).unwrap();
            })
        });

        // With metrics enabled
        let recorder = MetricsRecorder::new("bench_queue");
        let buffer2 = Arc::new(DynamicCircularBuffer::<i32>::new(Config::default()).unwrap());
        let instrumented = recorder.wrap_arc(buffer2);

        group.bench_function("with_metrics", |b| {
            b.iter(|| {
                instrumented.push(black_box(42)).unwrap();
                black_box(instrumented.pop()).unwrap();
            })
        });

        group.finish();
    }

    pub fn bench_metrics_batch(c: &mut Criterion) {
        let mut group = c.benchmark_group("metrics_batch");

        for batch_size in [10, 100, 1000] {
            let config = Config::default()
                .with_initial_capacity(batch_size * 2)
                .with_min_capacity(batch_size * 2);

            // Baseline
            let buffer = DynamicCircularBuffer::<i32>::new(config.clone()).unwrap();
            let items: Vec<i32> = (0..batch_size as i32).collect();

            group.throughput(Throughput::Elements(batch_size as u64));
            group.bench_function(format!("baseline_{}", batch_size), |b| {
                b.iter_batched(
                    || items.clone(),
                    |data| {
                        buffer.push_batch(black_box(data)).unwrap();
                        black_box(buffer.pop_batch(batch_size)).unwrap();
                    },
                    BatchSize::SmallInput,
                )
            });

            // With metrics
            let recorder = MetricsRecorder::new("bench");
            let buffer2 = Arc::new(DynamicCircularBuffer::<i32>::new(config).unwrap());
            let instrumented = recorder.wrap_arc(buffer2);

            group.bench_function(format!("instrumented_{}", batch_size), |b| {
                b.iter_batched(
                    || items.clone(),
                    |data| {
                        instrumented.push_batch(black_box(data)).unwrap();
                        black_box(instrumented.pop_batch(batch_size)).unwrap();
                    },
                    BatchSize::SmallInput,
                )
            });
        }

        group.finish();
    }
}

// ============================================================================
// Criterion setup
// ============================================================================

#[cfg(feature = "priority")]
criterion_group!(
    priority_benches_group,
    priority_benches::bench_priority_push_pop,
    priority_benches::bench_priority_fair_queuing,
    priority_benches::bench_priority_batch,
    priority_benches::bench_priority_throughput,
);

#[cfg(feature = "persistent")]
criterion_group!(
    persistent_benches_group,
    persistent_benches::bench_persistent_push_pop,
    persistent_benches::bench_persistent_batch,
    persistent_benches::bench_persistent_sync_modes,
);

#[cfg(feature = "metrics")]
criterion_group!(
    metrics_benches_group,
    metrics_benches::bench_metrics_overhead,
    metrics_benches::bench_metrics_batch,
);

// Conditional main based on features
#[cfg(all(feature = "priority", feature = "persistent", feature = "metrics"))]
criterion_main!(priority_benches_group, persistent_benches_group, metrics_benches_group);

#[cfg(all(feature = "priority", feature = "persistent", not(feature = "metrics")))]
criterion_main!(priority_benches_group, persistent_benches_group);

#[cfg(all(feature = "priority", not(feature = "persistent"), feature = "metrics"))]
criterion_main!(priority_benches_group, metrics_benches_group);

#[cfg(all(not(feature = "priority"), feature = "persistent", feature = "metrics"))]
criterion_main!(persistent_benches_group, metrics_benches_group);

#[cfg(all(feature = "priority", not(feature = "persistent"), not(feature = "metrics")))]
criterion_main!(priority_benches_group);

#[cfg(all(not(feature = "priority"), feature = "persistent", not(feature = "metrics")))]
criterion_main!(persistent_benches_group);

#[cfg(all(not(feature = "priority"), not(feature = "persistent"), feature = "metrics"))]
criterion_main!(metrics_benches_group);

// Fallback for when no features are enabled
#[cfg(all(not(feature = "priority"), not(feature = "persistent"), not(feature = "metrics")))]
fn main() {
    println!("No benchmark features enabled. Run with --features priority,persistent,metrics");
}
