//! Streamed transactions on the subscriber (streaming = on / parallel; worker.c's
//! stream_write_change, subxact_info_add, stream_abort_internal and
//! apply_spooled_messages): a transaction the publisher streams while it is in progress has
//! its changes spooled to a temporary file, a rolled-back subtransaction's changes cut off
//! at its Stream Abort, and the rest applied at its Stream Commit as one local transaction.
//! Memory stays at a write buffer however big the transaction is.

use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};

/// One streamed transaction's spooled messages: each `len (u32) ++ message` (the message
/// without the (sub)transaction id streaming adds).
pub struct Spool {
    out: BufWriter<std::fs::File>,
    len: u64,
    /// Its subtransactions with changes, in order, and where each one's first change is.
    subxacts: Vec<(u32, u64)>,
}

impl Spool {
    pub fn new(dir: &std::path::Path) -> Result<Spool, String> {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let _ = std::fs::create_dir_all(dir);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = dir.join(format!("stream-{}-{n}", std::process::id()));
        let f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|e| format!("could not create file \"{}\": {e}", path.display()))?;
        // (Unnamed while open: gone with the worker, even after a crash.)
        let _ = std::fs::remove_file(&path);
        Ok(Spool { out: BufWriter::with_capacity(64 << 10, f), len: 0, subxacts: Vec::new() })
    }

    /// stream_write_change: a change of (sub)transaction `xid` of top-level `top`.
    pub fn write(&mut self, top: u32, xid: u32, msg: &[u8]) -> Result<(), String> {
        if xid != top && !self.subxacts.iter().any(|s| s.0 == xid) {
            self.subxacts.push((xid, self.len));
        }
        let n = u32::try_from(msg.len()).map_err(|_| "streamed change too large".to_string())?;
        let io = |e: std::io::Error| format!("could not write to streaming transaction's changes file: {e}");
        self.out.write_all(&n.to_be_bytes()).map_err(io)?;
        self.out.write_all(msg).map_err(io)?;
        self.len += 4 + msg.len() as u64;
        Ok(())
    }

    /// stream_abort_internal for a subtransaction: its changes, and those of the
    /// subtransactions after it, are gone.
    pub fn abort_sub(&mut self, subxid: u32) -> Result<(), String> {
        let Some(i) = self.subxacts.iter().position(|s| s.0 == subxid) else { return Ok(()) };
        let at = self.subxacts[i].1;
        self.subxacts.truncate(i);
        let io = |e: std::io::Error| format!("could not truncate streaming transaction's changes file: {e}");
        self.out.flush().map_err(io)?;
        let f = self.out.get_mut();
        f.set_len(at).map_err(io)?;
        f.seek(SeekFrom::Start(at)).map_err(io)?;
        self.len = at;
        Ok(())
    }

    /// apply_spooled_messages: every message, in order.
    pub fn replay(self, mut each: impl FnMut(&[u8]) -> Result<(), String>) -> Result<(), String> {
        let io = |e: std::io::Error| format!("could not read from streaming transaction's changes file: {e}");
        let mut f = self.out.into_inner().map_err(|e| io(e.into_error()))?;
        f.seek(SeekFrom::Start(0)).map_err(io)?;
        let mut r = BufReader::with_capacity(64 << 10, f.take(self.len));
        let mut msg = Vec::new();
        loop {
            let mut n = [0u8; 4];
            match r.read_exact(&mut n) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(()),
                Err(e) => return Err(io(e)),
            }
            msg.resize(u32::from_be_bytes(n) as usize, 0);
            r.read_exact(&mut msg).map_err(io)?;
            each(&msg)?;
        }
    }
}
