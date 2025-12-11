//! Metrics and observability integration for ElasticQ.
//!
//! This module provides integration with the `metrics` crate for production
//! monitoring and observability. It exposes key metrics as counters, gauges,
//! and histograms that can be collected by various backends (Prometheus, etc.).

use metrics_crate::{counter, gauge, histogram, describe_counter, describe_gauge, describe_histogram, Unit};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

/// Metric names as constants
pub mod names {
    /// Counter: Total messages enqueued
    pub const MESSAGES_ENQUEUED: &str = "elasticq_messages_enqueued_total";
    /// Counter: Total messages dequeued
    pub const MESSAGES_DEQUEUED: &str = "elasticq_messages_dequeued_total";
    /// Counter: Total messages dropped (due to capacity limits)
    pub const MESSAGES_DROPPED: &str = "elasticq_messages_dropped_total";
    /// Gauge: Current queue depth
    pub const QUEUE_DEPTH: &str = "elasticq_queue_depth";
    /// Gauge: Current queue capacity
    pub const QUEUE_CAPACITY: &str = "elasticq_queue_capacity";
    /// Counter: Total resize operations
    pub const RESIZE_OPERATIONS: &str = "elasticq_resize_operations_total";
    /// Histogram: Enqueue latency in seconds
    pub const ENQUEUE_LATENCY: &str = "elasticq_enqueue_latency_seconds";
    /// Histogram: Dequeue latency in seconds
    pub const DEQUEUE_LATENCY: &str = "elasticq_dequeue_latency_seconds";
    /// Counter: Backpressure events (when buffer is full)
    pub const BACKPRESSURE_EVENTS: &str = "elasticq_backpressure_events_total";
    /// Gauge: Queue utilization ratio (0.0 - 1.0)
    pub const QUEUE_UTILIZATION: &str = "elasticq_queue_utilization_ratio";
}

/// Register metric descriptions with the metrics registry.
///
/// Call this once at application startup to provide descriptions for all metrics.
///
/// # Example
///
/// ```ignore
/// use elasticq::metrics::register_metrics;
///
/// fn main() {
///     // Initialize your metrics backend (e.g., prometheus)
///     // ...
///
///     // Register ElasticQ metric descriptions
///     register_metrics();
/// }
/// ```
pub fn register_metrics() {
    describe_counter!(
        names::MESSAGES_ENQUEUED,
        Unit::Count,
        "Total number of messages successfully enqueued"
    );
    describe_counter!(
        names::MESSAGES_DEQUEUED,
        Unit::Count,
        "Total number of messages successfully dequeued"
    );
    describe_counter!(
        names::MESSAGES_DROPPED,
        Unit::Count,
        "Total number of messages dropped due to capacity limits"
    );
    describe_gauge!(
        names::QUEUE_DEPTH,
        Unit::Count,
        "Current number of messages in the queue"
    );
    describe_gauge!(
        names::QUEUE_CAPACITY,
        Unit::Count,
        "Current capacity of the queue"
    );
    describe_counter!(
        names::RESIZE_OPERATIONS,
        Unit::Count,
        "Total number of queue resize operations"
    );
    describe_histogram!(
        names::ENQUEUE_LATENCY,
        Unit::Seconds,
        "Time taken to enqueue a message"
    );
    describe_histogram!(
        names::DEQUEUE_LATENCY,
        Unit::Seconds,
        "Time taken to dequeue a message"
    );
    describe_counter!(
        names::BACKPRESSURE_EVENTS,
        Unit::Count,
        "Number of times enqueue was rejected due to full queue"
    );
    describe_gauge!(
        names::QUEUE_UTILIZATION,
        Unit::Count,
        "Queue utilization ratio (depth / capacity)"
    );
}

/// A metrics recorder for a specific buffer instance.
///
/// This struct wraps a buffer and automatically records metrics for all operations.
/// Uses static queue name labels for efficient metric recording.
///
/// # Example
///
/// ```ignore
/// use elasticq::{DynamicCircularBuffer, Config};
/// use elasticq::metrics::MetricsRecorder;
///
/// let buffer = DynamicCircularBuffer::<i32>::new(Config::default()).unwrap();
/// let recorder = MetricsRecorder::new("my_queue");
///
/// // Record a push operation
/// let start = std::time::Instant::now();
/// buffer.push(42).unwrap();
/// recorder.record_enqueue(start);
///
/// // Or use the instrumented wrapper
/// let instrumented = recorder.wrap(buffer);
/// instrumented.push(42).unwrap(); // Metrics recorded automatically
/// ```
#[derive(Clone)]
pub struct MetricsRecorder {
    /// Queue name for labeling
    name: &'static str,
    /// Whether metrics are enabled
    enabled: Arc<AtomicBool>,
}

