//! Priority Queue implementation with multiple priority levels.
//!
//! This module provides a priority-aware circular buffer that maintains
//! separate queues for different priority levels, useful for QoS-based
//! message processing like MQTT.

use crate::{BufferError, BufferResult, Config};
use parking_lot::{Mutex, RwLock};
use std::collections::VecDeque;
use std::sync::Arc;

/// Default number of priority levels (matches MQTT QoS levels: 0, 1, 2)
pub const DEFAULT_PRIORITY_LEVELS: usize = 3;

/// Priority level type (higher values = higher priority)
pub type Priority = u8;

/// Configuration for the priority buffer
#[derive(Clone, Debug)]
pub struct PriorityConfig {
    /// Base configuration for capacity and resize behavior
    pub base: Config,
    /// Number of priority levels (default: 3 for MQTT QoS)
    pub priority_levels: usize,
    /// Enable weighted fair queuing to prevent starvation
    pub fair_queuing: bool,
    /// Maximum consecutive items to pop from a single priority level
    /// when fair_queuing is enabled (default: 10)
    pub max_consecutive_per_priority: usize,
}

impl Default for PriorityConfig {
    fn default() -> Self {
        Self {
            base: Config::default(),
            priority_levels: DEFAULT_PRIORITY_LEVELS,
            fair_queuing: true,
            max_consecutive_per_priority: 10,
        }
    }
}

impl PriorityConfig {
    /// Create a new priority config with default settings
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the base configuration
    pub fn with_base_config(mut self, config: Config) -> Self {
        self.base = config;
        self
    }

    /// Set the number of priority levels
    pub fn with_priority_levels(mut self, levels: usize) -> Self {
        self.priority_levels = levels;
        self
    }

    /// Enable or disable fair queuing
    pub fn with_fair_queuing(mut self, enabled: bool) -> Self {
        self.fair_queuing = enabled;
        self
    }

    /// Set maximum consecutive items per priority level
    pub fn with_max_consecutive(mut self, max: usize) -> Self {
        self.max_consecutive_per_priority = max;
        self
    }

    /// Validate the configuration
    pub fn validate(&self) -> BufferResult<()> {
        self.base.validate()?;
        if self.priority_levels == 0 {
            return Err(BufferError::InvalidConfiguration(
                "priority_levels must be greater than 0".to_string(),
            ));
        }
        if self.priority_levels > 256 {
            return Err(BufferError::InvalidConfiguration(
                "priority_levels must be <= 256".to_string(),
            ));
        }
        if self.max_consecutive_per_priority == 0 {
            return Err(BufferError::InvalidConfiguration(
                "max_consecutive_per_priority must be greater than 0".to_string(),
            ));
        }
        Ok(())
    }
}

/// Statistics for the priority buffer
#[derive(Clone, Debug, Default)]
pub struct PriorityStats {
    /// Total items pushed
    pub total_pushed: u64,
    /// Total items popped
    pub total_popped: u64,
    /// Items pushed per priority level
    pub pushed_per_priority: Vec<u64>,
    /// Items popped per priority level
    pub popped_per_priority: Vec<u64>,
    /// Current length per priority level
    pub len_per_priority: Vec<usize>,
}

/// A thread-safe priority queue with multiple priority levels.
///
/// Higher priority values indicate higher priority (will be dequeued first).
/// Supports configurable fair queuing to prevent starvation of lower priorities.
///
/// # Example
///
/// ```
/// use elasticq::priority::{PriorityCircularBuffer, PriorityConfig};
///
/// // Disable fair queuing for strict priority ordering
/// let config = PriorityConfig::default().with_fair_queuing(false);
/// let buffer = PriorityCircularBuffer::<String>::new(config).unwrap();
///
/// // Push with different priorities (0 = low, 1 = medium, 2 = high)
/// buffer.push_with_priority("low priority".to_string(), 0).unwrap();
/// buffer.push_with_priority("high priority".to_string(), 2).unwrap();
/// buffer.push_with_priority("medium priority".to_string(), 1).unwrap();
///
/// // Pop returns highest priority first
/// assert_eq!(buffer.pop().unwrap(), "high priority");
/// assert_eq!(buffer.pop().unwrap(), "medium priority");
/// assert_eq!(buffer.pop().unwrap(), "low priority");
/// ```
pub struct PriorityCircularBuffer<T> {
    /// Separate queue for each priority level (index = priority)
    queues: Vec<Mutex<VecDeque<T>>>,
    /// Current logical capacity per queue
    capacities: Vec<RwLock<usize>>,
    /// Configuration
    config: PriorityConfig,
    /// Statistics
    stats: Mutex<PriorityStats>,
    /// Fair queuing state: consecutive pops from current priority
    fair_queue_state: Mutex<FairQueueState>,
}

