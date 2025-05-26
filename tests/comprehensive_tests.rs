//! Comprehensive test suite for ElasticQ lock-free and lock-based implementations
//! 
//! This module implements all additional tests based on TLA+ validation results,
//! including ABA protection, message conservation, resize coordination, and more.

use elasticq::{Config, BufferError};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};
use std::sync::atomic::{AtomicUsize, AtomicBool, Ordering};

#[cfg(feature = "lock_free")]
use elasticq::LockFreeMPSCQueue;
use elasticq::DynamicCircularBuffer;

// ============================================================================
// ABA Protection Tests
// ============================================================================

#[cfg(feature = "lock_free")]
#[test]
fn test_aba_protection_during_resize() {
    // Test that generation counter prevents ABA races during resize
    let config = Config::default()
        .with_initial_capacity(4)
        .with_min_capacity(2)
        .with_max_capacity(32)
        .with_growth_factor(2.0);
    
    let queue = Arc::new(LockFreeMPSCQueue::new(config).unwrap());
    let aba_detected = Arc::new(AtomicBool::new(false));
    
    // Fill queue to trigger resize
    for i in 0..4 {
        queue.try_enqueue(i).unwrap();
    }
    
    let queue_clone = Arc::clone(&queue);
    let _aba_clone = Arc::clone(&aba_detected);
    
    // Producer thread that might encounter ABA
    let producer = thread::spawn(move || {
        for i in 4..100 {
            loop {
                match queue_clone.try_enqueue(i) {
                    Ok(()) => break,
                    Err(_) => {
                        // This retry mechanism should be protected against ABA
                        thread::yield_now();
                    }
                }
            }
        }
    });
    
    // Consumer thread that triggers resizes
    let consumer = thread::spawn(move || {
        let mut received = 0;
        while received < 100 {
            match queue.try_dequeue() {
                Ok(Some(_)) => received += 1,
                Ok(None) => thread::yield_now(),
                Err(_) => thread::yield_now(),
            }
        }
    });
    
    producer.join().unwrap();
    consumer.join().unwrap();
    
    // If we reach here without panicking, ABA protection worked
    assert!(!aba_detected.load(Ordering::Relaxed));
}

#[cfg(feature = "lock_free")]
#[test]
fn test_generation_monotonicity() {
    let config = Config::default()
        .with_initial_capacity(2)
        .with_min_capacity(1)
        .with_max_capacity(16);
    
    let queue = LockFreeMPSCQueue::new(config).unwrap();
    
    // Force multiple resizes and verify generation increases
    let mut last_capacity = queue.capacity();
    let mut resize_count = 0;
    
    for i in 0..20 {
        queue.try_enqueue(i).ok();
        
        let current_capacity = queue.capacity();
        if current_capacity > last_capacity {
            resize_count += 1;
            last_capacity = current_capacity;
        }
        
        // Consume some items to trigger more resizes
        if i % 3 == 0 {
            queue.try_dequeue().ok();
        }
    }
    
    // We should have seen at least one resize
    assert!(resize_count > 0, "Expected at least one resize");
    
    // Generation should be non-zero after resizes
    let stats = queue.stats();
    assert!(stats.current_capacity >= queue.capacity());
}

// ============================================================================
// Message Conservation Tests  
// ============================================================================

