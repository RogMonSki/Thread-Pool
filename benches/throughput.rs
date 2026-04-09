/// Throughput benchmarks comparing three pool implementations.
///
/// Run with:
///   cargo bench
///
/// Open the HTML report:
///   target/criterion/report/index.html
use criterion::{BenchmarkId, Criterion, black_box, criterion_group, criterion_main};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use thread_pool::{ThreadPool, work_stealing::WorkStealingPool};

const THREADS: usize = 4;

fn bench_throughput(c: &mut Criterion) {
    let mut group = c.benchmark_group("pool_throughput");

    for &n_jobs in &[100usize, 1_000, 10_000] {
        // ── ThreadPool (sync_channel + Mutex) ──────────────────────────────
        group.bench_with_input(
            BenchmarkId::new("ThreadPool", n_jobs),
            &n_jobs,
            |b, &n_jobs| {
                b.iter(|| {
                    let pool = ThreadPool::new(THREADS, n_jobs + 16).unwrap();
                    let done = Arc::new(AtomicUsize::new(0));
                    for _ in 0..n_jobs {
                        let done = Arc::clone(&done);
                        pool.execute(move || {
                            done.fetch_add(1, Ordering::Relaxed);
                        })
                        .unwrap();
                    }
                    drop(pool); // blocks until all workers are done
                    black_box(done.load(Ordering::Relaxed));
                });
            },
        );

        // ── WorkStealingPool (crossbeam-deque, no central Mutex) ──────────
        group.bench_with_input(
            BenchmarkId::new("WorkStealingPool", n_jobs),
            &n_jobs,
            |b, &n_jobs| {
                b.iter(|| {
                    let pool = WorkStealingPool::new(THREADS).unwrap();
                    let done = Arc::new(AtomicUsize::new(0));
                    for _ in 0..n_jobs {
                        let done = Arc::clone(&done);
                        pool.execute(move || {
                            done.fetch_add(1, Ordering::Relaxed);
                        })
                        .unwrap();
                    }
                    drop(pool);
                    black_box(done.load(Ordering::Relaxed));
                });
            },
        );

        // ── Rayon (baseline — work-stealing with async-signal-safe ops) ───
        group.bench_with_input(
            BenchmarkId::new("rayon", n_jobs),
            &n_jobs,
            |b, &n_jobs| {
                b.iter(|| {
                    let pool = rayon::ThreadPoolBuilder::new()
                        .num_threads(THREADS)
                        .build()
                        .unwrap();
                    let done = Arc::new(AtomicUsize::new(0));
                    pool.scope(|s| {
                        for _ in 0..n_jobs {
                            let done = Arc::clone(&done);
                            s.spawn(move |_| {
                                done.fetch_add(1, Ordering::Relaxed);
                            });
                        }
                    });
                    black_box(done.load(Ordering::Relaxed));
                });
            },
        );
    }

    group.finish();
}

criterion_group!(benches, bench_throughput);
criterion_main!(benches);
