# thread_pool

A Rust thread pool library built from scratch, progressing from a minimal
channel-based implementation through to four distinct pool types with metrics,
panic recovery, and async support.

## Pool types

| Type | Queue | Best for |
|---|---|---|
| `ThreadPool` | Bounded `mpsc::sync_channel` | General-purpose work with backpressure |
| `WorkStealingPool` | Per-thread deques + `Injector` | High-throughput, low-latency dispatch |
| `PriorityThreadPool` | `BinaryHeap` | Jobs with different urgency levels |
| `AsyncPool` | Tokio runtime | Async I/O tasks |

## Usage

### `ThreadPool`

Fixed number of OS threads sharing a bounded job queue. `execute()` blocks when
the queue is full, providing natural backpressure.

```rust
use thread_pool::ThreadPool;

let pool = ThreadPool::new(4, /*queue_bound=*/16).unwrap();

pool.execute(|| println!("Hello from a worker!")).unwrap();

// Dropping the pool waits for all in-flight jobs to finish.
drop(pool);
```

#### Metrics

```rust
let pool = ThreadPool::new(4, 16).unwrap();
let metrics = pool.metrics();

for _ in 0..8 {
    pool.execute(|| { /* work */ }).unwrap();
}
drop(pool);

let snap = metrics.snapshot();
println!("queued={} completed={} panicked={}", snap.queued, snap.completed, snap.panicked);
```

### `WorkStealingPool`

Each worker owns a LIFO deque. Idle workers steal from neighbours via
`crossbeam-deque`, removing the central `Mutex` bottleneck. The injector queue
is unbounded; `execute()` never blocks.

```rust
use thread_pool::work_stealing::WorkStealingPool;

let pool = WorkStealingPool::new(4).unwrap();
pool.execute(|| println!("Stolen or local — doesn't matter!")).unwrap();
drop(pool);
```

### `PriorityThreadPool`

Jobs are stored in a max-heap. Higher `priority` values run first. Within the
same priority level, earlier submissions run first (FIFO tie-breaking).

```rust
use thread_pool::priority::PriorityThreadPool;

let pool = PriorityThreadPool::new(4).unwrap();

pool.execute_with_priority(1,  || println!("low")).unwrap();
pool.execute_with_priority(10, || println!("high")).unwrap();
pool.execute_with_priority(5,  || println!("mid")).unwrap();

// Output order: high → mid → low
drop(pool);
```

The default `execute()` method submits with priority `0`.

### `AsyncPool`

Wraps a multi-thread Tokio runtime. Use `spawn()` to fire-and-forget a future,
or `block_on()` to drive one to completion from synchronous code.

```rust
use thread_pool::async_pool::AsyncPool;

let pool = AsyncPool::new(4).unwrap();

// Block the current thread until the future resolves.
let answer = pool.block_on(async { 6 * 7 });
assert_eq!(answer, 42);

// Fire-and-forget.
let handle = pool.spawn(async { println!("async task"); });
pool.block_on(async { handle.await.unwrap(); });
```

> `block_on` must be called from synchronous (non-async) code. Calling it from
> inside a Tokio task will panic.

## Panic recovery

All pool types catch panics in worker threads using `std::panic::catch_unwind`.
A panicking job logs to stderr and the worker continues running — it does not
crash the pool or affect other jobs.

```rust
let pool = ThreadPool::new(2, 4).unwrap();

pool.execute(|| panic!("this is fine")).unwrap();

// Pool is still usable after the panic.
pool.execute(|| println!("still running")).unwrap();
```

## Graceful shutdown

Every pool type drains queued jobs and joins all worker threads when dropped.
No jobs are silently abandoned.

## Running the tests

```bash
cargo test
```

25 tests across unit, integration, and doctests.

## Benchmarks

Compares `ThreadPool`, `WorkStealingPool`, and `rayon` at 100 / 1 000 / 10 000
jobs across 4 threads.

```bash
cargo bench
```

An HTML report is written to `target/criterion/report/index.html`.

## Project structure

```
thread_pool/
├── src/
│   ├── lib.rs           # ThreadPool, Metrics, PoolError
│   ├── work_stealing.rs # WorkStealingPool
│   ├── priority.rs      # PriorityThreadPool
│   ├── async_pool.rs    # AsyncPool
│   └── main.rs          # Demo
├── tests/
│   └── integration.rs   # Black-box tests against the public API
└── benches/
    └── throughput.rs    # Criterion benchmarks
```
