//! Advanced concurrency tests using loom for model checking
//!
//! These tests use the loom crate to verify correctness under all possible
//! thread interleavings, providing strong guarantees about concurrent behavior.

#[cfg(loom)]
mod loom_tests {
    use loom::sync::Arc;
    use loom::thread;
    use elasticq::{Config, LockFreeMPSCQueue};
    
    #[test]
    fn loom_mpsc_basic_correctness() {
        loom::model(|| {
            let config = Config::default()
                .with_initial_capacity(2)
                .with_min_capacity(1)
                .with_max_capacity(4);
            
            let queue = Arc::new(LockFreeMPSCQueue::new(config).unwrap());
            
            // Two producers, one consumer
            let queue1 = Arc::clone(&queue);
            let queue2 = Arc::clone(&queue);
            let queue3 = Arc::clone(&queue);
            
            let producer1 = thread::spawn(move || {
                queue1.try_enqueue(1).ok();
            });
            
            let producer2 = thread::spawn(move || {
                queue2.try_enqueue(2).ok();
            });
            
            let consumer = thread::spawn(move || {
                let mut received = Vec::new();
                for _ in 0..3 { // Try to receive all messages
                    if let Ok(Some(msg)) = queue3.try_dequeue() {
                        received.push(msg);
                    }
                }
                received
            });
            
            producer1.join().unwrap();
            producer2.join().unwrap();
            let received = consumer.join().unwrap();
            
            // At least some messages should be received
            // (exact count depends on scheduling)
            assert!(!received.is_empty() || queue.len() > 0);
        });
    }
    
    #[test]
    fn loom_resize_atomicity() {
        loom::model(|| {
            let config = Config::default()
                .with_initial_capacity(1)
                .with_min_capacity(1)
                .with_max_capacity(4);
            
            let queue = Arc::new(LockFreeMPSCQueue::new(config).unwrap());
            
            let queue1 = Arc::clone(&queue);
            let queue2 = Arc::clone(&queue);
            
            // Producer that triggers resize
            let producer = thread::spawn(move || {
                for i in 0..3 {
                    while queue1.try_enqueue(i).is_err() {
                        thread::yield_now();
                    }
                }
            });
            
            // Consumer that enables resize
            let consumer = thread::spawn(move || {
                let mut received = 0;
                for _ in 0..5 {
                    if queue2.try_dequeue().unwrap().is_some() {
                        received += 1;
                    }
                }
                received
            });
            
            producer.join().unwrap();
            let received = consumer.join().unwrap();
            
            // Verify queue is in consistent state
            let stats = queue.stats();
            let conservation = stats.messages_dequeued + stats.current_size + stats.messages_dropped;
            assert_eq!(conservation, stats.messages_enqueued);
        });
    }
    
    #[test]
    fn loom_aba_protection() {
        loom::model(|| {
            let config = Config::default()
                .with_initial_capacity(2)
                .with_min_capacity(1)
                .with_max_capacity(8);
            
            let queue = Arc::new(LockFreeMPSCQueue::new(config).unwrap());
            
            // Fill queue initially
            queue.try_enqueue(100).unwrap();
            queue.try_enqueue(200).unwrap();
            
            let queue1 = Arc::clone(&queue);
            let queue2 = Arc::clone(&queue);
            let queue3 = Arc::clone(&queue);
            
            // Producer that might encounter ABA
            let producer = thread::spawn(move || {
                queue1.try_enqueue(300).ok();
            });
            
            // Consumer that triggers resize (ABA scenario)
            let consumer = thread::spawn(move || {
                queue2.try_dequeue().ok();
                queue2.try_dequeue().ok();
            });
            
            // Second producer
            let producer2 = thread::spawn(move || {
                queue3.try_enqueue(400).ok();
            });
            
            producer.join().unwrap();
            consumer.join().unwrap(); 
            producer2.join().unwrap();
            
            // Queue should be in valid state regardless of scheduling
            let stats = queue.stats();
            assert!(stats.current_capacity <= 8);
        });
    }
}

// Regular concurrency tests (non-loom)

