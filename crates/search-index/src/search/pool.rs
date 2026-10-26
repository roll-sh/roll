//! The worker threads of concurrent searches, started on demand and gone when idle.
//!
//! Spawning a thread per slice costs tens of microseconds per search — as much as a whole
//! medium query. Workers here take slices from one process-wide queue instead and exit after
//! [`IDLE`] without work, so a node that stops searching holds no search thread (the engine's
//! zero-idle-cost rule) while a busy one reuses warm threads. At most one worker per core
//! exists; a search beyond that waits in the queue (its calling thread works meanwhile, see
//! `Searcher::search_concurrent`). Parked workers form a stack and work goes to the most
//! recently parked one (warm caches, an awake core); the rest stay parked until they time out.
//! A job's result reaches its caller before the worker is back on the stack, so callers that
//! search again right away hand the pool a "hold" ([`execute_held`]) the worker drops only
//! once it has re-parked: waiting for every hold makes back-to-back searches wake the same
//! warm workers.
//! Jobs own everything they touch (`Arc`s), so no job outlives
//! borrowed data; a job that panics is reported by the caller's result channel closing.

use std::collections::VecDeque;
use std::sync::OnceLock;
use std::thread::Thread;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

/// How long a worker waits for work before it exits.
pub const IDLE: Duration = Duration::from_secs(2);

type Job = Box<dyn FnOnce() + Send + 'static>;
/// Dropped by the worker once it is back in the pool (see [`execute_held`]).
type Hold = Option<Box<dyn Send + 'static>>;

struct Pool {
    state: Mutex<State>,
    max: usize,
    idle: Duration,
}

#[derive(Default)]
struct State {
    queue: VecDeque<(Job, Hold)>,
    threads: usize,
    /// Parked workers, most recently idle last: work goes to the top of the stack. A worker
    /// that just finished has a warm cache and a core still awake, while one parked for long
    /// may sit on a core in a deep sleep state; waking in FIFO order (a condition variable's)
    /// rotates through the coldest threads whenever there are more workers than jobs and
    /// measurably slows every slice. Invariant: every queued job either has a worker popped
    /// off this stack for it or waits for a busy worker, which checks the queue before it
    /// parks; so a worker on the stack never needs to look at the queue.
    idle: Vec<(u64, Thread)>,
    next_id: u64,
}

fn pool() -> &'static Pool {
    static P: OnceLock<Pool> = OnceLock::new();
    P.get_or_init(|| Pool::new(cores().max(2), IDLE))
}

/// The CPUs this process may use (`std::thread::available_parallelism`), looked up once.
/// The std call re-reads the cgroup's `cpu.max` and the mount table on every call (a few
/// `open`/`read`/`statx` per call): called per search, it was more than a quarter of the CPU
/// time of a `size: 0` search.
pub fn cores() -> usize {
    static N: OnceLock<usize> = OnceLock::new();
    *N.get_or_init(|| std::thread::available_parallelism().map_or(1, |n| n.get()))
}

