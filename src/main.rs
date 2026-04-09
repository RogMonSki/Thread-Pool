use thread_pool::{
    ThreadPool,
    async_pool::AsyncPool,
    priority::PriorityThreadPool,
    work_stealing::WorkStealingPool,
};

fn main() {
    demo_thread_pool();
    demo_work_stealing();
    demo_priority();
    demo_async();
    println!("\nAll demos complete.");
}

fn demo_thread_pool() {
    println!("── ThreadPool (with metrics) ────────────────────────────");
    let pool = ThreadPool::new(4, 16).expect("failed to create pool");
    let metrics = pool.metrics();

    for i in 0..8 {
        pool.execute(move || {
            println!("  [thread-pool] job {i} on {:?}", std::thread::current().id());
        })
        .unwrap();
    }

    drop(pool);

    let snap = metrics.snapshot();
    println!(
        "  metrics → queued={} completed={} panicked={} running={}",
        snap.queued, snap.completed, snap.panicked, snap.running
    );
}

fn demo_work_stealing() {
    println!("\n── WorkStealingPool ─────────────────────────────────────");
    let pool = WorkStealingPool::new(4).expect("failed to create work-stealing pool");

    for i in 0..8 {
        pool.execute(move || {
            println!("  [work-steal] job {i} on {:?}", std::thread::current().id());
        })
        .unwrap();
    }

    drop(pool);
    println!("  work-stealing pool shut down cleanly");
}

fn demo_priority() {
    println!("\n── PriorityThreadPool ───────────────────────────────────");
    let pool = PriorityThreadPool::new(2).expect("failed to create priority pool");

    pool.execute_with_priority(1, || println!("  [priority] low  (1)")).unwrap();
    pool.execute_with_priority(10, || println!("  [priority] high (10)")).unwrap();
    pool.execute_with_priority(5, || println!("  [priority] mid  (5)")).unwrap();

    drop(pool);
    println!("  priority pool shut down cleanly");
}

fn demo_async() {
    println!("\n── AsyncPool ────────────────────────────────────────────");
    let pool = AsyncPool::new(4).expect("failed to create async pool");

    // block_on: synchronously drive a future to completion.
    let result = pool.block_on(async {
        let a = tokio::spawn(async { 6u64 });
        let b = tokio::spawn(async { 7u64 });
        a.await.unwrap() * b.await.unwrap()
    });
    println!("  [async] 6 × 7 = {result}");

    // spawn: fire-and-forget.
    let handles: Vec<_> = (0..4)
        .map(|i| {
            pool.spawn(async move {
                println!("  [async] spawned task {i}");
            })
        })
        .collect();

    pool.block_on(async {
        for h in handles {
            h.await.unwrap();
        }
    });
    println!("  async pool shut down cleanly");
}