#[cfg(feature = "lock_free")]
#[cfg(not(loom))]
mod stress_tests {
    use elasticq::{Config, LockFreeMPSCQueue};
    use std::sync::{Arc, Barrier};
    use std::thread;
    use std::time::{Duration, Instant};
    use std::sync::atomic::{AtomicUsize, AtomicBool, Ordering};
    
    #[test]
    fn test_memory_reclamation_high_frequency_resize() {
        let config = Config::default()
            .with_initial_capacity(4)
            .with_min_capacity(2)
            .with_max_capacity(256);
        
        let queue = Arc::new(LockFreeMPSCQueue::new(config).unwrap());
        let num_producers = 3;
        let num_messages = 5000;
        
        let mut producers = Vec::new();
        for producer_id in 0..num_producers {
            let queue_clone = Arc::clone(&queue);
            producers.push(thread::spawn(move || {
                for i in 0..num_messages {
                    let value = (producer_id * num_messages + i) as u64;
                    while queue_clone.try_enqueue(value).is_err() {
                        thread::yield_now();
                    }
                    
                    // Create memory pressure by occasionally sleeping
                    if i % 1000 == 0 {
                        thread::sleep(Duration::from_micros(1));
                    }
                }
            }));
        }
        
        let mut received = 0;
        let expected_total = num_producers * num_messages;
        let mut resize_count = 0;
        let mut last_capacity = queue.capacity();
        
        while received < expected_total {
            if let Ok(Some(_value)) = queue.try_dequeue() {
                received += 1;
                
                // Track resizes to ensure memory reclamation occurs
                let current_capacity = queue.capacity();
                if current_capacity != last_capacity {
                    resize_count += 1;
                    last_capacity = current_capacity;
                }
                
                // Simulate variable consumer speed to trigger resizes
                if received % 500 == 0 {
                    thread::sleep(Duration::from_micros(10));
                }
            } else {
                thread::yield_now();
            }
        }
        
        for producer in producers {
            producer.join().unwrap();
        }
        
        assert_eq!(received, expected_total);
        assert!(resize_count > 0, "Expected at least one resize during memory reclamation test");
        
        // Verify final memory state is clean
        let stats = queue.stats();
        assert_eq!(stats.current_size, 0);
        let conservation = stats.messages_dequeued + stats.current_size + stats.messages_dropped;
        assert_eq!(conservation, stats.messages_enqueued);
    }

    #[test]
    fn test_memory_reclamation_safety() {
        let config = Config::default()
            .with_initial_capacity(4)
            .with_min_capacity(2)
            .with_max_capacity(32);
        
        let queue = Arc::new(LockFreeMPSCQueue::new(config).unwrap());
        let iterations = 1000;
        
        // Producer that forces many resizes
        let queue_clone = Arc::clone(&queue);
        let producer = thread::spawn(move || {
            for i in 0..iterations {
                while queue_clone.try_enqueue(i).is_err() {
                    thread::yield_now();
                }
                
                // Occasionally check that queue is still valid
                if i % 100 == 0 {
                    let stats = queue_clone.stats();
                    assert!(stats.current_capacity >= 2);
                    assert!(stats.current_capacity <= 32);
                }
            }
        });
        
        // Consumer that triggers many resizes through dequeue
        let queue_clone = Arc::clone(&queue);
        let consumer = thread::spawn(move || {
            let mut received = 0;
            while received < iterations {
                match queue_clone.try_dequeue() {
                    Ok(Some(_)) => received += 1,
                    Ok(None) => thread::yield_now(),
                    Err(_) => thread::yield_now(),
                }
                
                // Verify invariants periodically
                if received % 100 == 0 {
                    let stats = queue_clone.stats();
                    let conservation = stats.messages_dequeued + stats.current_size + stats.messages_dropped;
                    assert_eq!(conservation, stats.messages_enqueued);
                }
            }
        });
        
        producer.join().unwrap();
        consumer.join().unwrap();
        
        // Final verification - no memory should be leaked
        let final_stats = queue.stats();
        assert_eq!(final_stats.messages_dequeued, iterations);
        
        // Queue should be empty and in valid state
        assert_eq!(queue.len(), 0);
        assert!(queue.capacity() >= 2);
        assert!(queue.capacity() <= 32);
    }
    
