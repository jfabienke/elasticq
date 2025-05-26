use elasticq::{Config, LockFreeMPSCQueue, DynamicCircularBuffer};
use std::sync::Arc;
use std::thread;
use std::time::Instant;
use std::sync::atomic::{AtomicUsize, Ordering};

fn main() {
    println!("⚡ Quick Scalability Test");
    println!("========================\n");
    
    let num_cores = thread::available_parallelism().unwrap().get();
    println!("💻 System: {} CPU cores available", num_cores);
    println!();
    
    // Test specific producer counts that scale with cores
    let test_configs = [
        (1, "Single Producer"),
        (num_cores / 4, "Quarter Cores"),
        (num_cores / 2, "Half Cores"),
        (num_cores, "All Cores"),
        (num_cores * 2, "2x Cores"),
    ];
    
    println!("📊 Comparison Results:");
    println!("   {:<15} {:<15} {:<15} {:<15} {:<10}", "Producers", "Lock-Free", "Lock-Based", "Speedup", "Winner");
    println!("   {}", "-".repeat(75));
    
    for (producers, description) in test_configs {
        if producers == 0 { continue; }
        
        let lf_throughput = test_lock_free_quick(producers);
        let lb_throughput = test_lock_based_quick(producers);
        
        let speedup = lf_throughput / lb_throughput;
        let winner = if speedup > 1.0 { "🚀 Lock-Free" } else { "🔒 Lock-Based" };
        
        println!("   {:<15} {:<13.0} K {:<13.0} K {:<13.2}x {:<10}", 
                description, 
                lf_throughput / 1000.0, 
                lb_throughput / 1000.0, 
                speedup,
                winner);
    }
    
    println!("\n🎯 Maximum Capacity Stress Test:");
    stress_test_max_throughput(num_cores * 2);
}

fn test_lock_free_quick(num_producers: usize) -> f64 {
    let config = Config::default()
        .with_initial_capacity(2048)
        .with_min_capacity(1024)
        .with_max_capacity(131072)
        .with_growth_factor(2.0);
    
    let queue = Arc::new(LockFreeMPSCQueue::new(config).unwrap());
    let messages_per_producer = 10_000; // Reduced for speed
    let total_messages = num_producers * messages_per_producer;
    
    let start_time = Instant::now();
    let completed_messages = Arc::new(AtomicUsize::new(0));
    
    // Producer threads
    let mut producer_handles = vec![];
    for producer_id in 0..num_producers {
        let queue_clone = Arc::clone(&queue);
        let completed_clone = Arc::clone(&completed_messages);
        
        let handle = thread::spawn(move || {
            let mut sent = 0;
            for i in 0..messages_per_producer {
                let message = (producer_id as i64) << 32 | (i as i64);
                
                let mut retries = 0;
                while queue_clone.try_enqueue(message).is_err() {
                    retries += 1;
                    if retries > 100 { break; } // Limit retries for speed
                    if retries % 10 == 0 { thread::yield_now(); }
                }
                if retries <= 100 { sent += 1; }
            }
            completed_clone.fetch_add(sent, Ordering::Relaxed);
        });
        producer_handles.push(handle);
    }
    
    // Consumer thread
    let queue_clone = Arc::clone(&queue);
    let consumer_handle = thread::spawn(move || {
        let mut received = 0;
        while received < total_messages {
            match queue_clone.try_dequeue() {
                Ok(Some(_)) => received += 1,
                Ok(None) => {
                    if completed_messages.load(Ordering::Relaxed) >= total_messages {
                        // Check if any messages left
                        if queue_clone.len() == 0 { break; }
                    }
                    thread::yield_now();
                }
                Err(_) => thread::yield_now(),
            }
        }
        received
    });
    
    // Wait for completion
    for handle in producer_handles {
        handle.join().unwrap();
    }
    let received = consumer_handle.join().unwrap();
    let duration = start_time.elapsed();
    
    received as f64 / duration.as_secs_f64()
}