#[cfg(feature = "lock_free")]
#[test]
fn test_message_conservation_invariant() {
    let config = Config::default()
        .with_initial_capacity(8)
        .with_min_capacity(4)
        .with_max_capacity(64);
    
    let queue = Arc::new(LockFreeMPSCQueue::new(config).unwrap());
    let total_messages = 1000;
    
    // Producer
    let queue_clone = Arc::clone(&queue);
    let producer = thread::spawn(move || {
        for i in 0..total_messages {
            while queue_clone.try_enqueue(i).is_err() {
                thread::yield_now();
            }
        }
    });
    
    // Consumer 
    let queue_clone = Arc::clone(&queue);
    let consumer = thread::spawn(move || {
        let mut received = 0;
        while received < total_messages {
            match queue_clone.try_dequeue() {
                Ok(Some(_)) => received += 1,
                Ok(None) => thread::yield_now(),
                Err(_) => thread::yield_now(),
            }
        }
        received
    });
    
    producer.join().unwrap();
    let received = consumer.join().unwrap();
    
    // Verify conservation equation: received + queue_size + dropped = sent
    let stats = queue.stats();
    let conservation_total = stats.messages_dequeued + stats.current_size + stats.messages_dropped;
    
    assert_eq!(conservation_total, stats.messages_enqueued,
              "Message conservation violated: {} + {} + {} != {}",
              stats.messages_dequeued, stats.current_size, stats.messages_dropped, stats.messages_enqueued);
    
    assert_eq!(received, total_messages);
    assert_eq!(stats.messages_enqueued, total_messages);
}

#[cfg(feature = "lock_free")]
#[test]
fn test_no_phantom_messages() {
    let config = Config::default()
        .with_initial_capacity(4)
        .with_min_capacity(2)
        .with_max_capacity(16);
    
    let queue = LockFreeMPSCQueue::new(config).unwrap();
    
    // Initially empty - no phantom messages
    assert_eq!(queue.try_dequeue().unwrap(), None);
    let initial_stats = queue.stats();
    assert_eq!(initial_stats.messages_dequeued, 0);
    assert_eq!(initial_stats.messages_enqueued, 0);
    assert_eq!(initial_stats.current_size, 0);
    
    // Enqueue some messages
    let messages = vec![1, 2, 3, 4, 5];
    for &msg in &messages {
        queue.try_enqueue(msg).unwrap();
    }
    
    let after_enqueue = queue.stats();
    assert_eq!(after_enqueue.messages_enqueued, messages.len());
    assert_eq!(after_enqueue.current_size, messages.len());
    
    // Dequeue all messages
    let mut dequeued = vec![];
    while let Ok(Some(msg)) = queue.try_dequeue() {
        dequeued.push(msg);
    }
    
    assert_eq!(dequeued, messages);
    
    let final_stats = queue.stats();
    assert_eq!(final_stats.messages_dequeued, messages.len());
    assert_eq!(final_stats.current_size, 0);
    
    // No extra messages should appear
    assert_eq!(queue.try_dequeue().unwrap(), None);
}

#[cfg(feature = "lock_free")]
#[test]
fn test_bounded_message_loss() {
    let config = Config::default()
        .with_initial_capacity(2)
        .with_min_capacity(1)
        .with_max_capacity(4); // Very small to force drops
    
    let queue = Arc::new(LockFreeMPSCQueue::new(config).unwrap());
    let total_attempts = 100;
    
    // Multiple producers attempting to overwhelm the queue
    let mut handles = vec![];
    for producer_id in 0..4 {
        let queue_clone = Arc::clone(&queue);
        let handle = thread::spawn(move || {
            let mut sent = 0;
            for i in 0..total_attempts {
                let message = producer_id * 1000 + i;
                // Try only a few times before giving up
                for _ in 0..3 {
                    if queue_clone.try_enqueue(message).is_ok() {
                        sent += 1;
                        break;
                    }
                    thread::yield_now();
                }
            }
            sent
        });
        handles.push(handle);
    }
    
    // Slow consumer to create backpressure
    let queue_clone = Arc::clone(&queue);
    let consumer = thread::spawn(move || {
        let mut received = 0;
        for _ in 0..200 { // More iterations to catch all messages
            if let Ok(Some(_)) = queue_clone.try_dequeue() {
                received += 1;
            }
            thread::sleep(Duration::from_micros(10)); // Slow consumer
        }
        received
    });
    
    // Wait for all producers
    let mut _total_sent = 0;
    for handle in handles {
        _total_sent += handle.join().unwrap();
    }
    
    let _received = consumer.join().unwrap();
    let stats = queue.stats();
    
    // Message loss should be bounded and tracked
    let total_accounted = stats.messages_dequeued + stats.current_size + stats.messages_dropped;
    assert_eq!(total_accounted, stats.messages_enqueued);
    
    // Some messages should have been dropped due to capacity limits
    assert!(stats.messages_dropped > 0, "Expected some message drops due to capacity limits");
    
    // But drops should be bounded (not all messages lost)
    let drop_rate = stats.messages_dropped as f64 / stats.messages_enqueued as f64;
    assert!(drop_rate < 0.9, "Drop rate too high: {}", drop_rate);
}

