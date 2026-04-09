use std::fmt;
use std::panic;
use std::sync::{
    Arc, Mutex, mpsc,
    atomic::{AtomicUsize, Ordering},
};
use std::thread;

pub mod async_pool;
pub mod priority;
pub mod work_stealing;

// ── Job type ──────────────────────────────────────────────────────────────────

pub(crate) type Job = Box<dyn FnOnce() + Send + 'static>;

// ── Message enum ──────────────────────────────────────────────────────────────

enum Message {
    NewJob(Job),
    Terminate,
}

// ── Error type ────────────────────────────────────────────────────────────────

/// Errors that can occur when creating or using any pool type in this crate.
#[derive(Debug)]
pub enum PoolError {
    /// The requested pool size was zero.
    InvalidSize,
    /// The internal channel has closed; no more jobs can be sent.
    SendError,
}

impl fmt::Display for PoolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PoolError::InvalidSize => write!(f, "thread pool size must be at least 1"),
            PoolError::SendError => write!(f, "failed to send job: channel is closed"),
        }
    }
}

impl std::error::Error for PoolError {}

// ── Metrics ───────────────────────────────────────────────────────────────────

/// A point-in-time snapshot of pool activity counters.
///
/// Each field is read atomically, but the snapshot as a whole is not atomic —
/// under load, `running + completed + panicked` may not equal `queued`.
#[derive(Debug, Clone)]
pub struct MetricsSnapshot {
    /// Total jobs submitted via `execute()` since the pool was created.
    pub queued: usize,
    /// Jobs currently executing on a worker thread.
    pub running: usize,
    /// Jobs that completed without panicking.
    pub completed: usize,
    /// Jobs that panicked (caught by `catch_unwind`).
    pub panicked: usize,
}

/// Shared, lock-free counters updated by the pool and its workers.
pub struct Metrics {
    pub queued: AtomicUsize,
    pub running: AtomicUsize,
    pub completed: AtomicUsize,
    pub panicked: AtomicUsize,
}

impl Metrics {
    fn new() -> Arc<Self> {
        Arc::new(Metrics {
            queued: AtomicUsize::new(0),
            running: AtomicUsize::new(0),
            completed: AtomicUsize::new(0),
            panicked: AtomicUsize::new(0),
        })
    }

    /// Read all counters and return a snapshot.
    pub fn snapshot(&self) -> MetricsSnapshot {
        MetricsSnapshot {
            queued: self.queued.load(Ordering::Relaxed),
            running: self.running.load(Ordering::Relaxed),
            completed: self.completed.load(Ordering::Relaxed),
            panicked: self.panicked.load(Ordering::Relaxed),
        }
    }
}

// ── Worker ────────────────────────────────────────────────────────────────────

struct Worker {
    id: usize,
    thread: Option<thread::JoinHandle<()>>,
}

impl Worker {
    fn new(
        id: usize,
        receiver: Arc<Mutex<mpsc::Receiver<Message>>>,
        metrics: Arc<Metrics>,
    ) -> Worker {
        let thread = thread::spawn(move || loop {
            let message = receiver.lock().unwrap().recv();

            match message {
                Ok(Message::NewJob(job)) => {
                    metrics.running.fetch_add(1, Ordering::Relaxed);
                    let result = panic::catch_unwind(panic::AssertUnwindSafe(job));
                    metrics.running.fetch_sub(1, Ordering::Relaxed);
                    match result {
                        Ok(_) => {
                            metrics.completed.fetch_add(1, Ordering::Relaxed);
                        }
                        Err(e) => {
                            metrics.panicked.fetch_add(1, Ordering::Relaxed);
                            eprintln!("Worker {id} caught a panic: {:?}", e);
                        }
                    }
                }
                Ok(Message::Terminate) | Err(_) => break,
            }
        });

        Worker {
            id,
            thread: Some(thread),
        }
    }
}

// ── ThreadPool ────────────────────────────────────────────────────────────────

/// A fixed-size thread pool that executes jobs concurrently.
///
/// Jobs are queued on a bounded channel and distributed across a set of worker
/// threads. When the pool is dropped, all workers finish their current job and
/// exit cleanly.
///
/// # Example
///
/// ```rust
/// use thread_pool::ThreadPool;
///
/// let pool = ThreadPool::new(4, 16).unwrap();
/// pool.execute(|| println!("Hello from a worker!")).unwrap();
/// ```
pub struct ThreadPool {
    workers: Vec<Worker>,
    sender: mpsc::SyncSender<Message>,
    metrics: Arc<Metrics>,
}

