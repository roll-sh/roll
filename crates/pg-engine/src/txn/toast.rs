//! A transaction's reads of out-of-line values (`storage::toast`): the rows it reads may
//! point at values it wrote itself (in its buffer or spilled), as the reader's snapshot
//! sees them, or at committed ones.

use crate::error::Result;
use crate::storage::toast;
use crate::storage::{Store, View};
use bytes::Bytes;
use std::sync::{Arc, OnceLock};

/// Where a reader's out-of-line values come from.
pub struct Source {
    /// The transaction's own writes of values, as the reader sees them, in key order.
    pub(super) own: Vec<(Vec<u8>, Option<Bytes>)>,
    /// Its spilled writes: their view and the journal mark they are read as of.
    pub(super) runs: Option<(Arc<View>, u64)>,
    /// The store, its sources taken at the first value read (most scans read none), and the
    /// reader's snapshot there.
    pub(super) store: Arc<Store>,
    pub(super) view: OnceLock<Arc<View>>,
    pub(super) ts: u64,
}

impl Source {
    /// The store's sources values are read from.
    fn view(&self) -> &Arc<View> {
        self.view.get_or_init(|| self.store.view())
    }

    /// Value `key` from the transaction's own writes, if it wrote it.
    fn own_value(&self, key: &[u8]) -> Result<Option<Bytes>> {
        if let Ok(i) = self.own.binary_search_by(|(k, _)| k.as_slice().cmp(key))
            && let Some(v) = &self.own[i].1
        {
            return Ok(Some(v.clone()));
        }
        if let Some((runs, mark)) = &self.runs
            && let Some((_, Some(b))) = runs.get_version(key, *mark)?
            && let (Some(v), _) = super::spill::decode_entry(b)
        {
            return Ok(Some(v));
        }
        Ok(None)
    }

    /// Out-of-line value `key`.
    pub fn fetch(&self, key: &[u8]) -> Result<Bytes> {
        match self.own_value(key)? {
            Some(v) => Ok(v),
            None => toast::read_value(self.view(), key, self.ts),
        }
    }

    /// The stored row `bytes` under `key` with the values of the columns `need` marks
    /// (None: all) put back (`toast::splice`).
    pub fn splice(&self, key: &[u8], bytes: &[u8], need: Option<&[bool]>) -> Result<Bytes> {
        Ok(Bytes::from(toast::splice(key, bytes, need, &mut |k| self.fetch(k))?))
    }

    /// `bytes` read under `key`, with all its values put back when it is a row that has
    /// some out of line.
    pub fn resolve(&self, key: &[u8], bytes: Bytes) -> Result<Bytes> {
        if !toast::has_external(&bytes) || !toast::is_row_key(key) {
            return Ok(bytes);
        }
        self.splice(key, &bytes, None)
    }
}

/// Steps a `Reader`'s cursor takes towards a value before it seeks to it instead.
const STEPS: usize = 4;

/// Out-of-line values read in their rows' order (a scan's), through a cursor kept from one
/// value to the next: a value close after the last one read costs a step, not a lookup.
/// Values read far apart (the rows a selective filter keeps) are read by key instead: a
/// cursor made at a value seeks every source, a read by key mostly one.
pub struct Reader {
    src: Arc<Source>,
    /// The store's values from the last one read on, newest versions.
    cur: Option<std::iter::Peekable<crate::storage::Scan>>,
    /// Values the cursor found within its steps, and ones it didn't.
    hits: u64,
    misses: u64,
}

impl Reader {
    pub fn new(src: Arc<Source>) -> Reader {
        Reader { src, cur: None, hits: 0, misses: 0 }
    }

    /// Out-of-line value `key`.
    pub fn fetch(&mut self, key: &[u8]) -> Result<Bytes> {
        if let Some(v) = self.src.own_value(key)? {
            return Ok(v);
        }
        let counted = |v: Bytes| {
            toast::VALUES_READ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Ok(v)
        };
        if let Some(c) = &mut self.cur {
            for _ in 0..STEPS {
                match c.peek() {
                    Some(Ok((k, _))) if k.as_slice() < key => {
                        c.next();
                    }
                    Some(Ok((k, _))) if k.as_slice() == key => {
                        self.hits += 1;
                        return counted(c.next().expect("a peeked entry")?.1);
                    }
                    _ => break,
                }
            }
        }
        // (The first value, or one after a run of near ones: a new cursor at it, over the
        // rest of its table's values. Else a read by key. A value whose newest version is a
        // deletion is read as of the snapshot.)
        let first = self.hits + self.misses == 0;
        self.misses += 1;
        if !first && self.hits < self.misses {
            self.cur = None;
            return toast::read_value(self.src.view(), key, self.src.ts);
        }
        let end = crate::storage::keys::prefix_end(&key[..8]);
        let mut c = self.src.view().scan(key, &end, u64::MAX >> 1).peekable();
        let hit = matches!(c.peek(), Some(Ok((k, _))) if k.as_slice() == key);
        let v = if hit { Some(c.next().expect("a peeked entry")?.1) } else { None };
        self.cur = Some(c);
        match v {
            Some(v) => counted(v),
            None => toast::read_value(self.src.view(), key, self.src.ts),
        }
    }

    /// `Source::splice` reading through this reader.
    pub fn splice(&mut self, key: &[u8], bytes: &[u8], need: Option<&[bool]>) -> Result<Bytes> {
        Ok(Bytes::from(toast::splice(key, bytes, need, &mut |k| self.fetch(k))?))
    }
}
