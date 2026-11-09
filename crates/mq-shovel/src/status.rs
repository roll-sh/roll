//! Live state of a shovel or federation link, as reported by the management API.

use parking_lot::Mutex;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

/// Lifecycle state, with RabbitMQ's names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Starting,
    Running,
    Terminated,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            State::Starting => "starting",
            State::Running => "running",
            State::Terminated => "terminated",
        }
    }
}

struct Inner {
    state: State,
    reason: Option<String>,
    since_ms: u64,
    connection: Option<String>,
}

/// Shared between a worker and the status API.
pub struct LinkStatus {
    inner: Mutex<Inner>,
    /// Messages fully transferred (acknowledged at the source).
    pub forwarded: AtomicU64,
    /// Messages left in the source queue (-1: unknown).
    pub remaining: AtomicI64,
}

impl Default for LinkStatus {
    fn default() -> Self {
        LinkStatus {
            inner: Mutex::new(Inner { state: State::Starting, reason: None, since_ms: mq_core::now_ms(), connection: None }),
            forwarded: AtomicU64::new(0),
            remaining: AtomicI64::new(-1),
        }
    }
}

impl LinkStatus {
    pub fn set(&self, state: State, reason: Option<String>) {
        let mut i = self.inner.lock();
        i.state = state;
        i.reason = reason;
        i.since_ms = mq_core::now_ms();
    }

    pub fn set_connection(&self, name: Option<String>) {
        self.inner.lock().connection = name;
    }

    pub fn state(&self) -> State {
        self.inner.lock().state
    }

    pub fn reason(&self) -> Option<String> {
        self.inner.lock().reason.clone()
    }

    pub fn connection(&self) -> Option<String> {
        self.inner.lock().connection.clone()
    }

    pub fn timestamp(&self) -> String {
        format_ts(self.inner.lock().since_ms)
    }

    pub fn forwarded(&self) -> u64 {
        self.forwarded.load(Ordering::Relaxed)
    }

    pub fn remaining(&self) -> Option<u64> {
        u64::try_from(self.remaining.load(Ordering::Relaxed)).ok()
    }
}

/// `YYYY-MM-DD HH:MM:SS` in UTC, the management API's timestamp format.
pub fn format_ts(ms: u64) -> String {
    let secs = ms / 1000;
    let (days, rem) = (secs / 86_400, secs % 86_400);
    let (y, m, d) = civil_from_days(days as i64);
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}", rem / 3600, rem % 3600 / 60, rem % 60)
}

/// Howard Hinnant's days-to-civil conversion.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

#[cfg(test)]
mod tests {
    #[test]
    fn timestamps() {
        assert_eq!(super::format_ts(0), "1970-01-01 00:00:00");
        assert_eq!(super::format_ts(1_700_000_000_000), "2023-11-14 22:13:20");
        assert_eq!(super::format_ts(951_782_400_000), "2000-02-29 00:00:00");
    }
}
