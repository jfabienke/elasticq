//! A thread-safe, dynamically resizable circular buffer implementation.
//!
//! This buffer is designed for high-throughput scenarios, particularly
//! suitable for use in MQTT proxy applications where efficient message
//! buffering is critical.

use std::collections::VecDeque;
use std::sync::Arc;

mod config;
pub use config::Config;

mod error;
pub use error::{BufferError, BufferResult};

#[cfg(feature = "lock_free")]
pub mod lock_free;
#[cfg(feature = "lock_free")]
pub use lock_free::{LockFreeMPSCQueue, QueueStats};

#[cfg(not(feature = "async"))]
use parking_lot::{Mutex, RwLock};
#[cfg(feature = "async")]
use tokio::sync::{Mutex, RwLock};

/// A thread-safe wrapper around `DynamicCircularBuffer`.
pub type ThreadSafeDynamicCircularBuffer<T> = Arc<DynamicCircularBuffer<T>>;

/// The main struct representing the dynamic circular buffer.
pub struct DynamicCircularBuffer<T> {
    buffer: Mutex<VecDeque<T>>,
    capacity: RwLock<usize>, // Stores the current *logical* capacity
    push_lock: Mutex<()>,    // Serializes push operations
    pop_lock: Mutex<()>,     // Serializes pop operations
    config: Config,
}

impl<T> DynamicCircularBuffer<T> {
    /// Creates a new `DynamicCircularBuffer` with the given configuration.
    ///
    /// # Examples
    ///
    /// ```
    /// use elasticq::{DynamicCircularBuffer, Config};
    ///
    /// let config = Config::default();
    /// // Specify the type parameter T, for example, as i32
    /// let buffer: DynamicCircularBuffer<i32> = DynamicCircularBuffer::new(config).expect("Failed to create buffer");
    /// // Or, less verbosely, by type hinting on the ::new call if preferred,
    /// // but the above is clearer for a `new` method example.
    /// // let buffer = DynamicCircularBuffer::<i32>::new(config).expect("Failed to create buffer");
    /// ```
    pub fn new(config: Config) -> BufferResult<Self> {
        config.validate()?;
        let initial_logical_capacity = config.initial_capacity;
        Ok(Self {
            buffer: Mutex::new(VecDeque::with_capacity(initial_logical_capacity)),
            capacity: RwLock::new(initial_logical_capacity),
            push_lock: Mutex::new(()),
            pop_lock: Mutex::new(()),
            config,
        })
    }

    /// Internal helper to perform the shrink operation.
    /// Assumes `buffer_ref` (from self.buffer.lock()) and `logical_capacity_ref` (from self.capacity.write())
    /// are already locked and provided by the caller.
    fn _perform_shrink(
        buffer_ref: &mut VecDeque<T>,
        logical_capacity_ref: &mut usize,
        new_logical_capacity: usize,
    ) -> BufferResult<()> {
        buffer_ref.truncate(new_logical_capacity);
        buffer_ref.shrink_to(new_logical_capacity); // shrink_to reduces to max(len, new_cap)
        *logical_capacity_ref = new_logical_capacity;
        Ok(())
    }

    /// Internal helper to perform the resize (grow) operation.
    /// Assumes `buffer_ref` (from self.buffer.lock()) and `logical_capacity_ref` (from self.capacity.write())
    /// are already locked and provided by the caller.
    fn _perform_resize(
        buffer_ref: &mut VecDeque<T>,
        logical_capacity_ref: &mut usize,
        new_logical_capacity: usize,
    ) -> BufferResult<()> {
        // Ensure VecDeque has enough physical capacity for the new logical capacity.
        // Only reserve if the new logical capacity is greater than the current physical capacity.
        let current_physical_capacity = buffer_ref.capacity();
        if new_logical_capacity > current_physical_capacity {
            // Calculate how much *additional* capacity is needed.
            // VecDeque::reserve allocates for *at least* `additional` more elements.
            let additional_needed = new_logical_capacity.saturating_sub(current_physical_capacity);
            buffer_ref.reserve(additional_needed);
        }
        *logical_capacity_ref = new_logical_capacity;
        Ok(())
    }
}

