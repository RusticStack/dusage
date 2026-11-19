//! Shared parallel work queue used by dusage and fastdel.
//!
//! # Design
//!
//! One mutex guarding one `VecDeque`, one condvar, and batched handoff: a worker
//! takes up to [`MAX_BATCH`] items per lock acquisition into a buffer it owns, runs
//! the callback over that buffer without touching the lock, then honours the whole
//! buffer as one completion. The lock is therefore touched once per batch instead
//! of once per item, which is what makes the millions-of-trivial-items shape fast;
//! the batch size adapts down to 1 as the queue drains, so the
//! thousands-of-items-that-push-children shape keeps near-item granularity.
//!
//! Why batching rather than sharded queues or a lock-free fast path: the queue must
//! terminate exactly when it is empty and nothing is in flight, which needs one
//! authoritative place to observe both. Sharding pushes that observation across
//! N mutexes (a worker would have to poll every shard before concluding the run is
//! over) and a lock-free ring needs an ordered mpmc deque that std does not
//! provide. Amortising the existing critical section over a batch keeps the
//! contract literally: still one mutex, still one condvar, still no polling.
//!
//! # Guarantees
//!
//! * `active` counts items handed to a worker and not yet finished, *including*
//!   items sitting in that worker's buffer. A buffered item is work in flight, so
//!   no worker can conclude "empty and idle" while another still holds items.
//! * A pop and its `active += n` happen in the same critical section; so does
//!   releasing the previous batch and taking the next one (`refill`).
//! * A worker that pushes children extends the run: the child is in the queue
//!   before the callback returns, and only an empty queue together with
//!   `active == 0` ends the run.
//! * A worker with an empty queue but work in flight waits on the condvar, so
//!   workers stay available for children instead of draining out of the run.
//! * A panic in the callback releases the rest of that worker's batch, marks the
//!   run aborted so the other workers stop promptly (they are notified), and the
//!   panic still propagates to the caller after every worker has stopped.
//!
//! Pushing is the other half of the cost: a condvar wake is a futex syscall, worth
//! a few hundred nanoseconds even when no worker is waiting, so `push` counts parked
//! workers under the lock and wakes one only when there is one to wake.
//!
//! Measured on a 2-core box (2M trivial items, 4x oversubscribed threads): 40-60
//! ns/item per-item-lock before, 4-5 ns/item with batched handoff. The tree shape
//! (thousands of items pushing 4 children each) went from ~235 ns/item to ~40.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex, MutexGuard};
use std::thread;

/// Largest number of items one worker takes per lock acquisition. Large enough that
/// lock traffic disappears under per-item work, small enough to bound how long a
/// worker can sit on work that another idle worker could be doing.
const MAX_BATCH: usize = 64;

struct State<T> {
    queue: VecDeque<T>,
    /// Handed to a worker and not yet finished; a buffered batch item counts.
    active: usize,
    /// Set when a callback panicked; remaining workers stop promptly.
    aborted: bool,
}

struct Shared<T> {
    state: Mutex<State<T>>,
    cv: Condvar,
    /// Workers parked in [`Shared::refill`]. A wake is a futex syscall, so `push`
    /// reads this under the lock and skips it when no worker can be waiting.
    parked: AtomicUsize,
}

