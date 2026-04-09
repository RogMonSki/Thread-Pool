use std::sync::{Arc, Mutex};
use thread_pool::ThreadPool;

#[test]
fn all_jobs_complete_before_drop() {
    let pool = ThreadPool::new(4, 16).unwrap();
    let results = Arc::new(Mutex::new(Vec::new()));

    for i in 0..16usize {
        let results = Arc::clone(&results);
        pool.execute(move || results.lock().unwrap().push(i)).unwrap();
    }

    drop(pool);

    let mut v = results.lock().unwrap().clone();
    v.sort();
    assert_eq!(v, (0..16).collect::<Vec<_>>());
}

#[test]
fn pool_survives_multiple_panics() {
    let pool = ThreadPool::new(2, 8).unwrap();
    let counter = Arc::new(Mutex::new(0usize));

    // Three panicking jobs followed by three good ones.
    for _ in 0..3 {
        pool.execute(|| panic!("boom")).unwrap();
    }
    for _ in 0..3 {
        let counter = Arc::clone(&counter);
        pool.execute(move || *counter.lock().unwrap() += 1).unwrap();
    }

    drop(pool);

    assert_eq!(*counter.lock().unwrap(), 3);
}

#[test]
fn zero_threads_is_rejected() {
    assert!(ThreadPool::new(0, 8).is_err());
}
