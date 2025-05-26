# elasticq

A thread-safe, dynamically resizable circular buffer (queue) for Rust, designed for high-throughput scenarios. Now featuring both **lock-based** and **lock-free** implementations optimized for different use cases.

## Features

### Core Features
*   **Elastic Sizing:** Automatically grows when full and shrinks when underutilized, within configurable limits.
*   **Thread-Safe:** Safe for concurrent use by multiple producers and consumers.
*   **Batch Operations:** Efficient `push_batch` and `pop_batch` methods for high-throughput.
*   **Asynchronous API (Optional):** Enable the `async` feature for `tokio`-based asynchronous methods.
*   **Configurable Behavior:** Fine-tune capacities, growth/shrink factors, and memory management.
*   **Clear Error Handling:** Provides distinct error types for conditions like buffer full/empty or timeouts.

### Implementation Variants

#### 🔒 **Lock-Based Implementation** (Default)
*   Uses `parking_lot` mutexes for synchronous operations
*   Optionally uses `tokio::sync` mutexes for asynchronous operations via the `async` feature
*   Excellent for general-purpose use with moderate concurrency
*   Predictable performance characteristics

#### 🚀 **Lock-Free Implementation** (New!)
*   **Zero-mutex MPSC queue** using atomic operations and epoch-based reclamation
*   **2.1x faster** than lock-based implementation in single-threaded scenarios
*   **46M+ messages/sec** throughput in optimized configurations
*   **Wait-free consumer operations** - no blocking or deadlocks possible
*   **Generation-based ABA protection** for safe concurrent operations
*   **Consumer-driven dynamic resizing** optimized for MQTT proxy use cases
*   Enable with the `lock_free` feature flag

## Table of Contents

