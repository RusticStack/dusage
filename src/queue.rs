//! Shared parallel work queue used by dusage and fastdel. Owned by wave-1 lane `queue`.

use std::collections::VecDeque;
use std::sync::{Condvar, Mutex, MutexGuard};
use std::thread;

struct State<T> {
    queue: VecDeque<T>,
    /// Items popped but not yet finished.
    active: usize,
    /// Set when a callback panicked; remaining workers stop promptly.
    aborted: bool,
}

struct Shared<T> {
    state: Mutex<State<T>>,
    cv: Condvar,
}

impl<T> Shared<T> {
    fn lock(&self) -> MutexGuard<'_, State<T>> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn push(&self, item: T) {
        self.lock().queue.push_back(item);
        self.cv.notify_one();
    }

    /// Block for the next item (marking it active) or `None` when all work is done / aborted.
    fn next(&self) -> Option<T> {
        let mut g = self.lock();
        loop {
            if g.aborted {
                return None;
            }
            if let Some(item) = g.queue.pop_front() {
                g.active += 1;
                return Some(item);
            }
            if g.active == 0 {
                drop(g);
                self.cv.notify_all();
                return None;
            }
            g = self.cv.wait(g).unwrap_or_else(|e| e.into_inner());
        }
    }
}

/// Decrements `active` when an item finishes; on unwind it also aborts the run.
struct ItemGuard<'a, T> {
    shared: &'a Shared<T>,
    finished: bool,
}

impl<T> Drop for ItemGuard<'_, T> {
    fn drop(&mut self) {
        let mut g = self.shared.lock();
        g.active -= 1;
        if !self.finished {
            g.aborted = true;
        }
        let wake = g.aborted || (g.queue.is_empty() && g.active == 0);
        drop(g);
        if wake {
            self.shared.cv.notify_all();
        }
    }
}

/// Consume `initial` work items on `threads` workers. `f` is called for each
/// item and may enqueue more work through the `push` handle it receives.
/// Returns when the queue is empty and every worker is idle. If `f` panics,
/// the other workers stop and the panic propagates to the caller.
pub fn par_for_each<T, F>(initial: Vec<T>, threads: usize, f: F)
where
    T: Send + 'static,
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
    };
    let (shared, f) = (&shared, &f);

    thread::scope(|s| {
        for _ in 0..threads.max(1) {
            s.spawn(move || {
                let push = |t: T| shared.push(t);
                while let Some(item) = shared.next() {
                    let mut guard = ItemGuard {
                        shared,
                        finished: false,
                    };
                    f(&item, &push);
                    guard.finished = true;
                }
            });
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::panic::{catch_unwind, AssertUnwindSafe};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::time::Duration;

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
    fn tree_one_thread() {
        run_tree(1, 3, 6);
    }

    #[test]
    fn tree_many_threads() {
        run_tree(64, 3, 6);
    }

    #[test]
    fn zero_threads_treated_as_one() {
        run_tree(0, 2, 5);
    }

    #[test]
    fn empty_initial_returns() {
        par_for_each(Vec::<u32>::new(), 8, |_, _| panic!("must not run"));
    }

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
            .expect("par_for_each hung after panic")
    }

    #[test]
    fn panic_propagates_without_hang() {
        assert!(panics_within_timeout(8, (0..100).collect()));
    }

    #[test]
    fn panic_single_thread_with_pending_work() {
        assert!(panics_within_timeout(1, vec![5, 1, 2, 3]));
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
}