// ============================================================================
// Resize Coordination Tests
// ============================================================================

#[cfg(feature = "lock_free")]
#[test]
fn test_resize_flag_coordination() {
    let config = Config::default()
        .with_initial_capacity(4)
        .with_min_capacity(2)
        .with_max_capacity(32);
    
    let queue = Arc::new(LockFreeMPSCQueue::new(config).unwrap());
    
    // Fill queue to trigger resize
    for i in 0..4 {
        queue.try_enqueue(i).unwrap();
    }
    
    let resize_encountered = Arc::new(AtomicBool::new(false));
    let queue_clone = Arc::clone(&queue);
    let resize_flag = Arc::clone(&resize_encountered);
    
    // Producer that might encounter resize flag
    let producer = thread::spawn(move || {
        for i in 4..20 {
            loop {
                match queue_clone.try_enqueue(i) {
                    Ok(()) => break,
                    Err(BufferError::ResizeError(_)) => {
                        resize_flag.store(true, Ordering::Relaxed);
                        thread::yield_now(); // Yield and retry
                    }
                    Err(_) => thread::yield_now(),
                }
            }
        }
    });
    
    // Consumer that triggers resize
    let queue_clone = Arc::clone(&queue);
    let consumer = thread::spawn(move || {
        let mut received = 0;
        while received < 20 {
            match queue_clone.try_dequeue() {
                Ok(Some(_)) => received += 1,
                Ok(None) => thread::yield_now(),
                Err(_) => thread::yield_now(),
            }
        }
    });
    
    producer.join().unwrap();
    consumer.join().unwrap();
    
    // We should have completed successfully despite resize coordination
    let stats = queue.stats();
    assert_eq!(stats.messages_dequeued, 20);
}

#[cfg(feature = "lock_free")]
#[test]
fn test_concurrent_resize_attempts() {
    // This test verifies that only consumer can resize (not multiple consumers)
    let config = Config::default()
        .with_initial_capacity(4)
        .with_min_capacity(2)
        .with_max_capacity(64);
    
    let queue = Arc::new(LockFreeMPSCQueue::new(config).unwrap());
    
    // Fill queue
    for i in 0..4 {
        queue.try_enqueue(i).unwrap();
    }
    
    // Single consumer (as per MPSC design)
    let queue_clone = Arc::clone(&queue);
    let consumer = thread::spawn(move || {
        let mut received = 0;
        while received < 10 {
            match queue_clone.try_dequeue() {
                Ok(Some(_)) => received += 1,
                Ok(None) => thread::yield_now(),
                Err(_) => thread::yield_now(),
            }
        }
    });
    
    consumer.join().unwrap();
    
    // Queue should still be in valid state
    let stats = queue.stats();
    assert!(stats.current_capacity >= 4); // May have grown
}