impl ThreadPool {
    /// Create a new `ThreadPool`.
    ///
    /// - `num_threads` — number of worker threads to spawn. Must be ≥ 1.
    /// - `queue_bound` — maximum number of pending jobs. `execute()` blocks when
    ///   the queue is full (backpressure). A smaller bound reduces memory usage
    ///   but increases contention; a larger bound absorbs bursts at the cost of
    ///   memory.
    ///
    /// # Errors
    ///
    /// Returns [`PoolError::InvalidSize`] if `num_threads` is zero.
    pub fn new(num_threads: usize, queue_bound: usize) -> Result<Self, PoolError> {
        if num_threads == 0 {
            return Err(PoolError::InvalidSize);
        }

        let (sender, receiver) = mpsc::sync_channel::<Message>(queue_bound);
        let receiver = Arc::new(Mutex::new(receiver));
        let metrics = Metrics::new();

        let workers = (0..num_threads)
            .map(|id| Worker::new(id, Arc::clone(&receiver), Arc::clone(&metrics)))
            .collect();

        Ok(ThreadPool {
            workers,
            sender,
            metrics,
        })
    }

    /// Submit a job for execution on one of the worker threads.
    ///
    /// Blocks if the internal queue is full (backpressure).
    ///
    /// # Errors
    ///
    /// Returns [`PoolError::SendError`] if all workers have exited.
    pub fn execute<F>(&self, f: F) -> Result<(), PoolError>
    where
        F: FnOnce() + Send + 'static,
    {
        self.metrics.queued.fetch_add(1, Ordering::Relaxed);
        self.sender
            .send(Message::NewJob(Box::new(f)))
            .map_err(|_| PoolError::SendError)
    }

    /// Return a handle to the shared metrics counters.
    pub fn metrics(&self) -> Arc<Metrics> {
        Arc::clone(&self.metrics)
    }
}

impl Drop for ThreadPool {
    fn drop(&mut self) {
        for _ in &self.workers {
            let _ = self.sender.send(Message::Terminate);
        }
        for worker in &mut self.workers {
            if let Some(thread) = worker.thread.take() {
                thread.join().unwrap_or_else(|e| {
                    eprintln!("Worker {} panicked on join: {:?}", worker.id, e);
                });
            }
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[test]
    fn test_jobs_run() {
        let pool = ThreadPool::new(4, 8).unwrap();
        let counter = Arc::new(Mutex::new(0usize));

        for _ in 0..8 {
            let counter = Arc::clone(&counter);
            pool.execute(move || {
                let mut n = counter.lock().unwrap();
                *n += 1;
            })
            .unwrap();
        }

        drop(pool);

        assert_eq!(*counter.lock().unwrap(), 8);
    }

    #[test]
    fn test_panic_does_not_kill_pool() {
        let pool = ThreadPool::new(2, 4).unwrap();

        pool.execute(|| panic!("intentional panic")).unwrap();

        thread::sleep(std::time::Duration::from_millis(100));

        let (tx, rx) = mpsc::channel();
        pool.execute(move || tx.send(42).unwrap()).unwrap();

        assert_eq!(
            rx.recv_timeout(std::time::Duration::from_secs(1)).unwrap(),
            42
        );
    }

    #[test]
    fn test_invalid_size_returns_error() {
        assert!(ThreadPool::new(0, 8).is_err());
    }

    #[test]
    fn test_metrics_track_jobs() {
        let pool = ThreadPool::new(2, 8).unwrap();
        let metrics = pool.metrics();

        for _ in 0..4 {
            pool.execute(|| {}).unwrap();
        }
        drop(pool);

        let snap = metrics.snapshot();
        assert_eq!(snap.queued, 4);
        assert_eq!(snap.completed, 4);
        assert_eq!(snap.panicked, 0);
        assert_eq!(snap.running, 0);
    }

    #[test]
    fn test_metrics_track_panics() {
        let pool = ThreadPool::new(2, 4).unwrap();
        let metrics = pool.metrics();

        pool.execute(|| panic!("tracked panic")).unwrap();
        drop(pool);

        let snap = metrics.snapshot();
        assert_eq!(snap.panicked, 1);
        assert_eq!(snap.completed, 0);
    }
}
