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
                // Buffer is full
                if buffer.capacity >= self.config.max_capacity {
                    // At max capacity, drop message
                    self.messages_dropped.fetch_add(1, Ordering::Relaxed);
                    return Err(BufferError::MaxCapacityReached(self.config.max_capacity));
                } else {
                    // Could potentially resize, but that's consumer's responsibility
                    self.messages_dropped.fetch_add(1, Ordering::Relaxed);
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
                    unsafe {
                        buffer.write(head_pos, item);
                    }
                    
                    self.messages_enqueued.fetch_add(1, Ordering::Relaxed);
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
            
            // Read the item before advancing tail
            let item = unsafe { buffer.read(tail_pos) };
            
            // Advance tail position
            if self.tail.compare_exchange_weak(
                tail_pos,
                tail_pos + 1,
                Ordering::Release,
                Ordering::Relaxed,
            ).is_ok() {
                self.messages_dequeued.fetch_add(1, Ordering::Relaxed);
                
                // Check if resize is needed (consumer-driven)
                self.try_resize_if_needed(guard)?;
                
                return Ok(Some(item));
            }
            
            // CAS failed, but we already read the item, so we need to put it back
            // This shouldn't happen in SPSC, but handle gracefully
            unsafe {
                buffer.write(tail_pos, item);
            }
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
        if current_size * 2 > buffer.capacity && buffer.capacity < self.config.max_capacity {
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
        
        // Copy existing items to new buffer
        let head_packed = self.head.load(Ordering::Acquire);
        let (head_pos, _head_gen) = Self::unpack_head(head_packed);
        let tail_pos = self.tail.load(Ordering::Acquire);
        
        for i in tail_pos..head_pos {
            let item = unsafe { current_buffer.read(i) };
            unsafe { new_buffer.write(i, item); }
        }
        
        // Increment generation for ABA protection
        let new_generation = self.generation.fetch_add(1, Ordering::AcqRel) + 1;
        
        // Update head with new generation
        let new_head_packed = Self::pack_head(head_pos, new_generation);
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
        QueueStats {
            messages_enqueued: self.messages_enqueued.load(Ordering::Relaxed),
            messages_dequeued: self.messages_dequeued.load(Ordering::Relaxed),
            messages_dropped: self.messages_dropped.load(Ordering::Relaxed),
            current_size: self.len(),
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
        
        // Fill initial capacity
        queue.try_enqueue(1).unwrap();
        queue.try_enqueue(2).unwrap();
        
        // Should trigger resize on next enqueue
        queue.try_enqueue(3).unwrap();
        queue.try_enqueue(4).unwrap();
        
        // Now at max capacity, should start dropping
        assert!(matches!(
            queue.try_enqueue(5),
            Err(BufferError::Full) | Err(BufferError::MaxCapacityReached(_))
        ));
    }
    
    #[test]
    fn test_mpsc_concurrent_access() {
        let queue = Arc::new(LockFreeMPSCQueue::new(test_config()).unwrap());
        let num_producers = 4;
        let messages_per_producer = 100;
        
        // Spawn producer threads
        let mut handles = vec![];
        for producer_id in 0..num_producers {
            let queue_clone = Arc::clone(&queue);
            let handle = thread::spawn(move || {
                for i in 0..messages_per_producer {
                    let message = producer_id * 1000 + i;
                    while queue_clone.try_enqueue(message).is_err() {
                        thread::yield_now();
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
            
            while received.len() < expected_total {
                if let Ok(Some(message)) = queue_clone.try_dequeue() {
                    received.push(message);
                } else {
                    thread::yield_now();
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
        
        // Verify all messages received
        assert_eq!(received.len(), num_producers * messages_per_producer);
        
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
        
        // Fill beyond initial capacity to trigger resize
        for i in 0..8 {
            queue.try_enqueue(i).unwrap();
        }
        
        // Consume some items to trigger resize check
        for _ in 0..4 {
            queue.try_dequeue().unwrap();
        }
        
        // Capacity should have grown
        assert!(queue.capacity() > 4);
        
        let stats = queue.stats();
        assert_eq!(stats.messages_enqueued, 8);
        assert_eq!(stats.messages_dequeued, 4);
    }
}