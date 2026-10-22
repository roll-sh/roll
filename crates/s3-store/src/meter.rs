//! Request and traffic counters, in memory between flushes into the `usage` table.

use std::collections::HashMap;
use std::sync::Mutex;

/// S3's request classes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    /// PUT, POST, COPY, LIST*, CreateMultipartUpload, UploadPart, Complete, bucket writes.
    A,
    /// GET, HEAD and everything else.
    B,
    /// DELETE and AbortMultipartUpload.
    Free,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Counters {
    pub class_a: u64,
    pub class_b: u64,
    pub bytes_in: u64,
    pub egress_internet: u64,
    pub egress_internal: u64,
}

#[derive(Default)]
pub struct Meter {
    counters: Mutex<HashMap<String, Counters>>,
}

impl Meter {
    fn with(&self, bucket_id: &str, f: impl FnOnce(&mut Counters)) {
        let mut m = self.counters.lock().unwrap_or_else(|e| e.into_inner());
        match m.get_mut(bucket_id) {
            Some(c) => f(c),
            None => f(m.entry(bucket_id.to_string()).or_default()),
        }
    }

    pub fn request(&self, bucket_id: &str, class: Class) {
        match class {
            Class::A => self.with(bucket_id, |c| c.class_a += 1),
            Class::B => self.with(bucket_id, |c| c.class_b += 1),
            Class::Free => {}
        }
    }

    pub fn bytes_in(&self, bucket_id: &str, n: u64) {
        if n > 0 {
            self.with(bucket_id, |c| c.bytes_in += n);
        }
    }

    /// Bytes sent to a client; `internal` when the peer is inside roll (free).
    pub fn egress(&self, bucket_id: &str, n: u64, internal: bool) {
        if n > 0 {
            self.with(bucket_id, |c| if internal { c.egress_internal += n } else { c.egress_internet += n });
        }
    }

    /// Everything counted since the last call.
    pub fn take(&self) -> HashMap<String, Counters> {
        std::mem::take(&mut *self.counters.lock().unwrap_or_else(|e| e.into_inner()))
    }

    /// Put counters back (a flush that failed).
    pub(crate) fn restore(&self, taken: HashMap<String, Counters>) {
        for (b, t) in taken {
            self.with(&b, |c| {
                c.class_a += t.class_a;
                c.class_b += t.class_b;
                c.bytes_in += t.bytes_in;
                c.egress_internet += t.egress_internet;
                c.egress_internal += t.egress_internal;
            });
        }
    }

    /// Not yet flushed, for one bucket.
    pub fn pending(&self, bucket_id: &str) -> Counters {
        self.counters.lock().unwrap_or_else(|e| e.into_inner()).get(bucket_id).cloned().unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_classes_and_restores_what_a_failed_flush_took() {
        let m = Meter::default();
        m.request("b", Class::A);
        m.request("b", Class::B);
        m.request("b", Class::Free);
        m.bytes_in("b", 0);
        m.egress("b", 0, false);
        m.bytes_in("b", 7);
        m.egress("b", 3, true);
        m.egress("b", 4, false);
        let want = Counters { class_a: 1, class_b: 1, bytes_in: 7, egress_internet: 4, egress_internal: 3 };
        assert_eq!(m.pending("b"), want);
        assert_eq!(m.pending("other"), Counters::default());
        m.request("only-free", Class::Free);
        let taken = m.take();
        assert_eq!(taken.len(), 1, "free requests and zero bytes create nothing");
        assert_eq!(m.pending("b"), Counters::default());
        m.request("b", Class::A);
        m.restore(taken);
        assert_eq!(m.pending("b"), Counters { class_a: 2, ..want });
    }
}
