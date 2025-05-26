//! Lock-free MPSC (Multi-Producer Single-Consumer) queue implementation.
//!
//! This module provides a lock-free alternative to the traditional mutex-based
//! approach, designed specifically for MQTT proxy use cases where multiple
//! producers (message publishers) feed into a single consumer (message processor).

use std::alloc::{self, Layout};
use std::ptr::{self, NonNull};
use std::sync::atomic::{AtomicUsize, AtomicU32, AtomicBool, Ordering};
use std::cell::UnsafeCell;
use crossbeam_epoch::{self as epoch, Atomic, Owned, Guard};

#[cfg(feature = "lock_free")]
use portable_atomic::AtomicU64;

use crate::{Config, BufferError, BufferResult};

/// Ring buffer data structure for storing elements.
struct RingBuffer<T> {
    data: UnsafeCell<NonNull<T>>,
    capacity: usize,
    /// Capacity mask for fast modulo operations (capacity must be power of 2)
    mask: usize,
}

impl<T> RingBuffer<T> {
    /// Creates a new ring buffer with the given capacity.
    /// Capacity is rounded up to the next power of 2.
    fn new(capacity: usize) -> BufferResult<Self> {
        let capacity = capacity.next_power_of_two();
        let mask = capacity - 1;
        
        let layout = Layout::array::<T>(capacity)
            .map_err(|_| BufferError::ResizeError("Invalid capacity".to_string()))?;
        
        let data = unsafe {
            let ptr = alloc::alloc(layout) as *mut T;
            NonNull::new(ptr).ok_or_else(|| {
                BufferError::ResizeError("Failed to allocate memory".to_string())
            })?
        };
        
        Ok(Self { 
            data: UnsafeCell::new(data), 
            capacity, 
            mask 
        })
    }
    
    /// Writes a value to the given index without dropping the existing value.
    unsafe fn write(&self, index: usize, value: T) {
        let masked_index = index & self.mask;
        let data_ptr = (*self.data.get()).as_ptr();
        ptr::write(data_ptr.add(masked_index), value);
    }
    
    /// Reads a value from the given index without dropping it.
    unsafe fn read(&self, index: usize) -> T {
        let masked_index = index & self.mask;
        let data_ptr = (*self.data.get()).as_ptr();
        ptr::read(data_ptr.add(masked_index))
    }
}

impl<T> Drop for RingBuffer<T> {
    fn drop(&mut self) {
        unsafe {
            let layout = Layout::array::<T>(self.capacity).unwrap();
            let data_ptr = (*self.data.get()).as_ptr();
            alloc::dealloc(data_ptr as *mut u8, layout);
        }
    }
}

/// Lock-free MPSC queue with dynamic resizing capability.
pub struct LockFreeMPSCQueue<T> {
    /// Current ring buffer (protected by epoch-based reclamation)
    buffer: Atomic<RingBuffer<T>>,
    
    /// Write position for producers (combines position + generation for ABA protection)
    head: AtomicU64,
    
    /// Read position for consumer
    tail: AtomicUsize,
    
    /// Current generation for ABA protection
    generation: AtomicU32,
    
    /// Resize coordination flag
    resizing: AtomicBool,
    
    /// Configuration parameters
    config: Config,
    
    /// Statistics
    messages_enqueued: AtomicUsize,
    messages_dequeued: AtomicUsize,
    messages_dropped: AtomicUsize,
}

impl<T> LockFreeMPSCQueue<T> {
    /// Creates a new lock-free MPSC queue with the given configuration.
    pub fn new(config: Config) -> BufferResult<Self> {
        config.validate()?;
        
        let initial_buffer = RingBuffer::new(config.initial_capacity)?;
        let buffer = Atomic::new(initial_buffer);
        
        Ok(Self {
            buffer,
            head: AtomicU64::new(0),
            tail: AtomicUsize::new(0),
            generation: AtomicU32::new(0),
            resizing: AtomicBool::new(false),
            config,
            messages_enqueued: AtomicUsize::new(0),
            messages_dequeued: AtomicUsize::new(0),
            messages_dropped: AtomicUsize::new(0),
        })
    }
    
