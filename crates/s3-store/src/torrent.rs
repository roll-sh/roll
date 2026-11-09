//! GetObjectTorrent's cache: a blob's BitTorrent piece hashes (`blob_torrent`).
//!
//! Blobs are immutable, so a blob's pieces never change: they're computed once (by `s3-server`,
//! streaming the object) and kept by blob, so every later request, from any key, copy, branch
//! or version sharing the blob, is free. The row goes with the blob's own row (the sweeper).
//! It's a cache: written relaxed (a crash may lose it, and it's computed again), and only while
//! the blob is known, so it can never outlive it.

use crate::error::Result;
use crate::store::Store;
use rusqlite::{Connection, OptionalExtension, params};

/// A blob's cached pieces: the piece length and the concatenated SHA-1 hashes (20 bytes each).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TorrentPieces {
    pub piece_length: u64,
    pub pieces: Vec<u8>,
}

/// Drop a deleted blob's pieces (in the transaction removing its row).
pub(crate) fn forget(c: &Connection, blob: &str) -> Result<()> {
    c.prepare_cached("DELETE FROM blob_torrent WHERE blob_id = ?1 AND NOT EXISTS (SELECT 1 FROM blobs WHERE id = ?1)")?.execute([blob])?;
    Ok(())
}

impl Store {
    /// The cached pieces of `blob`, if they were computed with `piece_length`.
    pub fn torrent_pieces(&self, blob: &str, piece_length: u64) -> Result<Option<TorrentPieces>> {
        self.read(|c| {
            let row: Option<(i64, Vec<u8>)> =
                c.prepare_cached("SELECT piece_length, pieces FROM blob_torrent WHERE blob_id = ?1")?.query_row([blob], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
            Ok(row.filter(|(n, p)| *n as u64 == piece_length && p.len() % 20 == 0).map(|(n, pieces)| TorrentPieces { piece_length: n as u64, pieces }))
        })
    }

    /// Keep `blob`'s pieces (relaxed: a lost write only means computing them again). Nothing is
    /// kept for a blob the metadata doesn't know any more.
    pub fn cache_torrent_pieces(&self, blob: &str, t: &TorrentPieces) -> Result<()> {
        let now = self.now();
        self.write_relaxed(|c| {
            c.prepare_cached(
                "INSERT INTO blob_torrent (blob_id, piece_length, pieces, created_at) SELECT ?1, ?2, ?3, ?4 WHERE EXISTS (SELECT 1 FROM blobs WHERE id = ?1)
                 ON CONFLICT (blob_id) DO UPDATE SET piece_length = excluded.piece_length, pieces = excluded.pieces, created_at = excluded.created_at",
            )?
            .execute(params![blob, t.piece_length as i64, t.pieces, now])?;
            Ok(())
        })
    }
}