    #[test]
    fn test_consumer_state_management() {
        let config = Config::default()
            .with_initial_capacity(8)
            .with_max_capacity(64);
        
        let queue = Arc::new(LockFreeMPSCQueue::new(config).unwrap());
        
        // Fill queue with producers
        let mut producer_handles = vec![];
        for producer_id in 0..4 {
            let queue_clone = Arc::clone(&queue);
            let handle = thread::spawn(move || {
                for i in 0..50 {
                    let message = producer_id * 1000 + i;
                    while queue_clone.try_enqueue(message).is_err() {
                        thread::yield_now();
                    }
                }
            });
            producer_handles.push(handle);
        }
        
        // Wait for producers to finish
        for handle in producer_handles {
            handle.join().unwrap();
        }
        
        // Now test consumer behavior
        let queue_clone = Arc::clone(&queue);
        let consumer = thread::spawn(move || {
            let mut phase1_received = 0;
            
            // Phase 1: Active consumption
            for _ in 0..100 {
                if queue_clone.try_dequeue().unwrap().is_some() {
                    phase1_received += 1;
                }
            }
            
            // Phase 2: Pause (simulate consumer being slow/stopped)
            thread::sleep(Duration::from_millis(10));
            
            // Phase 3: Resume consumption
            let mut phase2_received = 0;
            while let Ok(Some(_)) = queue_clone.try_dequeue() {
                phase2_received += 1;
            }
            
            (phase1_received, phase2_received)
        });
        
        let (phase1, phase2) = consumer.join().unwrap();
        let total_received = phase1 + phase2;
        
        // Should have received all 200 messages (4 producers × 50 messages)
        assert_eq!(total_received, 200);
        
        // Queue should be empty
        assert_eq!(queue.len(), 0);
        
        // Verify final state
        let stats = queue.stats();
        assert_eq!(stats.messages_dequeued, 200);
        assert_eq!(stats.messages_enqueued, 200);
        assert_eq!(stats.current_size, 0);
    }
    
