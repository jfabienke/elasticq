//! Property-based tests for ElasticQ implementations
//!
//! These tests use the proptest crate to verify properties hold across
//! large ranges of inputs and operation sequences, providing much stronger
//! correctness guarantees than unit tests alone.

use proptest::prelude::*;
use elasticq::{Config, BufferError};
use std::sync::Arc;
use std::thread;
use std::collections::HashMap;

#[cfg(feature = "lock_free")]
use elasticq::LockFreeMPSCQueue;
use elasticq::DynamicCircularBuffer;

// ============================================================================
// Property-Based Test Strategies
// ============================================================================

// Generate valid configurations
fn config_strategy() -> impl Strategy<Value = Config> {
    (
        1..1024usize,   // min_capacity
        1..2048usize,   // initial_capacity  
        1..4096usize,   // max_capacity
        1.01..10.0f64,  // growth_factor
        0.01..0.99f64,  // shrink_threshold
    ).prop_filter_map("valid config", |(min_cap, init_cap, max_cap, growth, shrink)| {
        if min_cap <= init_cap && init_cap <= max_cap {
            Some(Config::default()
                .with_min_capacity(min_cap)
                .with_initial_capacity(init_cap)
                .with_max_capacity(max_cap)
                .with_growth_factor(growth)
                .with_shrink_threshold(shrink))
        } else {
            None
        }
    })
}

// Generate sequences of operations
#[derive(Clone, Debug)]
enum Operation {
    Enqueue(i32),
    Dequeue,
    BatchEnqueue(Vec<i32>),
    BatchDequeue(usize),
}

fn operation_strategy() -> impl Strategy<Value = Operation> {
    prop_oneof![
        any::<i32>().prop_map(Operation::Enqueue),
        Just(Operation::Dequeue),
        prop::collection::vec(any::<i32>(), 1..10).prop_map(Operation::BatchEnqueue),
        (1..20usize).prop_map(Operation::BatchDequeue),
    ]
}

// ============================================================================
// FIFO Ordering Properties
// ============================================================================

proptest! {
    #[test]
    #[cfg(feature = "lock_free")]
    fn prop_lock_free_fifo_ordering_maintained(
        config in config_strategy(),
        operations in prop::collection::vec(
            (0..4usize, any::<i32>()), // (producer_id, message)
            0..100
        )
    ) {
        let queue = LockFreeMPSCQueue::new(config).unwrap();
        let mut expected_order = Vec::new();
        
        // Single-threaded simulation of MPSC behavior
        for (producer_id, message) in operations {
            let tagged_message = (producer_id as i32) * 100000 + message;
            if queue.try_enqueue(tagged_message).is_ok() {
                expected_order.push(tagged_message);
            }
        }
        
        // Dequeue all messages
        let mut actual_order = Vec::new();
        while let Ok(Some(msg)) = queue.try_dequeue() {
            actual_order.push(msg);
        }
        
        // FIFO property: order should be preserved
        prop_assert_eq!(actual_order, expected_order);
    }
    
    #[test]
    fn prop_lock_based_fifo_ordering_maintained(
        config in config_strategy(),
        messages in prop::collection::vec(any::<i32>(), 0..100)
    ) {
        let buffer = DynamicCircularBuffer::new(config).unwrap();
        let mut successfully_pushed = Vec::new();
        
        // Push messages (some might fail due to capacity)
        for message in messages {
            if buffer.push(message).is_ok() {
                successfully_pushed.push(message);
            }
        }
        
        // Pop all messages
        let mut popped_messages = Vec::new();
        while let Ok(msg) = buffer.pop() {
            popped_messages.push(msg);
        }
        
        // FIFO property: order should be preserved
        prop_assert_eq!(popped_messages, successfully_pushed);
    }
}

// ============================================================================
// Message Conservation Properties
// ============================================================================

