/// A thread pool that uses per-worker deques and work-stealing to eliminate
/// the central `Mutex` bottleneck present in a channel-based pool.
///
/// Each worker owns a `crossbeam_deque::Worker` (LIFO local queue). Idle
/// workers first try their own queue, then the shared `Injector`, then steal
/// from a random neighbour's queue. Workers park when truly idle and are
/// unparked by `execute()`.
///
/// # Example
///
/// ```rust
/// use thread_pool::work_stealing::WorkStealingPool;
///
/// let pool = WorkStealingPool::new(4).unwrap();
/// pool.execute(|| println!("Hello from a work-stealing worker!")).unwrap();
/// ```
use crate::{Job, PoolError};
use crossbeam_deque::{Injector, Steal, Stealer, Worker as LocalQueue};
use std::panic;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
    mpsc,
};
use std::thread;
use std::time::Duration;

// ── Task-finding helper ───────────────────────────────────────────────────────

/// Try to find a job from the worker's own queue, the injector, or by stealing
/// from a neighbour. Skips `self_id` when iterating stealers.
fn find_task(
    self_id: usize,
    local: &LocalQueue<Job>,
    injector: &Injector<Job>,
    stealers: &[Stealer<Job>],
) -> Option<Job> {
    // 1. Own local queue (LIFO — hottest job, best cache locality).
    local.pop().or_else(|| {
        // 2. Batch-steal from the global injector into the local queue, then pop.
        loop {
            match injector.steal_batch_and_pop(local) {
                Steal::Success(job) => return Some(job),
                Steal::Empty => break,
                Steal::Retry => {}
            }
        }
        // 3. Steal from a neighbour (skip ourselves).
        stealers
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != self_id)
            .find_map(|(_, s): (usize, &Stealer<Job>)| match s.steal() {
                Steal::Success(job) => Some(job),
                _ => None,
            })
    })
}

// ── WorkStealingPool ──────────────────────────────────────────────────────────

pub struct WorkStealingPool {
    injector: Arc<Injector<Job>>,
    /// One `thread::Thread` handle per worker, used to call `unpark()`.
    handles: Vec<thread::Thread>,
    shutdown: Arc<AtomicBool>,
    threads: Vec<Option<thread::JoinHandle<()>>>,
    /// Round-robin index for choosing which worker to wake on `execute()`.
    next_wakeup: AtomicUsize,
}

impl WorkStealingPool {
    /// Create a new work-stealing pool with `num_threads` workers.
    ///
    /// # Errors
    ///
    /// Returns [`PoolError::InvalidSize`] if `num_threads` is zero.
    pub fn new(num_threads: usize) -> Result<Self, PoolError> {
        if num_threads == 0 {
            return Err(PoolError::InvalidSize);
        }

        let injector: Arc<Injector<Job>> = Arc::new(Injector::new());
        let shutdown = Arc::new(AtomicBool::new(false));

        // Create per-worker LIFO deques and collect their stealers.
        let deques: Vec<LocalQueue<Job>> = (0..num_threads).map(|_| LocalQueue::new_lifo()).collect();
        let stealers: Arc<Vec<Stealer<Job>>> =
            Arc::new(deques.iter().map(|d: &LocalQueue<Job>| d.stealer()).collect());

        // Each spawned thread sends its `thread::Thread` back so the pool can
        // call `unpark()` on it later.
        let (reg_tx, reg_rx) = mpsc::sync_channel::<thread::Thread>(num_threads);

        let mut threads = Vec::with_capacity(num_threads);

        for (id, local) in deques.into_iter().enumerate() {
            let injector = Arc::clone(&injector);
            let stealers = Arc::clone(&stealers);
            let shutdown = Arc::clone(&shutdown);
            let reg_tx = reg_tx.clone();

            let handle = thread::spawn(move || {
                // Register this thread's handle with the pool constructor.
                reg_tx.send(thread::current()).unwrap();
                drop(reg_tx);

                loop {
                    if let Some(job) = find_task(id, &local, &injector, &stealers) {
                        let _ = panic::catch_unwind(panic::AssertUnwindSafe(job));
                    } else if shutdown.load(Ordering::Acquire) {
                        // No work available and shutdown is requested → exit.
                        // Checking work first ensures we drain the injector before
                        // exiting, mirroring ThreadPool's drain-then-terminate behaviour.
                        break;
                    } else {
                        // Nothing to do — park with a timeout so we eventually
                        // re-check the shutdown flag even if unpark() is missed.
                        thread::park_timeout(Duration::from_millis(5));
                    }
                }
            });

            threads.push(Some(handle));
        }
        drop(reg_tx); // drop our copy so the channel closes when all threads register

        // Collect handles in the order threads registered (order may differ from
        // spawn order, which is fine — we only need them for unparking).
        let handles: Vec<thread::Thread> = (0..num_threads)
            .map(|_| reg_rx.recv().expect("worker thread failed to register"))
            .collect();

        Ok(WorkStealingPool {
            injector,
            handles,
            shutdown,
            threads,
            next_wakeup: AtomicUsize::new(0),
        })
    }

    /// Submit a job for execution. Returns immediately (the injector queue is
    /// unbounded). Wakes one idle worker via `unpark()`.
    ///
    /// # Errors
    ///
    /// Returns [`PoolError::SendError`] if the pool has been shut down.
    pub fn execute<F>(&self, f: F) -> Result<(), PoolError>
    where
        F: FnOnce() + Send + 'static,
    {
        if self.shutdown.load(Ordering::Relaxed) {
            return Err(PoolError::SendError);
        }
        self.injector.push(Box::new(f));
        // Wake one worker round-robin. If it's already busy, the park token is
        // consumed on its next park() call — no wakeup is ever lost.
        let idx = self.next_wakeup.fetch_add(1, Ordering::Relaxed) % self.handles.len();
        self.handles[idx].unpark();
        Ok(())
    }
}

impl Drop for WorkStealingPool {
    fn drop(&mut self) {
        // Signal shutdown, then unpark all workers so they see the flag.
        self.shutdown.store(true, Ordering::Release);
        for h in &self.handles {
            h.unpark();
        }
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
    fn test_work_stealing_executes_all_jobs() {
        let pool = WorkStealingPool::new(4).unwrap();
        let counter = Arc::new(Mutex::new(0usize));

        for _ in 0..16 {
            let counter = Arc::clone(&counter);
            pool.execute(move || *counter.lock().unwrap() += 1).unwrap();
        }

        drop(pool);

        assert_eq!(*counter.lock().unwrap(), 16);
    }

    #[test]
    fn test_work_stealing_survives_panic() {
        let pool = WorkStealingPool::new(2).unwrap();

        pool.execute(|| panic!("work-stealing panic")).unwrap();

        thread::sleep(Duration::from_millis(50));

        let (tx, rx) = mpsc::channel();
        pool.execute(move || tx.send(99).unwrap()).unwrap();

        assert_eq!(
            rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            99
        );
    }

    #[test]
    fn test_invalid_size() {
        assert!(WorkStealingPool::new(0).is_err());
    }
}