thread_local! {
    static IN_WORKER: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Whether the current thread is a pool worker (nested concurrent searches run inline there,
/// so workers never wait on each other).
pub fn in_worker() -> bool {
    IN_WORKER.with(|w| w.get())
}

/// Runs `job` on a worker: the most recently idle one, a new one while under one per core,
/// else the next free one.
pub fn execute(job: Job) {
    pool().execute(job, None);
}

/// [`execute`], and drops `hold` only once the worker that ran `job` is back in the pool
/// (parked on top of the idle stack, or onto its next job). A caller that waits for its
/// jobs by a channel closing passes a sender clone here: when the channel closes, every
/// worker of the search has re-parked, so the next search wakes the same warm threads
/// instead of whichever parked earlier (results arrive before a worker gets back).
pub fn execute_held(job: Job, hold: Box<dyn Send + 'static>) {
    pool().execute(job, Some(hold));
}

/// Runs `f(0)`, …, `f(n - 1)` on the calling thread and up to `helpers` pool workers, and
/// returns once every call has returned; a panic in any call is resumed here afterwards.
///
/// `f` may borrow from the caller (unlike [`execute`]'s `'static` jobs): workers reach it
/// through a pointer in shared state and claim indices from an atomic counter, and the caller
/// returns only after every claimed index has finished. A worker that starts late finds no
/// index left and never touches `f`, so the pointer is never used once this returns. The
/// caller claims indices too, so the call completes even when every worker is busy with
/// searches (it then runs everything itself). Inside a worker, everything runs inline.
pub fn for_each_index(n: usize, helpers: usize, f: &(dyn Fn(usize) + Sync)) {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    if n == 0 {
        return;
    }
    if n == 1 || helpers == 0 || in_worker() {
        (0..n).for_each(f);
        return;
    }
    struct Shared {
        /// `f`, lifetime erased: dereferenced only for a claimed index below `n`.
        f: *const (dyn Fn(usize) + Sync + 'static),
        n: usize,
        next: AtomicUsize,
        done: Mutex<(usize, Option<Box<dyn std::any::Any + Send>>)>,
        caller: Thread,
    }
    // SAFETY: `f` is `Sync`, and it is only called while `for_each_index` waits (see below).
    unsafe impl Send for Shared {}
    unsafe impl Sync for Shared {}
    impl Shared {
        /// Claims and runs indices until none is left.
        fn drain(&self) {
            loop {
                let i = self.next.fetch_add(1, Ordering::AcqRel);
                if i >= self.n {
                    return;
                }
                // SAFETY: `i < n` was claimed before the caller saw `done == n`, so the caller
                // is still waiting and `f` is alive.
                let f = unsafe { &*self.f };
                let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(i)));
                let mut d = self.done.lock();
                d.0 += 1;
                if let Err(p) = r {
                    d.1.get_or_insert(p);
                }
                if d.0 == self.n {
                    self.caller.unpark();
                }
            }
        }
    }
    // SAFETY: only the lifetime is erased; see `Shared::f`.
    let f: *const (dyn Fn(usize) + Sync + 'static) = unsafe { std::mem::transmute(f as *const (dyn Fn(usize) + Sync)) };
    let shared = Arc::new(Shared { f, n, next: AtomicUsize::new(0), done: Mutex::new((0, None)), caller: std::thread::current() });
    for _ in 0..helpers.min(n - 1) {
        let s = shared.clone();
        execute(Box::new(move || s.drain()));
    }
    shared.drain();
    loop {
        let mut d = shared.done.lock();
        if d.0 == n {
            if let Some(p) = d.1.take() {
                drop(d);
                std::panic::resume_unwind(p);
            }
            return;
        }
        drop(d);
        std::thread::park();
    }
}

/// Live worker threads (for tests and stats).
pub fn threads() -> usize {
    pool().state.lock().threads
}

impl Pool {
    fn new(max: usize, idle: Duration) -> Pool {
        Pool { state: Mutex::new(State::default()), max, idle }
    }

    fn execute(&'static self, job: Job, hold: Hold) {
        let mut st = self.state.lock();
        st.queue.push_back((job, hold));
        if let Some((_, t)) = st.idle.pop() {
            drop(st);
            t.unpark();
            return;
        }
        if st.threads < self.max {
            st.threads += 1;
            st.next_id += 1;
            let id = st.next_id;
            drop(st);
            let spawned = std::thread::Builder::new().name("roll-search".into()).spawn(move || self.worker(id));
            if spawned.is_err() {
                // Out of threads: the queued job waits for a running worker; with none left, run
                // it here rather than lose it.
                let mut st = self.state.lock();
                st.threads -= 1;
                if st.threads == 0
                    && let Some((j, h)) = st.queue.pop_front()
                {
                    drop(st);
                    j();
                    drop(h);
                }
            }
        }
    }

    fn worker(&'static self, id: u64) {
        IN_WORKER.with(|w| w.set(true));
        let mut st = self.state.lock();
        let mut hold: Hold = None;
        loop {
            if let Some((job, h)) = st.queue.pop_front() {
                drop(st);
                // The previous job's hold goes once this worker has its next job.
                drop(hold.take());
                // A panicking job only drops its result sender; the worker carries on.
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(job));
                hold = h;
                st = self.state.lock();
                continue;
            }
            st.idle.push((id, std::thread::current()));
            let deadline = Instant::now() + self.idle;
            drop(st);
            drop(hold.take());
            loop {
                std::thread::park_timeout(deadline.saturating_duration_since(Instant::now()));
                st = self.state.lock();
                // Popped off the stack: `execute` queued a job for this worker.
                let Some(i) = st.idle.iter().position(|(w, _)| *w == id) else { break };
                if Instant::now() >= deadline {
                    st.idle.remove(i);
                    st.threads -= 1;
                    return;
                }
                // A spurious wake-up.
                drop(st);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A private pool, so other tests' concurrent searches can't wake or park its workers.
    fn private(max: usize, idle: Duration) -> &'static Pool {
        Box::leak(Box::new(Pool::new(max, idle)))
    }

    /// Runs `job` and waits until the worker that ran it is back in the pool.
    fn run_held<T: Send + 'static>(p: &'static Pool, job: impl FnOnce() -> T + Send + 'static) -> T {
        let (tx, rx) = std::sync::mpsc::channel();
        let held = tx.clone();
        p.execute(Box::new(move || tx.send(job()).unwrap()), Some(Box::new(held)));
        let v = rx.recv().unwrap();
        // Closed only when the hold drops too.
        assert!(rx.recv().is_err());
        v
    }

    #[test]
    fn work_goes_to_the_most_recently_idle_worker() {
        let p = private(4, Duration::from_secs(30));
        // Two workers, both busy at once, then both back in the pool.
        let gate = Arc::new(std::sync::Barrier::new(3));
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        for _ in 0..2 {
            let (g, h) = (gate.clone(), tx.clone());
            p.execute(Box::new(move || {
                g.wait();
            }), Some(Box::new(h)));
        }
        drop(tx);
        gate.wait();
        assert!(rx.recv().is_err());
        assert_eq!(p.state.lock().idle.len(), 2);
        let ran: Vec<_> = (0..50).map(|_| run_held(p, || std::thread::current().id())).collect();
        // A condition variable's FIFO order would alternate between the two.
        assert!(ran.iter().all(|&t| t == ran[0]), "{ran:?}");
        let st = p.state.lock();
        assert_eq!((st.threads, st.idle.len()), (2, 2));
    }

    #[test]
    fn a_hold_drops_only_after_the_worker_parks() {
        let p = private(2, Duration::from_secs(30));
        for _ in 0..200 {
            run_held(p, || ());
            // The worker that ran the job is on the stack by the time the hold dropped.
            let st = p.state.lock();
            assert_eq!(st.idle.len(), st.threads);
        }
    }

    #[test]
    fn runs_every_job_and_survives_panics() {
        let p = private(4, Duration::from_millis(200));
        let done = Arc::new(AtomicUsize::new(0));
        let (tx, rx) = std::sync::mpsc::channel();
        for i in 0..200 {
            let (done, tx) = (done.clone(), tx.clone());
            p.execute(
                Box::new(move || {
                    if i % 50 == 7 {
                        panic!("job {i}");
                    }
                    done.fetch_add(1, Ordering::Relaxed);
                    tx.send(in_worker()).unwrap();
                }),
                None,
            );
        }
        drop(tx);
        let flags: Vec<bool> = rx.iter().collect();
        assert_eq!(flags.len(), 196);
        assert!(flags.iter().all(|&w| w));
        assert_eq!(done.load(Ordering::Relaxed), 196);
        assert!(p.state.lock().threads <= p.max);
        assert!(!in_worker());
    }

    #[test]
    fn workers_exit_when_idle() {
        let idle = Duration::from_millis(100);
        let p = private(4, idle);
        let (tx, rx) = std::sync::mpsc::channel();
        for _ in 0..4 {
            let tx = tx.clone();
            p.execute(Box::new(move || tx.send(()).unwrap()), None);
        }
        for _ in 0..4 {
            rx.recv().unwrap();
        }
        let t = Instant::now();
        while p.state.lock().threads > 0 {
            assert!(t.elapsed() < idle * 50, "{} workers still alive", p.state.lock().threads);
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(p.state.lock().idle.is_empty());
    }

    #[test]
    fn global_pool_runs_held_jobs() {
        let (tx, rx) = std::sync::mpsc::channel();
        let held = tx.clone();
        execute_held(Box::new(move || tx.send(in_worker()).unwrap()), Box::new(held));
        assert!(rx.recv().unwrap());
        assert!(rx.recv().is_err());
    }

    /// Borrowed state, every index exactly once, across threads, back to back.
    #[test]
    fn for_each_index_runs_every_index_once() {
        for n in [0usize, 1, 2, 7, 64, 1000] {
            let hits: Vec<AtomicUsize> = (0..n).map(|_| AtomicUsize::new(0)).collect();
            let threads = Mutex::new(std::collections::HashSet::new());
            for_each_index(n, 8, &|i| {
                hits[i].fetch_add(1, Ordering::Relaxed);
                threads.lock().insert(std::thread::current().id());
                if n == 1000 {
                    std::thread::sleep(Duration::from_micros(50));
                }
            });
            assert!(hits.iter().all(|h| h.load(Ordering::Relaxed) == 1), "n {n}");
            if n == 1000 {
                assert!(threads.lock().len() > 1);
            }
        }
    }

    /// A panicking index still lets every other index finish, then reaches the caller.
    #[test]
    fn for_each_index_resumes_panics() {
        let done = AtomicUsize::new(0);
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            for_each_index(50, 4, &|i| {
                if i == 17 {
                    panic!("index 17");
                }
                done.fetch_add(1, Ordering::Relaxed);
            })
        }));
        assert_eq!(*r.unwrap_err().downcast::<&str>().unwrap(), "index 17");
        assert_eq!(done.load(Ordering::Relaxed), 49);
    }
}
