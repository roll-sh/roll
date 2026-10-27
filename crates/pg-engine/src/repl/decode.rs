//! Reading the change stream back (`log`'s records): commit records in position order, and
//! each transaction's change records with the positions they took.

use super::log::{self, Marker, Rec};
use crate::error::Result;
use crate::storage::Store;
use std::sync::Arc;

/// The commit records of positions `from..to`, in order, read at the store's visible state.
pub fn markers(store: &Arc<Store>, from: u64, to: u64) -> impl Iterator<Item = Result<Marker>> {
    let ts = store.visible_ts();
    store
        .scan(&log::commit_key(from.max(1)), &log::commit_key(to), ts)
        .map(|r| r.and_then(|(_, v)| Marker::decode(&v)))
}

/// A change record with its position.
pub struct Change {
    pub lsn: u64,
    pub rec: Rec,
    /// A row a table rewrite wrote (`log::rewrite_insert`).
    pub rewrite: bool,
}

/// Transaction `m`'s change records, in order, with the positions they took after its
/// commit record's base (`Marker::first_lsn`).
pub fn changes(store: &Arc<Store>, m: &Marker) -> impl Iterator<Item = Result<Change>> {
    let ts = store.visible_ts();
    let (a, b) = log::changes_range(m.xid);
    let mut at = m.first_lsn;
    store.scan(&a, &b, ts).map(move |r| {
        let (_, v) = r?;
        let lsn = at;
        at += log::record_size(v.len());
        Ok(Change { lsn, rewrite: v.first() == Some(&b'W'), rec: log::decode_rec(&v)? })
    })
}

/// The non-transactional message a `Message` record holds.
pub fn message_of(m: &Marker) -> Option<(String, Vec<u8>)> {
    match log::decode_rec(m.message.as_deref()?) {
        Ok(Rec::Message { prefix, content, .. }) => Some((prefix, content)),
        _ => None,
    }
}