proptest! {
    #[test]
    #[cfg(feature = "lock_free")]
    fn prop_lock_free_message_conservation(
        config in config_strategy(),
        operations in prop::collection::vec(operation_strategy(), 0..50)
    ) {
        let queue = LockFreeMPSCQueue::new(config).unwrap();
        let mut expected_enqueued = 0usize;
        let mut expected_dequeued = 0usize;
        
        for operation in operations {
            match operation {
                Operation::Enqueue(msg) => {
                    if queue.try_enqueue(msg).is_ok() {
                        expected_enqueued += 1;
                    }
                }
                Operation::Dequeue => {
                    if queue.try_dequeue().unwrap().is_some() {
                        expected_dequeued += 1;
                    }
                }
                Operation::BatchEnqueue(msgs) => {
                    for msg in msgs {
                        if queue.try_enqueue(msg).is_ok() {
                            expected_enqueued += 1;
                        }
                    }
                }
                Operation::BatchDequeue(count) => {
                    for _ in 0..count {
                        if queue.try_dequeue().unwrap().is_some() {
                            expected_dequeued += 1;
                        } else {
                            break;
                        }
                    }
                }
            }
        }
        
        let stats = queue.stats();
        
        // Conservation property: enqueued = dequeued + remaining + dropped
        let conservation_total = stats.messages_dequeued + stats.current_size + stats.messages_dropped;
        prop_assert_eq!(conservation_total, stats.messages_enqueued);
        
        // Our tracking should match stats
        prop_assert_eq!(stats.messages_enqueued, expected_enqueued);
        prop_assert_eq!(stats.messages_dequeued, expected_dequeued);
    }
    
    #[test]
    fn prop_lock_based_message_conservation(
        config in config_strategy(),
        operations in prop::collection::vec(operation_strategy(), 0..50)
    ) {
        let buffer = DynamicCircularBuffer::new(config).unwrap();
        let mut total_pushed = 0usize;
        let mut total_popped = 0usize;
        
        for operation in operations {
            match operation {
                Operation::Enqueue(msg) => {
                    if buffer.push(msg).is_ok() {
                        total_pushed += 1;
                    }
                }
                Operation::Dequeue => {
                    if buffer.pop().is_ok() {
                        total_popped += 1;
                    }
                }
                Operation::BatchEnqueue(msgs) => {
                    if buffer.push_batch(msgs.clone()).is_ok() {
                        total_pushed += msgs.len();
                    }
                }
                Operation::BatchDequeue(count) => {
                    let popped = buffer.pop_batch(count).unwrap_or_default();
                    total_popped += popped.len();
                }
            }
        }
        
        let remaining = buffer.len();
        
        // Conservation property: pushed = popped + remaining
        prop_assert_eq!(total_pushed, total_popped + remaining);
    }
}

// ============================================================================
// Capacity Bound Properties
// ============================================================================

proptest! {
    #[test]
    #[cfg(feature = "lock_free")]
    fn prop_lock_free_bounded_capacity_never_exceeded(
        initial_cap in 1..64usize,
        max_cap in 1..128usize,
        operations in prop::collection::vec(
            any::<bool>(), // true = enqueue, false = dequeue
            0..200
        )
    ) {
        let max_cap = max_cap.max(initial_cap);
        let config = Config::default()
            .with_initial_capacity(initial_cap)
            .with_min_capacity(1)
            .with_max_capacity(max_cap);
        
        let queue = LockFreeMPSCQueue::new(config).unwrap();
        let mut message_counter = 0i32;
        
        for is_enqueue in operations {
            if is_enqueue {
                queue.try_enqueue(message_counter).ok();
                message_counter += 1;
            } else {
                queue.try_dequeue().ok();
            }
            
            // Invariant: capacity never exceeds maximum
            prop_assert!(queue.capacity() <= max_cap);
            
            // Invariant: size never exceeds capacity
            prop_assert!(queue.len() <= queue.capacity());
        }
    }
    
    #[test]
    fn prop_lock_based_bounded_capacity_never_exceeded(
        initial_cap in 1..64usize,
        max_cap in 1..128usize,
        operations in prop::collection::vec(
            any::<bool>(), // true = push, false = pop
            0..200
        )
    ) {
        let max_cap = max_cap.max(initial_cap);
        let config = Config::default()
            .with_initial_capacity(initial_cap)
            .with_min_capacity(1)
            .with_max_capacity(max_cap);
        
        let buffer = DynamicCircularBuffer::new(config).unwrap();
        let mut message_counter = 0i32;
        
        for is_push in operations {
            if is_push {
                buffer.push(message_counter).ok();
                message_counter += 1;
            } else {
                buffer.pop().ok();
            }
            
            // Invariant: capacity never exceeds maximum
            prop_assert!(buffer.capacity() <= max_cap);
            
            // Invariant: size never exceeds capacity
            prop_assert!(buffer.len() <= buffer.capacity());
        }
    }
}

