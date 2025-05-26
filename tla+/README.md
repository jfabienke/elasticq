# TLA+ Formal Specification for Lock-Free MPSC Queue

This directory contains formal specifications for the lock-free Multi-Producer Single-Consumer (MPSC) queue implementation used in ElasticQ.

## Files

- **`LockFreeMPSCQueue.tla`** - Main TLA+ specification
- **`LockFreeMPSCQueue.cfg`** - Model checker configuration  
- **`README.md`** - This documentation

## Key Properties Verified

### Safety Properties
1. **FIFO Ordering** - Messages are consumed in the order they were produced
2. **Bounded Capacity** - Buffer never exceeds maximum configured capacity
3. **Message Conservation** - No messages are lost except when explicitly dropped due to capacity limits
4. **ABA Protection** - Generation counter prevents ABA race conditions
5. **Resize Safety** - Only consumer can initiate resize operations
6. **Single Consumer** - At most one consumer thread is active

### Liveness Properties
1. **Consumer Progress** - If messages exist and consumer is active, messages are eventually consumed
2. **Resize Progress** - Resize operations eventually complete
3. **Producer Fairness** - Producers can eventually enqueue when buffer has space

## Running the Model Checker

### Prerequisites
- Install TLA+ tools: [TLA+ Homepage](https://lamport.azurewebsites.net/tla/tla.html)
- Or use TLA+ VS Code extension

### Model Checking Commands

```bash
# Check all properties with TLC model checker
tlc LockFreeMPSCQueue.tla -config LockFreeMPSCQueue.cfg

# Generate state graph for visualization
tlc LockFreeMPSCQueue.tla -config LockFreeMPSCQueue.cfg -dump dot LockFreeMPSCQueue.dot
```

### Configuration Parameters

The model uses small finite values for tractable model checking:

- **MaxProducers**: 3 producer threads
- **MaxMessages**: 10 unique message values  
- **InitialCapacity**: 4 slots
- **MaxCapacity**: 16 slots maximum
- **GrowthFactor**: 2x capacity growth

These can be adjusted in `LockFreeMPSCQueue.cfg` based on verification needs vs. state space size.

## Model Structure

### State Variables
- **buffer** - Ring buffer contents as sequence
- **capacity** - Current buffer capacity
- **head/tail** - Atomic position counters
- **generation** - ABA protection counter
- **resizing** - Resize coordination flag

### Actions
- **ProducerEnqueue** - Producers add messages (with capacity checking)
- **ConsumerDequeue** - Consumer removes messages
- **ConsumerResize** - Consumer-driven capacity expansion
- **Thread Join/Leave** - Dynamic producer/consumer lifecycle

## Integration with Rust Implementation

This specification guides the Rust implementation by:

1. **Defining precise semantics** for concurrent operations
2. **Identifying edge cases** during resize operations  
3. **Verifying correctness** of the algorithm before coding
4. **Providing reference behavior** for testing

The verified properties ensure the Rust implementation will be:
- **Memory safe** (no use-after-free during resize)
- **Data race free** (proper atomic operations)
- **Deadlock free** (consumer-driven design)
- **Correct** (FIFO ordering, bounded memory)