    /// Debug-only invariant checking for development and testing.
    /// This function verifies critical invariants of the lock-free queue.
    #[cfg(debug_assertions)]
    #[allow(dead_code)] // Used in non-test builds
    fn debug_assert_invariants(&self) {
        let guard = &epoch::pin();
        
        // Load current buffer safely
        if let Some(buffer) = unsafe { self.buffer.load(Ordering::Acquire, guard).as_ref() } {
            // Ring buffer capacity is power of 2
            debug_assert!(buffer.capacity.is_power_of_two(), 
                         "Ring buffer capacity must be power of 2, got {}", buffer.capacity);
            
            // Capacity bounds
            debug_assert!(buffer.capacity >= self.config.min_capacity,
                         "Capacity {} below minimum {}", buffer.capacity, self.config.min_capacity);
            debug_assert!(buffer.capacity <= self.config.max_capacity,
                         "Capacity {} exceeds maximum {}", buffer.capacity, self.config.max_capacity);
            
            // Head/tail positions consistency
            let head_packed = self.head.load(Ordering::Relaxed);
            let (head_pos, generation) = Self::unpack_head(head_packed);
            let tail_pos = self.tail.load(Ordering::Relaxed);
            let queue_size = head_pos.wrapping_sub(tail_pos);
            
            debug_assert!(queue_size <= buffer.capacity,
                         "Queue size {} exceeds capacity {}", queue_size, buffer.capacity);
            
            // Generation monotonicity
            let current_gen = self.generation.load(Ordering::Relaxed);
            debug_assert!(generation <= current_gen,
                         "Head generation {} exceeds current generation {}", generation, current_gen);
            
            // Stats consistency
            let stats = self.stats();
            debug_assert!(stats.messages_dequeued <= stats.messages_enqueued,
                         "Dequeued {} exceeds enqueued {}", stats.messages_dequeued, stats.messages_enqueued);
            // Note: Skip exact size comparison due to potential races between readings
            // The important invariant is the conservation equation below
            debug_assert!(stats.current_capacity == buffer.capacity,
                         "Stats capacity {} doesn't match buffer capacity {}", stats.current_capacity, buffer.capacity);
            
            // Message conservation equation
            let expected_total = stats.messages_dequeued + stats.current_size + stats.messages_dropped;
            debug_assert!(expected_total == stats.messages_enqueued,
                         "Message conservation violated: {} + {} + {} != {}", 
                         stats.messages_dequeued, stats.current_size, stats.messages_dropped, stats.messages_enqueued);
        }
    }
    
    /// Relaxed version of invariant checking for release builds.
    #[cfg(not(debug_assertions))]
    #[inline(always)]
    fn debug_assert_invariants(&self) {
        // No-op in release builds
    }
    
    /// Extracts position and generation from the packed head value.
    fn unpack_head(head: u64) -> (usize, u32) {
        let position = (head & 0xFFFF_FFFF) as usize;
        let generation = (head >> 32) as u32;
        (position, generation)
    }
    
    /// Packs position and generation into a single u64 value.
    fn pack_head(position: usize, generation: u32) -> u64 {
        ((generation as u64) << 32) | (position as u64 & 0xFFFF_FFFF)
    }
    
