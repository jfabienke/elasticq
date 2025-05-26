use elasticq::{Config, LockFreeMPSCQueue, DynamicCircularBuffer};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use std::sync::atomic::{AtomicUsize, Ordering};

fn main() {
    println!("🚀 Scalability Test: Lock-Free vs Lock-Based");
    println!("============================================\n");
    
    let num_cores = thread::available_parallelism().unwrap().get();
    println!("💻 System Info:");
    println!("   Available CPU cores: {}", num_cores);
    println!("   Test configurations: 1 to {} producers", num_cores * 2);
    println!();
    
    // Test different producer counts
    let producer_counts = [1, 2, 4, num_cores, num_cores * 2];
    
    for &producers in &producer_counts {
        if producers > num_cores * 2 { continue; }
        
        println!("📊 Testing with {} producers", producers);
        println!("   {}", "=".repeat(30));
        
        test_lock_free_scalability(producers);
        test_lock_based_scalability(producers);
        println!();
    }
    
    // Stress test with maximum producers
    stress_test_maximum_load(num_cores * 2);
}

fn test_lock_free_scalability(num_producers: usize) {
    let config = Config::default()
        .with_initial_capacity(4096)
        .with_min_capacity(1024)
        .with_max_capacity(1048576)
        .with_growth_factor(2.0);
    
    let queue = Arc::new(LockFreeMPSCQueue::new(config).unwrap());
    let messages_per_producer = 100_000;
    let total_messages = num_producers * messages_per_producer;
    
    println!("   🔓 Lock-Free Implementation:");
    
    // Statistics tracking
    let messages_sent = Arc::new(AtomicUsize::new(0));
    let messages_failed = Arc::new(AtomicUsize::new(0));
    
    let start_time = Instant::now();
    
    // Spawn producer threads
    let mut producer_handles = vec![];
    for producer_id in 0..num_producers {
        let queue_clone = Arc::clone(&queue);
        let sent_counter = Arc::clone(&messages_sent);
        let failed_counter = Arc::clone(&messages_failed);
        
        let handle = thread::spawn(move || {
            let mut local_sent = 0;
            let mut local_failed = 0;
            let producer_start = Instant::now();
            
            for i in 0..messages_per_producer {
                let message = (producer_id as u64) << 32 | (i as u64);
                
                let mut retries = 0;
                loop {
                    match queue_clone.try_enqueue(message as i64) {
                        Ok(()) => {
                            local_sent += 1;
                            break;
                        }
                        Err(_) => {
                            local_failed += 1;
                            retries += 1;
                            if retries > 1000 {
                                // Give up after too many retries
                                break;
                            }
                            if retries % 100 == 0 {
                                thread::yield_now();
                            }
                        }
                    }
                }
            }
            
            let producer_duration = producer_start.elapsed();
            sent_counter.fetch_add(local_sent, Ordering::Relaxed);
            failed_counter.fetch_add(local_failed, Ordering::Relaxed);
            
            (local_sent, local_failed, producer_duration)
        });
        producer_handles.push(handle);
    }
    
    // Consumer thread
    let queue_clone = Arc::clone(&queue);
    let consumer_handle = thread::spawn(move || {
        let mut received = 0;
        let mut consumer_failures = 0;
        let consumer_start = Instant::now();
        let mut last_report = Instant::now();
        
        while received < total_messages {
            match queue_clone.try_dequeue() {
                Ok(Some(_message)) => {
                    received += 1;
                    
                    // Progress reporting
                    if last_report.elapsed() > Duration::from_secs(1) {
                        println!("     Consumer progress: {}/{} ({:.1}%)", 
                                received, total_messages, 
                                (received as f64 / total_messages as f64) * 100.0);
                        last_report = Instant::now();
                    }
                }
                Ok(None) => {
                    consumer_failures += 1;
                    if consumer_failures % 10000 == 0 {
                        thread::yield_now();
                    }
                }
                Err(_) => {
                    consumer_failures += 1;
                    thread::yield_now();
                }
            }
        }
        
        let consumer_duration = consumer_start.elapsed();
        (received, consumer_failures, consumer_duration)
    });
    
    // Wait for all producers to complete
    let mut producer_results = vec![];
    for handle in producer_handles {
        producer_results.push(handle.join().unwrap());
    }
    
    // Wait for consumer to complete
    let (received, consumer_failures, consumer_duration) = consumer_handle.join().unwrap();
    let total_duration = start_time.elapsed();
    
    // Collect statistics
    let total_sent = messages_sent.load(Ordering::Relaxed);
    let total_failed = messages_failed.load(Ordering::Relaxed);
    let stats = queue.stats();
    
    // Report results
    println!("     ✅ Results:");
    println!("        Total duration: {:?}", total_duration);
    println!("        Messages sent: {}/{} ({:.1}%)", total_sent, total_messages, 
             (total_sent as f64 / total_messages as f64) * 100.0);
    println!("        Messages received: {}", received);
    println!("        Messages failed: {}", total_failed);
    println!("        Consumer failures: {}", consumer_failures);
    println!("        Throughput: {:.0} msg/sec", total_sent as f64 / total_duration.as_secs_f64());
    println!("        Consumer rate: {:.0} msg/sec", received as f64 / consumer_duration.as_secs_f64());
    println!("        Final queue stats: {:?}", stats);
    
    // Producer performance breakdown
    println!("     📈 Producer Performance:");
    for (i, (sent, failed, duration)) in producer_results.iter().enumerate() {
        let rate = *sent as f64 / duration.as_secs_f64();
        println!("        Producer {}: {} sent, {} failed, {:.0} msg/sec", i, sent, failed, rate);
    }
}