    #[test]
    fn test_high_contention_scenario() {
        let config = Config::default()
            .with_initial_capacity(16)
            .with_min_capacity(8)
            .with_max_capacity(128);
        
        let queue = Arc::new(LockFreeMPSCQueue::new(config).unwrap());
        let num_producers = 8;
        let messages_per_producer = 1000;
        let total_messages = num_producers * messages_per_producer;
        
        let start_barrier = Arc::new(Barrier::new(num_producers + 1));
        let contention_detected = Arc::new(AtomicUsize::new(0));
        
        // Launch many producers simultaneously
        let mut producer_handles = vec![];
        for producer_id in 0..num_producers {
            let queue_clone = Arc::clone(&queue);
            let barrier_clone = Arc::clone(&start_barrier);
            let contention_counter = Arc::clone(&contention_detected);
            
            let handle = thread::spawn(move || {
                barrier_clone.wait(); // Synchronize start for maximum contention
                
                let mut sent = 0;
                let mut retry_count = 0;
                
                for i in 0..messages_per_producer {
                    let message = producer_id * 10000 + i;
                    
                    loop {
                        match queue_clone.try_enqueue(message) {
                            Ok(()) => {
                                sent += 1;
                                break;
                            }
                            Err(_) => {
                                retry_count += 1;
                                if retry_count % 10 == 0 {
                                    contention_counter.fetch_add(1, Ordering::Relaxed);
                                }
                                thread::yield_now();
                            }
                        }
                    }
                }
                (producer_id, sent, retry_count)
            });
            producer_handles.push(handle);
        }
        
        // Single consumer under high load
        let queue_clone = Arc::clone(&queue);
        let consumer = thread::spawn(move || {
            start_barrier.wait(); // Start with producers
            
            let mut received = 0;
            let start_time = Instant::now();
            
            while received < total_messages && start_time.elapsed() < Duration::from_secs(10) {
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
        
        // Analyze results
        let total_sent: usize = producer_results.iter().map(|(_, sent, _)| sent).sum();
        let total_retries: usize = producer_results.iter().map(|(_, _, retries)| retries).sum();
        let contention_events = contention_detected.load(Ordering::Relaxed);
        
        println!("High contention test results:");
        println!("  Total sent: {}", total_sent);
        println!("  Total received: {}", received);
        println!("  Total retries: {}", total_retries);
        println!("  Contention events: {}", contention_events);
        
        // Correctness assertions
        assert_eq!(received, total_sent, "Message loss detected");
        assert_eq!(total_sent, total_messages, "Not all messages were sent");
        
        // Performance assertions
        assert!(contention_events > 0, "Expected some contention under high load");
        
        // Final state verification
        let stats = queue.stats();
        assert_eq!(stats.messages_dequeued, total_messages);
        assert_eq!(stats.messages_enqueued, total_messages);
        assert_eq!(stats.current_size, 0);
        
        // Conservation equation
        let conservation = stats.messages_dequeued + stats.current_size + stats.messages_dropped;
        assert_eq!(conservation, stats.messages_enqueued);
    }
    
    #[test]
    fn test_long_running_stability() {
        let config = Config::default()
            .with_initial_capacity(32)
            .with_min_capacity(16)
            .with_max_capacity(512);
        
        let queue = Arc::new(LockFreeMPSCQueue::new(config).unwrap());
        let duration = Duration::from_secs(2); // Reduced for CI
        let stop_flag = Arc::new(AtomicBool::new(false));
        
        // Continuous producer
        let queue_clone = Arc::clone(&queue);
        let stop_clone = Arc::clone(&stop_flag);
        let producer = thread::spawn(move || {
            let mut sent = 0;
            let mut message_id = 0;
            
            while !stop_clone.load(Ordering::Relaxed) {
                if queue_clone.try_enqueue(message_id).is_ok() {
                    sent += 1;
                    message_id += 1;
                }
                
                // Yield occasionally to prevent tight loop
                if sent % 1000 == 0 {
                    thread::yield_now();
                }
            }
            sent
        });
        
        // Continuous consumer
        let queue_clone = Arc::clone(&queue);
        let stop_clone = Arc::clone(&stop_flag);
        let consumer = thread::spawn(move || {
            let mut received = 0;
            let mut last_message = -1;
            
            while !stop_clone.load(Ordering::Relaxed) {
                match queue_clone.try_dequeue() {
                    Ok(Some(msg)) => {
                        received += 1;
                        
                        // Verify FIFO ordering
                        assert!(msg > last_message, "FIFO ordering violated: {} <= {}", msg, last_message);
                        last_message = msg;
                    }
                    Ok(None) => thread::yield_now(),
                    Err(_) => thread::yield_now(),
                }
            }
            received
        });
        
        // Run for specified duration
        thread::sleep(duration);
        stop_flag.store(true, Ordering::Relaxed);
        
        let sent = producer.join().unwrap();
        let received = consumer.join().unwrap();
        
        // Drain remaining messages
        let mut final_received = received;
        while let Ok(Some(_)) = queue.try_dequeue() {
            final_received += 1;
        }
        
        println!("Long-running stability test:");
        println!("  Duration: {:?}", duration);
        println!("  Messages sent: {}", sent);
        println!("  Messages received: {}", final_received);
        println!("  Throughput: {:.0} msg/sec", sent as f64 / duration.as_secs_f64());
        
        // Verify correctness
        assert_eq!(final_received, sent, "Message loss in long-running test");
        
        // Verify final state
        let stats = queue.stats();
        assert_eq!(stats.current_size, 0);
        assert_eq!(stats.messages_dequeued, sent);
        assert_eq!(stats.messages_enqueued, sent);
        
        // No significant performance degradation expected
        let throughput = sent as f64 / duration.as_secs_f64();
        assert!(throughput > 100_000.0, "Throughput too low: {:.0} msg/sec", throughput);
    }

    #[test]
    fn test_consumer_head_movement_consistency() {
        let config = Config::default()
            .with_initial_capacity(8)
            .with_min_capacity(4)
            .with_max_capacity(32);
        
        let queue = Arc::new(LockFreeMPSCQueue::new(config).unwrap());
        
        // Fill queue partially
        for i in 0..5 {
            queue.try_enqueue(i).unwrap();
        }
        
        let queue_clone = Arc::clone(&queue);
        let consumer = thread::spawn(move || {
            let mut consumed = Vec::new();
            
            // Consume messages one by one, with verification
            for _ in 0..5 {
                if let Ok(Some(msg)) = queue_clone.try_dequeue() {
                    consumed.push(msg);
                    
                    // Verify queue state consistency after each dequeue
                    let stats = queue_clone.stats();
                    let conservation = stats.messages_dequeued + stats.current_size + stats.messages_dropped;
                    assert_eq!(conservation, stats.messages_enqueued);
                }
            }
            consumed
        });
        
        let consumed = consumer.join().unwrap();
        
        // Verify FIFO ordering
        assert_eq!(consumed, vec![0, 1, 2, 3, 4]);
        
        // Verify final empty state
        assert_eq!(queue.len(), 0);
        assert!(queue.try_dequeue().unwrap().is_none());
    }

    #[test]
    fn test_concurrent_resize_safety() {
        let config = Config::default()
            .with_initial_capacity(2)
            .with_min_capacity(1)
            .with_max_capacity(64);
        
        let queue = Arc::new(LockFreeMPSCQueue::new(config).unwrap());
        let num_iterations = 1000;
        
        // Producer that creates resize pressure
        let queue_clone = Arc::clone(&queue);
        let producer = thread::spawn(move || {
            for i in 0..num_iterations {
                while queue_clone.try_enqueue(i as i32).is_err() {
                    thread::yield_now();
                }
                
                // Occasionally yield to allow consumer to catch up
                if i % 100 == 0 {
                    thread::yield_now();
                }
            }
        });
        
        // Consumer that also creates resize pressure (in opposite direction)
        let queue_clone = Arc::clone(&queue);
        let consumer = thread::spawn(move || {
            let mut received = 0;
            let mut messages = Vec::new();
            
            while received < num_iterations {
                if let Ok(Some(msg)) = queue_clone.try_dequeue() {
                    messages.push(msg);
                    received += 1;
                    
                    // Verify ordering during resize operations
                    if messages.len() > 1 {
                        let len = messages.len();
                        assert!(messages[len-1] > messages[len-2], 
                               "FIFO violated during resize: {} <= {}", 
                               messages[len-1], messages[len-2]);
                    }
                } else {
                    thread::yield_now();
                }
            }
            messages
        });
        
        producer.join().unwrap();
        let received_messages = consumer.join().unwrap();
        
        // Verify all messages received in order
        assert_eq!(received_messages.len(), num_iterations);
        for (i, &msg) in received_messages.iter().enumerate() {
            assert_eq!(msg, i as i32);
        }
        
        // Verify final state
        let stats = queue.stats();
        assert_eq!(stats.current_size, 0);
        assert_eq!(stats.messages_dequeued, num_iterations);
        assert_eq!(stats.messages_enqueued, num_iterations);
    }

    #[test]
    fn test_generation_counter_rollover_safety() {
        let config = Config::default()
            .with_initial_capacity(4)
            .with_min_capacity(2)
            .with_max_capacity(16);
        
        let queue = Arc::new(LockFreeMPSCQueue::new(config).unwrap());
        let num_cycles = 100;
        
        // Simulate many enqueue/dequeue cycles to test generation counter behavior
        for cycle in 0..num_cycles {
            // Fill queue
            for i in 0..8 {
                let message = cycle * 100 + i;
                while queue.try_enqueue(message).is_err() {
                    thread::yield_now();
                }
            }
            
            // Drain queue
            let mut received_in_cycle = Vec::new();
            while let Ok(Some(msg)) = queue.try_dequeue() {
                received_in_cycle.push(msg);
            }
            
            // Verify cycle messages are in order
            for (i, &msg) in received_in_cycle.iter().enumerate() {
                let expected = cycle * 100 + i as i32;
                assert_eq!(msg, expected, "Message order violated in cycle {}", cycle);
            }
            
            // Verify queue is empty between cycles
            assert_eq!(queue.len(), 0);
        }
        
        // Final verification
        let stats = queue.stats();
        let expected_total = (num_cycles * 8) as usize;
        assert_eq!(stats.messages_dequeued, expected_total);
        assert_eq!(stats.messages_enqueued, expected_total);
    }
}