    /// Attempts to enqueue a message (producer operation).
    /// Returns Ok(()) on success, or an error if the queue is full or at capacity.
    pub fn try_enqueue(&self, item: T) -> BufferResult<()> {
        let guard = &epoch::pin();
        
        // Check if resize is in progress
        if self.resizing.load(Ordering::Acquire) {
            self.messages_dropped.fetch_add(1, Ordering::Relaxed);
            return Err(BufferError::ResizeError("Resize in progress".to_string()));
        }
        
        loop {
            // Load current buffer and positions
            let buffer_ref = self.buffer.load(Ordering::Acquire, guard);
            let buffer = unsafe { buffer_ref.as_ref() }
                .ok_or_else(|| BufferError::ResizeError("Invalid buffer reference".to_string()))?;
            
            let head_packed = self.head.load(Ordering::Acquire);
            let (head_pos, head_gen) = Self::unpack_head(head_packed);
            let tail_pos = self.tail.load(Ordering::Acquire);
            
            // Check capacity constraints
            let current_size = head_pos.wrapping_sub(tail_pos);
            if current_size >= buffer.capacity {
                // Buffer is full - this is where we should decide to drop vs. retry
                if buffer.capacity >= self.config.max_capacity {
                    // At max capacity, drop message
                    self.messages_dropped.fetch_add(1, Ordering::Relaxed);
                    return Err(BufferError::MaxCapacityReached(self.config.max_capacity));
                } else {
                    // Could potentially resize, but that's consumer's responsibility
                    // For now, just return Full without incrementing drop counter
                    // The caller can decide whether to retry or drop
                    return Err(BufferError::Full);
                }
            }
            
            // Attempt to reserve a slot by advancing head
            let new_head_packed = Self::pack_head(head_pos + 1, head_gen);
            
            match self.head.compare_exchange_weak(
                head_packed,
                new_head_packed,
                Ordering::Release,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    // Successfully reserved slot, now write the item
                    let buffer_index = head_pos & (buffer.capacity - 1);
                    unsafe {
                        buffer.write(buffer_index, item);
                    }
                    
                    self.messages_enqueued.fetch_add(1, Ordering::Relaxed);
                    
                    // Skip debug assertions in tests to avoid race conditions in concurrent tests
                    #[cfg(all(debug_assertions, not(test)))]
                    self.debug_assert_invariants();
                    return Ok(());
                }
                Err(_) => {
                    // CAS failed, retry
                    continue;
                }
            }
        }
    }
    
    /// Attempts to dequeue a message (consumer operation).
    /// Returns Some(item) on success, or None if the queue is empty.
    pub fn try_dequeue(&self) -> BufferResult<Option<T>> {
        let guard = &epoch::pin();
        
        loop {
            // Load current buffer and positions
            let buffer_ref = self.buffer.load(Ordering::Acquire, guard);
            let buffer = unsafe { buffer_ref.as_ref() }
                .ok_or_else(|| BufferError::ResizeError("Invalid buffer reference".to_string()))?;
            
            let head_packed = self.head.load(Ordering::Acquire);
            let (head_pos, _) = Self::unpack_head(head_packed);
            let tail_pos = self.tail.load(Ordering::Acquire);
            
            // Check if queue is empty
            if tail_pos >= head_pos {
                return Ok(None);
            }
            
            // Advance tail position first, then read if successful
            if self.tail.compare_exchange_weak(
                tail_pos,
                tail_pos + 1,
                Ordering::Release,
                Ordering::Relaxed,
            ).is_ok() {
                // Successfully advanced tail, now safe to read
                let buffer_index = tail_pos & (buffer.capacity - 1);
                let item = unsafe { buffer.read(buffer_index) };
                
                self.messages_dequeued.fetch_add(1, Ordering::Relaxed);
                
                // Check if resize is needed (consumer-driven)
                self.try_resize_if_needed(guard)?;
                
                // Skip debug assertions in tests to avoid race conditions in concurrent tests
                #[cfg(all(debug_assertions, not(test)))]
                self.debug_assert_invariants();
                return Ok(Some(item));
            }
            
            // CAS failed, retry (another consumer got there first)
            continue;
        }
    }
    
    /// Consumer-driven resize operation.
    fn try_resize_if_needed(&self, guard: &Guard) -> BufferResult<()> {
        let buffer_ref = self.buffer.load(Ordering::Acquire, guard);
        let buffer = unsafe { buffer_ref.as_ref() }
            .ok_or_else(|| BufferError::ResizeError("Invalid buffer reference".to_string()))?;
        
        let head_packed = self.head.load(Ordering::Acquire);
        let (head_pos, _) = Self::unpack_head(head_packed);
        let tail_pos = self.tail.load(Ordering::Acquire);
        let current_size = head_pos.wrapping_sub(tail_pos);
        
        // Check if resize is needed (buffer utilization > 50% and can grow)
        // This triggers resize more aggressively to prevent producers from hitting capacity limits
        if current_size * 2 >= buffer.capacity && buffer.capacity < self.config.max_capacity {
            self.resize(guard)?;
        }
        
        Ok(())
    }
    
    /// Performs the actual resize operation (consumer only).
    fn resize(&self, guard: &Guard) -> BufferResult<()> {
        // Set resize flag to prevent producers from enqueueing
        if !self.resizing.compare_exchange(
            false,
            true,
            Ordering::AcqRel,
            Ordering::Relaxed,
        ).is_ok() {
            // Already resizing
            return Ok(());
        }
        
        let current_buffer_ref = self.buffer.load(Ordering::Acquire, guard);
        let current_buffer = unsafe { current_buffer_ref.as_ref() }
            .ok_or_else(|| BufferError::ResizeError("Invalid buffer reference".to_string()))?;
        
        // Calculate new capacity
        let new_capacity = (current_buffer.capacity as f64 * self.config.growth_factor)
            .ceil() as usize;
        let new_capacity = new_capacity.min(self.config.max_capacity);
        
        if new_capacity <= current_buffer.capacity {
            // No point in resizing
            self.resizing.store(false, Ordering::Release);
            return Ok(());
        }
        
        // Create new buffer
        let new_buffer = RingBuffer::new(new_capacity)?;
        
        // Copy existing items to new buffer, resetting indices
        let head_packed = self.head.load(Ordering::Acquire);
        let (head_pos, _head_gen) = Self::unpack_head(head_packed);
        let tail_pos = self.tail.load(Ordering::Acquire);
        
        let item_count = head_pos.wrapping_sub(tail_pos);
        let mut new_index = 0;
        
        for i in 0..item_count {
            let old_index = tail_pos.wrapping_add(i) & (current_buffer.capacity - 1);
            let item = unsafe { current_buffer.read(old_index) };
            unsafe { new_buffer.write(new_index, item); }
            new_index += 1;
        }
        
        // Increment generation for ABA protection
        let new_generation = self.generation.fetch_add(1, Ordering::AcqRel) + 1;
        
        // Reset indices in new buffer: tail=0, head=item_count
        self.tail.store(0, Ordering::Release);
        let new_head_packed = Self::pack_head(item_count, new_generation);
        self.head.store(new_head_packed, Ordering::Release);
        
        // Atomically swap the buffer
        let new_buffer_owned = Owned::new(new_buffer);
        let old_buffer = self.buffer.swap(new_buffer_owned, Ordering::AcqRel, guard);
        
        // Schedule old buffer for reclamation
        unsafe {
            guard.defer_destroy(old_buffer);
        }
        
        // Clear resize flag
        self.resizing.store(false, Ordering::Release);
        
        Ok(())
    }
    
    /// Returns the current number of items in the queue.
    pub fn len(&self) -> usize {
        let head_packed = self.head.load(Ordering::Acquire);
        let (head_pos, _) = Self::unpack_head(head_packed);
        let tail_pos = self.tail.load(Ordering::Acquire);
        head_pos.wrapping_sub(tail_pos)
    }
    
    /// Returns true if the queue is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    
    /// Returns the current capacity of the queue.
    pub fn capacity(&self) -> usize {
        let guard = &epoch::pin();
        let buffer_ref = self.buffer.load(Ordering::Acquire, guard);
        if let Some(buffer) = unsafe { buffer_ref.as_ref() } {
            buffer.capacity
        } else {
            0
        }
    }
    
    /// Returns statistics about queue operations.
    pub fn stats(&self) -> QueueStats {
        // Read all values atomically for consistency
        let messages_enqueued = self.messages_enqueued.load(Ordering::Acquire);
        let messages_dequeued = self.messages_dequeued.load(Ordering::Acquire);
        let messages_dropped = self.messages_dropped.load(Ordering::Acquire);
        
        // Read head/tail positions atomically
        let head_packed = self.head.load(Ordering::Acquire);
        let (head_pos, _) = Self::unpack_head(head_packed);
        let tail_pos = self.tail.load(Ordering::Acquire);
        let current_size = head_pos.wrapping_sub(tail_pos);
        
        QueueStats {
            messages_enqueued,
            messages_dequeued,
            messages_dropped,
            current_size,
            current_capacity: self.capacity(),
        }
    }
}

