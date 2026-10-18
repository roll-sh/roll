//! Keeper wire protocol.
//!
//! Every frame is length-prefixed and carries a request id, so a client pipelines any number
//! of requests over one connection and matches replies as they arrive (replies for different
//! logs may come back out of order):
//!
//! ```text
//! request  [len u32][id u64][op u8][body]
//! reply    [len u32][id u64][status u8][body]
//! ```
//!
//! Integers are big-endian, strings are `[len u16][utf-8]`, entry lists are
//! `[count u32]([len u32][bytes])*`.
//!
//! Keepers usually sit in other availability zones, where every byte is billed, so the two
//! sides compress large frames once both have said they can (`Ping`/`Pong` carry feature
//! bits; peers that predate them send none and never see a compressed frame). A compressed
//! frame sets the top bit of its op or status byte and carries `[raw len u32][zstd]` instead
//! of its body.

use anyhow::{Context, bail};
use bytes::{Buf, BufMut, Bytes, BytesMut};
use tokio::io::{AsyncRead, AsyncReadExt};

/// Largest frame either side accepts.
pub const MAX_FRAME: usize = 256 << 20;

/// Feature bit: this side reads zstd-compressed frames.
pub const FEATURE_ZSTD: u8 = 1;
/// Features this build supports.
pub const FEATURES: u8 = FEATURE_ZSTD;
/// Set on the op or status byte of a compressed frame.
const COMPRESSED: u8 = 0x80;
/// Frames smaller than this aren't worth compressing.
const COMPRESS_MIN: usize = 1024;
/// Frames that compress worse than this ratio go as they are...
const COMPRESS_WORTH: f64 = 0.9;
/// ...and so do the next this many, before trying again.
const COMPRESS_BACKOFF: u32 = 64;

/// Epoch history of a log: `(first index, epoch)` ranges in ascending order. Entry `i` was
/// written by the epoch of the last range starting at or before `i`.
pub type History = Vec<(u64, u64)>;

/// A keeper's view of one log.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LogState {
    /// Highest epoch the keeper has been fenced with.
    pub epoch: u64,
    /// Logical first index (prefix truncation point).
    pub trim: u64,
    /// Everything below this index lives in object storage.
    pub tiered: u64,
    /// The index the next entry gets.
    pub next: u64,
    /// First index the keeper still holds on disk.
    pub hot_first: u64,
    pub history: History,
    /// Local segments as `(first index, bytes)`.
    pub segments: Vec<(u64, u64)>,
}

