//! The pull and push filter chains of `mbuf.c`. A chain is a stack: level 0 is the message
//! buffer itself, level n the n-th filter on top of it; a filter reads from (or writes to)
//! the level below it. PostgreSQL creates and frees its filters in stack order, so a chain
//! here is a `Vec`.

use super::Pgp;
use super::super::px::{PXE_PGP_CORRUPT_DATA, Px, PxErr};

/// A pull filter (`PullFilterOps`): what it reads from `below` (a level of `r`).
pub(super) trait PullOp {
    /// `pull`: up to `len` bytes; empty at the end. The default passes reads through.
    fn pull(&mut self, r: &mut Reader, below: usize, len: usize, ctx: &mut Pgp) -> Px<Vec<u8>> {
        r.read(below, len, ctx)
    }

    /// `free`.
    fn free(&mut self, _ctx: &mut Pgp) {}
}

struct Passthrough;

impl PullOp for Passthrough {}

struct PullLayer {
    op: Box<dyn PullOp>,
    buflen: usize,
}

/// A pull chain over a message.
pub(super) struct Reader {
    data: Vec<u8>,
    pos: usize,
    layers: Vec<PullLayer>,
}

impl Reader {
    pub fn new(data: &[u8]) -> Reader {
        Reader {
            data: data.to_vec(),
            pos: 0,
            layers: Vec::new(),
        }
    }

    /// The level a new filter would read from.
    pub fn top(&self) -> usize {
        self.layers.len()
    }

    /// `pullf_create` on the top level: `buflen` is what the filter's init returned (reads
    /// through it are cut to that length).
    pub fn push(&mut self, op: Box<dyn PullOp>, buflen: usize) -> usize {
        self.layers.push(PullLayer { op, buflen });
        self.layers.len()
    }

    /// `pullf_free` of the top filter.
    pub fn pop(&mut self, ctx: &mut Pgp) {
        if let Some(mut l) = self.layers.pop() {
            l.op.free(ctx);
        }
    }

    /// Free filters down to `level`.
    pub fn pop_to(&mut self, level: usize, ctx: &mut Pgp) {
        while self.layers.len() > level {
            self.pop(ctx);
        }
    }

    /// `pullf_read` at `level`: may return less than asked, nothing at the end.
    pub fn read(&mut self, level: usize, len: usize, ctx: &mut Pgp) -> Px<Vec<u8>> {
        if level == 0 {
            let n = len.min(self.data.len() - self.pos);
            let out = self.data[self.pos..self.pos + n].to_vec();
            self.pos += n;
            return Ok(out);
        }
        let i = level - 1;
        let len = match self.layers[i].buflen {
            0 => len,
            b => len.min(b),
        };
        let mut op = std::mem::replace(&mut self.layers[i].op, Box::new(Passthrough));
        let r = op.pull(self, i, len, ctx);
        self.layers[i].op = op;
        r
    }

    /// `pullf_read_max`: up to `len` bytes, reading on until there are that many or the end.
    pub fn read_max(&mut self, level: usize, len: usize, ctx: &mut Pgp) -> Px<Vec<u8>> {
        let mut out = self.read(level, len, ctx)?;
        if out.is_empty() || out.len() == len {
            return Ok(out);
        }
        while out.len() < len {
            let more = self.read(level, len - out.len(), ctx)?;
            if more.is_empty() {
                break;
            }
            out.extend_from_slice(&more);
        }
        Ok(out)
    }

    /// `pullf_read_fixed`: exactly `len` bytes, or corrupt data.
    pub fn read_fixed(&mut self, level: usize, len: usize, ctx: &mut Pgp) -> Px<Vec<u8>> {
        let out = self.read_max(level, len, ctx)?;
        if out.len() != len {
            super::debug(format!("pullf_read_fixed: need={len} got={}", out.len()));
            return Err(PxErr::Code(PXE_PGP_CORRUPT_DATA));
        }
        Ok(out)
    }

    /// `GETBYTE`.
    pub fn byte(&mut self, level: usize, ctx: &mut Pgp) -> Px<u8> {
        Ok(self.read_fixed(level, 1, ctx)?[0])
    }
}

/// A push filter (`PushFilterOps`): what it writes to `below` (a level of `w`).
pub(super) trait PushOp {
    /// `push`: the default writes the data on.
    fn push(&mut self, w: &mut Writer, below: usize, data: &[u8]) -> Px<()> {
        w.write(below, data)
    }

    /// `flush`.
    fn flush(&mut self, _w: &mut Writer, _below: usize) -> Px<()> {
        Ok(())
    }
}

struct PushPass;

impl PushOp for PushPass {}

struct PushLayer {
    op: Box<dyn PushOp>,
    block_size: usize,
    buf: Vec<u8>,
}

/// A push chain into a message buffer.
#[derive(Default)]
pub(super) struct Writer {
    pub out: Vec<u8>,
    layers: Vec<PushLayer>,
}

impl Writer {
    pub fn top(&self) -> usize {
        self.layers.len()
    }

    /// `pushf_create` on the top level, with the block size its init returned.
    pub fn push(&mut self, op: Box<dyn PushOp>, block_size: usize) -> usize {
        self.layers.push(PushLayer {
            op,
            block_size,
            buf: Vec::new(),
        });
        self.layers.len()
    }

    pub fn pop(&mut self) {
        self.layers.pop();
    }

    fn process(&mut self, i: usize, data: &[u8]) -> Px<()> {
        let mut op = std::mem::replace(&mut self.layers[i].op, Box::new(PushPass));
        let r = op.push(self, i, data);
        self.layers[i].op = op;
        r
    }

    /// `pushf_write` at `level`: buffered into blocks of the filter's size.
    pub fn write(&mut self, level: usize, data: &[u8]) -> Px<()> {
        if level == 0 {
            self.out.extend_from_slice(data);
            return Ok(());
        }
        let i = level - 1;
        let bs = self.layers[i].block_size;
        if bs == 0 {
            return self.process(i, data);
        }
        let mut data = data;
        let need = bs - self.layers[i].buf.len();
        if need > 0 {
            if data.len() < need {
                self.layers[i].buf.extend_from_slice(data);
                return Ok(());
            }
            self.layers[i].buf.extend_from_slice(&data[..need]);
            data = &data[need..];
        }
        let block = std::mem::take(&mut self.layers[i].buf);
        self.process(i, &block)?;
        while !data.is_empty() {
            if data.len() > bs {
                self.process(i, &data[..bs])?;
                data = &data[bs..];
            } else {
                self.layers[i].buf.extend_from_slice(data);
                break;
            }
        }
        Ok(())
    }

    /// `pushf_flush` from `level` down: each filter's buffered data (even none), then its
    /// flush.
    pub fn flush(&mut self, level: usize) -> Px<()> {
        for i in (0..level).rev() {
            if self.layers[i].block_size > 0 {
                let block = std::mem::take(&mut self.layers[i].buf);
                self.process(i, &block)?;
            }
            let mut op = std::mem::replace(&mut self.layers[i].op, Box::new(PushPass));
            let r = op.flush(self, i);
            self.layers[i].op = op;
            r?;
        }
        Ok(())
    }
}

