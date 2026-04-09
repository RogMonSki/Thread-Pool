/// An async-capable thread pool backed by a Tokio multi-thread runtime.
///
/// Unlike [`ThreadPool`] (which runs synchronous closures on OS threads),
/// `AsyncPool` schedules `async` futures on Tokio's cooperative task runtime.
/// This is the right choice when jobs do async I/O; for CPU-bound work the
/// synchronous pools are more appropriate.
///
/// # Example
///
/// ```rust
/// use thread_pool::async_pool::AsyncPool;
///
/// let pool = AsyncPool::new(4).unwrap();
///
/// // Run a future to completion on the pool.
/// let answer = pool.block_on(async { 6 * 7 });
/// assert_eq!(answer, 42);
///
/// // Fire-and-forget: spawn a future without waiting.
/// pool.spawn(async { println!("Hello from an async worker!") });
/// ```
///
/// [`ThreadPool`]: crate::ThreadPool
use crate::PoolError;
use std::future::Future;
use tokio::runtime::Runtime;
use tokio::task::JoinHandle;

pub struct AsyncPool {
    runtime: Runtime,
}

impl AsyncPool {
    /// Create a new `AsyncPool` with a Tokio multi-thread runtime.
    ///
    /// - `num_threads` — number of worker threads in the runtime. Must be ≥ 1.
    ///
    /// # Errors
    ///
    /// Returns [`PoolError::InvalidSize`] if `num_threads` is zero.
    /// Returns [`PoolError::SendError`] if the runtime cannot be initialised.
    pub fn new(num_threads: usize) -> Result<Self, PoolError> {
        if num_threads == 0 {
            return Err(PoolError::InvalidSize);
        }
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(num_threads)
            .build()
            .map_err(|_| PoolError::SendError)?;
        Ok(AsyncPool { runtime })
    }

    /// Spawn a future on the runtime and return its `JoinHandle`.
    ///
    /// The future is `Send + 'static`, mirroring `tokio::spawn`. The returned
    /// handle can be awaited inside another `block_on` call if needed.
    pub fn spawn<F>(&self, future: F) -> JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        self.runtime.spawn(future)
    }

    /// Block the **calling thread** until `future` completes and return its
    /// output.
    ///
    /// # Panics
    ///
    /// Panics if called from within an async context (i.e. from inside a
    /// Tokio task). Use `.await` or `spawn()` from async code instead.
    pub fn block_on<F: Future>(&self, future: F) -> F::Output {
        self.runtime.block_on(future)
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    #[test]
    fn test_block_on_returns_value() {
        let pool = AsyncPool::new(2).unwrap();
        let result = pool.block_on(async { 6 * 7 });
        assert_eq!(result, 42);
    }

    #[test]
    fn test_spawn_and_await() {
        let pool = AsyncPool::new(2).unwrap();
        let counter = Arc::new(AtomicUsize::new(0));

        let handles: Vec<_> = (0..8)
            .map(|_| {
                let counter = Arc::clone(&counter);
                pool.spawn(async move {
                    counter.fetch_add(1, Ordering::Relaxed);
                })
            })
            .collect();

        // Await all spawned tasks.
        pool.block_on(async {
            for h in handles {
                h.await.unwrap();
            }
        });

        assert_eq!(counter.load(Ordering::Relaxed), 8);
    }

    #[test]
    fn test_invalid_size() {
        assert!(AsyncPool::new(0).is_err());
    }
}