impl<T: Send + Sync + 'static> DynamicCircularBuffer<T> {
    /// Pushes an item into the buffer.
    ///
    /// # Examples
    ///
    /// ```
    /// # use elasticq::{DynamicCircularBuffer, Config};
    /// # let buffer = DynamicCircularBuffer::new(Config::default()).unwrap();
    /// buffer.push(42).expect("Failed to push item");
    /// ```
    pub fn push(&self, item: T) -> BufferResult<()> {
        let _push_lock_guard = self.push_lock.lock();
        let mut buffer_guard = self.buffer.lock();

        // Check against max_capacity using current length + 1 for the item to be pushed.
        if buffer_guard.len() >= self.config.max_capacity {
            return Err(BufferError::MaxCapacityReached(self.config.max_capacity));
        }
        // If max_capacity allows exactly one more item, but buffer is full *to its current logical capacity*,
        // it still needs to grow if max_capacity > current logical capacity.
        if buffer_guard.len() == self.config.max_capacity {
            // Already at absolute max
            return Err(BufferError::Full); // Or MaxCapacityReached, depends on preference
        }

        let current_len_before_push = buffer_guard.len();

        let (should_attempt_grow, growth_factor_val, max_cap_val) = {
            let capacity_read_guard = self.capacity.read();
            let current_logical_cap = *capacity_read_guard;
            (
                // Grow if current items >= current logical capacity, and we're not at max_capacity
                current_len_before_push >= current_logical_cap
                    && current_logical_cap < self.config.max_capacity,
                self.config.growth_factor,
                self.config.max_capacity,
            )
        };

        if should_attempt_grow {
            let mut capacity_write_guard = self.capacity.write();
            // Re-check condition with the current logical capacity from the write lock.
            // The len used for new capacity calculation should be the number of items *before* the current push
            // if we are at capacity, or *after* if we are adding and that pushes us over.
            // Let's use current_len_before_push + 1 (potential new length) as the basis for growth.
            let potential_len_after_push = current_len_before_push + 1;

            if potential_len_after_push > *capacity_write_guard
                && *capacity_write_guard < max_cap_val
            {
                // Calculate new capacity based on what it *will be* after this push.
                // If current_len_before_push == *capacity_write_guard, then it's full.
                // The new capacity should be at least potential_len_after_push.
                let new_potential_logical_capacity =
                    (*capacity_write_guard as f64 * growth_factor_val).ceil() as usize;

                // Ensure new capacity is at least one greater than current length, and capped by max_capacity.
                let new_logical_capacity_target = new_potential_logical_capacity
                    .max(potential_len_after_push) // Must be able to hold the new item
                    .min(max_cap_val);

                if new_logical_capacity_target > *capacity_write_guard {
                    // Only resize if actually growing
                    DynamicCircularBuffer::<T>::_perform_resize(
                        &mut *buffer_guard,
                        &mut *capacity_write_guard,
                        new_logical_capacity_target,
                    )
                    .map_err(|e| BufferError::ResizeError(e.to_string()))?;
                }
            }
        }

        // After potential resize, check logical capacity again before pushing.
        // This handles the case where resize didn't happen or didn't make enough space
        // (e.g. already at max_capacity).
        let current_logical_cap_after_resize_check = *self.capacity.read(); // Quick read lock
        if buffer_guard.len() >= current_logical_cap_after_resize_check {
            // If still full with respect to logical capacity and we are not at max_capacity
            // this implies a logic error in growth or max_capacity handling,
            // or we are truly at max_capacity and the initial check should have caught it.
            // For safety, if at max_capacity, return Full.
            if current_logical_cap_after_resize_check == self.config.max_capacity {
                return Err(BufferError::Full); // Or MaxCapacityReached
            }
            // This state (full w.r.t logical_cap but not max_cap, and grow didn't occur/suffice)
            // should ideally not be reached if grow logic is perfect.
            // However, if we are here, it means we can't push.
            return Err(BufferError::Full);
        }

        buffer_guard.push_back(item);
        Ok(())
    }

    /// Pops an item from the buffer.
    ///
    /// # Examples
    ///
    /// ```
    /// # use elasticq::{DynamicCircularBuffer, Config};
    /// # let buffer = DynamicCircularBuffer::new(Config::default()).unwrap();
    /// # buffer.push(42).unwrap();
    /// let item = buffer.pop().expect("Failed to pop item");
    /// assert_eq!(item, 42);
    /// ```
    pub fn pop(&self) -> BufferResult<T> {
        let _pop_lock_guard = self.pop_lock.lock();
        let mut buffer_guard = self.buffer.lock();

        if let Some(item) = buffer_guard.pop_front() {
            let current_len_after_pop = buffer_guard.len();

            let (should_attempt_shrink, min_cap_val, shrink_thresh_val, growth_factor_val) = {
                let capacity_read_guard = self.capacity.read();
                let current_logical_cap = *capacity_read_guard;
                (
                    current_len_after_pop
                        <= (current_logical_cap as f64 * self.config.shrink_threshold) as usize
                        && current_logical_cap > self.config.min_capacity,
                    self.config.min_capacity,
                    self.config.shrink_threshold,
                    self.config.growth_factor,
                )
            };

            if should_attempt_shrink {
                let mut capacity_write_guard = self.capacity.write();
                // Re-check condition with current logical capacity from write lock
                if current_len_after_pop
                    <= (*capacity_write_guard as f64 * shrink_thresh_val) as usize
                    && *capacity_write_guard > min_cap_val
                {
                    // Base shrink calculation on current logical capacity
                    let new_potential_logical_capacity = // <<< This calculation now uses *capacity_write_guard
                        (*capacity_write_guard as f64 / growth_factor_val).floor() as usize;

                    let new_logical_capacity_target = new_potential_logical_capacity
                        .max(current_len_after_pop) // Must be able to hold current items
                        .max(min_cap_val); // And not below min_capacity

                    if new_logical_capacity_target < *capacity_write_guard {
                        // Only shrink if target is smaller
                        DynamicCircularBuffer::<T>::_perform_shrink(
                            &mut *buffer_guard,
                            &mut *capacity_write_guard,
                            new_logical_capacity_target,
                        )
                        .map_err(|e| BufferError::ResizeError(e.to_string()))?;
                    }
                }
            }
            Ok(item)
        } else {
            Err(BufferError::Empty)
        }
    }

    /// Pushes multiple items into the buffer.
    ///
    /// # Examples
    ///
    /// ```
    /// # use elasticq::{DynamicCircularBuffer, Config};
    /// # let buffer = DynamicCircularBuffer::new(Config::default()).unwrap();
    /// buffer.push_batch(vec![1, 2, 3]).expect("Failed to push batch");
    /// ```
    pub fn push_batch(&self, items: Vec<T>) -> BufferResult<()> {
        if items.is_empty() {
            return Ok(());
        }
        let num_items_to_push = items.len();

        let _push_lock_guard = self.push_lock.lock();
        let mut buffer_guard = self.buffer.lock();

        let current_len_before_push = buffer_guard.len();
        let potential_len_after_push = current_len_before_push + num_items_to_push;

        if potential_len_after_push > self.config.max_capacity {
            return Err(BufferError::MaxCapacityReached(self.config.max_capacity));
        }

        let (should_attempt_grow, growth_factor_val, max_cap_val) = {
            let capacity_read_guard = self.capacity.read();
            let current_logical_cap = *capacity_read_guard;
            (
                potential_len_after_push > current_logical_cap
                    && current_logical_cap < self.config.max_capacity,
                self.config.growth_factor,
                self.config.max_capacity,
            )
        };

        if should_attempt_grow {
            let mut capacity_write_guard = self.capacity.write();
            // Re-check condition
            if potential_len_after_push > *capacity_write_guard
                && *capacity_write_guard < max_cap_val
            {
                let new_potential_logical_capacity =
                    (*capacity_write_guard as f64 * growth_factor_val).ceil() as usize;

                let new_logical_capacity_target = new_potential_logical_capacity
                    .max(potential_len_after_push) // Must hold all items
                    .min(max_cap_val);

                if new_logical_capacity_target > *capacity_write_guard {
                    DynamicCircularBuffer::<T>::_perform_resize(
                        &mut *buffer_guard,
                        &mut *capacity_write_guard,
                        new_logical_capacity_target,
                    )
                    .map_err(|e| BufferError::ResizeError(e.to_string()))?;
                }
            }
        }

        // After potential resize, check logical capacity again
        let current_logical_cap_after_resize_check = *self.capacity.read();
        if potential_len_after_push > current_logical_cap_after_resize_check {
            // This implies we hit max_capacity or couldn't grow enough.
            // The initial check `potential_len_after_push > self.config.max_capacity` should cover this.
            // If we are here, it's likely because logical_capacity is max_capacity.
            return Err(BufferError::Full); // Not enough space even after resize attempt
        }

        buffer_guard.extend(items);
        Ok(())
    }

    /// Pops multiple items from the buffer.
    ///
    /// # Examples
    ///
    /// ```
    /// # use elasticq::{DynamicCircularBuffer, Config};
    /// # let buffer = DynamicCircularBuffer::new(Config::default()).unwrap();
    /// # buffer.push_batch(vec![1, 2, 3]).unwrap();
    /// let items = buffer.pop_batch(2).expect("Failed to pop batch");
    /// assert_eq!(items, vec![1, 2]);
    /// ```
    pub fn pop_batch(&self, max_items_to_pop: usize) -> BufferResult<Vec<T>> {
        if max_items_to_pop == 0 {
            return Ok(Vec::new());
        }
        let _pop_lock_guard = self.pop_lock.lock();
        let mut buffer_guard = self.buffer.lock();

        if buffer_guard.is_empty() {
            return Ok(Vec::new()); // Consistent with BufferError::Empty for single pop, but batch can return empty Vec
        }

        let num_to_pop = max_items_to_pop.min(buffer_guard.len());
        let items = buffer_guard.drain(..num_to_pop).collect::<Vec<T>>();

        // Shrink logic
        let current_len_after_pop = buffer_guard.len();
        // let vecdeque_physical_capacity = buffer_guard.capacity(); // Not directly used in this shrink calculation

        let (should_attempt_shrink, min_cap_val, shrink_thresh_val, growth_factor_val) = {
            let capacity_read_guard = self.capacity.read();
            let current_logical_cap = *capacity_read_guard;
            (
                current_len_after_pop
                    <= (current_logical_cap as f64 * self.config.shrink_threshold) as usize
                    && current_logical_cap > self.config.min_capacity,
                self.config.min_capacity,
                self.config.shrink_threshold,
                self.config.growth_factor,
            )
        };

        if should_attempt_shrink {
            let mut capacity_write_guard = self.capacity.write();
            if current_len_after_pop <= (*capacity_write_guard as f64 * shrink_thresh_val) as usize
                && *capacity_write_guard > min_cap_val
            {
                let new_potential_logical_capacity =
                    (*capacity_write_guard as f64 / growth_factor_val).floor() as usize;
                let new_logical_capacity_target = new_potential_logical_capacity
                    .max(current_len_after_pop)
                    .max(min_cap_val);

                if new_logical_capacity_target < *capacity_write_guard {
                    DynamicCircularBuffer::<T>::_perform_shrink(
                        &mut *buffer_guard,
                        &mut *capacity_write_guard,
                        new_logical_capacity_target,
                    )
                    .map_err(|e| BufferError::ResizeError(e.to_string()))?;
                }
            }
        }
        Ok(items)
    }

    // --- Asynchronous methods ---

    /// Asynchronously pushes an item into the buffer.
    #[cfg(feature = "async")]
    pub async fn push_async(&self, item: T) -> BufferResult<()> {
        let _push_lock_guard = self.push_lock.lock().await;
        let mut buffer_guard = self.buffer.lock().await;

        if buffer_guard.len() >= self.config.max_capacity {
            return Err(BufferError::MaxCapacityReached(self.config.max_capacity));
        }
        if buffer_guard.len() == self.config.max_capacity {
            return Err(BufferError::Full);
        }

        let current_len_before_push = buffer_guard.len();

        let (should_attempt_grow, growth_factor_val, max_cap_val) = {
            let capacity_read_guard = self.capacity.read().await;
            let current_logical_cap = *capacity_read_guard;
            (
                current_len_before_push >= current_logical_cap
                    && current_logical_cap < self.config.max_capacity,
                self.config.growth_factor,
                self.config.max_capacity,
            )
        };

        if should_attempt_grow {
            let mut capacity_write_guard = self.capacity.write().await;
            let potential_len_after_push = current_len_before_push + 1;
            if potential_len_after_push > *capacity_write_guard
                && *capacity_write_guard < max_cap_val
            {
                let new_potential_logical_capacity =
                    (*capacity_write_guard as f64 * growth_factor_val).ceil() as usize;
                let new_logical_capacity_target = new_potential_logical_capacity
                    .max(potential_len_after_push)
                    .min(max_cap_val);
                if new_logical_capacity_target > *capacity_write_guard {
                    DynamicCircularBuffer::<T>::_perform_resize(
                        &mut *buffer_guard,
                        &mut *capacity_write_guard,
                        new_logical_capacity_target,
                    )
                    .map_err(|e| BufferError::ResizeError(e.to_string()))?;
                }
            }
        }

        let current_logical_cap_after_resize_check = *self.capacity.read().await;
        if buffer_guard.len() >= current_logical_cap_after_resize_check {
            if current_logical_cap_after_resize_check == self.config.max_capacity {
                return Err(BufferError::Full);
            }
            return Err(BufferError::Full);
        }

        buffer_guard.push_back(item);
        Ok(())
    }

    /// Asynchronously pops an item from the buffer.
    #[cfg(feature = "async")]
    pub async fn pop_async(&self) -> BufferResult<T> {
        let _pop_lock_guard = self.pop_lock.lock().await;
        let mut buffer_guard = self.buffer.lock().await;

        if let Some(item) = buffer_guard.pop_front() {
            let current_len_after_pop = buffer_guard.len();
            // let vecdeque_physical_capacity = buffer_guard.capacity();

            let (should_attempt_shrink, min_cap_val, shrink_thresh_val, growth_factor_val) = {
                let capacity_read_guard = self.capacity.read().await;
                let current_logical_cap = *capacity_read_guard;
                (
                    current_len_after_pop
                        <= (current_logical_cap as f64 * self.config.shrink_threshold) as usize
                        && current_logical_cap > self.config.min_capacity,
                    self.config.min_capacity,
                    self.config.shrink_threshold,
                    self.config.growth_factor,
                )
            };

            if should_attempt_shrink {
                let mut capacity_write_guard = self.capacity.write().await;
                if current_len_after_pop
                    <= (*capacity_write_guard as f64 * shrink_thresh_val) as usize
                    && *capacity_write_guard > min_cap_val
                {
                    let new_potential_logical_capacity =
                        (*capacity_write_guard as f64 / growth_factor_val).floor() as usize;
                    let new_logical_capacity_target = new_potential_logical_capacity
                        .max(current_len_after_pop)
                        .max(min_cap_val);
                    if new_logical_capacity_target < *capacity_write_guard {
                        DynamicCircularBuffer::<T>::_perform_shrink(
                            &mut *buffer_guard,
                            &mut *capacity_write_guard,
                            new_logical_capacity_target,
                        )
                        .map_err(|e| BufferError::ResizeError(e.to_string()))?;
                    }
                }
            }
            Ok(item)
        } else {
            Err(BufferError::Empty)
        }
    }

    /// Asynchronously pushes an item with a timeout.
    #[cfg(feature = "async")]
    pub async fn push_async_timeout(&self, item: T, timeout: Duration) -> BufferResult<()> {
        // Correctly use the result of the inner future for map_err
        match tokio::time::timeout(timeout, self.push_async(item)).await {
            Ok(Ok(())) => Ok(()),                         // Inner push_async succeeded
            Ok(Err(e)) => Err(e), // Inner push_async failed with BufferError
            Err(_) => Err(BufferError::Timeout(timeout)), // tokio::time::timeout itself timed out
        }
    }

    /// Asynchronously pops an item with a timeout.
    #[cfg(feature = "async")]
    pub async fn pop_async_timeout(&self, timeout: Duration) -> BufferResult<T> {
        match tokio::time::timeout(timeout, self.pop_async()).await {
            Ok(Ok(item)) => Ok(item),
            Ok(Err(e)) => Err(e),
            Err(_) => Err(BufferError::Timeout(timeout)),
        }
    }

    /// Asynchronously pushes multiple items into the buffer.
    #[cfg(feature = "async")]
    pub async fn push_batch_async(&self, items: Vec<T>) -> BufferResult<()> {
        if items.is_empty() {
            return Ok(());
        }
        let num_items_to_push = items.len();

        let _push_lock_guard = self.push_lock.lock().await;
        let mut buffer_guard = self.buffer.lock().await;

        let current_len_before_push = buffer_guard.len();
        let potential_len_after_push = current_len_before_push + num_items_to_push;

        if potential_len_after_push > self.config.max_capacity {
            return Err(BufferError::MaxCapacityReached(self.config.max_capacity));
        }

        let (should_attempt_grow, growth_factor_val, max_cap_val) = {
            let capacity_read_guard = self.capacity.read().await;
            let current_logical_cap = *capacity_read_guard;
            (
                potential_len_after_push > current_logical_cap
                    && current_logical_cap < self.config.max_capacity,
                self.config.growth_factor,
                self.config.max_capacity,
            )
        };

        if should_attempt_grow {
            let mut capacity_write_guard = self.capacity.write().await;
            if potential_len_after_push > *capacity_write_guard
                && *capacity_write_guard < max_cap_val
            {
                let new_potential_logical_capacity =
                    (*capacity_write_guard as f64 * growth_factor_val).ceil() as usize;
                let new_logical_capacity_target = new_potential_logical_capacity
                    .max(potential_len_after_push)
                    .min(max_cap_val);

                if new_logical_capacity_target > *capacity_write_guard {
                    DynamicCircularBuffer::<T>::_perform_resize(
                        &mut *buffer_guard,
                        &mut *capacity_write_guard,
                        new_logical_capacity_target,
                    )
                    .map_err(|e| BufferError::ResizeError(e.to_string()))?;
                }
            }
        }

        let current_logical_cap_after_resize_check = *self.capacity.read().await;
        if potential_len_after_push > current_logical_cap_after_resize_check {
            return Err(BufferError::Full);
        }

        buffer_guard.extend(items);
        Ok(())
    }

    /// Asynchronously pops up to `max_items` from the buffer.
    #[cfg(feature = "async")]
    pub async fn pop_batch_async(&self, max_items_to_pop: usize) -> BufferResult<Vec<T>> {
        if max_items_to_pop == 0 {
            return Ok(Vec::new());
        }
        let _pop_lock_guard = self.pop_lock.lock().await;
        let mut buffer_guard = self.buffer.lock().await;

        if buffer_guard.is_empty() {
            return Ok(Vec::new());
        }

        let num_to_pop = max_items_to_pop.min(buffer_guard.len());
        let items = buffer_guard.drain(..num_to_pop).collect::<Vec<T>>();

        let current_len_after_pop = buffer_guard.len();
        // let vecdeque_physical_capacity = buffer_guard.capacity();

        let (should_attempt_shrink, min_cap_val, shrink_thresh_val, growth_factor_val) = {
            let capacity_read_guard = self.capacity.read().await;
            let current_logical_cap = *capacity_read_guard;
            (
                current_len_after_pop
                    <= (current_logical_cap as f64 * self.config.shrink_threshold) as usize
                    && current_logical_cap > self.config.min_capacity,
                self.config.min_capacity,
                self.config.shrink_threshold,
                self.config.growth_factor,
            )
        };

        if should_attempt_shrink {
            let mut capacity_write_guard = self.capacity.write().await;
            if current_len_after_pop <= (*capacity_write_guard as f64 * shrink_thresh_val) as usize
                && *capacity_write_guard > min_cap_val
            {
                let new_potential_logical_capacity =
                    (*capacity_write_guard as f64 / growth_factor_val).floor() as usize;
                let new_logical_capacity_target = new_potential_logical_capacity
                    .max(current_len_after_pop)
                    .max(min_cap_val);

                if new_logical_capacity_target < *capacity_write_guard {
                    DynamicCircularBuffer::<T>::_perform_shrink(
                        &mut *buffer_guard,
                        &mut *capacity_write_guard,
                        new_logical_capacity_target,
                    )
                    .map_err(|e| BufferError::ResizeError(e.to_string()))?;
                }
            }
        }
        Ok(items)
    }

    /// Asynchronously pushes multiple items into the buffer with a timeout.
    #[cfg(feature = "async")]
    pub async fn push_batch_async_timeout(
        &self,
        items: Vec<T>,
        timeout: Duration,
    ) -> BufferResult<()> {
        match tokio::time::timeout(timeout, self.push_batch_async(items)).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(e),
            Err(_) => Err(BufferError::Timeout(timeout)),
        }
    }

    /// Asynchronously pops up to `max_items` from the buffer with a timeout.
    #[cfg(feature = "async")]
    pub async fn pop_batch_async_timeout(
        &self,
        max_items: usize,
        timeout: Duration,
    ) -> BufferResult<Vec<T>> {
        match tokio::time::timeout(timeout, self.pop_batch_async(max_items)).await {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(e)) => Err(e),
            Err(_) => Err(BufferError::Timeout(timeout)),
        }
    }
}