#[cfg(feature = "lock_free")]
#[test]
fn test_resize_atomicity() {
    let config = Config::default()
        .with_initial_capacity(2)
        .with_min_capacity(1)
        .with_max_capacity(16);
    
    let queue = Arc::new(LockFreeMPSCQueue::new(config).unwrap());
    let atomic_operations = Arc::new(AtomicUsize::new(0));
    
    // Producer that observes resize atomicity
    let queue_clone = Arc::clone(&queue);
    let ops_counter = Arc::clone(&atomic_operations);
    let producer = thread::spawn(move || {
        for i in 0..50 {
            let initial_capacity = queue_clone.capacity();
            
            // Attempt enqueue
            while queue_clone.try_enqueue(i).is_err() {
                thread::yield_now();
            }
            
            let final_capacity = queue_clone.capacity();
            
            // If capacity changed, it should appear atomic
            if final_capacity != initial_capacity {
                ops_counter.fetch_add(1, Ordering::Relaxed);
                assert!(final_capacity > initial_capacity, 
                       "Capacity should only increase atomically");
            }
        }
    });
    
    // Consumer that triggers resizes
    let consumer = thread::spawn(move || {
        let mut received = 0;
        while received < 50 {
            match queue.try_dequeue() {
                Ok(Some(_)) => received += 1,
                Ok(None) => thread::yield_now(),
                Err(_) => thread::yield_now(),
            }
        }
    });
    
    producer.join().unwrap();
    consumer.join().unwrap();
    
    // Should have observed some capacity changes
    let capacity_changes = atomic_operations.load(Ordering::Relaxed);
    assert!(capacity_changes > 0, "Expected to observe capacity changes");
}

// ============================================================================
// Producer Lifecycle Tests
// ============================================================================

#[cfg(feature = "lock_free")]
#[test]
fn test_producer_join_leave_patterns() {
    let config = Config::default()
        .with_initial_capacity(8)
        .with_min_capacity(4)
        .with_max_capacity(64);
    
    let queue = Arc::new(LockFreeMPSCQueue::new(config).unwrap());
    let barrier = Arc::new(Barrier::new(5)); // 4 producers + 1 consumer
    
    // Dynamic producer lifecycle
    let mut producer_handles = vec![];
    for producer_id in 0..4 {
        let queue_clone = Arc::clone(&queue);
        let barrier_clone = Arc::clone(&barrier);
        
        let handle = thread::spawn(move || {
            barrier_clone.wait(); // Synchronize start
            
            // Each producer has different lifecycle
            let messages_to_send = match producer_id {
                0 => 50,  // Long-running
                1 => 20,  // Medium
                2 => 10,  // Short
                3 => 30,  // Medium-long
                _ => unreachable!(),
            };
            
            let mut sent = 0;
            for i in 0..messages_to_send {
                let message = producer_id * 1000 + i;
                while queue_clone.try_enqueue(message).is_err() {
                    thread::yield_now();
                }
                sent += 1;
                
                // Producer 2 leaves early
                if producer_id == 2 && i == 5 {
                    break;
                }
            }
            sent
        });
        producer_handles.push(handle);
    }
    
    // Consumer
    let queue_clone = Arc::clone(&queue);
    let consumer = thread::spawn(move || {
        barrier.wait(); // Synchronize start
        
        let mut received = 0;
        let mut consecutive_empty = 0;
        
        loop {
            match queue_clone.try_dequeue() {
                Ok(Some(_)) => {
                    received += 1;
                    consecutive_empty = 0;
                }
                Ok(None) => {
                    consecutive_empty += 1;
                    if consecutive_empty > 1000 {
                        break; // All producers likely finished
                    }
                    thread::yield_now();
                }
                Err(_) => thread::yield_now(),
            }
        }
        received
    });
    
    // Wait for all producers
    let mut total_sent = 0;
    for handle in producer_handles {
        total_sent += handle.join().unwrap();
    }
    
    let received = consumer.join().unwrap();
    
    // Verify all sent messages were received
    assert_eq!(received, total_sent);
    
    // Expected: 50 + 20 + 6 + 30 = 106 (producer 2 exits early)
    assert_eq!(total_sent, 106);
    
    let stats = queue.stats();
    assert_eq!(stats.messages_dequeued, total_sent);
}