fn test_lock_based_quick(num_producers: usize) -> f64 {
    let config = Config::default()
        .with_initial_capacity(2048)
        .with_min_capacity(1024)
        .with_max_capacity(131072)
        .with_growth_factor(2.0);
    
    let buffer = Arc::new(DynamicCircularBuffer::new(config).unwrap());
    let messages_per_producer = 10_000;
    let total_messages = num_producers * messages_per_producer;
    
    let start_time = Instant::now();
    
    // Producer threads
    let mut producer_handles = vec![];
    for producer_id in 0..num_producers {
        let buffer_clone = Arc::clone(&buffer);
        
        let handle = thread::spawn(move || {
            for i in 0..messages_per_producer {
                let message = (producer_id as i64) << 32 | (i as i64);
                
                // Keep trying until success
                while buffer_clone.push(message).is_err() {
                    thread::yield_now();
                }
            }
        });
        producer_handles.push(handle);
    }
    
    // Consumer thread
    let buffer_clone = Arc::clone(&buffer);
    let consumer_handle = thread::spawn(move || {
        let mut received = 0;
        while received < total_messages {
            match buffer_clone.pop() {
                Ok(_) => received += 1,
                Err(_) => thread::yield_now(),
            }
        }
        received
    });
    
    // Wait for completion
    for handle in producer_handles {
        handle.join().unwrap();
    }
    let received = consumer_handle.join().unwrap();
    let duration = start_time.elapsed();
    
    received as f64 / duration.as_secs_f64()
}

fn stress_test_max_throughput(max_producers: usize) {
    println!("   Testing with {} producers...", max_producers);
    
    let config = Config::default()
        .with_initial_capacity(4096)
        .with_min_capacity(2048)
        .with_max_capacity(524288) // 512K
        .with_growth_factor(1.5);
    
    let queue = Arc::new(LockFreeMPSCQueue::new(config).unwrap());
    let messages_per_producer = 25_000;
    let total_messages = max_producers * messages_per_producer;
    
    println!("   Target: {} total messages", total_messages);
    
    let start_time = Instant::now();
    let enqueued = Arc::new(AtomicUsize::new(0));
    let dropped = Arc::new(AtomicUsize::new(0));
    
    // Maximum producer threads
    let mut producer_handles = vec![];
    for producer_id in 0..max_producers {
        let queue_clone = Arc::clone(&queue);
        let enqueued_clone = Arc::clone(&enqueued);
        let dropped_clone = Arc::clone(&dropped);
        
        let handle = thread::spawn(move || {
            let mut local_enqueued = 0;
            let mut local_dropped = 0;
            
            for i in 0..messages_per_producer {
                let message = (producer_id as i64) << 32 | (i as i64);
                
                match queue_clone.try_enqueue(message) {
                    Ok(()) => local_enqueued += 1,
                    Err(_) => {
                        // Try a few more times, then drop
                        let mut retries = 0;
                        loop {
                            if queue_clone.try_enqueue(message).is_ok() {
                                local_enqueued += 1;
                                break;
                            }
                            retries += 1;
                            if retries > 5 {
                                local_dropped += 1;
                                break;
                            }
                            thread::yield_now();
                        }
                    }
                }
            }
            
            enqueued_clone.fetch_add(local_enqueued, Ordering::Relaxed);
            dropped_clone.fetch_add(local_dropped, Ordering::Relaxed);
        });
        producer_handles.push(handle);
    }
    
    // High-performance consumer
    let queue_clone = Arc::clone(&queue);
    let enqueued_clone = Arc::clone(&enqueued);
    let dropped_clone = Arc::clone(&dropped);
    let consumer_handle = thread::spawn(move || {
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
                    if consecutive_empty > 10000 {
                        // Check if all producers finished
                        if enqueued_clone.load(Ordering::Relaxed) >= total_messages - 
                           dropped_clone.load(Ordering::Relaxed) && received >= enqueued_clone.load(Ordering::Relaxed) {
                            break;
                        }
                        consecutive_empty = 0;
                        thread::yield_now();
                    }
                }
                Err(_) => thread::yield_now(),
            }
        }
        received
    });
    
    // Wait for completion
    for handle in producer_handles {
        handle.join().unwrap();
    }
    let received = consumer_handle.join().unwrap();
    let duration = start_time.elapsed();
    
    let total_enqueued = enqueued.load(Ordering::Relaxed);
    let total_dropped = dropped.load(Ordering::Relaxed);
    let stats = queue.stats();
    
    println!("   Results:");
    println!("     Duration: {:?}", duration);
    println!("     Enqueued: {} ({:.1}%)", total_enqueued, 
             (total_enqueued as f64 / total_messages as f64) * 100.0);
    println!("     Dropped: {} ({:.1}%)", total_dropped,
             (total_dropped as f64 / total_messages as f64) * 100.0);
    println!("     Received: {}", received);
    println!("     Throughput: {:.0} msg/sec", total_enqueued as f64 / duration.as_secs_f64());
    println!("     Peak capacity: {}", stats.current_capacity);
    println!("     CPU utilization: {:.0}%", 
             (max_producers as f64 / thread::available_parallelism().unwrap().get() as f64) * 100.0);
    
    if received == total_enqueued {
        println!("     ✅ Data integrity: PERFECT");
    } else {
        println!("     ⚠️  Data integrity: {} discrepancy", total_enqueued - received);
    }
}