impl<T> DynamicCircularBuffer<T> {
    /// Returns the number of items in the buffer.
    /// Note: For async, prefer `len_async` if very high contention on the lock is expected,
    /// but for simple length check, this sync version is often fine.
    pub fn len(&self) -> usize {
        #[cfg(not(feature = "async"))]
        {
            self.buffer.lock().len()
        }
        #[cfg(feature = "async")]
        {
            // This is a potentially blocking call in an async context if used from one.
            // For a quick check, it might be acceptable, but for true async,
            // one might offer a `len_async().await` method.
            // However, `parking_lot::Mutex` when used with `tokio::sync::Mutex`
            // for the `async` feature implies `tokio::sync::Mutex` is the one being used.
            // The type alias for MutexGuard should ensure this.
            // Let's assume self.buffer.lock() returns a tokio MutexGuard here.
            // This part needs careful consideration of which mutex is actually in play.
            //
            // Re-evaluating: The cfg selects the Mutex type. So if "async" is on,
            // self.buffer is tokio::sync::Mutex, and .lock() is async.
            // This method `len()` is NOT async. So, it CANNOT call .await.
            // This indicates a design flaw if `len()` is called from async code
            // without the "async" feature, or if it's intended to be callable
            // synchronously even with the "async" feature.
            //
            // Let's assume for now that if "async" feature is on, users are expected
            // to primarily use async methods or handle blocking carefully.
            // A truly non-blocking len would require an async fn.
            //
            // Given the project structure, if `async` is enabled, `self.buffer.lock()` is `tokio::sync::Mutex::lock()`.
            // This method `len()` is NOT `async fn`.
            // This means `self.buffer.lock().len()` when `async` is on will NOT compile if `lock()` is the async one.
            //
            // The `parking_lot` and `tokio::sync` imports are cfg-gated for the *types* `Mutex`, `RwLock`.
            //
            // Simplest fix for `len()`, `is_empty()`, `capacity()` for now:
            // These should ideally also be async if the internal locks are async.
            // Or, they accept the blocking nature for simplicity if called from sync code
            // even when async feature is on.
            //
            // For a quick fix to make it compile, we might need to use `try_lock`
            // or make these async.
            //
            // Let's stick to the original intent: these are sync methods.
            // If async feature is on, and these are called from a sync context,
            // `blocking_lock()` from tokio's mutex could be an option, or
            // simply acknowledge they might block.
            // The original code would have had `parking_lot::Mutex` if `!async`
            // and `tokio::sync::Mutex` if `async`.
            // A `tokio::sync::Mutex` cannot be locked synchronously without `blocking_lock`.
            //
            // The most straightforward way is to make these `async` as well if the feature is on,
            // or provide `_blocking_` versions.
            //
            // Given the original structure, it implies:
            // - If `not(feature = "async")`, `self.buffer.lock()` is `parking_lot::Mutex::lock()`.
            // - If `feature = "async"`, `self.buffer.lock()` is `tokio::sync::Mutex::lock()`.
            // The `len()` method as written will thus fail to compile if `feature = "async"`
            // because `tokio::sync::Mutex::lock()` returns a future.

            // To maintain sync API for these simple accessors, and assuming they are
            // called infrequently or where blocking is acceptable:
            // This is tricky. `parking_lot::Mutex` is fine. `tokio::sync::Mutex` needs `blocking_lock()`.
            // The `use` statements correctly switch the *type* of `Mutex`.
            // So, `self.buffer.lock()` will correctly call the respective lock method.
            //
            // The issue is that `tokio::sync::Mutex::lock()` is an async function.
            // A non-async function `len()` cannot `.await` it.
            //
            // This implies that `len()`, `is_empty()`, `capacity()` must also be `async`
            // if the `async` feature is enabled, or they should use `try_lock` and
            // handle the case where the lock isn't available (which is not ideal for `len`).
            //
            // Simplest correct approach: these also become async if the feature is on.
            // However, the original code had them as sync. This implies that perhaps
            // these methods were not intended to be used in a hot async path, or
            // the interaction was overlooked.

            // For now, let's assume the user must call these from a context that can block
            // if the async feature is on, and we use `blocking_lock`. This requires
            // `tokio::sync::Mutex` to expose `blocking_lock`.
            // `tokio::sync::Mutex` does have `blocking_lock()`.

            // Corrected approach for these utility methods:
            // They should mirror the locking mechanism.
            // The cfg was on the *use* statement, not the method definition.
            // This means if `feature = "async"`, `Mutex` *is* `tokio::sync::Mutex`.
            // So `self.buffer.lock()` is an async call.
            //
            // EITHER:
            // 1. Make these methods `async fn` too if `feature = "async"`.
            //    This changes the public API based on a feature flag, which can be confusing.
            // OR:
            // 2. Use `blocking_lock()` for `tokio::sync::Mutex` in these specific sync methods.
            //    This is generally okay for infrequent utility/debug calls.
            // OR:
            // 3. Remove them or clearly document they are problematic with `async` feature.

            // Let's go with option 2 for least API change, assuming these are utility.
            self.buffer.blocking_lock().len()
        }
    }

