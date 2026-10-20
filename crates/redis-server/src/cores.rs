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
    /// Started on first use: a process serving one database runs one core thread (more only
    /// while it spreads), not one per CPU.
    cores: Vec<OnceLock<Handle>>,
    /// The CPU each core thread is pinned to, if pinning.
    cpus: Vec<usize>,
    loads: Vec<Load>,
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
        let loads: Vec<Load> = (0..n).map(|_| Load::default()).collect();
        // Each core thread on a CPU of its own (of those the process may use), unless ROLL_PIN=0.
        let cpus = if std::env::var("ROLL_PIN").is_ok_and(|v| v == "0") { Vec::new() } else { affinity::allowed() };
        Self { cores: (0..n).map(|_| OnceLock::new()).collect(), cpus, loads, next: AtomicUsize::new(0), slow: OnceLock::new() }
    }

    /// Core `i`'s runtime, its thread started now if it wasn't.
    fn core(&self, i: usize) -> &Handle {
        self.cores[i].get_or_init(|| {
            let (tx, rx) = std::sync::mpsc::channel();
            let cpu = (!self.cpus.is_empty()).then(|| self.cpus[i % self.cpus.len()]);
            std::thread::Builder::new()
                .name(format!("roll-core-{i}"))
                .spawn(move || {
                    if let Some(cpu) = cpu {
                        affinity::pin(cpu);
                    }
                    let rt = Builder::new_current_thread()
                        .enable_all()
                        .on_thread_park(move || pool().loads[i].parked())
                        .on_thread_unpark(move || pool().loads[i].unparked())
                        .build()
                        .expect("core runtime");
                    CURRENT.set(Some(i));
                    let _ = tx.send(rt.handle().clone());
                    rt.block_on(std::future::pending::<()>());
                })
                .expect("core thread");
            rx.recv().expect("core started")
        })
    }

    /// How busy core `i` was lately (percent, over about the last 100ms).
    pub fn load(&self, i: usize) -> u32 {
        let i = i % self.loads.len();
        // A core not started yet is idle.
        if self.cores[i].get().is_none() {
            return 0;
        }
        self.loads[i].percent()
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

    /// The core a connection of a database homed on `home` runs on: the home core, or when the
    /// database is spread over `spread` more cores, one of those (its commands still run on the
    /// home core, which then does little else; the others do the connections' I/O and parsing).
    pub fn place_for(&self, home: usize, spread: usize, client: u64) -> usize {
        let spread = spread.min(self.cores.len() - 1);
        if spread == 0 {
            return home;
        }
        (home + 1 + client as usize % spread) % self.cores.len()
    }

    pub fn handle(&self, place: Place) -> &Handle {
        match place {
            Place::Core(i) => self.core(i % self.cores.len()),
            Place::Slow => self.slow.get_or_init(|| Builder::new_multi_thread().worker_threads(2).thread_name("roll-slow").on_thread_park(crate::idle_collect).enable_all().build().expect("slow runtime")).handle(),
        }
    }

    pub fn spawn<F: Future<Output = ()> + Send + 'static>(&self, place: Place, f: F) {
        self.handle(place).spawn(f);
    }

    /// Every started core hands its free memory back ([`crate::release_memory_throttled`]): a
    /// database going idle may have spread its connections over cores that run nothing else.
    pub fn purge_started(&self) {
        for (i, c) in self.cores.iter().enumerate() {
            if c.get().is_some() {
                self.core(i).spawn(async { crate::release_memory_throttled() });
            }
        }
    }

    /// Spawn the future `f` makes, made there: its state is allocated (and later freed) by the
    /// thread that runs it, not the one handing it over, so that thread's allocator gets it back
    /// when it goes idle (a connection moving to its core).
    pub fn spawn_built<F, Fut>(&self, place: Place, f: F)
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        self.handle(place).spawn(async move {
            tokio::spawn(f());
        });
    }
}

/// A core's idle time, from its runtime's park hooks, and the busy share last measured.
#[derive(Default)]
struct Load {
    /// Monotonic µs spent parked so far, and when the current park began (0: not parked).
    idle_us: std::sync::atomic::AtomicU64,
    parked_at: std::sync::atomic::AtomicU64,
    /// The last measurement: when, the idle total then, and the busy percent over the window.
    window: parking_lot::Mutex<(u64, u64)>,
    percent: std::sync::atomic::AtomicU32,
}

impl Load {
    fn parked(&self) {
        self.parked_at.store(redis_engine::util::mono_us().max(1), Ordering::Relaxed);
    }

    fn unparked(&self) {
        let at = self.parked_at.swap(0, Ordering::Relaxed);
        if at > 0 {
            self.idle_us.fetch_add(redis_engine::util::mono_us().saturating_sub(at), Ordering::Relaxed);
        }
    }

    /// Busy percent over the last window of at least 100ms (a saturated core rarely parks, so
    /// this is measured by whoever asks, not by the core).
    fn percent(&self) -> u32 {
        let now = redis_engine::util::mono_us();
        let Some(mut w) = self.window.try_lock() else { return self.percent.load(Ordering::Relaxed) };
        let elapsed = now.saturating_sub(w.0);
        if elapsed >= 100_000 {
            let at = self.parked_at.load(Ordering::Relaxed);
            let idle = self.idle_us.load(Ordering::Relaxed) + if at > 0 { now.saturating_sub(at) } else { 0 };
            let busy = elapsed.saturating_sub(idle.saturating_sub(w.1));
            if w.0 > 0 {
                self.percent.store((busy * 100 / elapsed).min(100) as u32, Ordering::Relaxed);
            }
            *w = (now, idle);
        }
        self.percent.load(Ordering::Relaxed)
    }
}

/// CPU affinity: on Linux, core threads stay on one CPU each, so their caches stay theirs and the
/// scheduler doesn't move a busy event loop around. Elsewhere, nothing.
mod affinity {
    /// The CPUs this process may run on (its affinity mask: taskset, container cpusets).
    #[cfg(target_os = "linux")]
    pub fn allowed() -> Vec<usize> {
        // SAFETY: `cpu_set_t` is plain data; the kernel fills it for this thread (the process's
        // mask, inherited by the threads started after).
        unsafe {
            let mut set: libc::cpu_set_t = std::mem::zeroed();
            if libc::sched_getaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &mut set) != 0 {
                return Vec::new();
            }
            (0..libc::CPU_SETSIZE as usize).filter(|&c| libc::CPU_ISSET(c, &set)).collect()
        }
    }

    /// Keep the calling thread on `cpu`.
    #[cfg(target_os = "linux")]
    pub fn pin(cpu: usize) {
        // SAFETY: as above; a failure leaves the thread unpinned.
        unsafe {
            let mut set: libc::cpu_set_t = std::mem::zeroed();
            libc::CPU_SET(cpu, &mut set);
            if libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set) != 0 {
                tracing::warn!(cpu, "could not pin a core thread");
            }
        }
    }

    #[cfg(not(target_os = "linux"))]
    pub fn allowed() -> Vec<usize> {
        Vec::new()
    }

    #[cfg(not(target_os = "linux"))]
    pub fn pin(_cpu: usize) {}
}
