/// A thread pool that executes jobs in priority order.
///
/// Jobs are stored in a `BinaryHeap` (max-heap). Higher `priority` values run
/// first. Within the same priority level, jobs execute in FIFO submission order.
///
/// Workers block on a `Condvar` when the heap is empty and are woken by
/// `execute_with_priority()`. On drop the pool drains remaining queued jobs
/// before workers exit.
///
/// # Example
///
/// ```rust
/// use thread_pool::priority::PriorityThreadPool;
///
/// let pool = PriorityThreadPool::new(4).unwrap();
/// pool.execute_with_priority(10, || println!("urgent")).unwrap();
/// pool.execute_with_priority(1,  || println!("low priority")).unwrap();
/// ```
use crate::{Job, PoolError};
use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::panic;
use std::sync::{
    Arc, Condvar, Mutex,
    atomic::{AtomicBool, AtomicU64, Ordering as AOrdering},
};
use std::thread;

// ── PriorityJob ───────────────────────────────────────────────────────────────

struct PriorityJob {
    /// Higher value = higher urgency. The `BinaryHeap` is a max-heap, so
    /// the job with the greatest `priority` is popped first.
    priority: i64,
    /// Monotonically increasing sequence number assigned at submission time.
    /// Within the same priority level, lower `seq` (submitted earlier) wins.
    seq: u64,
    job: Job,
}

impl PartialEq for PriorityJob {
    fn eq(&self, other: &Self) -> bool {
        self.priority == other.priority && self.seq == other.seq
    }
}
impl Eq for PriorityJob {}

impl PartialOrd for PriorityJob {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for PriorityJob {
    fn cmp(&self, other: &Self) -> Ordering {
        // Primary: higher priority first.
        // Secondary: lower seq (earlier submission) first within same priority.
        self.priority
            .cmp(&other.priority)
            .then_with(|| other.seq.cmp(&self.seq))
    }
}

// ── PriorityThreadPool ────────────────────────────────────────────────────────

type SharedQueue = Arc<(Mutex<BinaryHeap<PriorityJob>>, Condvar)>;

pub struct PriorityThreadPool {
    shared: SharedQueue,
    shutdown: Arc<AtomicBool>,
    seq: AtomicU64,
    threads: Vec<Option<thread::JoinHandle<()>>>,
}

impl PriorityThreadPool {
    /// Create a new priority pool with `num_threads` workers.
    ///
    /// # Errors
    ///
    /// Returns [`PoolError::InvalidSize`] if `num_threads` is zero.
    pub fn new(num_threads: usize) -> Result<Self, PoolError> {
        if num_threads == 0 {
            return Err(PoolError::InvalidSize);
        }

        let shared: SharedQueue = Arc::new((Mutex::new(BinaryHeap::new()), Condvar::new()));
        let shutdown = Arc::new(AtomicBool::new(false));
        let mut threads = Vec::with_capacity(num_threads);

        for _ in 0..num_threads {
            let shared = Arc::clone(&shared);
            let shutdown = Arc::clone(&shutdown);

            let handle = thread::spawn(move || {
                loop {
                    let job = {
                        let (lock, cvar) = &*shared;

                        // Wait while the heap is empty AND we haven't been asked
                        // to shut down. `wait_while` re-checks the predicate under
                        // the lock, preventing lost-wakeup races.
                        let mut guard = cvar
                            .wait_while(lock.lock().unwrap(), |heap| {
                                heap.is_empty() && !shutdown.load(AOrdering::Acquire)
                            })
                            .unwrap();

                        // Drain remaining jobs even after shutdown is set.
                        match guard.pop() {
                            Some(pj) => pj.job,
                            None => break, // heap empty + shutdown → exit
                        }
                    }; // lock released here

                    let _ = panic::catch_unwind(panic::AssertUnwindSafe(job));
                }
            });

            threads.push(Some(handle));
        }

        Ok(PriorityThreadPool {
            shared,
            shutdown,
            seq: AtomicU64::new(0),
            threads,
        })
    }

    /// Submit a job with the given priority.
    ///
    /// Higher values run sooner. Jobs with equal priority execute in FIFO order.
    ///
    /// # Errors
    ///
    /// Returns [`PoolError::SendError`] if the pool has been shut down.
    pub fn execute_with_priority<F>(&self, priority: i64, f: F) -> Result<(), PoolError>
    where
        F: FnOnce() + Send + 'static,
    {
        if self.shutdown.load(AOrdering::Relaxed) {
            return Err(PoolError::SendError);
        }
        let seq = self.seq.fetch_add(1, AOrdering::Relaxed);
        let (lock, cvar) = &*self.shared;
        lock.lock().unwrap().push(PriorityJob {
            priority,
            seq,
            job: Box::new(f),
        });
        cvar.notify_one();
        Ok(())
    }

    /// Submit a job with default priority (0).
    pub fn execute<F>(&self, f: F) -> Result<(), PoolError>
    where
        F: FnOnce() + Send + 'static,
    {
        self.execute_with_priority(0, f)
    }
}

impl Drop for PriorityThreadPool {
    fn drop(&mut self) {
        // Signal shutdown. Workers will drain remaining jobs then exit when the
        // heap is empty.
        self.shutdown.store(true, AOrdering::Release);
        let (_, cvar) = &*self.shared;
        cvar.notify_all();

        for t in &mut self.threads {
            if let Some(thread) = t.take() {
                thread.join().unwrap_or_default();
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
    fn test_priority_ordering() {
        let pool = PriorityThreadPool::new(1).unwrap(); // single worker = deterministic order
        let log: Arc<Mutex<Vec<i64>>> = Arc::new(Mutex::new(Vec::new()));

        // Submit low-priority first; high-priority should still run first if the
        // worker hasn't picked up the low one yet. With a single worker that blocks
        // on each job, order is determined by heap priority, not submission time.
        // We submit all before the worker picks any up by using a barrier.
        use std::sync::Barrier;
        let barrier = Arc::new(Barrier::new(2));

        // First job: stall the worker until all jobs are queued.
        let barrier_clone = Arc::clone(&barrier);
        pool.execute_with_priority(0, move || {
            barrier_clone.wait(); // release when all jobs are submitted
        })
        .unwrap();

        for p in [1i64, 5, 3, 2, 4] {
            let log = Arc::clone(&log);
            pool.execute_with_priority(p, move || log.lock().unwrap().push(p))
                .unwrap();
        }

        barrier.wait(); // let the stalling job finish → worker now picks highest next

        drop(pool);

        let order = log.lock().unwrap().clone();
        // Each value must be ≤ the previous (descending priority).
        for w in order.windows(2) {
            assert!(w[0] >= w[1], "expected descending priority, got {:?}", order);
        }
    }

    #[test]
    fn test_priority_all_jobs_run() {
        let pool = PriorityThreadPool::new(4).unwrap();
        let counter = Arc::new(Mutex::new(0usize));

        for i in 0..12 {
            let counter = Arc::clone(&counter);
            pool.execute_with_priority(i % 3, move || *counter.lock().unwrap() += 1)
                .unwrap();
        }

        drop(pool);

        assert_eq!(*counter.lock().unwrap(), 12);
    }

    #[test]
    fn test_priority_invalid_size() {
        assert!(PriorityThreadPool::new(0).is_err());
    }
}