// ============================================================================
// Concurrent Property Tests
// ============================================================================

proptest! {
    #[test]
    #[cfg(feature = "lock_free")]
    fn prop_lock_free_concurrent_mpsc_correctness(
        config in config_strategy(),
        producer_counts in 1..5usize,
        messages_per_producer in 1..50usize
    ) {
        let queue = Arc::new(LockFreeMPSCQueue::new(config).unwrap());
        let total_messages = producer_counts * messages_per_producer;
        
        // Launch producers
        let mut producer_handles = vec![];
        for producer_id in 0..producer_counts {
            let queue_clone = Arc::clone(&queue);
            let handle = thread::spawn(move || {
                let mut sent = 0;
                for i in 0..messages_per_producer {
                    let message = (producer_id as i32) * 10000 + (i as i32);
                    if queue_clone.try_enqueue(message).is_ok() {
                        sent += 1;
                    }
                }
                sent
            });
            producer_handles.push(handle);
        }
        
        // Single consumer
        let queue_clone = Arc::clone(&queue);
        let consumer = thread::spawn(move || {
            let mut received = Vec::new();
            for _ in 0..total_messages * 2 { // Allow extra iterations
                match queue_clone.try_dequeue() {
                    Ok(Some(msg)) => received.push(msg),
                    Ok(None) => thread::yield_now(),
                    Err(_) => thread::yield_now(),
                }
                
                if received.len() == total_messages {
                    break;
                }
            }
            received
        });
        
        // Collect results
        let mut total_sent = 0;
        for handle in producer_handles {
            total_sent += handle.join().unwrap();
        }
        
        let received_messages = consumer.join().unwrap();
        
        // Properties
        prop_assert_eq!(received_messages.len(), total_sent);
        
        // Verify no duplicate messages
        let mut message_counts = HashMap::new();
        for msg in &received_messages {
            *message_counts.entry(*msg).or_insert(0) += 1;
        }
        
        for (msg, count) in message_counts {
            prop_assert_eq!(count, 1, "Duplicate message detected: {}", msg);
        }
        
        // Final conservation check
        let stats = queue.stats();
        let conservation_total = stats.messages_dequeued + stats.current_size + stats.messages_dropped;
        prop_assert_eq!(conservation_total, stats.messages_enqueued);
    }
}

// ============================================================================
// Resize Property Tests
// ============================================================================