impl MetricsRecorder {
    /// Create a new metrics recorder with the given queue name.
    ///
    /// Note: The name must be a static string for efficient metric recording.
    pub fn new(name: &'static str) -> Self {
        Self {
            name,
            enabled: Arc::new(AtomicBool::new(true)),
        }
    }

    /// Enable or disable metrics recording.
    pub fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::Relaxed);
    }

    /// Check if metrics recording is enabled.
    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    /// Get the queue name.
    pub fn name(&self) -> &'static str {
        self.name
    }

    /// Record a successful enqueue operation.
    pub fn record_enqueue(&self, start: Instant) {
        if !self.is_enabled() {
            return;
        }

        let duration = start.elapsed();
        counter!(names::MESSAGES_ENQUEUED, "queue" => self.name).increment(1);
        histogram!(names::ENQUEUE_LATENCY, "queue" => self.name).record(duration.as_secs_f64());
    }

    /// Record a successful dequeue operation.
    pub fn record_dequeue(&self, start: Instant) {
        if !self.is_enabled() {
            return;
        }

        let duration = start.elapsed();
        counter!(names::MESSAGES_DEQUEUED, "queue" => self.name).increment(1);
        histogram!(names::DEQUEUE_LATENCY, "queue" => self.name).record(duration.as_secs_f64());
    }

    /// Record a dropped message (due to capacity limits).
    pub fn record_dropped(&self) {
        if !self.is_enabled() {
            return;
        }

        counter!(names::MESSAGES_DROPPED, "queue" => self.name).increment(1);
    }

    /// Record a backpressure event (enqueue rejected due to full queue).
    pub fn record_backpressure(&self) {
        if !self.is_enabled() {
            return;
        }

        counter!(names::BACKPRESSURE_EVENTS, "queue" => self.name).increment(1);
    }

    /// Record a resize operation.
    pub fn record_resize(&self) {
        if !self.is_enabled() {
            return;
        }

        counter!(names::RESIZE_OPERATIONS, "queue" => self.name).increment(1);
    }

    /// Update queue depth gauge.
    pub fn set_queue_depth(&self, depth: usize) {
        if !self.is_enabled() {
            return;
        }

        gauge!(names::QUEUE_DEPTH, "queue" => self.name).set(depth as f64);
    }

    /// Update queue capacity gauge.
    pub fn set_queue_capacity(&self, capacity: usize) {
        if !self.is_enabled() {
            return;
        }

        gauge!(names::QUEUE_CAPACITY, "queue" => self.name).set(capacity as f64);
    }

    /// Update queue utilization ratio.
    pub fn set_utilization(&self, depth: usize, capacity: usize) {
        if !self.is_enabled() {
            return;
        }

        let utilization = if capacity > 0 {
            depth as f64 / capacity as f64
        } else {
            0.0
        };
        gauge!(names::QUEUE_UTILIZATION, "queue" => self.name).set(utilization);
    }

    /// Record a batch of enqueue operations.
    pub fn record_batch_enqueue(&self, count: usize, start: Instant) {
        if !self.is_enabled() {
            return;
        }

        let duration = start.elapsed();
        counter!(names::MESSAGES_ENQUEUED, "queue" => self.name).increment(count as u64);
        // Record average latency per item
        let per_item_duration = duration.as_secs_f64() / count as f64;
        histogram!(names::ENQUEUE_LATENCY, "queue" => self.name).record(per_item_duration);
    }

    /// Record a batch of dequeue operations.
    pub fn record_batch_dequeue(&self, count: usize, start: Instant) {
        if !self.is_enabled() {
            return;
        }

        let duration = start.elapsed();
        counter!(names::MESSAGES_DEQUEUED, "queue" => self.name).increment(count as u64);
        let per_item_duration = duration.as_secs_f64() / count as f64;
        histogram!(names::DEQUEUE_LATENCY, "queue" => self.name).record(per_item_duration);
    }

    /// Create an instrumented buffer wrapper.
    pub fn wrap<T>(&self, buffer: crate::DynamicCircularBuffer<T>) -> InstrumentedBuffer<T> {
        InstrumentedBuffer {
            buffer,
            recorder: self.clone(),
        }
    }

    /// Create an instrumented buffer wrapper from an Arc.
    pub fn wrap_arc<T>(
        &self,
        buffer: std::sync::Arc<crate::DynamicCircularBuffer<T>>,
    ) -> InstrumentedBufferRef<T> {
        InstrumentedBufferRef {
            buffer,
            recorder: self.clone(),
        }
    }
}

/// A buffer wrapper that automatically records metrics.
pub struct InstrumentedBuffer<T> {
    buffer: crate::DynamicCircularBuffer<T>,
    recorder: MetricsRecorder,
}