1.  [Installation](#installation)
2.  [Quick Start](#quick-start)
    *   [Lock-Based Usage](#lock-based-usage-default)
    *   [Lock-Free Usage](#lock-free-usage-mpsc)
    *   [Asynchronous Usage](#asynchronous-usage)
3.  [Configuration](#configuration)
4.  [API Reference](#api-reference)
5.  [Performance Analysis](#performance-analysis)
    *   [Lock-Free vs Lock-Based Comparison](#lock-free-vs-lock-based-comparison)
    *   [Scalability Characteristics](#scalability-characteristics)
    *   [MQTT Proxy Benchmarks](#mqtt-proxy-benchmarks)
6.  [Formal Verification](#formal-verification)
7.  [Use Cases & Recommendations](#use-cases--recommendations)
8.  [Contributing](#contributing)
9.  [License](#license)

## Installation

### Basic Installation (Lock-Based)
```toml
[dependencies]
elasticq = "0.1.0"
```

### Lock-Free Implementation
```toml
[dependencies]
elasticq = { version = "0.1.0", features = ["lock_free"] }
```

### With Async Support
```toml
[dependencies]
elasticq = { version = "0.1.0", features = ["async"] }
tokio = { version = "1", features = ["sync", "time"] }
```

### All Features
```toml
[dependencies]
elasticq = { version = "0.1.0", features = ["async", "lock_free"] }
tokio = { version = "1", features = ["sync", "time"] }
```

## Quick Start

### Lock-Based Usage (Default)

```rust
use elasticq::{DynamicCircularBuffer, Config, BufferError};

fn main() -> Result<(), BufferError> {
    // Create buffer with default configuration
    let buffer = DynamicCircularBuffer::<i32>::new(Config::default())?;

    // Push some items
    buffer.push(10)?;
    buffer.push(20)?;
    println!("Buffer length: {}", buffer.len()); // Output: 2

    // Pop an item
    let item = buffer.pop()?;
    assert_eq!(item, 10);
    println!("Popped: {}", item);

    // Batch operations for higher throughput
    buffer.push_batch(vec![30, 40, 50])?;
    let items = buffer.pop_batch(2)?;
    assert_eq!(items, vec![20, 30]);

    Ok(())
}
```

### Lock-Free Usage (MPSC)

Perfect for MQTT proxy scenarios with multiple publishers and a single message processor:

```rust
use elasticq::{LockFreeMPSCQueue, Config, BufferError};
use std::sync::Arc;
use std::thread;

fn main() -> Result<(), BufferError> {
    // Configure for MQTT proxy use case
    let config = Config::default()
        .with_initial_capacity(1024)
        .with_max_capacity(1048576); // 1M messages max
    
    let queue = Arc::new(LockFreeMPSCQueue::new(config)?);

    // Multiple producers (MQTT publishers)
    let mut producers = vec![];
    for producer_id in 0..4 {
        let queue_clone = Arc::clone(&queue);
        let handle = thread::spawn(move || {
            for i in 0..1000 {
                let message = format!("msg_{}_{}", producer_id, i);
                // Non-blocking enqueue with retry
                while queue_clone.try_enqueue(message.clone()).is_err() {
                    thread::yield_now();
                }
            }
        });
        producers.push(handle);
    }

    // Single consumer (MQTT message processor)
    let queue_clone = Arc::clone(&queue);
    let consumer = thread::spawn(move || {
        let mut processed = 0;
        while processed < 4000 {
            match queue_clone.try_dequeue() {
                Ok(Some(message)) => {
                    // Process message
                    println!("Processing: {}", message);
                    processed += 1;
                }
                Ok(None) => thread::yield_now(), // Queue empty, yield
                Err(_) => thread::yield_now(),   // Resize in progress
            }
        }
    });

    // Wait for completion
    for handle in producers {
        handle.join().unwrap();
    }
    consumer.join().unwrap();

    // Check statistics
    let stats = queue.stats();
    println!("Final stats: {:?}", stats);
    
    Ok(())
}
```

### Asynchronous Usage

Make sure you have enabled the `async` feature and have `tokio` as a dependency.

```rust
use elasticq::{DynamicCircularBuffer, Config, BufferError};
use std::time::Duration;

#[tokio::main]
async fn main() -> Result<(), BufferError> {
    let buffer = DynamicCircularBuffer::<String>::new(Config::default())?;

    // Asynchronously push
    buffer.push_async("hello".to_string()).await?;
    buffer.push_async("world".to_string()).await?;

    // Asynchronously pop with timeout
    match buffer.pop_async_timeout(Duration::from_millis(100)).await {
        Ok(item) => println!("Popped async: {}", item), // Expected: "hello"
        Err(BufferError::Timeout(_)) => println!("Pop operation timed out"),
        Err(e) => return Err(e),
    }

    // Async batch operations
    let messages = vec!["batch1".to_string(), "batch2".to_string()];
    buffer.push_batch_async(messages).await?;

    let popped_batch = buffer.pop_batch_async(2).await?;
    println!("Popped async batch: {:?}", popped_batch); // Expected: ["world", "batch1"]

    // Attempt to pop from an empty buffer, which should return BufferError::Empty quickly
    // (pop_async and pop_batch_async don't wait if buffer is empty)
    match buffer.pop_batch_async_timeout(2, Duration::from_secs(1)).await {
        Ok(items) if items.is_empty() => println!("Popped empty batch as expected after draining."),
        // Ok(items) => println!("Unexpectedly popped items: {:?}", items), // This case might not be hit if Empty is preferred
        Err(BufferError::Empty) => println!("Buffer empty as expected."),
        Err(e) => return Err(e),
    }

    Ok(())
}
```

## Configuration

The buffer's behavior can be customized using the `Config` struct:

```rust
use elasticq::Config; // Note: If Config is public, it's from elasticq directly.
                     // If it's meant to be constructed differently, adjust this example.
use std::time::Duration;

let config = Config::default()
    .with_initial_capacity(512)         // Initial number of elements the buffer can hold
    .with_min_capacity(256)             // Minimum capacity the buffer will shrink to
    .with_max_capacity(8192)            // Maximum capacity the buffer will grow to
    .with_growth_factor(1.5)            // Factor by which capacity increases (e.g., 1.5 = 50% increase)
    .with_shrink_threshold(0.3)         // Shrink if usage is <= 30% of current capacity
    .with_pop_timeout(Duration::from_secs(5))  // Default pop timeout (currently not auto-used by methods)
    .with_push_timeout(Duration::from_secs(5)); // Default push timeout (currently not auto-used by methods)

// Important: Ensure config is valid before creating the buffer!
// `DynamicCircularBuffer::new(config)` will validate it and return `Err(BufferError::InvalidConfiguration)` if not.
// Key rules:
// - initial_capacity must be between min_capacity and max_capacity.
// - min_capacity cannot be greater than max_capacity.
// - Capacities must be > 0.
// - growth_factor must be > 1.0.
// - shrink_threshold must be between 0.0 and 1.0 (exclusive).
```
The `push_timeout` and `pop_timeout` fields in `Config` are placeholders for potential future enhancements; currently, timeout methods require an explicit `Duration` argument.

## API Highlights

The main struct is `DynamicCircularBuffer<T>`. Key methods include:

*   `new(config: Config) -> Result<Self, BufferError>`: Creates a new buffer.
*   `push(&self, item: T) -> Result<(), BufferError>`
*   `pop(&self) -> Result<T, BufferError>`
*   `push_batch(&self, items: Vec<T>) -> Result<(), BufferError>`
*   `pop_batch(&self, max_items: usize) -> Result<Vec<T>, BufferError>`
*   Async variants (if `async` feature enabled): `push_async`, `pop_async`, `push_batch_async`, `pop_batch_async`, and `*_timeout` versions.
*   Utilities: `len()`, `is_empty()`, `capacity()`, `clear()`, `iter() -> Vec<T> (clones items)`, `drain() -> Vec<T> (consumes items)`.

## Performance Analysis

Performance benchmarks were conducted on a Mac Studio with M1 Ultra (20 CPU cores). Results demonstrate significant improvements with the lock-free implementation.

### Lock-Free vs Lock-Based Comparison

| Implementation | Single-Threaded | 4 Producers | Advantages |
|---------------|-----------------|-------------|------------|
| **Lock-Free** | **46.6M msg/sec** | Varies | Wait-free operations, no deadlocks |
| **Lock-Based** | **22.0M msg/sec** | Stable | Predictable under high contention |
| **Speedup** | **🚀 2.1x** | Scenario-dependent | Lock-free wins for MPSC patterns |

### Scalability Characteristics

#### Lock-Free MPSC (Recommended for MQTT Proxy)
*   **Single Producer:** Excellent performance (46M+ msg/sec)
*   **Multiple Producers:** Good performance with consumer-driven resize
*   **Zero Deadlock Risk:** Wait-free consumer operations
*   **Memory Efficiency:** Epoch-based reclamation prevents memory leaks

#### Lock-Based (General Purpose)
*   **Baseline (1P/1C):** ~7.5 million items/second
*   **Optimal (2P/2C):** Peaked at ~12.3 million items/second  
*   **High Contention (4P+):** Performance degrades due to lock contention
*   **Batch Operations:** Significantly better - 1.1 ns/item for 1000-item batches

### MQTT Proxy Benchmarks

Real-world MQTT proxy simulation (4 publishers → 1 processor):
*   **Lock-Free Implementation:** 2.4M messages/sec sustained throughput
*   **Dynamic Resizing:** Capacity scales from 1K → 8K+ automatically
*   **Message Loss:** <1% under extreme load (configurable backpressure)
*   **Latency:** Sub-millisecond processing for 4,000 message batches

## Formal Verification

The lock-free implementation includes **TLA+ formal specifications** located in `tla+/` directory:

*   **`LockFreeMPSCQueue.tla`** - Complete formal model of the lock-free algorithm
*   **Safety Properties Verified:**
    *   FIFO ordering maintained under all concurrent operations
    *   Bounded capacity with no memory leaks
    *   Message conservation (no phantom messages or unexpected losses)
    *   ABA protection prevents race conditions
    *   Single consumer constraint enforced
*   **Liveness Properties Verified:**
    *   Consumer progress guarantees
    *   Resize operation completion
    *   Producer fairness under contention

To run verification:
```bash
# Requires TLA+ tools installation
tlc LockFreeMPSCQueue.tla -config LockFreeMPSCQueue.cfg
```

## Use Cases & Recommendations

### 🚀 **Choose Lock-Free Implementation When:**
*   **MQTT Proxy/Broker:** Multiple publishers, single message processor
*   **Event Streaming:** High-throughput event ingestion with single consumer
*   **Real-time Systems:** Deterministic latency requirements (no blocking)
*   **Single Producer:** Maximum performance for single-threaded producers
*   **Zero Deadlock Tolerance:** Systems that cannot afford blocking

### 🔒 **Choose Lock-Based Implementation When:**
*   **General Purpose:** Balanced multi-producer multi-consumer workloads
*   **Moderate Concurrency:** 2-4 threads with mixed operations
*   **Async/Await Patterns:** Tokio-based applications with async methods
*   **Predictable Performance:** Consistent behavior under varying load
*   **Complex Operations:** Need for batch operations and flexible API

### Configuration Recommendations

#### MQTT Proxy Configuration
```rust
let config = Config::default()
    .with_initial_capacity(1024)      // Start with 1K messages
    .with_max_capacity(1048576)       // Allow up to 1M messages
    .with_growth_factor(2.0)          // Double capacity when full
    .with_min_capacity(512);          // Shrink to 512 minimum
```

#### High-Throughput Streaming
```rust
let config = Config::default()
    .with_initial_capacity(8192)      // Larger initial buffer
    .with_max_capacity(16777216)      // 16M message capacity
    .with_growth_factor(1.5)          // Moderate growth
    .with_shrink_threshold(0.25);     // Shrink when 25% utilized
```

## Design Considerations & Limitations

*   **Locking Strategy:** The buffer uses a `Mutex` around the internal `VecDeque` and an `RwLock` for its logical capacity. Additionally, `push_lock: Mutex<()>` and `pop_lock: Mutex<()>` serialize all push operations against each other and all pop operations against each other. This design prioritizes correctness by ensuring that complex sequences like resize/shrink decisions and actions are atomic with respect to other operations of the same kind.
*   **Scalability Trade-off:** The coarse-grained `push_lock` and `pop_lock` are the primary reason for limited scalability beyond a few concurrent threads for *single-item* operations.
*   **Async Utility Methods:** Methods like `len()`, `is_empty()`, and `capacity()` are synchronous. When the `async` feature is enabled (and thus `tokio::sync` locks are used internally), these methods use `blocking_lock()` (or equivalent). This means they can block an async runtime if called from one and the lock is heavily contended. For critical async paths, use with awareness.
*   **`iter()` Performance:** `iter()` clones all items in the buffer. This can be costly for large buffers or items that are expensive to clone. `drain()` is more efficient if items are to be consumed and removed.

## Contributing

Contributions are welcome! Please feel free to submit issues or pull requests. For major changes, please open an issue first to discuss your proposed changes.

### Priority Areas for Contribution

*   **Performance Optimizations:** Further improvements to lock-free algorithms
*   **Additional Algorithms:** SPSC, MPMC implementations
*   **Platform Testing:** Verification on different architectures
*   **Documentation:** Examples, tutorials, and API documentation
*   **Formal Verification:** Extended TLA+ models and proofs

### Development Commands

```bash
# Run all tests
cargo test

# Run with lock-free feature
cargo test --features lock_free

# Run benchmarks
cargo bench

# Run lock-free vs lock-based benchmarks
cargo bench --features lock_free

# Run TLA+ verification (requires TLA+ tools)
cd tla+ && tlc LockFreeMPSCQueue.tla -config LockFreeMPSCQueue.cfg

# Run examples
cargo run --example lock_free_demo --features lock_free
cargo run --example performance_summary --features lock_free
```

## License

This project is licensed under the MIT License. Please see the `LICENSE` file in the repository for the full license text.

[![Ask DeepWiki](https://deepwiki.com/badge.svg)](https://deepwiki.com/jfabienke/elasticq)
