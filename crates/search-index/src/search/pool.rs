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
    P.get_or_init(|| Pool::new(std::thread::available_parallelism().map_or(4, |n| n.get()).max(2), IDLE))
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
}
