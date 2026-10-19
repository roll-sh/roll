//! Thread per core: each core thread runs an event loop of its own, and every database has a
//! home core where its connections run. A database's data then stays in one core's caches, its
//! engine lock is uncontended, and connections don't bounce between threads; idle cores sleep in
//! the kernel. Connections that must block in place (the Lua debugger) run on a small
//! multi-thread runtime instead.

use std::cell::Cell;
use std::future::Future;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::runtime::{Builder, Handle, Runtime};

pub struct Cores {
    cores: Vec<Handle>,
    /// Next home core to hand out.
    next: AtomicUsize,
    slow: OnceLock<Runtime>,
}

/// Where a connection should run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Place {
    Core(usize),
    /// The multi-thread runtime for connections that block in place.
    Slow,
}

thread_local! {
    static CURRENT: Cell<Option<usize>> = const { Cell::new(None) };
}

/// The core this thread runs, if it is a core thread.
pub fn current() -> Option<usize> {
    CURRENT.get()
}

/// The process's cores: `ROLL_CORES` of them, else one per available CPU.
pub fn pool() -> &'static Cores {
    static POOL: OnceLock<Cores> = OnceLock::new();
    POOL.get_or_init(|| {
        let n = std::env::var("ROLL_CORES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or_else(|| std::thread::available_parallelism().map_or(1, |n| n.get()))
            .max(1);
        Cores::start(n)
    })
}

impl Cores {
    fn start(n: usize) -> Self {
        let cores = (0..n)
            .map(|i| {
                let (tx, rx) = std::sync::mpsc::channel();
                std::thread::Builder::new()
                    .name(format!("roll-core-{i}"))
                    .spawn(move || {
                        let rt = Builder::new_current_thread().enable_all().build().expect("core runtime");
                        CURRENT.set(Some(i));
                        let _ = tx.send(rt.handle().clone());
                        rt.block_on(std::future::pending::<()>());
                    })
                    .expect("core thread");
                rx.recv().expect("core started")
            })
            .collect();
        Self { cores, next: AtomicUsize::new(0), slow: OnceLock::new() }
    }

    pub fn len(&self) -> usize {
        self.cores.len()
    }

    pub fn is_empty(&self) -> bool {
        self.cores.is_empty()
    }

    /// A home core for a new database (round robin).
    pub fn assign(&self) -> usize {
        self.next.fetch_add(1, Ordering::Relaxed) % self.cores.len()
    }

    pub fn handle(&self, place: Place) -> &Handle {
        match place {
            Place::Core(i) => &self.cores[i % self.cores.len()],
            Place::Slow => self.slow.get_or_init(|| Builder::new_multi_thread().worker_threads(2).thread_name("roll-slow").enable_all().build().expect("slow runtime")).handle(),
        }
    }

    pub fn spawn<F: Future<Output = ()> + Send + 'static>(&self, place: Place, f: F) {
        self.handle(place).spawn(f);
    }
}