fn test_lock_based_scalability(num_producers: usize) {
    let config = Config::default()
        .with_initial_capacity(4096)
        .with_min_capacity(1024)
        .with_max_capacity(1048576)
        .with_growth_factor(2.0);
    
    let buffer = Arc::new(DynamicCircularBuffer::new(config).unwrap());
    let messages_per_producer = 100_000;
    let total_messages = num_producers * messages_per_producer;
    
    println!("   🔒 Lock-Based Implementation:");
    
    let start_time = Instant::now();
    
    // Spawn producer threads
    let mut producer_handles = vec![];
    for producer_id in 0..num_producers {
        let buffer_clone = Arc::clone(&buffer);
        
        let handle = thread::spawn(move || {
            let mut sent = 0;
            let producer_start = Instant::now();
            
            for i in 0..messages_per_producer {
                let message = (producer_id as u64) << 32 | (i as u64);
                
                match buffer_clone.push(message as i64) {
                    Ok(()) => sent += 1,
                    Err(_) => {
                        // For fair comparison, keep trying like lock-free version
                        loop {
                            thread::yield_now();
                            if buffer_clone.push(message as i64).is_ok() {
                                sent += 1;
                                break;
                            }
                        }
                    }
                }
            }
            
            let producer_duration = producer_start.elapsed();
            (sent, producer_duration)
        });
        producer_handles.push(handle);
    }
    
    // Consumer thread
    let buffer_clone = Arc::clone(&buffer);
    let consumer_handle = thread::spawn(move || {
        let mut received = 0;
        let consumer_start = Instant::now();
        let mut last_report = Instant::now();
        
        while received < total_messages {
            match buffer_clone.pop() {
                Ok(_message) => {
                    received += 1;
                    
                    // Progress reporting
                    if last_report.elapsed() > Duration::from_secs(1) {
                        println!("     Consumer progress: {}/{} ({:.1}%)", 
                                received, total_messages, 
                                (received as f64 / total_messages as f64) * 100.0);
                        last_report = Instant::now();
                    }
                }
                Err(_) => {
                    thread::yield_now();
                }
            }
        }
        
        let consumer_duration = consumer_start.elapsed();
        (received, consumer_duration)
    });
    
    // Wait for all producers to complete
    let mut producer_results = vec![];
    for handle in producer_handles {
        producer_results.push(handle.join().unwrap());
    }
    
    // Wait for consumer to complete
    let (received, consumer_duration) = consumer_handle.join().unwrap();
    let total_duration = start_time.elapsed();
    
    // Report results
    let total_sent: usize = producer_results.iter().map(|(sent, _)| sent).sum();
    
    println!("     ✅ Results:");
    println!("        Total duration: {:?}", total_duration);
    println!("        Messages sent: {}", total_sent);
    println!("        Messages received: {}", received);
    println!("        Throughput: {:.0} msg/sec", total_sent as f64 / total_duration.as_secs_f64());
    println!("        Consumer rate: {:.0} msg/sec", received as f64 / consumer_duration.as_secs_f64());
    
    // Producer performance breakdown
    println!("     📈 Producer Performance:");
    for (i, (sent, duration)) in producer_results.iter().enumerate() {
        let rate = *sent as f64 / duration.as_secs_f64();
        println!("        Producer {}: {} sent, {:.0} msg/sec", i, sent, rate);
    }
}