impl<T> Shared<T> {
    fn lock(&self) -> MutexGuard<'_, State<T>> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn push(&self, item: T) {
        let mut g = self.lock();
        g.queue.push_back(item);
        let parked = self.parked.load(Ordering::Relaxed) > 0;
        drop(g);
        if parked {
            self.cv.notify_one();
        }
    }

    /// One critical section that both credits `finished` completions and appends up
    /// to `want` items to `buf`. Blocks while the queue is empty but items are still
    /// in flight. Returns how many items were appended; 0 means the worker must stop.
    fn refill(&self, buf: &mut Vec<T>, want: usize, finished: &mut usize) -> usize {
        let mut g = self.lock();
        if *finished > 0 {
            g.active -= *finished;
            *finished = 0;
        }
        loop {
            if g.aborted {
                return 0;
            }
            let n = g.queue.len().min(want);
            if n > 0 {
                buf.extend(g.queue.drain(..n));
                g.active += n;
                return n;
            }
            if g.active == 0 {
                drop(g);
                self.cv.notify_all();
                return 0;
            }
            // Incremented under the lock so a pushing worker either sees this
            // worker parked or finds the queue item that this worker will see.
            self.parked.fetch_add(1, Ordering::Relaxed);
            g = self.cv.wait(g).unwrap_or_else(|e| e.into_inner());
            self.parked.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

/// Releases the items a worker still holds if its callback unwinds, and marks the
/// run aborted so the other workers do not wait for work that will never come.
struct Batch<'a, T> {
    shared: &'a Shared<T>,
    /// Taken from the queue, counted in `active`, not yet finished.
    owed: usize,
}

impl<T> Drop for Batch<'_, T> {
    fn drop(&mut self) {
        if self.owed == 0 {
            return;
        }
        let mut g = self.shared.lock();
        g.active -= self.owed;
        g.aborted = true;
        drop(g);
        self.shared.cv.notify_all();
    }
}

fn worker<T, F>(shared: &Shared<T>, f: &F)
where
    T: Send,
    F: Fn(&T, &dyn Fn(T)) + Send + Sync,
{
    let push = |item: T| shared.push(item);
    let mut buf: Vec<T> = Vec::with_capacity(MAX_BATCH);
    let mut want = 1;
    let mut finished = 0;
    loop {
        let n = shared.refill(&mut buf, want, &mut finished);
        if n == 0 {
            return;
        }
        // A full batch means the queue is under pressure: take more next time. A
        // short one means it is draining: match its size so the tail stays fine
        // grained for the push-heavy shape.
        want = if n == want {
            (want * 2).min(MAX_BATCH)
        } else {
            n
        };
        let mut batch = Batch { shared, owed: n };
        for item in buf.drain(..) {
            f(&item, &push);
            batch.owed -= 1;
        }
        finished = n;
    }
}

