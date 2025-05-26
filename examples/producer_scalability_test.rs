use elasticq::{Config, lock_free::LockFreeMPSCQueue};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use std::sync::atomic::{AtomicUsize, Ordering};

fn main() {
    println!("🚀 Producer Scalability Analysis: 1-20 Producers");
    println!("=================================================\n");
    
    let num_cores = thread::available_parallelism().unwrap().get();
    println!("💻 System Info:");
    println!("   Available CPU cores: {}", num_cores);
    println!("   Testing: 1 to 20 producers with single consumer");
    println!();
    
    // Store results for analysis
    let mut results = vec![];
    
    // Test each producer count from 1 to 20
    for num_producers in 1..=20 {
        println!("📊 Testing with {} producer{}", num_producers, if num_producers == 1 { "" } else { "s" });
        
        let result = benchmark_lock_free(num_producers);
        
        println!("   ✅ Throughput: {:.0} msg/sec, Success Rate: {:.1}%\n", 
                result.throughput, result.success_rate);
        
        results.push((num_producers, result));
                
        // Brief pause between tests to let system settle
        thread::sleep(Duration::from_millis(100));
    }
    
    // Analysis and summary
    print_summary_analysis(&results);
}

#[derive(Clone)]
struct BenchmarkResult {
    throughput: f64,
    success_rate: f64,
    consumer_throughput: f64,
    total_duration: Duration,
    queue_peak_capacity: usize,
    messages_sent: usize,
    messages_received: usize,
    messages_failed: usize,
}

fn benchmark_lock_free(num_producers: usize) -> BenchmarkResult {
    let config = Config::default()
        .with_initial_capacity(1024)
        .with_min_capacity(512)
        .with_max_capacity(262144) // 256K messages max
        .with_growth_factor(2.0);
    
    let queue = Arc::new(LockFreeMPSCQueue::new(config).unwrap());
    let messages_per_producer = 50_000; // Moderate load for consistent comparison
    let total_expected = num_producers * messages_per_producer;
    
    // Shared counters
    let messages_sent = Arc::new(AtomicUsize::new(0));
    let messages_failed = Arc::new(AtomicUsize::new(0));
    
    let start_time = Instant::now();
    
    // Launch producers
    let mut producer_handles = vec![];
    for producer_id in 0..num_producers {
        let queue_clone: Arc<LockFreeMPSCQueue<i64>> = Arc::clone(&queue);
        let sent_counter = Arc::clone(&messages_sent);
        let failed_counter = Arc::clone(&messages_failed);
        
        let handle = thread::spawn(move || {
            let mut local_sent = 0;
            let mut local_failed = 0;
            
            for i in 0..messages_per_producer {
                let message = (producer_id as u64) << 32 | (i as u64);
                
                // Try to enqueue with bounded retries
                let mut retries = 0;
                loop {
                    match queue_clone.try_enqueue(message as i64) {
                        Ok(()) => {
                            local_sent += 1;
                            break;
                        }
                        Err(_) => {
                            retries += 1;
                            if retries > 100 {
                                local_failed += 1;
                                break; // Give up after 100 retries
                            }
                            if retries % 10 == 0 {
                                thread::yield_now();
                            }
                        }
                    }
                }
            }
            
            sent_counter.fetch_add(local_sent, Ordering::Relaxed);
            failed_counter.fetch_add(local_failed, Ordering::Relaxed);
            
            (local_sent, local_failed)
        });
        producer_handles.push(handle);
    }
    
    // Single consumer
    let queue_clone: Arc<LockFreeMPSCQueue<i64>> = Arc::clone(&queue);
    let consumer_handle = thread::spawn(move || {
        let mut received = 0;
        let mut empty_attempts = 0;
        let consumer_start = Instant::now();
        let mut peak_capacity = 0;
        
        loop {
            match queue_clone.try_dequeue() {
                Ok(Some(_)) => {
                    received += 1;
                    empty_attempts = 0;
                    
                    // Track peak capacity
                    let current_capacity = queue_clone.capacity();
                    if current_capacity > peak_capacity {
                        peak_capacity = current_capacity;
                    }
                }
                Ok(None) => {
                    empty_attempts += 1;
                    if empty_attempts > 50000 {
                        // Check if we should stop
                        thread::sleep(Duration::from_micros(10));
                        if empty_attempts > 100000 {
                            break; // Assume producers are done
                        }
                    }
                    if empty_attempts % 1000 == 0 {
                        thread::yield_now();
                    }
                }
                Err(_) => {
                    thread::yield_now();
                }
            }
        }
        
        let consumer_duration = consumer_start.elapsed();
        (received, consumer_duration, peak_capacity)
    });
    
    // Wait for producers
    for handle in producer_handles {
        handle.join().unwrap();
    }
    
    // Give consumer a moment to catch up
    thread::sleep(Duration::from_millis(100));
    
    // Wait for consumer
    let (received, consumer_duration, peak_capacity) = consumer_handle.join().unwrap();
    let total_duration = start_time.elapsed();
    
    // Collect final statistics
    let sent = messages_sent.load(Ordering::Relaxed);
    let failed = messages_failed.load(Ordering::Relaxed);
    
    BenchmarkResult {
        throughput: sent as f64 / total_duration.as_secs_f64(),
        success_rate: (sent as f64 / total_expected as f64) * 100.0,
        consumer_throughput: received as f64 / consumer_duration.as_secs_f64(),
        total_duration,
        queue_peak_capacity: peak_capacity,
        messages_sent: sent,
        messages_received: received,
        messages_failed: failed,
    }
}

