//! Scoped library-test evidence for the actual index-build participants.

use crate::error::{Error, Result};
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

pub(crate) type Candidate = (Vec<u8>, usize, Vec<u8>);

#[derive(Clone, Debug, Default)]
pub(crate) struct Participant {
    pub thread: String,
    pub chunks: Vec<usize>,
    pub rows: usize,
    pub validated: usize,
    pub model_returns: usize,
    pub expanded_text: usize,
    pub minimum: Option<Candidate>,
    pub error: Option<Error>,
    pub coordinator_aborted: bool,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct Trace {
    pub identity: Option<(u32, u32, String)>,
    pub model: bool,
    pub requested: usize,
    pub reserved: usize,
    pub spawned: usize,
    pub participants: BTreeMap<usize, Participant>,
    pub serial_rows: usize,
    pub serial_validated: usize,
    pub fold: Vec<(usize, Option<Candidate>)>,
    pub minimum: Option<Candidate>,
    pub outcome: Option<Error>,
    pub gate_error: Option<String>,
}

#[derive(Default)]
struct State {
    trace: Trace,
    claimed: usize,
    completed: BTreeSet<usize>,
    aborted: Option<Error>,
}

pub(crate) struct Observer {
    state: Mutex<State>,
    changed: Condvar,
    coordinate: bool,
}

thread_local! {
    static CURRENT: RefCell<Option<Arc<Observer>>> = const { RefCell::new(None) };
}

pub(crate) fn current() -> Option<Arc<Observer>> {
    CURRENT.with(|c| c.borrow().clone())
}

pub(crate) fn capture<T>(coordinate: bool, f: impl FnOnce() -> Result<T>) -> (Result<T>, Trace) {
    let observer = Arc::new(Observer { state: Mutex::new(State::default()), changed: Condvar::new(), coordinate });
    struct Reset(Option<Arc<Observer>>);
    impl Drop for Reset {
        fn drop(&mut self) {
            CURRENT.with(|c| *c.borrow_mut() = self.0.take());
        }
    }
    let _reset = Reset(CURRENT.with(|c| c.replace(Some(observer.clone()))));
    let result = f();
    let mut state = observer.state.lock().unwrap();
    state.trace.outcome = result.as_ref().err().cloned();
    (result, state.trace.clone())
}

impl Observer {
    pub(crate) fn identify(&self, table: u32, index: u32, name: &str) {
        let mut s = self.state.lock().unwrap();
        assert!(s.trace.identity.is_none(), "capture must contain one build");
        s.trace.identity = Some((table, index, name.into()));
    }

    pub(crate) fn reserve(&self, model: bool, requested: usize, reserved: usize) {
        let mut s = self.state.lock().unwrap();
        s.trace.model = model;
        s.trace.requested = requested;
        s.trace.reserved = reserved;
        if self.coordinate && (requested != 2 || reserved != 2) {
            s.trace.gate_error = Some("two workers were not reserved".into());
            self.changed.notify_all();
        }
    }

    pub(crate) fn spawned(&self, ok: bool) {
        let mut s = self.state.lock().unwrap();
        if ok {
            s.trace.spawned += 1;
        } else {
            s.trace.gate_error = Some("worker spawn failed".into());
            self.changed.notify_all();
        }
    }

    fn wait<'a>(&self, mut s: std::sync::MutexGuard<'a, State>, id: usize, ready: impl Fn(&State) -> bool) -> Result<Option<std::sync::MutexGuard<'a, State>>> {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !ready(&s) && s.trace.gate_error.is_none() && s.aborted.is_none() {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                s.trace.gate_error = Some("first-chunk coordination timed out".into());
                self.changed.notify_all();
                break;
            }
            s = self.changed.wait_timeout(s, left).unwrap().0;
        }
        if let Some(e) = &s.trace.gate_error {
            return Err(crate::error::internal(e));
        }
        if s.aborted.is_some() && !ready(&s) {
            s.trace.participants.entry(id).or_default().coordinator_aborted = true;
            return Ok(None);
        }
        Ok(Some(s))
    }

    // Reserve the first three chunks in participant order. Nobody can consume another
    // chunk until all three participants have finished their reserved chunk.
    pub(crate) fn first<T>(&self, id: usize, chunks: &Mutex<VecDeque<(usize, T)>>) -> Result<Option<(usize, T)>> {
        if !self.coordinate {
            return Ok(chunks.lock().unwrap().pop_front());
        }
        let s = self.state.lock().unwrap();
        let Some(mut s) = self.wait(s, id, |s| s.claimed == id)? else { return Ok(None) };
        let next = chunks.lock().unwrap().pop_front();
        if next.is_none() {
            s.trace.gate_error = Some("fewer than three chunks".into());
        }
        s.claimed += 1;
        self.changed.notify_all();
        let Some(s) = self.wait(s, id, |s| s.claimed == 3)? else { return Ok(None) };
        drop(s);
        Ok(next)
    }

    pub(crate) fn completed_first(&self, id: usize) -> Result<bool> {
        if self.coordinate {
            let mut s = self.state.lock().unwrap();
            s.completed.insert(id);
            self.changed.notify_all();
            return Ok(self.wait(s, id, |s| s.completed.len() == 3)?.is_some());
        }
        Ok(true)
    }

    pub(crate) fn chunk(&self, id: usize, chunk: usize) {
        let mut s = self.state.lock().unwrap();
        let p = s.trace.participants.entry(id).or_default();
        p.thread = format!("{:?}/{}", std::thread::current().id(), std::thread::current().name().unwrap_or("unnamed"));
        p.chunks.push(chunk);
    }

    pub(crate) fn row(&self, id: usize) {
        self.state.lock().unwrap().trace.participants.entry(id).or_default().rows += 1;
    }

    pub(crate) fn expanded(&self, id: usize, bytes: usize) {
        let mut s = self.state.lock().unwrap();
        let p = s.trace.participants.entry(id).or_default();
        p.expanded_text = p.expanded_text.max(bytes);
    }

    pub(crate) fn validated(&self, id: usize, minimum: &Option<Candidate>, expanded_text: usize) {
        let mut s = self.state.lock().unwrap();
        let p = s.trace.participants.entry(id).or_default();
        p.validated += 1;
        p.minimum = minimum.clone();
        p.expanded_text = p.expanded_text.max(expanded_text);
    }

    pub(crate) fn model_return(&self, id: usize) {
        self.state.lock().unwrap().trace.participants.entry(id).or_default().model_returns += 1;
    }

    pub(crate) fn error(&self, id: usize, error: &Error) {
        let mut s = self.state.lock().unwrap();
        if s.trace.gate_error.is_some() {
            return;
        }
        s.trace.participants.entry(id).or_default().error = Some(error.clone());
        // Release every waiter if a participant fails in its first chunk.
        s.aborted = Some(error.clone());
        self.changed.notify_all();
    }

    pub(crate) fn serial(&self, validated: bool) {
        let mut s = self.state.lock().unwrap();
        s.trace.serial_rows += 1;
        s.trace.serial_validated += usize::from(validated);
    }

    pub(crate) fn folded(&self, id: usize, candidate: &Option<Candidate>, minimum: &Option<Candidate>) {
        let mut s = self.state.lock().unwrap();
        s.trace.fold.push((id, candidate.clone()));
        s.trace.minimum = minimum.clone();
    }
}