impl<T: Send + Sync + 'static> InstrumentedBuffer<T> {
    /// Push an item with automatic metrics recording.
    pub fn push(&self, item: T) -> crate::BufferResult<()> {
        let start = Instant::now();
        let result = self.buffer.push(item);

        match &result {
            Ok(()) => {
                self.recorder.record_enqueue(start);
                self.update_gauges();
            }
            Err(crate::BufferError::Full) | Err(crate::BufferError::MaxCapacityReached(_)) => {
                self.recorder.record_backpressure();
            }
            _ => {}
        }

        result
    }

    /// Pop an item with automatic metrics recording.
    pub fn pop(&self) -> crate::BufferResult<T> {
        let start = Instant::now();
        let result = self.buffer.pop();

        if result.is_ok() {
            self.recorder.record_dequeue(start);
            self.update_gauges();
        }

        result
    }

    /// Push a batch with automatic metrics recording.
    pub fn push_batch(&self, items: Vec<T>) -> crate::BufferResult<()> {
        let count = items.len();
        let start = Instant::now();
        let result = self.buffer.push_batch(items);

        match &result {
            Ok(()) => {
                self.recorder.record_batch_enqueue(count, start);
                self.update_gauges();
            }
            Err(crate::BufferError::Full) | Err(crate::BufferError::MaxCapacityReached(_)) => {
                self.recorder.record_backpressure();
            }
            _ => {}
        }

        result
    }

    /// Pop a batch with automatic metrics recording.
    pub fn pop_batch(&self, max_items: usize) -> crate::BufferResult<Vec<T>> {
        let start = Instant::now();
        let result = self.buffer.pop_batch(max_items);

        if let Ok(ref items) = result {
            if !items.is_empty() {
                self.recorder.record_batch_dequeue(items.len(), start);
                self.update_gauges();
            }
        }

        result
    }

    /// Get the current queue length.
    pub fn len(&self) -> usize {
        self.buffer.len()
    }

    /// Check if the queue is empty.
    pub fn is_empty(&self) -> bool {
        self.buffer.is_empty()
    }

    /// Get the current capacity.
    pub fn capacity(&self) -> usize {
        self.buffer.capacity()
    }

    /// Get a reference to the underlying buffer.
    pub fn inner(&self) -> &crate::DynamicCircularBuffer<T> {
        &self.buffer
    }

    /// Get the metrics recorder.
    pub fn recorder(&self) -> &MetricsRecorder {
        &self.recorder
    }

    fn update_gauges(&self) {
        let depth = self.buffer.len();
        let capacity = self.buffer.capacity();
        self.recorder.set_queue_depth(depth);
        self.recorder.set_queue_capacity(capacity);
        self.recorder.set_utilization(depth, capacity);
    }
}

/// A buffer reference wrapper that automatically records metrics.
pub struct InstrumentedBufferRef<T> {
    buffer: std::sync::Arc<crate::DynamicCircularBuffer<T>>,
    recorder: MetricsRecorder,
}

impl<T> Clone for InstrumentedBufferRef<T> {
    fn clone(&self) -> Self {
        Self {
            buffer: self.buffer.clone(),
            recorder: self.recorder.clone(),
        }
    }
}

impl<T: Send + Sync + 'static> InstrumentedBufferRef<T> {
    /// Push an item with automatic metrics recording.
    pub fn push(&self, item: T) -> crate::BufferResult<()> {
        let start = Instant::now();
        let result = self.buffer.push(item);

        match &result {
            Ok(()) => {
                self.recorder.record_enqueue(start);
                self.update_gauges();
            }
            Err(crate::BufferError::Full) | Err(crate::BufferError::MaxCapacityReached(_)) => {
                self.recorder.record_backpressure();
            }
            _ => {}
        }

        result
    }

    /// Pop an item with automatic metrics recording.
    pub fn pop(&self) -> crate::BufferResult<T> {
        let start = Instant::now();
        let result = self.buffer.pop();

        if result.is_ok() {
            self.recorder.record_dequeue(start);
            self.update_gauges();
        }

        result
    }

    /// Push a batch with automatic metrics recording.
    pub fn push_batch(&self, items: Vec<T>) -> crate::BufferResult<()> {
        let count = items.len();
        let start = Instant::now();
        let result = self.buffer.push_batch(items);

        match &result {
            Ok(()) => {
                self.recorder.record_batch_enqueue(count, start);
                self.update_gauges();
            }
            Err(crate::BufferError::Full) | Err(crate::BufferError::MaxCapacityReached(_)) => {
                self.recorder.record_backpressure();
            }
            _ => {}
        }

        result
    }

    /// Pop a batch with automatic metrics recording.
    pub fn pop_batch(&self, max_items: usize) -> crate::BufferResult<Vec<T>> {
        let start = Instant::now();
        let result = self.buffer.pop_batch(max_items);

        if let Ok(ref items) = result {
            if !items.is_empty() {
                self.recorder.record_batch_dequeue(items.len(), start);
                self.update_gauges();
            }
        }

        result
    }

    /// Get the current queue length.
    pub fn len(&self) -> usize {
        self.buffer.len()
    }

    /// Check if the queue is empty.
    pub fn is_empty(&self) -> bool {
        self.buffer.is_empty()
    }

    /// Get the current capacity.
    pub fn capacity(&self) -> usize {
        self.buffer.capacity()
    }

    /// Get a reference to the underlying buffer.
    pub fn inner(&self) -> &std::sync::Arc<crate::DynamicCircularBuffer<T>> {
        &self.buffer
    }

    /// Get the metrics recorder.
    pub fn recorder(&self) -> &MetricsRecorder {
        &self.recorder
    }

    fn update_gauges(&self) {
        let depth = self.buffer.len();
        let capacity = self.buffer.capacity();
        self.recorder.set_queue_depth(depth);
        self.recorder.set_queue_capacity(capacity);
        self.recorder.set_utilization(depth, capacity);
    }
}