    /// Returns true if the buffer is empty.
    pub fn is_empty(&self) -> bool {
        #[cfg(not(feature = "async"))]
        {
            self.buffer.lock().is_empty()
        }
        #[cfg(feature = "async")]
        {
            self.buffer.blocking_lock().is_empty()
        }
    }

    /// Returns the current *logical* capacity of the buffer.
    pub fn capacity(&self) -> usize {
        #[cfg(not(feature = "async"))]
        {
            *self.capacity.read()
        }
        #[cfg(feature = "async")]
        {
            *self.capacity.blocking_read()
        }
    }

    /// Clears the buffer, removing all items. Also resets logical capacity to initial_capacity.
    pub fn clear(&self) {
        #[cfg(not(feature = "async"))]
        {
            let mut buffer_guard = self.buffer.lock();
            let mut capacity_guard = self.capacity.write();
            buffer_guard.clear();
            buffer_guard.shrink_to(0); // Free up VecDeque's internal allocation
            *capacity_guard = self.config.initial_capacity.max(self.config.min_capacity); // Reset logical capacity
                                                                                          // Re-allocate VecDeque to initial capacity to avoid immediate resize on next push
            buffer_guard.reserve(self.config.initial_capacity);
        }
        #[cfg(feature = "async")]
        {
            let mut buffer_guard = self.buffer.blocking_lock();
            let mut capacity_guard = self.capacity.blocking_write();
            buffer_guard.clear();
            buffer_guard.shrink_to(0);
            *capacity_guard = self.config.initial_capacity.max(self.config.min_capacity);
            buffer_guard.reserve(self.config.initial_capacity);
        }
    }