fn stress_test_maximum_load(max_producers: usize) {
    println!("🔥 Stress Test: Maximum Load ({} producers)", max_producers);
    println!("   {}", "=".repeat(50));
    
    let config = Config::default()
        .with_initial_capacity(8192)
        .with_min_capacity(4096)
        .with_max_capacity(2097152) // 2MB
        .with_growth_factor(1.5);
    
    let queue = Arc::new(LockFreeMPSCQueue::new(config).unwrap());
    let messages_per_producer = 50_000;
    let total_messages = max_producers * messages_per_producer;
    
    println!("   Configuration:");
    println!("     Producers: {}", max_producers);
    println!("     Messages per producer: {}", messages_per_producer);
    println!("     Total messages: {}", total_messages);
    println!("     Max capacity: 2MB");
    println!();
    
    let start_time = Instant::now();
    
    // Spawn maximum producer threads
    let mut producer_handles = vec![];
    let messages_enqueued = Arc::new(AtomicUsize::new(0));
    let messages_dropped = Arc::new(AtomicUsize::new(0));
    
    for producer_id in 0..max_producers {
        let queue_clone = Arc::clone(&queue);
        let enqueued_counter = Arc::clone(&messages_enqueued);
        let dropped_counter = Arc::clone(&messages_dropped);
        
        let handle = thread::spawn(move || {
            let mut local_enqueued = 0;
            let mut local_dropped = 0;
            
            for i in 0..messages_per_producer {
                let message = (producer_id as u64) << 32 | (i as u64);
                
                // Try to enqueue with limited retries for stress test
                let mut attempts = 0;
                loop {
                    match queue_clone.try_enqueue(message as i64) {
                        Ok(()) => {
                            local_enqueued += 1;
                            break;
                        }
                        Err(_) => {
                            attempts += 1;
                            if attempts > 10 {
                                // Drop message after 10 attempts
                                local_dropped += 1;
                                break;
                            }
                            thread::yield_now();
                        }
                    }
                }
            }
            
            enqueued_counter.fetch_add(local_enqueued, Ordering::Relaxed);
            dropped_counter.fetch_add(local_dropped, Ordering::Relaxed);
            
            (local_enqueued, local_dropped)
        });
        producer_handles.push(handle);
    }
    
    // Aggressive consumer
    let queue_clone = Arc::clone(&queue);
    let enqueued_counter_clone = Arc::clone(&messages_enqueued);
    let dropped_counter_clone = Arc::clone(&messages_dropped);
    let consumer_handle = thread::spawn(move || {
        let mut received = 0;
        let mut empty_polls = 0;
        let consumer_start = Instant::now();
        let mut last_report = Instant::now();
        
        loop {
            match queue_clone.try_dequeue() {
                Ok(Some(_)) => {
                    received += 1;
                    empty_polls = 0;
                    
                    if last_report.elapsed() > Duration::from_millis(500) {
                        let stats = queue_clone.stats();
                        println!("     Live stats: received={}, queue_size={}, capacity={}", 
                                received, stats.current_size, stats.current_capacity);
                        last_report = Instant::now();
                    }
                }
                Ok(None) => {
                    empty_polls += 1;
                    if empty_polls > 100000 {
                        // Check if all producers are done
                        let total_enqueued = enqueued_counter_clone.load(Ordering::Relaxed);
                        if total_enqueued + dropped_counter_clone.load(Ordering::Relaxed) >= 
                           max_producers * messages_per_producer && received >= total_enqueued {
                            break;
                        }
                        empty_polls = 0;
                        thread::sleep(Duration::from_micros(10));
                    }
                }
                Err(_) => thread::yield_now(),
            }
        }
        
        let consumer_duration = consumer_start.elapsed();
        (received, consumer_duration)
    });
    
    // Wait for all threads
    let mut producer_results = vec![];
    for handle in producer_handles {
        producer_results.push(handle.join().unwrap());
    }
    
    let (received, consumer_duration) = consumer_handle.join().unwrap();
    let total_duration = start_time.elapsed();
    
    // Final statistics
    let total_enqueued = messages_enqueued.load(Ordering::Relaxed);
    let total_dropped = messages_dropped.load(Ordering::Relaxed);
    let final_stats = queue.stats();
    
    println!("\n   🎯 Stress Test Results:");
    println!("     Total duration: {:?}", total_duration);
    println!("     Messages enqueued: {}/{} ({:.1}%)", 
             total_enqueued, total_messages,
             (total_enqueued as f64 / total_messages as f64) * 100.0);
    println!("     Messages dropped: {} ({:.1}%)", 
             total_dropped,
             (total_dropped as f64 / total_messages as f64) * 100.0);
    println!("     Messages received: {}", received);
    println!("     Overall throughput: {:.0} msg/sec", 
             total_enqueued as f64 / total_duration.as_secs_f64());
    println!("     Consumer throughput: {:.0} msg/sec", 
             received as f64 / consumer_duration.as_secs_f64());
    println!("     Peak queue capacity: {}", final_stats.current_capacity);
    println!("     System utilization: {:.1}%", 
             (max_producers as f64 / thread::available_parallelism().unwrap().get() as f64) * 100.0);
    
    // Check for any data integrity issues
    if received != total_enqueued {
        println!("     ⚠️  Warning: Message count mismatch!");
    } else {
        println!("     ✅ Message integrity: PASSED");
    }
}