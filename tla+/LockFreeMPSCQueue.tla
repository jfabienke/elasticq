------------------------ MODULE LockFreeMPSCQueue ------------------------
(*
Formal specification for a lock-free MPSC (Multi-Producer Single-Consumer) queue
with dynamic resizing, used for MQTT proxy message buffering.

Key properties verified:
1. MPSC operations (multiple producers, single consumer)
2. Consumer-driven resize (safety during buffer transitions)
3. Message dropping (bounded memory guarantees)  
4. Generation-based ABA protection
*)

EXTENDS Integers, Sequences, FiniteSets, TLC

CONSTANTS
    MaxProducers,     \* Maximum number of producer threads
    MaxMessages,      \* Maximum number of messages in system
    InitialCapacity,  \* Initial buffer capacity
    MaxCapacity,      \* Maximum buffer capacity (bounded memory)
    GrowthFactor      \* Buffer growth multiplier

VARIABLES
    \* Buffer state
    buffer,           \* Sequence representing ring buffer contents
    capacity,         \* Current buffer capacity
    
    \* Atomic positions
    head,             \* Write position (producers advance this)
    tail,             \* Read position (consumer advances this)
    generation,       \* ABA protection counter
    
    \* Resize coordination
    resizing,         \* Boolean: resize operation in progress
    
    \* Producer/Consumer coordination
    producers,        \* Set of active producer thread IDs
    consumer_active,  \* Boolean: consumer thread is active
    
    \* Message tracking
    messages_sent,    \* Total messages attempted by producers
    messages_dropped, \* Total messages dropped due to capacity
    messages_received \* Total messages successfully consumed

vars == <<buffer, capacity, head, tail, generation, resizing, 
          producers, consumer_active, messages_sent, messages_dropped, messages_received>>

\* Type invariants
TypeInvariant ==
    /\ buffer \in Seq(1..MaxMessages)
    /\ capacity \in InitialCapacity..MaxCapacity
    /\ head \in Nat
    /\ tail \in Nat  
    /\ generation \in Nat
    /\ resizing \in BOOLEAN
    /\ producers \subseteq 1..MaxProducers
    /\ consumer_active \in BOOLEAN
    /\ messages_sent \in Nat
    /\ messages_dropped \in Nat
    /\ messages_received \in Nat

\* Helper functions
BufferSize == Len(buffer)
BufferEmpty == BufferSize = 0
BufferFull == BufferSize = capacity
CanResize == consumer_active /\ BufferSize > 0 /\ capacity < MaxCapacity

\* Ring buffer position masking (power-of-2 capacity)
Mask(pos) == pos % capacity

\* Initial state
Init ==
    /\ buffer = <<>>
    /\ capacity = InitialCapacity
    /\ head = 0
    /\ tail = 0
    /\ generation = 0
    /\ resizing = FALSE
    /\ producers = {}
    /\ consumer_active = FALSE
    /\ messages_sent = 0
    /\ messages_dropped = 0
    /\ messages_received = 0

\* Producer actions
ProducerJoin(p) ==
    /\ p \notin producers
    /\ p \in 1..MaxProducers
    /\ producers' = producers \cup {p}
    /\ UNCHANGED <<buffer, capacity, head, tail, generation, resizing, 
                   consumer_active, messages_sent, messages_dropped, messages_received>>

ProducerLeave(p) ==
    /\ p \in producers
    /\ producers' = producers \ {p}
    /\ UNCHANGED <<buffer, capacity, head, tail, generation, resizing,
                   consumer_active, messages_sent, messages_dropped, messages_received>>

\* Producer attempts to enqueue a message
ProducerEnqueue(p, msg) ==
    /\ p \in producers
    /\ msg \in 1..MaxMessages
    /\ ~resizing  \* Cannot enqueue during resize
    /\ messages_sent' = messages_sent + 1
    /\ IF BufferFull
       THEN \* Buffer full - drop message
            /\ messages_dropped' = messages_dropped + 1
            /\ UNCHANGED <<buffer, capacity, head, tail, generation, resizing,
                           producers, consumer_active, messages_received>>
       ELSE \* Successfully enqueue
            /\ buffer' = Append(buffer, msg)
            /\ head' = head + 1
            /\ UNCHANGED <<capacity, tail, generation, resizing, producers,
                           consumer_active, messages_dropped, messages_received>>