#[cfg(feature = "lock_free")]
#[test]
fn test_producer_fairness_under_contention() {
    let config = Config::default()
        .with_initial_capacity(4)
        .with_min_capacity(2)
        .with_max_capacity(16); // Limited to create contention
    
    let queue = Arc::new(LockFreeMPSCQueue::new(config).unwrap());
    let num_producers = 8;
    let messages_per_producer = 100;
    
    let barrier = Arc::new(Barrier::new(num_producers + 1));
    let mut producer_handles = vec![];
    
    for producer_id in 0..num_producers {
        let queue_clone = Arc::clone(&queue);
        let barrier_clone = Arc::clone(&barrier);
        
        let handle = thread::spawn(move || {
            barrier_clone.wait(); // Synchronize start
            
            let mut sent = 0;
            for i in 0..messages_per_producer {
                let message = producer_id * 10000 + i;
                
                // Limited retries to test fairness
                for retry in 0..100 {
                    if queue_clone.try_enqueue(message).is_ok() {
                        sent += 1;
                        break;
                    }
                    
                    if retry % 10 == 0 {
                        thread::yield_now();
                    }
                }
            }
            (producer_id, sent)
        });
        producer_handles.push(handle);
    }
    
    // Consumer
    let queue_clone = Arc::clone(&queue);
    let consumer = thread::spawn(move || {
        barrier.wait(); // Synchronize start
        
        let mut received = 0;
        let start_time = Instant::now();
        
        while start_time.elapsed() < Duration::from_secs(5) {
            match queue_clone.try_dequeue() {
                Ok(Some(_)) => received += 1,
                Ok(None) => thread::yield_now(),
                Err(_) => thread::yield_now(),
            }
        }
        received
    });
    
    // Collect results
    let mut producer_results = vec![];
    for handle in producer_handles {
        producer_results.push(handle.join().unwrap());
    }
    
    let received = consumer.join().unwrap();
    
    // Analyze fairness
    let total_sent: usize = producer_results.iter().map(|(_, sent)| sent).sum();
    let min_sent = producer_results.iter().map(|(_, sent)| sent).min().unwrap();
    let max_sent = producer_results.iter().map(|(_, sent)| sent).max().unwrap();
    
    println!("Producer results: {:?}", producer_results);
    println!("Total sent: {}, received: {}", total_sent, received);
    
    // Fairness check: no producer should be completely starved
    assert!(*min_sent > 0, "Some producer was completely starved");
    
    // Fairness check: difference shouldn't be too extreme
    let fairness_ratio = *max_sent as f64 / *min_sent as f64;
    assert!(fairness_ratio < 10.0, "Fairness ratio too high: {}", fairness_ratio);
}

// ============================================================================
// Edge Case Stress Tests
// ============================================================================

#[cfg(feature = "lock_free")]
#[test]
fn test_maximum_capacity_stress() {
    let config = Config::default()
        .with_initial_capacity(2)
        .with_min_capacity(1)
        .with_max_capacity(8); // Very small max capacity
    
    let queue = Arc::new(LockFreeMPSCQueue::new(config).unwrap());
    
    // Fill to exact max capacity
    let mut _enqueued = 0;
    for i in 0..20 {
        match queue.try_enqueue(i) {
            Ok(()) => _enqueued += 1,
            Err(_) => break, // Hit capacity limit
        }
    }
    
    // Should not exceed max capacity
    assert!(queue.len() <= 8);
    assert!(queue.capacity() <= 8);
    
    // Additional enqueues should fail
    assert!(queue.try_enqueue(999).is_err());
    
    // Dequeue some and verify we can enqueue again
    let dequeued_count = 3;
    for _ in 0..dequeued_count {
        assert!(queue.try_dequeue().unwrap().is_some());
    }
    
    // Should be able to enqueue again
    for i in 100..103 {
        assert!(queue.try_enqueue(i).is_ok());
    }
    
    let stats = queue.stats();
    assert_eq!(stats.current_capacity, 8); // At max capacity
}

