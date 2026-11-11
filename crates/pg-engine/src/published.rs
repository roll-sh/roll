//! A value every session reads and that is seldom replaced (a database's catalog, its
//! transaction id table, the store's view), read without writing a cache line other threads
//! read too.
//!
//! Reading an `RwLock<Arc<T>>` takes the lock (an atomic add and a subtract on its word) and
//! counts the reference: three writes, each moving a line every session's thread shares to
//! this core, many times a statement. Here a connection's thread keeps the value it last read
//! with the generation it had (`Copy`, under a mutex only that thread and a replacement take);
//! a reader loads the generation, which changes only when the value is replaced, and borrows
//! its copy while that is the one. A replacement empties every thread's copy, so an idle
//! connection holds no old value (an old view of the store holds memtables and files).

use parking_lot::{Mutex, RwLock, RwLockWriteGuard};
use std::any::Any;
use std::cell::RefCell;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

pub struct Published<T> {
    cur: RwLock<Arc<T>>,
    /// Moves on with every replacement, under the write lock.
    generation: AtomicU64,
    /// Which one it is in the threads' caches.
    id: u64,
    /// The threads' copies.
    copies: Mutex<Vec<Arc<Copy<T>>>>,
}

/// One thread's copy and its generation.
struct Copy<T>(Mutex<Option<(u64, Arc<T>)>>);

thread_local! {
    /// This thread's copies: (`Published::id`, its `Arc<Copy<T>>`).
    static CACHE: RefCell<Vec<(u64, Box<dyn Any>)>> = const { RefCell::new(Vec::new()) };
    /// This thread keeps copies (`keep_on_this_thread`).
    static KEEPS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Keep copies on this thread from now on (a connection's). Other threads (background work,
/// parallel workers) read the lock as before.
pub fn keep_on_this_thread() {
    KEEPS.with(|k| k.set(true));
}

impl<T: Send + Sync + 'static> Published<T> {
    pub fn new(v: Arc<T>) -> Published<T> {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Published {
            cur: RwLock::new(v),
            generation: AtomicU64::new(0),
            id: NEXT.fetch_add(1, Ordering::Relaxed),
            copies: Mutex::new(Vec::new()),
        }
    }

    /// The current value.
    pub fn load(&self) -> Arc<T> {
        self.with(Arc::clone)
    }

    /// `f` of the current value, without a reference of its own (`f` runs with this
    /// thread's copy locked: a nested read of the same value takes the lock).
    pub fn with<R>(&self, f: impl FnOnce(&Arc<T>) -> R) -> R {
        let g = self.generation.load(Ordering::Acquire);
        let mut f = Some(f);
        if let Ok(Some(r)) = CACHE.try_with(|c| {
            let c = c.try_borrow().ok()?;
            let copy = c.iter().find(|e| e.0 == self.id)?.1.downcast_ref::<Arc<Copy<T>>>()?;
            let slot = copy.0.try_lock()?;
            let (cg, v) = slot.as_ref().filter(|(cg, _)| *cg == g)?;
            debug_assert_eq!(*cg, g);
            f.take().map(|f| f(v))
        }) {
            return r;
        }
        let f = f.expect("not taken without a result");
        let (v, g) = {
            let r = self.cur.read();
            // (Under the lock the generation is the value's.)
            (r.clone(), self.generation.load(Ordering::Acquire))
        };
        if KEEPS.with(std::cell::Cell::get) {
            self.keep(g, &v);
        }
        f(&v)
    }

    /// This thread's copy is `v`, of generation `g`.
    fn keep(&self, g: u64, v: &Arc<T>) {
        let _ = CACHE.try_with(|c| {
            let Ok(mut c) = c.try_borrow_mut() else { return };
            let copy = match c.iter().find(|e| e.0 == self.id).and_then(|e| e.1.downcast_ref::<Arc<Copy<T>>>()) {
                Some(copy) => copy.clone(),
                None => {
                    let copy = Arc::new(Copy(Mutex::new(None)));
                    {
                        let mut all = self.copies.lock();
                        // (Copies of threads that ended go.)
                        all.retain(|x| Arc::strong_count(x) > 1);
                        all.push(copy.clone());
                    }
                    // (A few: one of a database the thread no longer reads goes first.)
                    if c.len() >= 8 {
                        c.remove(0);
                    }
                    c.push((self.id, Box::new(copy.clone())));
                    copy
                }
            };
            if let Some(mut s) = copy.0.try_lock() {
                let old = s.replace((g, v.clone()));
                drop(s);
                drop(old);
            }
        });
    }

    /// The value, to replace: the generation moves on when the guard goes, and the threads'
    /// copies are emptied.
    pub fn write(&self) -> PublishedWrite<'_, T> {
        PublishedWrite { g: Some(self.cur.write()), p: self }
    }

    /// Empty every thread's copy, one at a time and holding no other lock (a reader holds
    /// its copy's lock while it reads).
    fn forget_copies(&self) {
        let all: Vec<Arc<Copy<T>>> = {
            let mut all = self.copies.lock();
            all.retain(|x| Arc::strong_count(x) > 1);
            all.clone()
        };
        for c in all {
            let old = c.0.lock().take();
            drop(old);
        }
    }
}

impl<T> Drop for Published<T> {
    fn drop(&mut self) {
        // (The threads' copies of a value that goes with it go too.)
        for c in self.copies.get_mut().drain(..) {
            c.0.lock().take();
        }
    }
}

pub struct PublishedWrite<'a, T: Send + Sync + 'static> {
    g: Option<RwLockWriteGuard<'a, Arc<T>>>,
    p: &'a Published<T>,
}

impl<T: Send + Sync + 'static> std::ops::Deref for PublishedWrite<'_, T> {
    type Target = Arc<T>;
    fn deref(&self) -> &Arc<T> {
        self.g.as_ref().expect("held until dropped")
    }
}

impl<T: Send + Sync + 'static> std::ops::DerefMut for PublishedWrite<'_, T> {
    fn deref_mut(&mut self) -> &mut Arc<T> {
        self.g.as_mut().expect("held until dropped")
    }
}

impl<T: Send + Sync + 'static> Drop for PublishedWrite<'_, T> {
    fn drop(&mut self) {
        // (Before the lock goes: a reader under it reads the value's generation.)
        self.p.generation.fetch_add(1, Ordering::Release);
        drop(self.g.take());
        self.p.forget_copies();
    }
}