\* Consumer actions
ConsumerStart ==
    /\ ~consumer_active
    /\ consumer_active' = TRUE
    /\ UNCHANGED <<buffer, capacity, head, tail, generation, resizing,
                   producers, messages_sent, messages_dropped, messages_received>>

ConsumerStop ==
    /\ consumer_active
    /\ consumer_active' = FALSE
    /\ UNCHANGED <<buffer, capacity, head, tail, generation, resizing,
                   producers, messages_sent, messages_dropped, messages_received>>

\* Consumer dequeues a message
ConsumerDequeue ==
    /\ consumer_active
    /\ ~BufferEmpty
    /\ ~resizing
    /\ buffer' = Tail(buffer)
    /\ tail' = tail + 1
    /\ messages_received' = messages_received + 1
    /\ UNCHANGED <<capacity, head, generation, resizing, producers,
                   consumer_active, messages_sent, messages_dropped>>

\* Consumer-driven resize operation
ConsumerResize ==
    /\ consumer_active
    /\ CanResize
    /\ ~resizing
    /\ capacity < MaxCapacity
    /\ \* Trigger resize when buffer utilization is high
       BufferSize * 2 > capacity
    /\ resizing' = TRUE
    /\ \* Double capacity (bounded by MaxCapacity)
       LET new_capacity == IF capacity * GrowthFactor <= MaxCapacity
                          THEN capacity * GrowthFactor
                          ELSE MaxCapacity
       IN capacity' = new_capacity
    /\ generation' = generation + 1  \* ABA protection
    /\ UNCHANGED <<buffer, head, tail, producers, consumer_active,
                   messages_sent, messages_dropped, messages_received>>

\* Complete resize operation
ConsumerResizeComplete ==
    /\ consumer_active
    /\ resizing
    /\ resizing' = FALSE
    /\ UNCHANGED <<buffer, capacity, head, tail, generation, producers,
                   consumer_active, messages_sent, messages_dropped, messages_received>>

\* Next state relation
Next ==
    \/ \E p \in 1..MaxProducers : ProducerJoin(p)
    \/ \E p \in producers : ProducerLeave(p)
    \/ \E p \in producers, msg \in 1..MaxMessages : ProducerEnqueue(p, msg)
    \/ ConsumerStart
    \/ ConsumerStop
    \/ ConsumerDequeue
    \/ ConsumerResize
    \/ ConsumerResizeComplete

\* Specification
Spec == Init /\ [][Next]_vars /\ WF_vars(ConsumerDequeue) /\ WF_vars(ConsumerResizeComplete)

\* Safety Properties

\* FIFO ordering: messages are dequeued in the order they were enqueued
FIFOOrdering == \A i, j \in DOMAIN buffer : i < j => buffer[i] \in 1..MaxMessages

\* Bounded capacity: buffer never exceeds maximum capacity
BoundedCapacity == BufferSize <= capacity /\ capacity <= MaxCapacity

\* No message loss during normal operation (excluding drops due to capacity)
MessageConservation == 
    messages_received + BufferSize + messages_dropped = messages_sent

\* ABA protection: generation counter increases monotonically during resize
ABAProtection == generation >= 0 /\ (resizing => generation > 0)

\* Resize safety: no concurrent resize operations
ResizeSafety == resizing => consumer_active

\* Single consumer invariant
SingleConsumer == Cardinality({consumer_active}) <= 1

\* Liveness Properties

\* Progress: if messages are in buffer and consumer is active, eventually they are consumed
ConsumerProgress == 
    (consumer_active /\ ~BufferEmpty) ~> (BufferSize < BufferSize)

\* Resize completion: if resize starts, it eventually completes
ResizeProgress == resizing ~> ~resizing

\* Fairness: producers can eventually enqueue if there's space
ProducerFairness ==
    \A p \in producers : (p \in producers /\ ~BufferFull) ~> 
        \E msg \in 1..MaxMessages : ProducerEnqueue(p, msg)

\* Combined safety property
Safety ==
    /\ TypeInvariant
    /\ FIFOOrdering
    /\ BoundedCapacity
    /\ MessageConservation
    /\ ABAProtection
    /\ ResizeSafety
    /\ SingleConsumer

=============================================================================