proptest! {
    #[test]
    #[cfg(feature = "lock_free")]
    fn prop_lock_free_resize_preserves_data(
        initial_cap in 2..16usize,
        max_cap in 16..64usize,
        fill_ratio in 0.5..2.0f64 // Ratio of initial capacity to fill
    ) {
        let config = Config::default()
            .with_initial_capacity(initial_cap)
            .with_min_capacity(1)
            .with_max_capacity(max_cap);
        
        let queue = LockFreeMPSCQueue::new(config).unwrap();
        let fill_count = (initial_cap as f64 * fill_ratio) as usize;
        
        // Fill queue to trigger potential resize
        let mut enqueued_messages = Vec::new();
        for i in 0..fill_count {
            if queue.try_enqueue(i as i32).is_ok() {
                enqueued_messages.push(i as i32);
            }
        }
        
        // Dequeue all and verify order
        let mut dequeued_messages = Vec::new();
        while let Ok(Some(msg)) = queue.try_dequeue() {
            dequeued_messages.push(msg);
        }
        
        // Property: resize doesn't lose or reorder data
        prop_assert_eq!(dequeued_messages, enqueued_messages);
        
        // Property: queue is empty after full drain
        prop_assert_eq!(queue.try_dequeue().unwrap(), None);
    }
    
    #[test]
    fn prop_lock_based_resize_preserves_data(
        initial_cap in 2..16usize,
        max_cap in 16..64usize,
        fill_ratio in 0.5..2.0f64
    ) {
        let config = Config::default()
            .with_initial_capacity(initial_cap)
            .with_min_capacity(1)
            .with_max_capacity(max_cap);
        
        let buffer = DynamicCircularBuffer::new(config).unwrap();
        let fill_count = (initial_cap as f64 * fill_ratio) as usize;
        
        // Fill buffer to trigger potential resize
        let mut pushed_messages = Vec::new();
        for i in 0..fill_count {
            if buffer.push(i as i32).is_ok() {
                pushed_messages.push(i as i32);
            }
        }
        
        // Pop all and verify order
        let mut popped_messages = Vec::new();
        while let Ok(msg) = buffer.pop() {
            popped_messages.push(msg);
        }
        
        // Property: resize doesn't lose or reorder data
        prop_assert_eq!(popped_messages, pushed_messages);
        
        // Property: buffer is empty after full drain
        prop_assert!(buffer.pop().is_err());
    }
}

// ============================================================================
// Error Handling Properties
// ============================================================================

proptest! {
    #[test]
    fn prop_error_classification_consistency(
        error_type in 0..5u8
    ) {
        let error = match error_type {
            0 => BufferError::Full,
            1 => BufferError::Empty,
            2 => BufferError::MaxCapacityReached(1000),
            3 => BufferError::InvalidConfiguration("test".into()),
            4 => BufferError::ResizeError("test".into()),
            _ => BufferError::Full,
        };
        
        // Properties about error classification
        match error {
            BufferError::Full | BufferError::MaxCapacityReached(_) => {
                prop_assert!(error.is_capacity_error());
                prop_assert!(error.is_retriable());
            }
            BufferError::Empty => {
                prop_assert!(!error.is_capacity_error());
                prop_assert!(!error.is_retriable());
            }
            BufferError::InvalidConfiguration(_) => {
                prop_assert!(!error.is_capacity_error());
                prop_assert!(!error.is_retriable());
            }
            BufferError::ResizeError(_) => {
                // Resize errors are generally retriable
                prop_assert!(error.is_retriable());
            }
            BufferError::Timeout(_) => {
                prop_assert!(error.is_retriable());
            }
            BufferError::InvalidOperation(_) => {
                prop_assert!(!error.is_retriable());
            }
            BufferError::Other(_) => {
                prop_assert!(error.is_retriable());
            }
        }
    }
}

// ============================================================================
// Configuration Property Tests
// ============================================================================

proptest! {
    #[test]
    fn prop_valid_configurations_accepted(
        min_cap in 1..100usize,
        growth_factor in 1.01..5.0f64,
        shrink_threshold in 0.01..0.99f64
    ) {
        let initial_cap = min_cap + 10;
        let max_cap = initial_cap + 50;
        
        let config = Config::default()
            .with_min_capacity(min_cap)
            .with_initial_capacity(initial_cap)
            .with_max_capacity(max_cap)
            .with_growth_factor(growth_factor)
            .with_shrink_threshold(shrink_threshold);
        
        // Property: valid configs should validate successfully
        prop_assert!(config.validate().is_ok());
        
        // Property: valid configs should create working buffers
        prop_assert!(DynamicCircularBuffer::<i32>::new(config.clone()).is_ok());
        
        #[cfg(feature = "lock_free")]
        {
            prop_assert!(LockFreeMPSCQueue::<i32>::new(config).is_ok());
        }
    }
    
    #[test]
    fn prop_invalid_configurations_rejected(
        min_cap in 50..100usize,
        max_cap in 1..50usize // max < min
    ) {
        let config = Config::default()
            .with_min_capacity(min_cap)
            .with_max_capacity(max_cap);
        
        // Property: invalid configs should be rejected
        prop_assert!(config.validate().is_err());
    }
}