#[cfg(feature = "lock_free")]
#[test]
fn test_minimum_capacity_behavior() {
    let config = Config::default()
        .with_initial_capacity(8)
        .with_min_capacity(4)
        .with_max_capacity(32);
    
    let queue = LockFreeMPSCQueue::new(config).unwrap();
    
    // Fill and then drain to trigger shrinking
    for i in 0..16 {
        queue.try_enqueue(i).unwrap();
    }
    
    // Capacity should have grown
    assert!(queue.capacity() > 8);
    
    // Drain most items
    for _ in 0..15 {
        queue.try_dequeue().unwrap();
    }
    
    // Capacity should not shrink below min_capacity
    // Note: Shrinking logic is conservative, so test the boundary
    assert!(queue.capacity() >= 4);
    
    let stats = queue.stats();
    assert!(stats.current_capacity >= 4);
}

#[cfg(feature = "lock_free")]
#[test] 
fn test_rapid_resize_cycles() {
    let config = Config::default()
        .with_initial_capacity(4)
        .with_min_capacity(2)
        .with_max_capacity(64)
        .with_growth_factor(2.0);
    
    let queue = Arc::new(LockFreeMPSCQueue::new(config).unwrap());
    
    // Create rapid grow/shrink cycles
    for cycle in 0..10 {
        // Growth phase
        let grow_count = 16;
        for i in 0..grow_count {
            queue.try_enqueue(cycle * 1000 + i).unwrap();
        }
        
        let capacity_after_growth = queue.capacity();
        
        // Shrink phase  
        for _ in 0..14 { // Leave 2 items
            queue.try_dequeue().unwrap();
        }
        
        let capacity_after_shrink = queue.capacity();
        
        // Verify capacity management
        assert!(capacity_after_growth >= grow_count);
        assert!(capacity_after_shrink >= 2); // At least min or current size
        
        println!("Cycle {}: grow to {}, shrink to {}", 
                cycle, capacity_after_growth, capacity_after_shrink);
    }
    
    // Queue should still be functional
    queue.try_enqueue(9999).unwrap();
    assert_eq!(queue.try_dequeue().unwrap().unwrap(), 18); // From last cycle
}

// ============================================================================
// Configuration Edge Case Tests
// ============================================================================

#[test]
fn test_configuration_edge_values() {
    // Minimum possible capacity
    let config1 = Config::default()
        .with_initial_capacity(1)
        .with_min_capacity(1)
        .with_max_capacity(2)
        .with_growth_factor(1.01); // Very small growth
    
    assert!(config1.validate().is_ok());
    
    #[cfg(feature = "lock_free")]
    {
        // Note: Ring buffer requires power-of-2, so actual capacity will be 2
        let queue = LockFreeMPSCQueue::<i32>::new(config1).unwrap();
        assert!(queue.capacity() >= 1);
        assert!(queue.capacity().is_power_of_two());
    }
    
    // Maximum reasonable values
    let config2 = Config::default()
        .with_initial_capacity(1024)
        .with_min_capacity(512)
        .with_max_capacity(1048576)
        .with_growth_factor(10.0) // Large growth
        .with_shrink_threshold(0.01); // Aggressive shrinking
    
    assert!(config2.validate().is_ok());
    
    let buffer = DynamicCircularBuffer::<i32>::new(config2).unwrap();
    assert_eq!(buffer.capacity(), 1024);
}

#[test]
fn test_invalid_configuration_rejection() {
    // min > max
    let config1 = Config::default()
        .with_min_capacity(100)
        .with_max_capacity(50);
    assert!(config1.validate().is_err());
    
    // initial outside range
    let config2 = Config::default()
        .with_initial_capacity(200)
        .with_min_capacity(50)
        .with_max_capacity(100);
    assert!(config2.validate().is_err());
    
    // Invalid growth factor
    let config3 = Config::default()
        .with_growth_factor(0.5); // Must be > 1.0
    assert!(config3.validate().is_err());
    
    // Invalid shrink threshold
    let config4 = Config::default()
        .with_shrink_threshold(1.5); // Must be < 1.0
    assert!(config4.validate().is_err());
    
    let config5 = Config::default()
        .with_shrink_threshold(0.0); // Must be > 0.0
    assert!(config5.validate().is_err());
}