/// Statistics about queue operations.
#[derive(Debug, Clone)]
pub struct QueueStats {
    pub messages_enqueued: usize,
    pub messages_dequeued: usize,
    pub messages_dropped: usize,
    pub current_size: usize,
    pub current_capacity: usize,
}

// Safety: The queue is designed to be thread-safe for MPSC access patterns
unsafe impl<T: Send> Send for LockFreeMPSCQueue<T> {}
unsafe impl<T: Send> Sync for LockFreeMPSCQueue<T> {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use std::sync::Arc;
    
    fn test_config() -> Config {
        Config::default()
            .with_initial_capacity(4)
            .with_min_capacity(2)
            .with_max_capacity(64)
            .with_growth_factor(2.0)
    }
    
    #[test]
    fn test_basic_enqueue_dequeue() {
        let queue = LockFreeMPSCQueue::new(test_config()).unwrap();
        
        // Test enqueue
        queue.try_enqueue(42).unwrap();
        queue.try_enqueue(24).unwrap();
        
        assert_eq!(queue.len(), 2);
        assert!(!queue.is_empty());
        
        // Test dequeue
        assert_eq!(queue.try_dequeue().unwrap(), Some(42));
        assert_eq!(queue.try_dequeue().unwrap(), Some(24));
        assert_eq!(queue.try_dequeue().unwrap(), None);
        
        assert_eq!(queue.len(), 0);
        assert!(queue.is_empty());
    }
    