fn print_summary_analysis(results: &[(usize, BenchmarkResult)]) {
    println!("📈 SCALABILITY ANALYSIS SUMMARY");
    println!("===============================\n");
    
    // Performance table
    println!("📊 Performance by Producer Count:");
    println!("   Producers │ Throughput │ Success │ Peak Capacity │ Duration");
    println!("   ──────────┼────────────┼─────────┼───────────────┼──────────");
    
    for (producers, result) in results {
        println!("   {:8} │ {:7.0} k/s │ {:6.1}% │ {:10} │ {:7.2}s",
                producers, 
                result.throughput / 1000.0,
                result.success_rate,
                result.queue_peak_capacity,
                result.total_duration.as_secs_f64());
    }
    
    println!();
    
    // Find optimal points
    let peak_throughput = results.iter()
        .max_by(|a, b| a.1.throughput.partial_cmp(&b.1.throughput).unwrap())
        .unwrap();
    
    let best_efficiency = results.iter()
        .max_by(|a, b| (a.1.throughput / a.0 as f64).partial_cmp(&(b.1.throughput / b.0 as f64)).unwrap())
        .unwrap();
    
    // Scalability characteristics
    println!("🎯 Key Findings:");
    println!("   Peak Throughput: {:.0} msg/sec with {} producers", 
             peak_throughput.1.throughput, peak_throughput.0);
    println!("   Best Efficiency: {:.0} msg/sec/producer with {} producers", 
             best_efficiency.1.throughput / best_efficiency.0 as f64, best_efficiency.0);
    
    // Calculate scalability metrics
    let single_producer_throughput = results[0].1.throughput;
    let max_producer_throughput = results.last().unwrap().1.throughput;
    let scalability_factor = max_producer_throughput / single_producer_throughput;
    
    println!("   Scalability Factor: {:.2}x (20 producers vs 1 producer)", scalability_factor);
    
    // Performance trends
    let mut increasing_trend = 0;
    let mut decreasing_trend = 0;
    
    for i in 1..results.len() {
        if results[i].1.throughput > results[i-1].1.throughput {
            increasing_trend += 1;
        } else {
            decreasing_trend += 1;
        }
    }
    
    println!("   Performance Trend: {} increases, {} decreases", increasing_trend, decreasing_trend);
    
    // Memory efficiency
    let avg_peak_capacity: f64 = results.iter().map(|(_, r)| r.queue_peak_capacity as f64).sum::<f64>() / results.len() as f64;
    println!("   Average Peak Capacity: {:.0} messages", avg_peak_capacity);
    
    // Success rate analysis
    let high_success_rate_count = results.iter().filter(|(_, r)| r.success_rate > 95.0).count();
    println!("   High Success Rate (>95%): {}/20 configurations", high_success_rate_count);
    
    println!();
    
    // Recommendations
    println!("💡 Recommendations:");
    
    if peak_throughput.0 <= 4 {
        println!("   ✅ MPSC pattern optimal for ≤4 producers");
    } else {
        println!("   ⚠️  Consider alternative patterns for >4 producers");
    }
    
    if scalability_factor > 1.5 {
        println!("   ✅ Good scalability characteristics ({}x improvement)", scalability_factor);
    } else {
        println!("   ⚠️  Limited scalability beyond single producer");
    }
    
    if high_success_rate_count >= 15 {
        println!("   ✅ Reliable performance across producer counts");
    } else {
        println!("   ⚠️  Message loss increases with high producer counts");
    }
    
    // Use case recommendations
    println!("\n🎯 Use Case Recommendations:");
    println!("   1-2 Producers:  Optimal for real-time systems");
    println!("   3-8 Producers:  Good for MQTT proxy scenarios"); 
    println!("   9-16 Producers: Moderate performance, monitor success rates");
    println!("   17+ Producers:  Consider sharding or alternative patterns");
}