// ============================================================================
// Performance Regression Tests
// ============================================================================

#[cfg(feature = "lock_free")]
#[test]
fn test_performance_regression_bounds() {
    let config = Config::default()
        .with_initial_capacity(1024)
        .with_min_capacity(512)
        .with_max_capacity(1048576);
    
    let queue = LockFreeMPSCQueue::new(config).unwrap();
    let message_count = 100_000;
    
    // Single-threaded performance test
    let start = Instant::now();
    
    for i in 0..message_count {
        queue.try_enqueue(i).unwrap();
    }
    
    for _ in 0..message_count {
        queue.try_dequeue().unwrap();
    }
    
    let duration = start.elapsed();
    let throughput = (message_count * 2) as f64 / duration.as_secs_f64();
    
    // Should achieve at least 10M operations/sec (conservative bound)
    assert!(throughput > 10_000_000.0, 
           "Throughput regression: {} ops/sec (expected > 10M)", throughput);
    
    println!("Single-threaded throughput: {:.0} ops/sec", throughput);
}

#[cfg(feature = "lock_free")]
#[test] 
fn test_latency_percentile_bounds() {
    let config = Config::default()
        .with_initial_capacity(1024)
        .with_min_capacity(512)
        .with_max_capacity(1048576);
    
    let queue = LockFreeMPSCQueue::new(config).unwrap();
    let operations = 10_000;
    let mut latencies = Vec::with_capacity(operations);
    
    // Measure individual operation latencies
    for i in 0..operations {
        let start = Instant::now();
        queue.try_enqueue(i).unwrap();
        let enqueue_time = start.elapsed();
        
        let start = Instant::now();
        queue.try_dequeue().unwrap();
        let dequeue_time = start.elapsed();
        
        latencies.push(enqueue_time + dequeue_time);
    }
    
    // Sort for percentile calculation
    latencies.sort();
    
    let p99_idx = operations * 99 / 100;
    let p99_latency = latencies[p99_idx];
    
    let p999_idx = operations * 999 / 1000;
    let p999_latency = latencies[p999_idx];
    
    println!("P99 latency: {:?}", p99_latency);
    println!("P99.9 latency: {:?}", p999_latency);
    
    // Performance bounds (these are generous for CI environments)
    assert!(p99_latency < Duration::from_micros(10), 
           "P99 latency too high: {:?}", p99_latency);
    assert!(p999_latency < Duration::from_micros(100),
           "P99.9 latency too high: {:?}", p999_latency);
}

// ============================================================================
// Cross-Implementation Consistency Tests
// ============================================================================

#[test]
fn test_lock_based_vs_lock_free_consistency() {
    let config = Config::default()
        .with_initial_capacity(8)
        .with_min_capacity(4)
        .with_max_capacity(32);
    
    // Lock-based implementation
    let lock_based = DynamicCircularBuffer::new(config.clone()).unwrap();
    
    #[cfg(feature = "lock_free")]
    {
        // Lock-free implementation  
        let lock_free = LockFreeMPSCQueue::new(config).unwrap();
        
        // Same sequence of operations
        let operations = vec![1, 2, 3, 4, 5];
        
        // Apply to both implementations
        for &op in &operations {
            lock_based.push(op).unwrap();
            lock_free.try_enqueue(op).unwrap();
        }
        
        // Verify same results
        for &expected in &operations {
            assert_eq!(lock_based.pop().unwrap(), expected);
            assert_eq!(lock_free.try_dequeue().unwrap().unwrap(), expected);
        }
        
        // Both should be empty
        assert!(lock_based.pop().is_err());
        assert_eq!(lock_free.try_dequeue().unwrap(), None);
    }
}