impl LogState {
    /// Epoch of the last entry (or of the entry before the log start when empty).
    pub fn last_epoch(&self) -> u64 {
        prev_epoch(&self.history, self.next)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Request {
    /// Raise the keeper's epoch for `log`; `strict` requires `epoch` to be strictly greater.
    Fence {
        log: String,
        epoch: u64,
        strict: bool,
    },
    /// Append entries written by `entry_epoch` at exactly `first`, whose predecessor was written
    /// by `prev_epoch`. `seal` starts a new segment file at `first`.
    Append {
        log: String,
        epoch: u64,
        first: u64,
        prev_epoch: u64,
        entry_epoch: u64,
        seal: bool,
        entries: Vec<Bytes>,
    },
    Read {
        log: String,
        from: u64,
        max: u32,
        max_bytes: u32,
    },
    State {
        log: String,
    },
    TruncatePrefix {
        log: String,
        epoch: u64,
        before: u64,
    },
    /// Drop entries at and after `from` (divergent, never-committed suffixes).
    TruncateSuffix {
        log: String,
        epoch: u64,
        from: u64,
    },
    /// Drop everything and restart the log at `at` (a replica too far behind to catch up).
    Reset {
        log: String,
        epoch: u64,
        at: u64,
        prev_epoch: u64,
        trim: u64,
        tiered: u64,
    },
    /// Everything below `upto` is in object storage; drop local copies.
    Tiered {
        log: String,
        epoch: u64,
        upto: u64,
    },
    Destroy {
        log: String,
        epoch: u64,
    },
    ListLogs,
    /// Liveness, and the [`FEATURES`] the sender supports.
    Ping {
        features: u8,
    },
}

impl Request {
    pub fn log(&self) -> Option<&str> {
        match self {
            Request::Fence { log, .. }
            | Request::Append { log, .. }
            | Request::Read { log, .. }
            | Request::State { log }
            | Request::TruncatePrefix { log, .. }
            | Request::TruncateSuffix { log, .. }
            | Request::Reset { log, .. }
            | Request::Tiered { log, .. }
            | Request::Destroy { log, .. } => Some(log),
            Request::ListLogs | Request::Ping { .. } => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reply {
    State(LogState),
    Appended {
        next: u64,
    },
    Entries {
        first: u64,
        entries: Vec<Bytes>,
    },
    Logs(Vec<String>),
    /// The [`FEATURES`] the keeper supports (0 from keepers that predate them).
    Pong {
        features: u8,
    },
    /// The log is fenced with a newer epoch.
    Fenced {
        epoch: u64,
    },
    /// The append does not continue the keeper's log (gap, overlap or divergent predecessor).
    Mismatch {
        next: u64,
        last_epoch: u64,
    },
    /// The requested entries are no longer on this keeper's disk (tiered or reset).
    NotHere {
        hot_first: u64,
    },
    Error(String),
}

mod op {
    pub const FENCE: u8 = 1;
    pub const APPEND: u8 = 2;
    pub const READ: u8 = 3;
    pub const STATE: u8 = 4;
    pub const TRUNCATE_PREFIX: u8 = 5;
    pub const TRUNCATE_SUFFIX: u8 = 6;
    pub const RESET: u8 = 7;
    pub const TIERED: u8 = 8;
    pub const DESTROY: u8 = 9;
    pub const LIST_LOGS: u8 = 10;
    pub const PING: u8 = 11;
}

mod status {
    pub const STATE: u8 = 0;
    pub const APPENDED: u8 = 1;
    pub const ENTRIES: u8 = 2;
    pub const LOGS: u8 = 3;
    pub const PONG: u8 = 4;
    pub const FENCED: u8 = 10;
    pub const MISMATCH: u8 = 11;
    pub const NOT_HERE: u8 = 12;
    pub const ERROR: u8 = 13;
}

fn frame(id: u64, code: u8, body: impl FnOnce(&mut BytesMut)) -> Bytes {
    let mut b = BytesMut::with_capacity(64);
    b.put_u32(0);
    b.put_u64(id);
    b.put_u8(code);
    body(&mut b);
    let len = (b.len() - 4) as u32;
    b[..4].copy_from_slice(&len.to_be_bytes());
    b.freeze()
}

fn put_str(b: &mut BytesMut, s: &str) {
    let s = &s.as_bytes()[..s.len().min(u16::MAX as usize)];
    b.put_u16(s.len() as u16);
    b.put_slice(s);
}

fn put_entries(b: &mut BytesMut, entries: &[Bytes]) {
    b.reserve(4 + entries.iter().map(|e| 4 + e.len()).sum::<usize>());
    b.put_u32(entries.len() as u32);
    for e in entries {
        b.put_u32(e.len() as u32);
        b.put_slice(e);
    }
}

fn put_pairs(b: &mut BytesMut, v: &[(u64, u64)]) {
    b.put_u32(v.len() as u32);
    for (x, y) in v {
        b.put_u64(*x);
        b.put_u64(*y);
    }
}

/// Bounds-checked reader over a frame body.
struct Rd(Bytes);

impl Rd {
    fn need(&self, n: usize) -> anyhow::Result<()> {
        if self.0.remaining() < n {
            bail!("truncated frame");
        }
        Ok(())
    }
    fn u8(&mut self) -> anyhow::Result<u8> {
        self.need(1)?;
        Ok(self.0.get_u8())
    }
    fn u32(&mut self) -> anyhow::Result<u32> {
        self.need(4)?;
        Ok(self.0.get_u32())
    }
    fn u64(&mut self) -> anyhow::Result<u64> {
        self.need(8)?;
        Ok(self.0.get_u64())
    }
    /// A trailing byte older peers don't send.
    fn opt_u8(&mut self) -> u8 {
        if self.0.has_remaining() { self.0.get_u8() } else { 0 }
    }
    fn bool(&mut self) -> anyhow::Result<bool> {
        Ok(self.u8()? != 0)
    }
    fn bytes(&mut self, n: usize) -> anyhow::Result<Bytes> {
        self.need(n)?;
        Ok(self.0.split_to(n))
    }
    fn str(&mut self) -> anyhow::Result<String> {
        self.need(2)?;
        let n = self.0.get_u16() as usize;
        let b = self.bytes(n)?;
        String::from_utf8(b.to_vec()).context("invalid utf-8")
    }
    fn entries(&mut self) -> anyhow::Result<Vec<Bytes>> {
        let n = self.u32()? as usize;
        // Each entry needs at least its 4-byte length; refuse counts the frame can't hold.
        self.need(n.saturating_mul(4))?;
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            let len = self.u32()? as usize;
            out.push(self.bytes(len)?);
        }
        Ok(out)
    }
    fn pairs(&mut self) -> anyhow::Result<Vec<(u64, u64)>> {
        let n = self.u32()? as usize;
        self.need(n.saturating_mul(16))?;
        (0..n).map(|_| Ok((self.u64()?, self.u64()?))).collect()
    }
}

pub fn encode_request(id: u64, req: &Request) -> Bytes {
    match req {
        Request::Fence { log, epoch, strict } => frame(id, op::FENCE, |b| {
            put_str(b, log);
            b.put_u64(*epoch);
            b.put_u8(*strict as u8);
        }),
        Request::Append { log, epoch, first, prev_epoch, entry_epoch, seal, entries } => frame(id, op::APPEND, |b| {
            put_str(b, log);
            b.put_u64(*epoch);
            b.put_u64(*first);
            b.put_u64(*prev_epoch);
            b.put_u64(*entry_epoch);
            b.put_u8(*seal as u8);
            put_entries(b, entries);
        }),
        Request::Read { log, from, max, max_bytes } => frame(id, op::READ, |b| {
            put_str(b, log);
            b.put_u64(*from);
            b.put_u32(*max);
            b.put_u32(*max_bytes);
        }),
        Request::State { log } => frame(id, op::STATE, |b| put_str(b, log)),
        Request::TruncatePrefix { log, epoch, before } => frame(id, op::TRUNCATE_PREFIX, |b| {
            put_str(b, log);
            b.put_u64(*epoch);
            b.put_u64(*before);
        }),
        Request::TruncateSuffix { log, epoch, from } => frame(id, op::TRUNCATE_SUFFIX, |b| {
            put_str(b, log);
            b.put_u64(*epoch);
            b.put_u64(*from);
        }),
        Request::Reset { log, epoch, at, prev_epoch, trim, tiered } => frame(id, op::RESET, |b| {
            put_str(b, log);
            b.put_u64(*epoch);
            b.put_u64(*at);
            b.put_u64(*prev_epoch);
            b.put_u64(*trim);
            b.put_u64(*tiered);
        }),
        Request::Tiered { log, epoch, upto } => frame(id, op::TIERED, |b| {
            put_str(b, log);
            b.put_u64(*epoch);
            b.put_u64(*upto);
        }),
        Request::Destroy { log, epoch } => frame(id, op::DESTROY, |b| {
            put_str(b, log);
            b.put_u64(*epoch);
        }),
        Request::ListLogs => frame(id, op::LIST_LOGS, |_| {}),
        Request::Ping { features } => frame(id, op::PING, |b| b.put_u8(*features)),
    }
}

/// Decode a request frame body (everything after the length prefix).
pub fn decode_request(body: Bytes) -> anyhow::Result<(u64, Request)> {
    let mut r = Rd(body);
    let id = r.u64()?;
    let req = match r.u8()? {
        op::FENCE => Request::Fence { log: r.str()?, epoch: r.u64()?, strict: r.bool()? },
        op::APPEND => Request::Append {
            log: r.str()?,
            epoch: r.u64()?,
            first: r.u64()?,
            prev_epoch: r.u64()?,
            entry_epoch: r.u64()?,
            seal: r.bool()?,
            entries: r.entries()?,
        },
        op::READ => Request::Read { log: r.str()?, from: r.u64()?, max: r.u32()?, max_bytes: r.u32()? },
        op::STATE => Request::State { log: r.str()? },
        op::TRUNCATE_PREFIX => Request::TruncatePrefix { log: r.str()?, epoch: r.u64()?, before: r.u64()? },
        op::TRUNCATE_SUFFIX => Request::TruncateSuffix { log: r.str()?, epoch: r.u64()?, from: r.u64()? },
        op::RESET => Request::Reset { log: r.str()?, epoch: r.u64()?, at: r.u64()?, prev_epoch: r.u64()?, trim: r.u64()?, tiered: r.u64()? },
        op::TIERED => Request::Tiered { log: r.str()?, epoch: r.u64()?, upto: r.u64()? },
        op::DESTROY => Request::Destroy { log: r.str()?, epoch: r.u64()? },
        op::LIST_LOGS => Request::ListLogs,
        op::PING => Request::Ping { features: r.opt_u8() },
        other => bail!("unknown op {other}"),
    };
    Ok((id, req))
}

pub fn encode_reply(id: u64, reply: &Reply) -> Bytes {
    match reply {
        Reply::State(s) => frame(id, status::STATE, |b| {
            b.put_u64(s.epoch);
            b.put_u64(s.trim);
            b.put_u64(s.tiered);
            b.put_u64(s.next);
            b.put_u64(s.hot_first);
            put_pairs(b, &s.history);
            put_pairs(b, &s.segments);
        }),
        Reply::Appended { next } => frame(id, status::APPENDED, |b| b.put_u64(*next)),
        Reply::Entries { first, entries } => frame(id, status::ENTRIES, |b| {
            b.put_u64(*first);
            put_entries(b, entries);
        }),
        Reply::Logs(logs) => frame(id, status::LOGS, |b| {
            b.put_u32(logs.len() as u32);
            for l in logs {
                put_str(b, l);
            }
        }),
        Reply::Pong { features } => frame(id, status::PONG, |b| b.put_u8(*features)),
        Reply::Fenced { epoch } => frame(id, status::FENCED, |b| b.put_u64(*epoch)),
        Reply::Mismatch { next, last_epoch } => frame(id, status::MISMATCH, |b| {
            b.put_u64(*next);
            b.put_u64(*last_epoch);
        }),
        Reply::NotHere { hot_first } => frame(id, status::NOT_HERE, |b| b.put_u64(*hot_first)),
        Reply::Error(e) => frame(id, status::ERROR, |b| put_str(b, e)),
    }
}

pub fn decode_reply(body: Bytes) -> anyhow::Result<(u64, Reply)> {
    let mut r = Rd(body);
    let id = r.u64()?;
    let reply = match r.u8()? {
        status::STATE => Reply::State(LogState {
            epoch: r.u64()?,
            trim: r.u64()?,
            tiered: r.u64()?,
            next: r.u64()?,
            hot_first: r.u64()?,
            history: r.pairs()?,
            segments: r.pairs()?,
        }),
        status::APPENDED => Reply::Appended { next: r.u64()? },
        status::ENTRIES => Reply::Entries { first: r.u64()?, entries: r.entries()? },
        status::LOGS => {
            let n = r.u32()? as usize;
            r.need(n.saturating_mul(2))?;
            Reply::Logs((0..n).map(|_| r.str()).collect::<anyhow::Result<_>>()?)
        }
        status::PONG => Reply::Pong { features: r.opt_u8() },
        status::FENCED => Reply::Fenced { epoch: r.u64()? },
        status::MISMATCH => Reply::Mismatch { next: r.u64()?, last_epoch: r.u64()? },
        status::NOT_HERE => Reply::NotHere { hot_first: r.u64()? },
        status::ERROR => Reply::Error(r.str()?),
        other => bail!("unknown status {other}"),
    };
    Ok((id, reply))
}

/// Compresses outgoing frames for a peer that reads them, and stops trying for a while when
/// the payload doesn't compress (already compressed or encrypted message bodies).
#[derive(Default)]
pub struct Compressor {
    skip: u32,
    /// Frame bytes before and after compression, for metrics.
    pub raw_bytes: u64,
    pub wire_bytes: u64,
}

impl Compressor {
    /// `frame` as it goes on the wire: compressed when that's worth it.
    pub fn frame(&mut self, frame: Bytes) -> Bytes {
        self.raw_bytes += frame.len() as u64;
        let out = self.compress(frame);
        self.wire_bytes += out.len() as u64;
        out
    }

    fn compress(&mut self, frame: Bytes) -> Bytes {
        // [len u32][id u64][code u8][body]
        if frame.len() < COMPRESS_MIN || frame[12] & COMPRESSED != 0 {
            return frame;
        }
        if self.skip > 0 {
            self.skip -= 1;
            return frame;
        }
        let body = &frame[13..];
        let Ok(z) = zstd::bulk::compress(body, 1) else { return frame };
        if z.len() as f64 > body.len() as f64 * COMPRESS_WORTH {
            self.skip = COMPRESS_BACKOFF;
            return frame;
        }
        let mut b = BytesMut::with_capacity(17 + z.len());
        b.put_u32((13 + z.len()) as u32);
        b.put_slice(&frame[4..12]);
        b.put_u8(frame[12] | COMPRESSED);
        b.put_u32(body.len() as u32);
        b.put_slice(&z);
        b.freeze()
    }
}

/// A frame body (as [`read_frame`] returns it) with any compression undone.
pub fn inflate(body: Bytes) -> anyhow::Result<Bytes> {
    if body.len() < 9 || body[8] & COMPRESSED == 0 {
        return Ok(body);
    }
    if body.len() < 13 {
        bail!("truncated compressed frame");
    }
    let raw = u32::from_be_bytes(body[9..13].try_into().unwrap()) as usize;
    if raw > MAX_FRAME {
        bail!("compressed frame too large ({raw} bytes)");
    }
    let data = zstd::bulk::decompress(&body[13..], raw).context("bad compressed frame")?;
    if data.len() != raw {
        bail!("compressed frame length mismatch");
    }
    let mut b = BytesMut::with_capacity(9 + raw);
    b.put_slice(&body[..8]);
    b.put_u8(body[8] & !COMPRESSED);
    b.put_slice(&data);
    Ok(b.freeze())
}

/// Read one frame body. `Ok(None)` on a clean EOF between frames.
pub async fn read_frame(r: &mut (impl AsyncRead + Unpin)) -> anyhow::Result<Option<Bytes>> {
    let mut len = [0u8; 4];
    match r.read_exact(&mut len).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let len = u32::from_be_bytes(len) as usize;
    if !(9..=MAX_FRAME).contains(&len) {
        bail!("bad frame length {len}");
    }
    let mut buf = BytesMut::zeroed(len);
    r.read_exact(&mut buf).await?;
    Ok(Some(buf.freeze()))
}

// ── epoch histories ──

/// Epoch that wrote entry `i`.
pub fn epoch_at(history: &[(u64, u64)], i: u64) -> u64 {
    match history.partition_point(|(s, _)| *s <= i) {
        0 => 0,
        n => history[n - 1].1,
    }
}

/// Epoch of the entry before `index` (0 at the start of the log).
pub fn prev_epoch(history: &[(u64, u64)], index: u64) -> u64 {
    if index == 0 { 0 } else { epoch_at(history, index - 1) }
}

/// Length of the longest common prefix of two logs given their histories and ends.
///
/// Relies on the log-matching invariant: an index written by the same epoch on two replicas
/// holds the same entry, and so does everything before it.
pub fn common_prefix(a: &[(u64, u64)], a_next: u64, b: &[(u64, u64)], b_next: u64) -> u64 {
    let ranges = |h: &[(u64, u64)], next: u64| -> Vec<(u64, u64, u64)> {
        h.iter().enumerate().map(|(i, &(s, e))| (s, h.get(i + 1).map(|x| x.0).unwrap_or(next).max(s), e)).collect()
    };
    let (ra, rb) = (ranges(a, a_next), ranges(b, b_next));
    let mut d = 0;
    for &(sa, ea, xa) in &ra {
        for &(sb, eb, xb) in &rb {
            let (lo, hi) = (sa.max(sb), ea.min(eb));
            if xa == xb && lo < hi {
                d = d.max(hi);
            }
        }
    }
    d.min(a_next).min(b_next)
}

/// Drop history ranges that only describe entries before `first - 1`.
pub fn prune_history(history: &mut History, first: u64) {
    let keep_from = first.saturating_sub(1);
    let n = history.partition_point(|(s, _)| *s <= keep_from);
    if n > 1 {
        history.drain(..n - 1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_roundtrip() {
        let reqs = vec![
            Request::Fence { log: "q/a".into(), epoch: 7, strict: true },
            Request::Append {
                log: "q/a".into(),
                epoch: 7,
                first: 10,
                prev_epoch: 3,
                entry_epoch: 7,
                seal: true,
                entries: vec![Bytes::from_static(b"x"), Bytes::new()],
            },
            Request::Read { log: "q".into(), from: 1, max: 2, max_bytes: 3 },
            Request::Reset { log: "q".into(), epoch: 1, at: 2, prev_epoch: 3, trim: 4, tiered: 5 },
            Request::ListLogs,
            Request::Ping { features: FEATURES },
        ];
        for (i, r) in reqs.into_iter().enumerate() {
            let f = encode_request(i as u64, &r);
            let (id, back) = decode_request(f.slice(4..)).unwrap();
            assert_eq!((id, back), (i as u64, r));
        }
        let st = LogState { epoch: 1, trim: 2, tiered: 3, next: 4, hot_first: 3, history: vec![(0, 1)], segments: vec![(3, 100)] };
        for rep in [
            Reply::State(st),
            Reply::Entries { first: 9, entries: vec![Bytes::from_static(b"abc")] },
            Reply::Logs(vec!["a".into(), "b/c".into()]),
            Reply::Mismatch { next: 1, last_epoch: 2 },
            Reply::Error("boom".into()),
            Reply::Pong { features: FEATURE_ZSTD },
        ] {
            let f = encode_reply(5, &rep);
            assert_eq!(decode_reply(f.slice(4..)).unwrap(), (5, rep));
        }
        // Hostile counts don't allocate or panic.
        let mut b = BytesMut::new();
        b.put_u64(1);
        b.put_u8(status::ENTRIES);
        b.put_u64(0);
        b.put_u32(u32::MAX);
        assert!(decode_reply(b.freeze()).is_err());
    }

    #[test]
    fn older_peers_send_no_features() {
        // A Ping or Pong from before feature bits is just the header.
        assert_eq!(decode_request(encode_request(1, &Request::ListLogs).slice(4..)).unwrap().1, Request::ListLogs);
        let mut b = BytesMut::new();
        b.put_u64(1);
        b.put_u8(op::PING);
        assert_eq!(decode_request(b.freeze()).unwrap().1, Request::Ping { features: 0 });
        let mut b = BytesMut::new();
        b.put_u64(1);
        b.put_u8(status::PONG);
        assert_eq!(decode_reply(b.freeze()).unwrap().1, Reply::Pong { features: 0 });
    }

    #[test]
    fn compressed_frames_roundtrip_and_incompressible_ones_go_as_they_are() {
        let mut c = Compressor::default();
        let text = Bytes::from(b"{\"order\":42,\"status\":\"shipped\",\"items\":[]}".repeat(100));
        let req = Request::Append { log: "q".into(), epoch: 1, first: 0, prev_epoch: 0, entry_epoch: 1, seal: false, entries: vec![text.clone(), text] };
        let raw = encode_request(7, &req);
        let wire = c.frame(raw.clone());
        assert!(wire.len() * 10 < raw.len(), "{} -> {}", raw.len(), wire.len());
        assert_eq!(decode_request(inflate(wire.slice(4..)).unwrap()).unwrap(), (7, req));
        // Small frames and uncompressed bodies pass through untouched.
        let ping = encode_request(8, &Request::Ping { features: FEATURES });
        assert_eq!(c.frame(ping.clone()), ping);
        assert_eq!(inflate(ping.slice(4..)).unwrap(), ping.slice(4..));
        // Random bytes don't compress: sent raw, and the next frames skip the attempt.
        let mut x = 0x9e3779b97f4a7c15u64;
        let noise: Vec<u8> = (0..8192).map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        }).collect();
        let reply = encode_reply(9, &Reply::Entries { first: 0, entries: vec![Bytes::from(noise)] });
        assert_eq!(c.frame(reply.clone()), reply);
        assert_eq!(c.skip, COMPRESS_BACKOFF);
        // A hostile length is refused before allocating.
        let mut b = BytesMut::new();
        b.put_u64(1);
        b.put_u8(status::ENTRIES | COMPRESSED);
        b.put_u32(u32::MAX);
        assert!(inflate(b.freeze()).is_err());
    }

    #[test]
    fn histories() {
        let h = vec![(0, 1), (10, 3), (15, 5)];
        assert_eq!(epoch_at(&h, 0), 1);
        assert_eq!(epoch_at(&h, 9), 1);
        assert_eq!(epoch_at(&h, 10), 3);
        assert_eq!(epoch_at(&h, 99), 5);
        assert_eq!(prev_epoch(&h, 10), 1);
        assert_eq!(prev_epoch(&h, 0), 0);
        // A replica that kept an uncommitted epoch-2 suffix diverges at 10.
        let r = vec![(0, 1), (10, 2)];
        assert_eq!(common_prefix(&h, 20, &r, 12), 10);
        // A lagging replica is a prefix.
        assert_eq!(common_prefix(&h, 20, &[(0, 1), (10, 3)], 12), 12);
        assert_eq!(common_prefix(&h, 20, &[], 0), 0);
        let mut p = h.clone();
        prune_history(&mut p, 12);
        assert_eq!(p, vec![(10, 3), (15, 5)]);
        prune_history(&mut p, 100);
        assert_eq!(p, vec![(15, 5)]);
    }
}
