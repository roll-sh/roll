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
    /// Requests and egress billed to the requester (Requester Pays), by (payer account,
    /// bucket id).
    paid: Mutex<HashMap<(String, String), Counters>>,
}

impl Meter {
    /// A request, billed to `payer`'s account when it's `Some` (a Requester Pays bucket's
    /// requester), else to the bucket's owner.
    pub fn request_by(&self, bucket_id: &str, payer: Option<&str>, class: Class) {
        let Some(p) = payer else { return self.request(bucket_id, class) };
        let mut m = self.paid.lock().unwrap_or_else(|e| e.into_inner());
        let c = m.entry((p.to_string(), bucket_id.to_string())).or_default();
        match class {
            Class::A => c.class_a += 1,
            Class::B => c.class_b += 1,
            Class::Free => {}
        }
    }

    /// Bytes sent to a client, billed to `payer` when it's `Some`.
    pub fn egress_by(&self, bucket_id: &str, payer: Option<&str>, n: u64, internal: bool) {
        let Some(p) = payer else { return self.egress(bucket_id, n, internal) };
        if n == 0 {
            return;
        }
        let mut m = self.paid.lock().unwrap_or_else(|e| e.into_inner());
        let c = m.entry((p.to_string(), bucket_id.to_string())).or_default();
        if internal { c.egress_internal += n } else { c.egress_internet += n }
    }

    /// Everything billed to requesters since the last call, by (payer, bucket id).
    pub fn take_paid(&self) -> HashMap<(String, String), Counters> {
        std::mem::take(&mut *self.paid.lock().unwrap_or_else(|e| e.into_inner()))
    }

    pub(crate) fn restore_paid(&self, taken: HashMap<(String, String), Counters>) {
        let mut m = self.paid.lock().unwrap_or_else(|e| e.into_inner());
        for (k, t) in taken {
            let c = m.entry(k).or_default();
            c.class_a += t.class_a;
            c.class_b += t.class_b;
            c.egress_internet += t.egress_internet;
            c.egress_internal += t.egress_internal;
        }
    }

    /// Not yet flushed, billed to `payer` for `bucket_id`.
    pub fn pending_paid(&self, payer: &str, bucket_id: &str) -> Counters {
        self.paid.lock().unwrap_or_else(|e| e.into_inner()).get(&(payer.to_string(), bucket_id.to_string())).cloned().unwrap_or_default()
    }

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

    #[test]
    fn requester_paid_requests_are_kept_apart() {
        let m = Meter::default();
        m.request_by("b", Some("111111111111"), Class::B);
        m.egress_by("b", Some("111111111111"), 10, false);
        m.request_by("b", None, Class::A);
        m.egress_by("b", None, 5, true);
        assert_eq!(m.pending("b"), Counters { class_a: 1, egress_internal: 5, ..Default::default() }, "the owner's share");
        assert_eq!(m.pending_paid("111111111111", "b"), Counters { class_b: 1, egress_internet: 10, ..Default::default() });
        let t = m.take_paid();
        assert_eq!(t.len(), 1);
        m.restore_paid(t);
        assert_eq!(m.pending_paid("111111111111", "b").class_b, 1);
    }
}