    /// Returns a clone of all items in the buffer.
    /// Note: This clones all data. Use with caution on large buffers.
    pub fn iter(&self) -> Vec<T>
    where
        T: Clone,
    {
        #[cfg(not(feature = "async"))]
        {
            let buffer_guard = self.buffer.lock();
            buffer_guard.iter().cloned().collect()
        }
        #[cfg(feature = "async")]
        {
            let buffer_guard = self.buffer.blocking_lock();
            buffer_guard.iter().cloned().collect()
        }
    }

    /// Drains the buffer, returning all items.
    /// This also implies the buffer might shrink if conditions are met.
    pub fn drain(&self) -> Vec<T> {
        // Draining is a "pop" like operation, so use pop_lock and handle shrink
        let _pop_lock_guard = {
            #[cfg(not(feature = "async"))]
            {
                self.pop_lock.lock()
            }
            #[cfg(feature = "async")]
            {
                self.pop_lock.blocking_lock()
            }
        };

        let mut buffer_guard = {
            #[cfg(not(feature = "async"))]
            {
                self.buffer.lock()
            }
            #[cfg(feature = "async")]
            {
                self.buffer.blocking_lock()
            }
        };

        let items = buffer_guard.drain(..).collect::<Vec<T>>();

        // After draining, the buffer is empty. Check for shrink.
        let current_len_after_pop = 0; // buffer_guard.len() will be 0

        let (should_attempt_shrink, min_cap_val, shrink_thresh_val, growth_factor_val) = {
            #[cfg(not(feature = "async"))]
            {
                let capacity_read_guard = self.capacity.read();
                let current_logical_cap = *capacity_read_guard;
                (
                    current_len_after_pop
                        <= (current_logical_cap as f64 * self.config.shrink_threshold) as usize
                        && current_logical_cap > self.config.min_capacity,
                    self.config.min_capacity,
                    self.config.shrink_threshold,
                    self.config.growth_factor,
                )
            }
            #[cfg(feature = "async")]
            {
                let capacity_read_guard = self.capacity.blocking_read();
                let current_logical_cap = *capacity_read_guard;
                (
                    current_len_after_pop
                        <= (current_logical_cap as f64 * self.config.shrink_threshold) as usize
                        && current_logical_cap > self.config.min_capacity,
                    self.config.min_capacity,
                    self.config.shrink_threshold,
                    self.config.growth_factor,
                )
            }
        };

        if should_attempt_shrink {
            #[cfg(not(feature = "async"))]
            {
                let mut capacity_write_guard = self.capacity.write();
                if current_len_after_pop
                    <= (*capacity_write_guard as f64 * shrink_thresh_val) as usize
                    && *capacity_write_guard > min_cap_val
                {
                    let new_potential_logical_capacity =
                        (*capacity_write_guard as f64 / growth_factor_val).floor() as usize;
                    let new_logical_capacity_target = new_potential_logical_capacity
                        .max(current_len_after_pop)
                        .max(min_cap_val);
                    if new_logical_capacity_target < *capacity_write_guard {
                        let _ = DynamicCircularBuffer::<T>::_perform_shrink(
                            &mut *buffer_guard,
                            &mut *capacity_write_guard,
                            new_logical_capacity_target,
                        );
                    }
                }
            }
            #[cfg(feature = "async")]
            {
                let mut capacity_write_guard = self.capacity.blocking_write();
                if current_len_after_pop
                    <= (*capacity_write_guard as f64 * shrink_thresh_val) as usize
                    && *capacity_write_guard > min_cap_val
                {
                    let new_potential_logical_capacity =
                        (*capacity_write_guard as f64 / growth_factor_val).floor() as usize;
                    let new_logical_capacity_target = new_potential_logical_capacity
                        .max(current_len_after_pop)
                        .max(min_cap_val);
                    if new_logical_capacity_target < *capacity_write_guard {
                        let _ = DynamicCircularBuffer::<T>::_perform_shrink(
                            &mut *buffer_guard,
                            &mut *capacity_write_guard,
                            new_logical_capacity_target,
                        );
                    }
                }
            }
        }
        items
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Helper to create a default config for tests, can be customized
    fn test_config() -> Config {
        Config::default()
    }

    #[test]
    fn test_push_pop() {
        let buffer = DynamicCircularBuffer::new(test_config()).unwrap();
        buffer.push(1).unwrap();
        buffer.push(2).unwrap();
        assert_eq!(buffer.len(), 2);
        assert_eq!(buffer.pop().unwrap(), 1);
        assert_eq!(buffer.len(), 1);
        assert_eq!(buffer.pop().unwrap(), 2);
        assert_eq!(buffer.len(), 0);
        assert!(matches!(buffer.pop(), Err(BufferError::Empty)));
    }

    #[test]
    fn test_resize_on_push() {
        // Renamed from test_resize
        let config = test_config()
            .with_initial_capacity(2)
            .with_min_capacity(2) // Prevent shrinking below initial for this test
            .with_max_capacity(8)
            .with_growth_factor(2.0);
        let buffer = DynamicCircularBuffer::new(config).unwrap();
        assert_eq!(buffer.capacity(), 2, "Initial capacity mismatch");

        buffer.push(1).unwrap(); // len=1, cap=2
        assert_eq!(buffer.capacity(), 2, "Capacity shouldn't change yet");
        buffer.push(2).unwrap(); // len=2, cap=2 (full w.r.t logical capacity)
        assert_eq!(buffer.capacity(), 2, "Capacity shouldn't change yet");

        println!(
            "Before push 3: len={}, cap={}",
            buffer.len(),
            buffer.capacity()
        );
        buffer.push(3).unwrap(); // len=3. Should trigger resize. cap_old=2, growth=2.0. new_cap = ceil(2*2.0)=4 or max(3, 4)=4
        println!(
            "After push 3: len={}, cap={}",
            buffer.len(),
            buffer.capacity()
        );

        assert_eq!(buffer.len(), 3, "Buffer length is incorrect");
        assert!(
            buffer.capacity() >= 3,
            "Buffer capacity should be >=3 after resize. Actual: {}",
            buffer.capacity()
        );
        assert!(
            buffer.capacity() <= 4,
            "Buffer capacity should be around 4. Actual: {}",
            buffer.capacity()
        ); // ceil(2*2.0)=4. max(3,4)=4

        buffer.push(4).unwrap(); // len=4, cap=4
        println!(
            "After push 4: len={}, cap={}",
            buffer.len(),
            buffer.capacity()
        );
        assert_eq!(buffer.len(), 4);
        assert_eq!(buffer.capacity(), 4);

        buffer.push(5).unwrap(); // len=5. Should trigger resize. cap_old=4, growth=2.0. new_cap = ceil(4*2.0)=8 or max(5,8)=8
        println!(
            "After push 5: len={}, cap={}",
            buffer.len(),
            buffer.capacity()
        );
        assert_eq!(buffer.len(), 5);
        assert!(
            buffer.capacity() >= 5,
            "Capacity should be >=5. Actual: {}",
            buffer.capacity()
        );
        assert!(
            buffer.capacity() <= 8,
            "Capacity should be <=8. Actual: {}",
            buffer.capacity()
        );
    }

    #[test]
    fn test_push_batch_resize() {
        let config = test_config()
            .with_initial_capacity(5)
            .with_min_capacity(1) // Or some value <= 5
            .with_max_capacity(20)
            .with_growth_factor(2.0);
        let buffer = DynamicCircularBuffer::new(config).unwrap();
        assert_eq!(buffer.capacity(), 5);

        // Push 5 items, should not resize yet
        buffer.push_batch(vec![1, 2, 3, 4, 5]).unwrap();
        assert_eq!(buffer.len(), 5);
        assert_eq!(buffer.capacity(), 5);

        // Push 3 more items, potential_len = 8. current_cap = 5. Should resize.
        // new_cap = ceil(5 * 2.0) = 10. max(8, 10) = 10.
        buffer.push_batch(vec![6, 7, 8]).unwrap();
        assert_eq!(buffer.len(), 8);
        assert_eq!(buffer.capacity(), 10);

        // Pop some, then push batch again
        let _ = buffer.pop_batch(4).unwrap(); // len = 4, cap = 10
        assert_eq!(buffer.len(), 4);

        // Push 7 items. potential_len = 4 + 7 = 11. current_cap = 10. Should resize.
        // new_cap = ceil(10 * 2.0) = 20. max(11, 20) = 20.
        buffer.push_batch(vec![9, 10, 11, 12, 13, 14, 15]).unwrap();
        assert_eq!(buffer.len(), 11);
        assert_eq!(buffer.capacity(), 20, "Capacity should be max_capacity");
    }

    #[test]
    fn test_max_capacity_single_push() {
        let config = test_config()
            .with_initial_capacity(2)
            .with_min_capacity(1) // Ensure min_capacity <= initial_capacity
            .with_max_capacity(3); // Ensure max_capacity >= initial_capacity
        let buffer = DynamicCircularBuffer::new(config).unwrap();

        buffer.push(1).unwrap(); // len=1, cap=2
        buffer.push(2).unwrap(); // len=2, cap=2

        // Next push should resize to max_capacity if possible, or use max_capacity
        // len=2, current_cap=2. potential_len=3.
        // should_grow: 2>=2 && 2<3 -> true.
        // new_potential_cap = ceil(2*2.0) = 4.
        // new_target = 4.max(3).min(3) = 3.
        buffer.push(3).unwrap(); // len=3, cap should become 3
        assert_eq!(buffer.len(), 3);
        assert_eq!(buffer.capacity(), 3, "Capacity should be max_capacity");

        // Attempt to push one more item, should fail
        let result = buffer.push(4);
        assert!(
            matches!(
                result,
                Err(BufferError::Full) | Err(BufferError::MaxCapacityReached(_))
            ),
            "Expected Full or MaxCapacityReached, got {:?}",
            result
        );
        assert_eq!(buffer.len(), 3, "Buffer length should remain 3");
    }

    #[test]
    fn test_max_capacity_push_batch() {
        let config = test_config()
            .with_initial_capacity(5)
            .with_min_capacity(1) // Or some value <= 5
            .with_max_capacity(10);
        let buffer = DynamicCircularBuffer::new(config).unwrap();

        // Push 8 items, should resize to 10 (max_capacity)
        // initial_cap=5. potential_len=8. should_grow: 8>5 && 5<10 -> true.
        // new_potential_cap = ceil(5*2.0)=10. new_target = 10.max(8).min(10) = 10.
        buffer.push_batch(vec![0, 1, 2, 3, 4, 5, 6, 7]).unwrap();
        assert_eq!(buffer.len(), 8);
        assert_eq!(buffer.capacity(), 10, "Capacity should be max_capacity");

        // Push 2 more items, fills to max_capacity
        buffer.push_batch(vec![8, 9]).unwrap();
        assert_eq!(buffer.len(), 10);
        assert_eq!(buffer.capacity(), 10);

        // Attempt to push more, should fail
        let result_single = buffer.push(10);
        assert!(
            matches!(
                result_single,
                Err(BufferError::Full) | Err(BufferError::MaxCapacityReached(_))
            ),
            "Expected Full on single push, got {:?}",
            result_single
        );

        let result_batch = buffer.push_batch(vec![10, 11]);
        assert!(
            matches!(result_batch, Err(BufferError::MaxCapacityReached(_))),
            "Expected MaxCapacityReached on batch push, got {:?}",
            result_batch
        );
        assert_eq!(buffer.len(), 10, "Length should remain 10");
    }

    #[test]
    fn test_clear() {
        let config = test_config()
            .with_min_capacity(10) // Set min_capacity first
            .with_initial_capacity(50) // Ensure initial is >= new min
            .with_max_capacity(100); // Ensure max is >= initial
        let buffer = DynamicCircularBuffer::new(config).unwrap();
        buffer.push_batch(vec![1, 2, 3, 4, 5]).unwrap();
        assert_eq!(buffer.len(), 5);
        assert!(buffer.capacity() >= 5); // Capacity might have grown

        let initial_cap_from_config = buffer.config.initial_capacity; // store before clear potentially changes it if config was moved

        buffer.clear();
        assert_eq!(buffer.len(), 0);
        assert!(buffer.is_empty());
        assert_eq!(
            buffer.capacity(),
            initial_cap_from_config.max(buffer.config.min_capacity),
            "Capacity should reset to initial/min after clear"
        );

        // Test push after clear
        buffer.push(100).unwrap();
        assert_eq!(buffer.len(), 1);
        assert_eq!(buffer.pop().unwrap(), 100);
    }

    #[test]
    fn test_iter() {
        let buffer = DynamicCircularBuffer::new(test_config()).unwrap();
        let data = vec![1, 2, 3, 4, 5];
        buffer.push_batch(data.clone()).unwrap();
        let items: Vec<i32> = buffer.iter();
        assert_eq!(items, data);
        assert_eq!(buffer.len(), data.len(), "Iter should not consume items");
    }

    #[test]
    fn test_drain() {
        let buffer = DynamicCircularBuffer::new(
            test_config().with_initial_capacity(10).with_min_capacity(5),
        )
        .unwrap();
        let data = vec![1, 2, 3, 4, 5, 6, 7, 8]; // len=8, cap=10
        buffer.push_batch(data.clone()).unwrap();

        let drained_items = buffer.drain();
        assert_eq!(drained_items, data);
        assert!(buffer.is_empty(), "Buffer should be empty after drain");
        assert_eq!(buffer.len(), 0);
        // After drain, len is 0. If 0 <= (cap * shrink_threshold) and cap > min_cap, it should shrink.
        // Initial cap=10. 0 <= (10 * 0.25) -> true. 10 > 5 -> true.
        // new_potential = floor(10 / 2.0) = 5.
        // new_target = 5.max(0).max(5) = 5.
        // So capacity should shrink to 5 (min_capacity).
        assert_eq!(
            buffer.capacity(),
            5,
            "Capacity should shrink to min_capacity after drain"
        );
    }

    #[test]
    fn test_shrink_on_pop() {
        // Renamed from test_shrink
        let config = test_config()
            .with_initial_capacity(10)
            .with_min_capacity(5)
            .with_max_capacity(20) // Give it room to grow if needed, though not expected here
            .with_shrink_threshold(0.3); // Shrink if len <= 30% of capacity
        let buffer = DynamicCircularBuffer::new(config).unwrap();

        // Fill to capacity
        buffer.push_batch((0..10).collect::<Vec<i32>>()).unwrap();
        assert_eq!(buffer.len(), 10);
        assert_eq!(buffer.capacity(), 10, "Initial capacity should be 10");

        // Pop items one by one and observe shrinking
        // Cap=10. Shrink if len <= 10 * 0.3 = 3.
        // Pop 1 (0): len=9. No shrink.
        // Pop 2 (1): len=8. No shrink.
        // Pop 3 (2): len=7. No shrink.
        // Pop 4 (3): len=6. No shrink.
        // Pop 5 (4): len=5. No shrink.
        // Pop 6 (5): len=4. No shrink.
        // Pop 7 (6): len=3. Shrink condition: 3 <= (10*0.3)=3. True.
        //            new_potential = floor(10/2.0) = 5.
        //            new_target = 5.max(3).max(5) = 5. Cap becomes 5.
        for i in 0..7 {
            let popped = buffer.pop().unwrap();
            assert_eq!(popped, i as i32);
            println!(
                "Popped item {}: val={}, new_len={}, new_cap={}",
                i,
                popped,
                buffer.len(),
                buffer.capacity()
            );
            if i < 6 {
                // Before 7th pop (item 6 is popped, len becomes 3)
                assert_eq!(
                    buffer.capacity(),
                    10,
                    "Capacity should not shrink yet at i={}",
                    i
                );
            } else {
                // After 7th pop (item 6), len is 3
                assert_eq!(
                    buffer.capacity(),
                    5,
                    "Capacity should have shrunk to 5 (min_capacity) at i={}",
                    i
                );
            }
        }

        assert_eq!(buffer.len(), 3, "Buffer should have 3 items remaining");
        assert_eq!(buffer.capacity(), 5, "Final capacity should be 5");

        // Pop more, should not shrink below min_capacity
        buffer.pop().unwrap(); // len=2, cap=5. Shrink cond: 2 <= (5*0.3)=1.5. False.
        assert_eq!(buffer.capacity(), 5);
        buffer.pop().unwrap(); // len=1, cap=5. Shrink cond: 1 <= 1.5. True.
                               // new_potential = floor(5/2.0)=2.
                               // new_target = 2.max(1).max(5)=5. No change as target not < current.
        assert_eq!(
            buffer.capacity(),
            5,
            "Capacity should not shrink below min_capacity"
        );
        buffer.pop().unwrap(); // len=0, cap=5
        assert_eq!(buffer.capacity(), 5);
    }

    #[test]
    fn test_pop_batch_shrink() {
        let config = test_config()
            .with_initial_capacity(20)
            .with_min_capacity(5)
            .with_shrink_threshold(0.25); // Shrink if len <= 25% of capacity
        let buffer = DynamicCircularBuffer::new(config).unwrap();

        buffer.push_batch((0..20).collect()).unwrap();
        assert_eq!(buffer.len(), 20);
        assert_eq!(buffer.capacity(), 20);

        // Pop 15 items. len becomes 5. cap is 20.
        // Shrink condition: 5 <= (20 * 0.25) = 5. True.
        // new_potential = floor(20 / 2.0) = 10.
        // new_target = 10.max(5).max(5) = 10.
        let _ = buffer.pop_batch(15).unwrap();
        assert_eq!(buffer.len(), 5);
        assert_eq!(
            buffer.capacity(),
            10,
            "Capacity should shrink after pop_batch"
        );

        // Pop 1 item. len becomes 4. cap is 10.
        // Shrink condition: 4 <= (10 * 0.25) = 2.5. False.
        let _ = buffer.pop().unwrap();
        assert_eq!(buffer.len(), 4);
        assert_eq!(buffer.capacity(), 10);

        // Pop 2 items. len becomes 2. cap is 10.
        // Shrink condition: 2 <= (10 * 0.25) = 2.5. True.
        // new_potential = floor(10 / 2.0) = 5.
        // new_target = 5.max(2).max(5) = 5.
        let _ = buffer.pop_batch(2).unwrap();
        assert_eq!(buffer.len(), 2);
        assert_eq!(
            buffer.capacity(),
            5,
            "Capacity should shrink to min_capacity"
        );
    }

    #[cfg(feature = "async")]
    mod async_tests {
        use super::*;
        use tokio::test as tokio_test; // Alias to avoid conflict if `test` is used elsewhere

        fn async_test_config() -> Config {
            Config::default()
        }

        #[tokio_test]
        async fn test_push_pop_async() {
            let buffer = DynamicCircularBuffer::new(async_test_config()).unwrap();
            buffer.push_async(1).await.unwrap();
            buffer.push_async(2).await.unwrap();
            assert_eq!(buffer.len(), 2); // Uses blocking_lock internally
            assert_eq!(buffer.pop_async().await.unwrap(), 1);
            assert_eq!(buffer.pop_async().await.unwrap(), 2);
            assert!(matches!(buffer.pop_async().await, Err(BufferError::Empty)));
        }

        #[tokio_test]
        async fn test_push_pop_async_timeout() {
            let buffer = DynamicCircularBuffer::new(
                async_test_config().with_pop_timeout(Duration::from_millis(10)),
            )
            .unwrap(); // Config timeout not used by method

            buffer
                .push_async_timeout(1, Duration::from_millis(100))
                .await
                .unwrap();

            assert_eq!(
                buffer
                    .pop_async_timeout(Duration::from_millis(100))
                    .await
                    .unwrap(),
                1
            );

            // This pop should timeout
            let result = buffer.pop_async_timeout(Duration::from_millis(10)).await;
            assert!(
                matches!(result, Err(BufferError::Timeout(_))),
                "Expected Timeout, got {:?}",
                result
            );
        }

        #[tokio_test]
        async fn test_push_async_timeout_on_full_does_not_deadlock() {
            // Test that timeout works correctly even if buffer is full and push_async would block indefinitely (if it waited)
            let config = async_test_config()
                .with_initial_capacity(1)
                .with_max_capacity(1);
            let buffer = DynamicCircularBuffer::new(config).unwrap();
            buffer.push_async(1).await.unwrap(); // Fill the buffer

            // This push_async would normally return BufferError::Full or MaxCapacityReached immediately.
            // The timeout is more to test the timeout wrapper itself.
            // If push_async had internal retries/waits, this would be more critical.
            let item = 2;
            let push_fut = buffer.push_async_timeout(item, Duration::from_millis(50));

            match tokio::time::timeout(Duration::from_millis(100), push_fut).await {
                Ok(Err(BufferError::Full)) | Ok(Err(BufferError::MaxCapacityReached(_))) => { /* Expected, push_async returned error before timeout */ }
                Ok(Err(BufferError::Timeout(_))) => { /* Also acceptable if push_async somehow waited and then timed out */ }
                Ok(Ok(())) => panic!("Push should have failed or timed out"),
                Err(_) => panic!("Outer timeout, push_async_timeout itself might have deadlocked or took too long"),
                Ok(Err(e)) => panic!("Unexpected BufferError: {:?}", e),
            }
        }

        #[tokio_test]
        async fn test_push_batch_async_resize() {
            let config = async_test_config()
                .with_initial_capacity(2)
                .with_max_capacity(5);
            let buffer = DynamicCircularBuffer::new(config).unwrap();

            buffer.push_batch_async(vec![1, 2]).await.unwrap(); // len=2, cap=2
            assert_eq!(buffer.capacity(), 2);

            // Push 2 more. potential_len=4. current_cap=2. max_cap=5.
            // new_potential_cap = ceil(2*2.0)=4. target=4.max(4).min(5)=4.
            buffer.push_batch_async(vec![3, 4]).await.unwrap(); // len=4, cap=4
            assert_eq!(buffer.len(), 4);
            assert_eq!(buffer.capacity(), 4);

            // Push 1 more. potential_len=5. current_cap=4. max_cap=5
            // new_potential_cap = ceil(4*2.0)=8. target=8.max(5).min(5)=5.
            buffer.push_batch_async(vec![5]).await.unwrap();
            assert_eq!(buffer.len(), 5);
            assert_eq!(buffer.capacity(), 5, "Capacity should be max_capacity");

            let result = buffer.push_batch_async(vec![6]).await;
            assert!(matches!(
                result,
                Err(BufferError::MaxCapacityReached(_)) | Err(BufferError::Full)
            ));
        }

        #[tokio_test]
        async fn test_pop_batch_async_shrink() {
            let config = async_test_config()
                .with_initial_capacity(10)
                .with_min_capacity(3)
                .with_shrink_threshold(0.4); // Shrink if len <= 40% of cap
            let buffer = DynamicCircularBuffer::new(config).unwrap();

            buffer.push_batch_async((0..10).collect()).await.unwrap(); // len=10, cap=10

            // Pop 6. len=4. cap=10. Shrink cond: 4 <= (10*0.4)=4. True.
            // new_potential = floor(10/2.0)=5. target=5.max(4).max(3)=5.
            let _ = buffer.pop_batch_async(6).await.unwrap();
            assert_eq!(buffer.len(), 4);
            assert_eq!(buffer.capacity(), 5);

            // Pop 2. len=2. cap=5. Shrink cond: 2 <= (5*0.4)=2. True.
            // new_potential = floor(5/2.0)=2. target=2.max(2).max(3)=3 (min_cap).
            let _ = buffer.pop_batch_async(2).await.unwrap();
            assert_eq!(buffer.len(), 2);
            assert_eq!(buffer.capacity(), 3, "Should shrink to min_capacity");
        }

        #[tokio_test]
        async fn test_push_batch_async_timeout() {
            let buffer = DynamicCircularBuffer::new(async_test_config()).unwrap();
            buffer
                .push_batch_async_timeout(vec![1, 2, 3], Duration::from_millis(100))
                .await
                .unwrap();
            assert_eq!(
                buffer
                    .pop_batch_async_timeout(3, Duration::from_millis(100))
                    .await
                    .unwrap(),
                vec![1, 2, 3]
            );

            // Pop from empty buffer, should timeout
            let result = buffer
                .pop_batch_async_timeout(1, Duration::from_millis(100))
                .await;
            // Original test expected Timeout. However, pop_batch_async (and pop_async)
            // now return Empty immediately if buffer is empty, they don't wait.
            // So, the timeout wrapper will return the Empty error before its own timeout fires.
            assert!(
                matches!(result, Err(BufferError::Empty)),
                "Expected Empty, got {:?}",
                result
            );
        }
    }
}