/// State for fair queuing algorithm
struct FairQueueState {
    /// Current priority being served
    current_priority: usize,
    /// Consecutive items popped from current priority
    consecutive_count: usize,
}

impl Default for FairQueueState {
    fn default() -> Self {
        Self {
            current_priority: 0,
            consecutive_count: 0,
        }
    }
}

/// Thread-safe wrapper type
pub type ThreadSafePriorityCircularBuffer<T> = Arc<PriorityCircularBuffer<T>>;

impl<T> PriorityCircularBuffer<T> {
    /// Creates a new priority buffer with the given configuration.
    pub fn new(config: PriorityConfig) -> BufferResult<Self> {
        config.validate()?;

        let initial_capacity = config.base.initial_capacity;
        let num_levels = config.priority_levels;

        let queues: Vec<_> = (0..num_levels)
            .map(|_| Mutex::new(VecDeque::with_capacity(initial_capacity)))
            .collect();

        let capacities: Vec<_> = (0..num_levels)
            .map(|_| RwLock::new(initial_capacity))
            .collect();

        let stats = PriorityStats {
            pushed_per_priority: vec![0; num_levels],
            popped_per_priority: vec![0; num_levels],
            len_per_priority: vec![0; num_levels],
            ..Default::default()
        };

        Ok(Self {
            queues,
            capacities,
            config,
            stats: Mutex::new(stats),
            fair_queue_state: Mutex::new(FairQueueState::default()),
        })
    }

    /// Returns the number of priority levels.
    pub fn priority_levels(&self) -> usize {
        self.config.priority_levels
    }

    /// Returns the total number of items across all priority levels.
    pub fn len(&self) -> usize {
        self.queues.iter().map(|q| q.lock().len()).sum()
    }

    /// Returns true if all queues are empty.
    pub fn is_empty(&self) -> bool {
        self.queues.iter().all(|q| q.lock().is_empty())
    }

    /// Returns the number of items at a specific priority level.
    pub fn len_at_priority(&self, priority: Priority) -> usize {
        let idx = priority as usize;
        if idx >= self.queues.len() {
            return 0;
        }
        self.queues[idx].lock().len()
    }

    /// Returns the current capacity for a specific priority level.
    pub fn capacity_at_priority(&self, priority: Priority) -> usize {
        let idx = priority as usize;
        if idx >= self.capacities.len() {
            return 0;
        }
        *self.capacities[idx].read()
    }

    /// Returns statistics about the buffer.
    pub fn stats(&self) -> PriorityStats {
        let mut stats = self.stats.lock().clone();
        // Update current lengths
        for (i, q) in self.queues.iter().enumerate() {
            stats.len_per_priority[i] = q.lock().len();
        }
        stats
    }

    /// Clears all priority queues.
    pub fn clear(&self) {
        for (i, queue) in self.queues.iter().enumerate() {
            let mut guard = queue.lock();
            guard.clear();
            guard.shrink_to(0);

            let mut cap_guard = self.capacities[i].write();
            *cap_guard = self.config.base.initial_capacity.max(self.config.base.min_capacity);
            guard.reserve(self.config.base.initial_capacity);
        }
    }
}