/// Consume `initial` work items on `threads` workers. `f` is called for each item
/// and may enqueue more work through the `push` handle it receives. Returns when the
/// queue is empty and no item is still in flight. If `f` panics, the other workers
/// stop and the panic propagates to the caller.
pub fn par_for_each<T, F>(initial: Vec<T>, threads: usize, f: F)
where
    T: Send,
    F: Fn(&T, &dyn Fn(T)) + Send + Sync,
{
    if initial.is_empty() {
        return;
    }
    let shared = Shared {
        state: Mutex::new(State {
            queue: initial.into(),
            active: 0,
            aborted: false,
        }),
        cv: Condvar::new(),
        parked: AtomicUsize::new(0),
    };
    let (shared, f) = (&shared, &f);

    thread::scope(|s| {
        for _ in 0..threads.max(1) {
            s.spawn(move || worker(shared, f));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::panic::{catch_unwind, AssertUnwindSafe};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    fn cores() -> usize {
        thread::available_parallelism().map_or(1, |n| n.get())
    }

    /// Node count of a full tree with `fanout` children per node, levels 0..=depth.
    fn tree_count(fanout: usize, depth: usize) -> usize {
        (0..=depth).map(|d| fanout.pow(d as u32)).sum()
    }

    fn run_tree(threads: usize, fanout: usize, depth: usize) {
        let seen: Vec<AtomicUsize> = (0..tree_count(fanout, depth))
            .map(|_| AtomicUsize::new(0))
            .collect();
        // Heap layout: children of n are n*fanout+1 ..= n*fanout+fanout.
        let first_leaf = tree_count(fanout, depth - 1);
        par_for_each(vec![0usize], threads, |&n, push| {
            seen[n].fetch_add(1, Ordering::SeqCst);
            if n < first_leaf {
                for c in 1..=fanout {
                    push(n * fanout + c);
                }
            }
        });
        for (i, c) in seen.iter().enumerate() {
            assert_eq!(c.load(Ordering::SeqCst), 1, "node {i}");
        }
    }

    #[test]
    fn empty_initial_returns() {
        par_for_each(Vec::<u32>::new(), 8, |_, _| panic!("must not run"));
    }

    #[test]
    fn one_item() {
        let seen = AtomicUsize::new(0);
        par_for_each(vec![7u32], 4, |&n, _| {
            assert_eq!(n, 7);
            seen.fetch_add(1, Ordering::SeqCst);
        });
        assert_eq!(seen.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn threads_one() {
        let seen = AtomicUsize::new(0);
        par_for_each((0..500usize).collect(), 1, |&n, push| {
            seen.fetch_add(1, Ordering::SeqCst);
            if n < 10 {
                push(n + 1000);
            }
        });
        assert_eq!(seen.load(Ordering::SeqCst), 510);
    }

    #[test]
    fn threads_exceed_items() {
        let seen = AtomicUsize::new(0);
        par_for_each((0..3usize).collect(), 64, |_, _| {
            seen.fetch_add(1, Ordering::Relaxed);
        });
        assert_eq!(seen.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn zero_threads_treated_as_one() {
        run_tree(0, 2, 5);
    }

    #[test]
    fn tree_one_thread() {
        run_tree(1, 3, 6);
    }

    #[test]
    fn tree_many_threads() {
        run_tree(64, 3, 6);
    }

    /// A chain hands over one item at a time: a worker that exits when it sees an
    /// empty queue would abandon the rest of the run.
    #[test]
    fn long_chain_keeps_queue_alive() {
        const LEN: usize = 20_000;
        let seen = AtomicUsize::new(0);
        par_for_each(vec![0usize], 8, |&n, push| {
            seen.fetch_add(1, Ordering::SeqCst);
            if n + 1 < LEN {
                push(n + 1);
            }
        });
        assert_eq!(seen.load(Ordering::SeqCst), LEN);
    }

    /// Every item is handed out exactly once, counted far beyond one batch per worker.
    #[test]
    fn exactly_once_under_contention() {
        const N: usize = 100_000;
        let seen = AtomicUsize::new(0);
        let sum = AtomicUsize::new(0);
        par_for_each((0..N).collect(), 8 * cores(), |&n, _| {
            seen.fetch_add(1, Ordering::SeqCst);
            sum.fetch_add(n, Ordering::SeqCst);
        });
        assert_eq!(seen.load(Ordering::SeqCst), N);
        assert_eq!(sum.load(Ordering::SeqCst), N * (N - 1) / 2);
    }

    /// Children are pushed from several levels down, and the run only ends once the
    /// deepest leaf has been visited.
    #[test]
    fn pushes_from_multiple_depths() {
        const DEPTH: usize = 6;
        let leaf_count = AtomicUsize::new(0);
        let node_count = AtomicUsize::new(0);
        // Item is (index, depth); each node spawns 2 children one level deeper.
        par_for_each(vec![(0usize, 0usize)], 6, |&(i, d), push| {
            node_count.fetch_add(1, Ordering::SeqCst);
            if d == DEPTH {
                leaf_count.fetch_add(1, Ordering::SeqCst);
            } else {
                push((2 * i + 1, d + 1));
                push((2 * i + 2, d + 1));
            }
        });
        assert_eq!(node_count.load(Ordering::SeqCst), (1 << (DEPTH + 1)) - 1);
        assert_eq!(leaf_count.load(Ordering::SeqCst), 1 << DEPTH);
    }

    /// Runs `par_for_each` on a helper thread so a hang fails instead of blocking the
    /// test harness; returns whether it panicked.
    fn panics_within_timeout(threads: usize, initial: Vec<u32>) -> bool {
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let r = catch_unwind(AssertUnwindSafe(|| {
                par_for_each(initial, threads, |&n, push| {
                    if n == 5 {
                        panic!("boom");
                    }
                    if n < 50 {
                        push(n + 1000);
                    }
                    thread::sleep(Duration::from_millis(1));
                });
            }));
            let _ = tx.send(r.is_err());
        });
        rx.recv_timeout(Duration::from_secs(10))
            .expect("par_for_each hung after a panic in the callback")
    }

    #[test]
    fn panic_propagates_without_hang() {
        assert!(panics_within_timeout(8, (0..100).collect()));
    }

    #[test]
    fn panic_single_thread_with_pending_work() {
        assert!(panics_within_timeout(1, vec![5, 1, 2, 3]));
    }

    /// The panic arrives after the other workers have stopped, so aborting cannot
    /// leave a worker sleeping on the condvar.
    #[test]
    fn panic_while_batch_is_buffered() {
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let r = catch_unwind(AssertUnwindSafe(|| {
                par_for_each((0..5000u32).collect(), 2, |&n, _| {
                    if n == 4999 {
                        panic!("boom");
                    }
                });
            }));
            let _ = tx.send(r.is_err());
        });
        assert!(rx
            .recv_timeout(Duration::from_secs(10))
            .expect("par_for_each hung after a panic with a buffered batch"));
    }

    #[test]
    fn many_short_items_stress() {
        let total = AtomicUsize::new(0);
        par_for_each((0..1000usize).collect(), 16, |&n, push| {
            total.fetch_add(1, Ordering::SeqCst);
            if n < 1000 {
                push(n + 1000);
                push(n + 2000);
            }
        });
        // 1000 initial items, each pushing 2 leaf children.
        assert_eq!(total.load(Ordering::SeqCst), 3000);
    }

    /// Throughput against a single-threaded loop, printed for the record. No timing
    /// assertion: a shared CI runner cannot be relied on to show a speedup.
    #[test]
    fn throughput_vs_sequential() {
        const N: usize = 200_000;
        let items: Vec<usize> = (0..N).collect();

        let t0 = Instant::now();
        let mut seq = 0usize;
        for v in &items {
            seq = seq.wrapping_add(std::hint::black_box(*v));
        }
        let sequential = t0.elapsed();

        let count = AtomicUsize::new(0);
        let t0 = Instant::now();
        par_for_each(items, 4 * cores(), |&v, _| {
            std::hint::black_box(v);
            count.fetch_add(1, Ordering::Relaxed);
        });
        let parallel = t0.elapsed();

        assert_eq!(seq, (0..N).sum::<usize>());
        assert_eq!(count.load(Ordering::SeqCst), N);
        println!(
            "throughput: {} items, parallel {:.0} ns/item vs sequential {:.0} ns/item ({:.2}x)",
            N,
            parallel.as_nanos() as f64 / N as f64,
            sequential.as_nanos() as f64 / N as f64,
            sequential.as_secs_f64() / parallel.as_secs_f64()
        );
    }

    /// Micro-benchmark over a fixed large workload; prints cost, asserts only that
    /// every item ran.
    #[test]
    fn bench_two_million_trivial_items() {
        const N: usize = 2_000_000;
        let threads = cores();
        let count = AtomicUsize::new(0);
        let t0 = Instant::now();
        par_for_each((0..N).collect(), threads, |&v, _| {
            std::hint::black_box(v);
            count.fetch_add(1, Ordering::Relaxed);
        });
        let el = t0.elapsed();
        assert_eq!(count.load(Ordering::SeqCst), N);
        println!(
            "bench: {N} items, {threads} threads: {:.1} ns/item, {:.1} Mitems/s",
            el.as_nanos() as f64 / N as f64,
            N as f64 / el.as_secs_f64() / 1e6
        );
    }

    /// The push-heavy shape: 1000 roots fanning out 4 ways over 3 levels.
    #[test]
    fn bench_tree_expansion() {
        const ROOTS: usize = 1000;
        const FANOUT: usize = 4;
        const DEPTH: usize = 3;
        let seen = AtomicUsize::new(0);
        let t0 = Instant::now();
        par_for_each(
            (0..ROOTS).map(|r| (r, 0usize)).collect(),
            cores(),
            |&(n, d), push| {
                seen.fetch_add(1, Ordering::Relaxed);
                if d < DEPTH {
                    for c in 0..FANOUT {
                        push((n * FANOUT + c, d + 1));
                    }
                }
            },
        );
        let el = t0.elapsed();
        let total = seen.load(Ordering::SeqCst);
        let expected = ROOTS * tree_count(FANOUT, DEPTH);
        assert_eq!(total, expected);
        println!(
            "bench tree: {total} items in {el:?} ({:.0} ns/item)",
            el.as_nanos() as f64 / total as f64
        );
    }
}