/// A timer guard that automatically records duration on drop.
pub struct TimerGuard<'a> {
    recorder: &'a MetricsRecorder,
    start: Instant,
    operation: TimerOperation,
    cancelled: bool,
}

enum TimerOperation {
    Enqueue,
    Dequeue,
}

impl<'a> TimerGuard<'a> {
    /// Create a new timer for enqueue operations.
    pub fn enqueue(recorder: &'a MetricsRecorder) -> Self {
        Self {
            recorder,
            start: Instant::now(),
            operation: TimerOperation::Enqueue,
            cancelled: false,
        }
    }

    /// Create a new timer for dequeue operations.
    pub fn dequeue(recorder: &'a MetricsRecorder) -> Self {
        Self {
            recorder,
            start: Instant::now(),
            operation: TimerOperation::Dequeue,
            cancelled: false,
        }
    }

    /// Complete the timer and record metrics.
    pub fn complete(self) {
        // Drop will handle the recording
    }

    /// Cancel the timer without recording metrics.
    pub fn cancel(mut self) {
        self.cancelled = true;
    }
}

impl<'a> Drop for TimerGuard<'a> {
    fn drop(&mut self) {
        if self.cancelled {
            return;
        }
        match self.operation {
            TimerOperation::Enqueue => self.recorder.record_enqueue(self.start),
            TimerOperation::Dequeue => self.recorder.record_dequeue(self.start),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn test_metrics_recorder_creation() {
        let recorder = MetricsRecorder::new("test_queue");
        assert_eq!(recorder.name(), "test_queue");
        assert!(recorder.is_enabled());
    }

    #[test]
    fn test_metrics_enable_disable() {
        let recorder = MetricsRecorder::new("test_queue");
        assert!(recorder.is_enabled());

        recorder.set_enabled(false);
        assert!(!recorder.is_enabled());

        recorder.set_enabled(true);
        assert!(recorder.is_enabled());
    }

    #[test]
    fn test_instrumented_buffer() {
        use crate::{Config, DynamicCircularBuffer};

        let buffer = DynamicCircularBuffer::<i32>::new(Config::default()).unwrap();
        let recorder = MetricsRecorder::new("test");
        let instrumented = recorder.wrap(buffer);

        instrumented.push(1).unwrap();
        instrumented.push(2).unwrap();

        assert_eq!(instrumented.len(), 2);
        assert_eq!(instrumented.pop().unwrap(), 1);
        assert_eq!(instrumented.len(), 1);
    }

    #[test]
    fn test_timer_guard() {
        let recorder = MetricsRecorder::new("test");

        // Test enqueue timer
        {
            let _guard = TimerGuard::enqueue(&recorder);
            std::thread::sleep(Duration::from_millis(1));
            // Timer records on drop
        }

        // Test dequeue timer
        {
            let guard = TimerGuard::dequeue(&recorder);
            std::thread::sleep(Duration::from_millis(1));
            guard.complete();
        }

        // Test cancel
        {
            let guard = TimerGuard::enqueue(&recorder);
            guard.cancel(); // Should not record
        }
    }

    #[test]
    fn test_batch_operations() {
        use crate::{Config, DynamicCircularBuffer};

        let buffer = DynamicCircularBuffer::<i32>::new(Config::default()).unwrap();
        let recorder = MetricsRecorder::new("test");
        let instrumented = recorder.wrap(buffer);

        instrumented.push_batch(vec![1, 2, 3, 4, 5]).unwrap();
        assert_eq!(instrumented.len(), 5);

        let items = instrumented.pop_batch(3).unwrap();
        assert_eq!(items, vec![1, 2, 3]);
        assert_eq!(instrumented.len(), 2);
    }
}