impl<T: Send + Sync + 'static> PriorityCircularBuffer<T> {
    /// Pushes an item with the specified priority.
    ///
    /// Priority is clamped to the valid range [0, priority_levels - 1].
    pub fn push_with_priority(&self, item: T, priority: Priority) -> BufferResult<()> {
        let idx = (priority as usize).min(self.config.priority_levels - 1);

        let mut queue_guard = self.queues[idx].lock();

        // Check against max_capacity
        if queue_guard.len() >= self.config.base.max_capacity {
            return Err(BufferError::MaxCapacityReached(self.config.base.max_capacity));
        }

        let current_len = queue_guard.len();
        let current_cap = *self.capacities[idx].read();

        // Check if we need to grow
        if current_len >= current_cap && current_cap < self.config.base.max_capacity {
            let mut cap_guard = self.capacities[idx].write();
            let new_cap = ((*cap_guard as f64) * self.config.base.growth_factor).ceil() as usize;
            let new_cap = new_cap.min(self.config.base.max_capacity);
            let current_physical_cap = queue_guard.capacity();
            if new_cap > current_physical_cap {
                queue_guard.reserve(new_cap - current_physical_cap);
            }
            *cap_guard = new_cap;
        }

        queue_guard.push_back(item);

        // Update stats
        let mut stats = self.stats.lock();
        stats.total_pushed += 1;
        stats.pushed_per_priority[idx] += 1;

        Ok(())
    }

    /// Pushes an item with the highest priority level.
    pub fn push_high_priority(&self, item: T) -> BufferResult<()> {
        let max_priority = (self.config.priority_levels - 1) as Priority;
        self.push_with_priority(item, max_priority)
    }

    /// Pushes an item with the lowest priority level (0).
    pub fn push_low_priority(&self, item: T) -> BufferResult<()> {
        self.push_with_priority(item, 0)
    }

    /// Pushes an item with default priority (middle priority level).
    pub fn push(&self, item: T) -> BufferResult<()> {
        let default_priority = (self.config.priority_levels / 2) as Priority;
        self.push_with_priority(item, default_priority)
    }

    /// Pops the highest priority item.
    ///
    /// If fair_queuing is enabled, this implements weighted fair queuing
    /// to prevent starvation of lower priority items.
    pub fn pop(&self) -> BufferResult<T> {
        if self.config.fair_queuing {
            self.pop_fair()
        } else {
            self.pop_strict()
        }
    }

    /// Pops using strict priority ordering (highest priority first, always).
    fn pop_strict(&self) -> BufferResult<T> {
        // Iterate from highest priority to lowest
        for idx in (0..self.config.priority_levels).rev() {
            let mut queue_guard = self.queues[idx].lock();
            if let Some(item) = queue_guard.pop_front() {
                self.maybe_shrink(idx, &mut queue_guard);

                let mut stats = self.stats.lock();
                stats.total_popped += 1;
                stats.popped_per_priority[idx] += 1;

                return Ok(item);
            }
        }
        Err(BufferError::Empty)
    }

    /// Pops using fair queuing to prevent starvation.
    fn pop_fair(&self) -> BufferResult<T> {
        let mut state = self.fair_queue_state.lock();

        // Try current priority first if we haven't exceeded max_consecutive
        if state.consecutive_count < self.config.max_consecutive_per_priority {
            let idx = state.current_priority;
            if idx < self.queues.len() {
                let mut queue_guard = self.queues[idx].lock();
                if let Some(item) = queue_guard.pop_front() {
                    state.consecutive_count += 1;
                    self.maybe_shrink(idx, &mut queue_guard);

                    let mut stats = self.stats.lock();
                    stats.total_popped += 1;
                    stats.popped_per_priority[idx] += 1;

                    return Ok(item);
                }
            }
        }

        // Move to next priority level (round-robin from high to low, then wrap)
        let start_priority = state.current_priority;
        for offset in 1..=self.config.priority_levels {
            // Prioritize higher priorities by checking them first
            let idx = if start_priority >= offset {
                start_priority - offset
            } else {
                self.config.priority_levels - 1 - (offset - start_priority - 1) % self.config.priority_levels
            };

            if idx < self.queues.len() {
                let mut queue_guard = self.queues[idx].lock();
                if let Some(item) = queue_guard.pop_front() {
                    state.current_priority = idx;
                    state.consecutive_count = 1;
                    self.maybe_shrink(idx, &mut queue_guard);

                    let mut stats = self.stats.lock();
                    stats.total_popped += 1;
                    stats.popped_per_priority[idx] += 1;

                    return Ok(item);
                }
            }
        }

        Err(BufferError::Empty)
    }

    /// Pops from a specific priority level only.
    pub fn pop_from_priority(&self, priority: Priority) -> BufferResult<T> {
        let idx = priority as usize;
        if idx >= self.queues.len() {
            return Err(BufferError::InvalidOperation(format!(
                "Priority {} exceeds maximum {}",
                priority,
                self.config.priority_levels - 1
            )));
        }

        let mut queue_guard = self.queues[idx].lock();
        if let Some(item) = queue_guard.pop_front() {
            self.maybe_shrink(idx, &mut queue_guard);

            let mut stats = self.stats.lock();
            stats.total_popped += 1;
            stats.popped_per_priority[idx] += 1;

            Ok(item)
        } else {
            Err(BufferError::Empty)
        }
    }

    /// Push a batch of items with the same priority.
    pub fn push_batch_with_priority(&self, items: Vec<T>, priority: Priority) -> BufferResult<()> {
        if items.is_empty() {
            return Ok(());
        }

        let idx = (priority as usize).min(self.config.priority_levels - 1);
        let num_items = items.len();

        let mut queue_guard = self.queues[idx].lock();
        let potential_len = queue_guard.len() + num_items;

        if potential_len > self.config.base.max_capacity {
            return Err(BufferError::MaxCapacityReached(self.config.base.max_capacity));
        }

        let current_cap = *self.capacities[idx].read();

        // Check if we need to grow
        if potential_len > current_cap && current_cap < self.config.base.max_capacity {
            let mut cap_guard = self.capacities[idx].write();
            let mut new_cap = *cap_guard;
            while new_cap < potential_len && new_cap < self.config.base.max_capacity {
                new_cap = ((new_cap as f64) * self.config.base.growth_factor).ceil() as usize;
            }
            new_cap = new_cap.min(self.config.base.max_capacity);
            let current_physical_cap = queue_guard.capacity();
            if new_cap > current_physical_cap {
                queue_guard.reserve(new_cap - current_physical_cap);
            }
            *cap_guard = new_cap;
        }

        queue_guard.extend(items);

        let mut stats = self.stats.lock();
        stats.total_pushed += num_items as u64;
        stats.pushed_per_priority[idx] += num_items as u64;

        Ok(())
    }

    /// Pop a batch of items, respecting priority ordering.
    pub fn pop_batch(&self, max_items: usize) -> BufferResult<Vec<T>> {
        if max_items == 0 {
            return Ok(Vec::new());
        }

        let mut result = Vec::with_capacity(max_items);

        for _ in 0..max_items {
            match self.pop() {
                Ok(item) => result.push(item),
                Err(BufferError::Empty) => break,
                Err(e) => return Err(e),
            }
        }

        Ok(result)
    }

    /// Helper to potentially shrink a queue after popping
    fn maybe_shrink(&self, idx: usize, queue_guard: &mut VecDeque<T>) {
        let current_len = queue_guard.len();
        let current_cap = *self.capacities[idx].read();

        let shrink_threshold = (current_cap as f64 * self.config.base.shrink_threshold) as usize;

        if current_len <= shrink_threshold && current_cap > self.config.base.min_capacity {
            let mut cap_guard = self.capacities[idx].write();
            let new_cap = ((*cap_guard as f64) / self.config.base.growth_factor).floor() as usize;
            let new_cap = new_cap.max(current_len).max(self.config.base.min_capacity);

            if new_cap < *cap_guard {
                queue_guard.shrink_to(new_cap);
                *cap_guard = new_cap;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_basic_priority_ordering() {
        let config = PriorityConfig::default().with_fair_queuing(false);
        let buffer = PriorityCircularBuffer::<i32>::new(config).unwrap();

        // Push items with different priorities
        buffer.push_with_priority(1, 0).unwrap(); // Low
        buffer.push_with_priority(2, 1).unwrap(); // Medium
        buffer.push_with_priority(3, 2).unwrap(); // High

        // Should pop in priority order (highest first)
        assert_eq!(buffer.pop().unwrap(), 3);
        assert_eq!(buffer.pop().unwrap(), 2);
        assert_eq!(buffer.pop().unwrap(), 1);
    }

    #[test]
    fn test_fifo_within_priority() {
        let config = PriorityConfig::default().with_fair_queuing(false);
        let buffer = PriorityCircularBuffer::<i32>::new(config).unwrap();

        // Push multiple items at same priority
        buffer.push_with_priority(1, 1).unwrap();
        buffer.push_with_priority(2, 1).unwrap();
        buffer.push_with_priority(3, 1).unwrap();

        // Should be FIFO within same priority
        assert_eq!(buffer.pop().unwrap(), 1);
        assert_eq!(buffer.pop().unwrap(), 2);
        assert_eq!(buffer.pop().unwrap(), 3);
    }

    #[test]
    fn test_push_high_low_priority() {
        let config = PriorityConfig::default()
            .with_priority_levels(5)
            .with_fair_queuing(false);
        let buffer = PriorityCircularBuffer::<&str>::new(config).unwrap();

        buffer.push_low_priority("low").unwrap();
        buffer.push_high_priority("high").unwrap();
        buffer.push("default").unwrap(); // middle priority (2)

        assert_eq!(buffer.pop().unwrap(), "high");
        assert_eq!(buffer.pop().unwrap(), "default");
        assert_eq!(buffer.pop().unwrap(), "low");
    }

    #[test]
    fn test_pop_from_specific_priority() {
        let config = PriorityConfig::default().with_fair_queuing(false);
        let buffer = PriorityCircularBuffer::<i32>::new(config).unwrap();

        buffer.push_with_priority(1, 0).unwrap();
        buffer.push_with_priority(2, 1).unwrap();
        buffer.push_with_priority(3, 2).unwrap();

        // Pop specifically from priority 1
        assert_eq!(buffer.pop_from_priority(1).unwrap(), 2);

        // Other priorities still have items
        assert_eq!(buffer.len_at_priority(0), 1);
        assert_eq!(buffer.len_at_priority(2), 1);
    }

    #[test]
    fn test_statistics() {
        let config = PriorityConfig::default();
        let buffer = PriorityCircularBuffer::<i32>::new(config).unwrap();

        buffer.push_with_priority(1, 0).unwrap();
        buffer.push_with_priority(2, 1).unwrap();
        buffer.push_with_priority(3, 2).unwrap();
        buffer.pop().unwrap();

        let stats = buffer.stats();
        assert_eq!(stats.total_pushed, 3);
        assert_eq!(stats.total_popped, 1);
        assert_eq!(stats.pushed_per_priority[0], 1);
        assert_eq!(stats.pushed_per_priority[1], 1);
        assert_eq!(stats.pushed_per_priority[2], 1);
    }

    #[test]
    fn test_batch_operations() {
        let config = PriorityConfig::default().with_fair_queuing(false);
        let buffer = PriorityCircularBuffer::<i32>::new(config).unwrap();

        // Push batch at high priority
        buffer.push_batch_with_priority(vec![1, 2, 3], 2).unwrap();
        // Push batch at low priority
        buffer.push_batch_with_priority(vec![4, 5, 6], 0).unwrap();

        assert_eq!(buffer.len(), 6);

        // Pop batch should get high priority first
        let items = buffer.pop_batch(4).unwrap();
        assert_eq!(items, vec![1, 2, 3, 4]);
    }

    #[test]
    fn test_empty_buffer() {
        let config = PriorityConfig::default();
        let buffer = PriorityCircularBuffer::<i32>::new(config).unwrap();

        assert!(buffer.is_empty());
        assert_eq!(buffer.len(), 0);
        assert!(matches!(buffer.pop(), Err(BufferError::Empty)));
    }

    #[test]
    fn test_clear() {
        let config = PriorityConfig::default();
        let buffer = PriorityCircularBuffer::<i32>::new(config).unwrap();

        buffer.push_with_priority(1, 0).unwrap();
        buffer.push_with_priority(2, 1).unwrap();
        buffer.push_with_priority(3, 2).unwrap();

        buffer.clear();

        assert!(buffer.is_empty());
        assert_eq!(buffer.len(), 0);
    }

    #[test]
    fn test_priority_clamping() {
        let config = PriorityConfig::default().with_priority_levels(3);
        let buffer = PriorityCircularBuffer::<i32>::new(config).unwrap();

        // Push with priority higher than max should be clamped
        buffer.push_with_priority(1, 100).unwrap();

        // Should be in highest priority queue (2)
        assert_eq!(buffer.len_at_priority(2), 1);
    }

    #[test]
    fn test_invalid_config() {
        let config = PriorityConfig::default().with_priority_levels(0);
        assert!(matches!(
            PriorityCircularBuffer::<i32>::new(config),
            Err(BufferError::InvalidConfiguration(_))
        ));
    }

    #[test]
    fn test_fair_queuing_prevents_starvation() {
        let config = PriorityConfig::default()
            .with_fair_queuing(true)
            .with_max_consecutive(2);
        let buffer = PriorityCircularBuffer::<i32>::new(config).unwrap();

        // Push many high priority items
        for i in 0..10 {
            buffer.push_with_priority(i, 2).unwrap();
        }
        // Push one low priority item
        buffer.push_with_priority(100, 0).unwrap();

        // With fair queuing, low priority item should eventually be served
        let mut found_low_priority = false;
        for _ in 0..11 {
            if let Ok(item) = buffer.pop() {
                if item == 100 {
                    found_low_priority = true;
                    break;
                }
            }
        }
        assert!(found_low_priority, "Low priority item should have been served due to fair queuing");
    }
}
