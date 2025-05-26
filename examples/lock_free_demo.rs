use elasticq::{Config, LockFreeMPSCQueue};
use std::sync::Arc;
use std::thread;
use std::time::Instant;

fn main() {
    println!("🚀 Lock-Free MPSC Queue Demo");
    println!("============================\n");
    
    // Configuration for MQTT proxy use case
    let config = Config::default()
        .with_initial_capacity(1024)
        .with_min_capacity(512)
        .with_max_capacity(1048576)
        .with_growth_factor(2.0);
    
    let queue = Arc::new(LockFreeMPSCQueue::new(config).unwrap());
    
    println!("📊 Initial Queue Stats:");
    println!("   Capacity: {}", queue.capacity());
    println!("   Size: {}", queue.len());
    println!();
    
    // Demo 1: Basic operations
    basic_operations_demo(&queue);
    
    // Demo 2: MPSC concurrent access
    mpsc_demo(&queue);
    
    // Demo 3: Performance characteristics
    performance_demo(&queue);
    
    println!("✅ Demo completed successfully!");
}

fn basic_operations_demo(queue: &Arc<LockFreeMPSCQueue<i32>>) {
    println!("🔧 Demo 1: Basic Operations");
    println!("---------------------------");
    
    // Enqueue some messages
    for i in 0..10 {
        queue.try_enqueue(i).unwrap();
    }
    
    println!("   Enqueued 10 messages");
    println!("   Queue size: {}", queue.len());
    
    // Dequeue some messages
    let mut dequeued = vec![];
    for _ in 0..5 {
        if let Ok(Some(msg)) = queue.try_dequeue() {
            dequeued.push(msg);
        }
    }
    
    println!("   Dequeued: {:?}", dequeued);
    println!("   Queue size: {}", queue.len());
    println!();
}

fn mpsc_demo(queue: &Arc<LockFreeMPSCQueue<i32>>) {
    println!("🔄 Demo 2: MPSC Concurrent Access");
    println!("----------------------------------");
    
    let num_producers = 4;
    let messages_per_producer = 1000;
    let total_messages = num_producers * messages_per_producer;
    
    let start_time = Instant::now();
    
    // Spawn producer threads
    let mut producer_handles = vec![];
    for producer_id in 0..num_producers {
        let queue_clone = Arc::clone(queue);
        let handle = thread::spawn(move || {
            let mut enqueued = 0;
            for i in 0..messages_per_producer {
                let message = producer_id * 1_000_000 + i;
                while queue_clone.try_enqueue(message).is_err() {
                    thread::yield_now();
                }
                enqueued += 1;
            }
            println!("   Producer {} enqueued {} messages", producer_id, enqueued);
        });
        producer_handles.push(handle);
    }
    
    // Consumer thread
    let queue_clone = Arc::clone(queue);
    let consumer_handle = thread::spawn(move || {
        let mut received = 0;
        let mut messages = vec![];
        
        while received < total_messages {
            match queue_clone.try_dequeue() {
                Ok(Some(message)) => {
                    messages.push(message);
                    received += 1;
                    if received % 1000 == 0 {
                        println!("   Consumer received {} messages", received);
                    }
                }
                Ok(None) => thread::yield_now(),
                Err(_) => thread::yield_now(),
            }
        }
        messages
    });
    
    // Wait for all producers
    for handle in producer_handles {
        handle.join().unwrap();
    }
    
    // Wait for consumer
    let received_messages = consumer_handle.join().unwrap();
    let duration = start_time.elapsed();
    
    println!("   Total messages processed: {}", received_messages.len());
    println!("   Time taken: {:?}", duration);
    println!("   Throughput: {:.0} msg/sec", total_messages as f64 / duration.as_secs_f64());
    
    let stats = queue.stats();
    println!("   Final stats: {:?}", stats);
    println!();
}

fn performance_demo(queue: &Arc<LockFreeMPSCQueue<i32>>) {
    println!("⚡ Demo 3: Performance Characteristics");
    println!("-------------------------------------");
    
    // Test resize behavior
    let initial_capacity = queue.capacity();
    println!("   Initial capacity: {}", initial_capacity);
    
    // Fill beyond capacity to trigger resize
    let fill_count = initial_capacity * 3;
    let start_time = Instant::now();
    
    for i in 0..fill_count {
        while queue.try_enqueue(i as i32).is_err() {
            thread::yield_now();
        }
    }
    
    let fill_duration = start_time.elapsed();
    println!("   Filled {} items in {:?}", fill_count, fill_duration);
    println!("   New capacity: {}", queue.capacity());
    println!("   Queue size: {}", queue.len());
    
    // Drain most items to potentially trigger shrink
    let drain_count = fill_count - 100;
    let start_time = Instant::now();
    
    for _ in 0..drain_count {
        while let Ok(None) = queue.try_dequeue() {
            thread::yield_now();
        }
    }
    
    let drain_duration = start_time.elapsed();
    println!("   Drained {} items in {:?}", drain_count, drain_duration);
    println!("   Final capacity: {}", queue.capacity());
    println!("   Final queue size: {}", queue.len());
    
    let final_stats = queue.stats();
    println!("   Performance stats: {:?}", final_stats);
    println!();
}