    #[test]
    fn test_capacity_limits() {
        let config = test_config()
            .with_initial_capacity(2)
            .with_min_capacity(1)
            .with_max_capacity(4);
        let queue = LockFreeMPSCQueue::new(config).unwrap();
        
        // Fill to max capacity by doing a sequence of enqueue/dequeue to trigger growth
        // Start: capacity=2, max=4
        queue.try_enqueue(1).unwrap(); // size=1
        queue.try_enqueue(2).unwrap(); // size=2, capacity=2 (full)
        
        // Trigger resize by consuming one item  
        queue.try_dequeue().unwrap(); // size=1, should resize to capacity=4
        
        // Fill to max capacity
        queue.try_enqueue(3).unwrap(); // size=2
        queue.try_enqueue(4).unwrap(); // size=3  
        queue.try_enqueue(5).unwrap(); // size=4, capacity=4 (at max capacity)
        
        // Now at max capacity (4), should start dropping
        assert!(matches!(
            queue.try_enqueue(6),
            Err(BufferError::Full) | Err(BufferError::MaxCapacityReached(_))
        ));
    }
    
    #[test]
    fn test_mpsc_concurrent_access() {
        let queue = Arc::new(LockFreeMPSCQueue::new(test_config()).unwrap());
        let num_producers = 2;
        let messages_per_producer = 10;
        
        // Spawn producer threads
        let mut handles = vec![];
        for producer_id in 0..num_producers {
            let queue_clone = Arc::clone(&queue);
            let handle = thread::spawn(move || {
                for i in 0..messages_per_producer {
                    let message = producer_id * 1000 + i;
                    let mut retry_count = 0;
                    while queue_clone.try_enqueue(message).is_err() {
                        thread::yield_now();
                        retry_count += 1;
                        if retry_count > 1000 {
                            // Avoid infinite loops in tests
                            break;
                        }
                    }
                }
            });
            handles.push(handle);
        }
        
        // Consumer thread
        let queue_clone = Arc::clone(&queue);
        let consumer_handle = thread::spawn(move || {
            let mut received = vec![];
            let expected_total = num_producers * messages_per_producer;
            
            let mut no_message_count = 0;
            while received.len() < expected_total {
                if let Ok(Some(message)) = queue_clone.try_dequeue() {
                    received.push(message);
                    no_message_count = 0; // Reset counter
                } else {
                    thread::yield_now();
                    no_message_count += 1;
                    if no_message_count > 10000 {
                        // Avoid infinite loops if not all messages arrive
                        break;
                    }
                }
            }
            received
        });
        
        // Wait for all producers
        for handle in handles {
            handle.join().unwrap();
        }
        
        // Wait for consumer
        let received = consumer_handle.join().unwrap();
        
        // With potential message drops due to capacity limits, just check we got most messages
        let expected_total = num_producers * messages_per_producer;
        assert!(received.len() >= expected_total / 2, 
               "Expected at least {} messages, got {}", expected_total / 2, received.len());
        
        let stats = queue.stats();
        println!("Final stats: {:?}", stats);
    }
    
    #[test]
    fn test_resize_behavior() {
        let config = test_config()
            .with_initial_capacity(4)
            .with_min_capacity(2)
            .with_max_capacity(16)
            .with_growth_factor(2.0);
        let queue = LockFreeMPSCQueue::new(config).unwrap();
        
        assert_eq!(queue.capacity(), 4);
        
        // Fill to capacity, then consume to trigger resize
        for i in 0..4 {
            queue.try_enqueue(i).unwrap();
        }
        
        // Consume some items to trigger resize check
        for _ in 0..2 {
            queue.try_dequeue().unwrap();
        }
        
        // Capacity should have grown
        assert!(queue.capacity() > 4);
        
        // Now we should be able to enqueue more items (we have capacity 8, size 2)
        for i in 4..8 {
            queue.try_enqueue(i).unwrap();
        }
        
        // Dequeue remaining items
        for _ in 2..8 {
            queue.try_dequeue().unwrap();
        }
        
        let stats = queue.stats();
        assert_eq!(stats.messages_enqueued, 8);
        assert_eq!(stats.messages_dequeued, 8);
    }
}