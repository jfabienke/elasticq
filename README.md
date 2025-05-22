# elasticq

A thread-safe, dynamically resizable circular buffer (queue) for Rust, designed for high-throughput scenarios.

## Features

*   **Elastic Sizing:** Automatically grows when full and shrinks when underutilized, within configurable limits.
*   **Thread-Safe:** Safe for concurrent use by multiple producers and consumers.
    *   Uses `parking_lot` mutexes for synchronous operations.
    *   Optionally uses `tokio::sync` mutexes for asynchronous operations via the `async` feature.
*   **Batch Operations:** Efficient `push_batch` and `pop_batch` methods for high-throughput.
*   **Asynchronous API (Optional):** Enable the `async` feature for `tokio`-based asynchronous methods (e.g., `push_async`, `pop_async_timeout`).
*   **Configurable Behavior:** Fine-tune capacities, growth/shrink factors.
*   **Clear Error Handling:** Provides distinct error types for conditions like buffer full/empty or timeouts.

## Table of Contents

1.  [Installation](#installation)
2.  [Basic Usage](#basic-usage)
    *   [Synchronous](#synchronous)
    *   [Asynchronous (with `async` feature)](#asynchronous-with-async-feature)
3.  [Configuration](#configuration)
4.  [API Highlights](#api-highlights)
5.  [Performance Insights](#performance-insights)
    *   [Single Operations](#single-operations)
    *   [Batch Operations](#batch-operations)
    *   [Dynamic Resizing/Shrinking](#dynamic-resizingshrinking)
    *   [Concurrency and Scalability](#concurrency-and-scalability)
6.  [Design Considerations & Limitations](#design-considerations--limitations)
7.  [Contributing](#contributing)
8.  [License](#license)

## Installation

Add this to your `Cargo.toml`:

```toml
[dependencies]
elasticq = "0.1.0" # Replace with the desired version
```

To enable asynchronous operations (requires Tokio):

```toml
[dependencies]
elasticq = { version = "0.1.0", features = ["async"] }
tokio = { version = "1", features = ["sync", "time"] } # Ensure tokio is also a dependency
```

## Basic Usage

### Synchronous

```rust
use elasticq::{DynamicCircularBuffer, Config, BufferError};

fn main() -> Result<(), BufferError> {
    // Use default configuration
    let buffer = DynamicCircularBuffer::<i32>::new(Config::default())?;

    // Push some items
    buffer.push(10)?;
    buffer.push(20)?;
    println!("Buffer length: {}", buffer.len()); // Output: 2

    // Pop an item
    let item = buffer.pop()?;
    assert_eq!(item, 10);
    println!("Popped: {}", item);

    // Batch operations
    buffer.push_batch(vec![30, 40, 50])?;
    println!("Buffer length after batch push: {}", buffer.len()); // Output: 4 (20, 30, 40, 50)

    let items = buffer.pop_batch(2)?;
    assert_eq!(items, vec![20, 30]);
    println!("Popped batch: {:?}", items);

    Ok(())
}
```

### Asynchronous (with `async` feature)

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

## Performance Insights

Benchmarks provide a general performance profile. Actual performance may vary based on workload and hardware.
These benchmarks were run on a Mac Studio with an M1 Ultra CPU.

### Single Operations
*   A single `push` followed by a `pop` operation (no resizing/shrinking) takes approximately **~34 nanoseconds**.

### Batch Operations
Batching significantly improves per-item throughput for sequential push/pop pairs:
*   **Batch of 10:** ~5.9 ns/item.
*   **Batch of 100:** ~2.0 ns/item.
*   **Batch of 1000:** ~1.1 ns/item.
*   **Conclusion:** For high throughput, batching is highly recommended.

### Dynamic Resizing/Shrinking
*   **Resizing (Growth):** The amortized cost per push was ~25 ns when frequent growth occurred (e.g., 128 pushes causing 3 resizes). This indicates a moderate and acceptable overhead.
*   **Shrinking:** The amortized cost per pop was ~17 ns when frequent shrinking occurred. This is comparable to a non-shrinking pop, suggesting efficient shrinking.

### Concurrency and Scalability
Concurrent benchmarks (multiple producers, multiple consumers operating on single items) revealed:
*   **Baseline (1 Producer, 1 Consumer):** ~7.5 million items/second.
*   **Optimal (2 Producers, 2 Consumers):** Peaked at ~12.3 million items/second.
*   **Increased Contention (e.g., 4P/4C, 8P/8C):** Throughput decreased (to ~7.7-9.6 million items/second) due to lock contention. The `push_lock` and `pop_lock` (which serialize all push and pop operations, respectively) become bottlenecks at higher thread counts for single-item operations.
*   **Asymmetric Load:**
    *   Many Producers, 1 Consumer (4P/1C): ~7.4 million items/second (consumer-bound).
    *   1 Producer, Many Consumers (1P/4C): ~11.6 million items/second (more efficient than 4P/1C).

**Conclusion on Concurrency:** `elasticq` provides good concurrent throughput for a small number of threads performing single-item operations. It does not scale linearly with many threads for such operations due to its locking strategy (chosen for correctness and implementation simplicity around dynamic resizing). For maximizing concurrent throughput, consider application-level sharding or ensure threads primarily use batch operations if the workload allows.

## Design Considerations & Limitations

*   **Locking Strategy:** The buffer uses a `Mutex` around the internal `VecDeque` and an `RwLock` for its logical capacity. Additionally, `push_lock: Mutex<()>` and `pop_lock: Mutex<()>` serialize all push operations against each other and all pop operations against each other. This design prioritizes correctness by ensuring that complex sequences like resize/shrink decisions and actions are atomic with respect to other operations of the same kind.
*   **Scalability Trade-off:** The coarse-grained `push_lock` and `pop_lock` are the primary reason for limited scalability beyond a few concurrent threads for *single-item* operations.
*   **Async Utility Methods:** Methods like `len()`, `is_empty()`, and `capacity()` are synchronous. When the `async` feature is enabled (and thus `tokio::sync` locks are used internally), these methods use `blocking_lock()` (or equivalent). This means they can block an async runtime if called from one and the lock is heavily contended. For critical async paths, use with awareness.
*   **`iter()` Performance:** `iter()` clones all items in the buffer. This can be costly for large buffers or items that are expensive to clone. `drain()` is more efficient if items are to be consumed and removed.

## Contributing

Contributions are welcome! Please feel free to submit issues or pull requests. For major changes, please open an issue first to discuss your proposed changes.

Top priority contributions are:

*   **Performance Improvements:** Enhancements that increase throughput or reduce latency. Here the Locking Strategy could be improved by using more fine-grained locks or by using a different data structure altogether.
*   **Documentation:** Clarifications or additions to the README, examples, or API documentation.
*   **Testing:** Additional tests that cover edge cases or improve code coverage.

## Running Tests

```bash
cargo test
```

## Running Benchmarks

```bash
cargo bench
```

## License

This project is licensed under the MIT License. Please see the `LICENSE` file in the repository for the full license text.

[![Ask DeepWiki](https://deepwiki.com/badge.svg)](https://deepwiki.com/jfabienke/elasticq)
