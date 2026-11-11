//! Compressed data packets (`pgp-compress.c`): ZIP (raw deflate) and ZLIB.

use super::super::px::{PXE_PGP_COMPRESSION_ERROR, PXE_PGP_CORRUPT_DATA, PXE_PGP_UNSUPPORTED_COMPR, Px, PxErr};
use super::mbuf::{PullOp, PushOp, Reader, Writer};
use super::{Pgp, debug};
use flate2::{Compress, Compression, Decompress, FlushCompress, FlushDecompress, Status};

const ZIP_OUT_BUF: usize = 8192;

struct Zip {
    z: Compress,
}

impl Zip {
    fn run(&mut self, w: &mut Writer, below: usize, mut data: &[u8], flush: FlushCompress) -> Px<()> {
        loop {
            let mut out = Vec::with_capacity(ZIP_OUT_BUF);
            let before = self.z.total_in();
            let st = self
                .z
                .compress_vec(data, &mut out, flush)
                .map_err(|_| PxErr::Code(PXE_PGP_COMPRESSION_ERROR))?;
            data = &data[(self.z.total_in() - before) as usize..];
            if !out.is_empty() {
                w.write(below, &out)?;
            }
            let done = match flush {
                FlushCompress::Finish => st == Status::StreamEnd,
                _ => data.is_empty(),
            };
            if done {
                return Ok(());
            }
        }
    }
}

impl PushOp for Zip {
    fn push(&mut self, w: &mut Writer, below: usize, data: &[u8]) -> Px<()> {
        if data.is_empty() {
            return Ok(());
        }
        self.run(w, below, data, FlushCompress::None)
    }

    fn flush(&mut self, w: &mut Writer, below: usize) -> Px<()> {
        self.run(w, below, &[], FlushCompress::Finish)
    }
}

/// `pgp_compress_filter` on top of the chain.
pub(super) fn compress_filter(ctx: &Pgp, w: &mut Writer) -> Px<usize> {
    let zlib = match ctx.compress_algo {
        1 => false,
        2 => true,
        _ => return Err(PxErr::Code(PXE_PGP_UNSUPPORTED_COMPR)),
    };
    let z = Compress::new(Compression::new(ctx.compress_level as u32), zlib);
    Ok(w.push(Box::new(Zip { z }), 8192))
}

/// The decompressing pull filter (`decompress_filter`).
struct Unzip {
    z: Decompress,
    input: Vec<u8>,
    out: Vec<u8>,
    pos: usize,
    eof: bool,
}

impl PullOp for Unzip {
    fn pull(&mut self, r: &mut Reader, below: usize, len: usize, ctx: &mut Pgp) -> Px<Vec<u8>> {
        loop {
            if self.pos < self.out.len() {
                let n = len.min(self.out.len() - self.pos);
                let d = self.out[self.pos..self.pos + n].to_vec();
                self.pos += n;
                return Ok(d);
            }
            if self.eof {
                return Ok(Vec::new());
            }
            if self.input.is_empty() {
                self.input = r.read(below, 8192, ctx)?;
            }
            let flush = if self.input.is_empty() { FlushDecompress::Finish } else { FlushDecompress::Sync };
            self.out = Vec::with_capacity(ZIP_OUT_BUF);
            self.pos = 0;
            let before = self.z.total_in();
            let res = self.z.decompress_vec(&self.input, &mut self.out, flush);
            let used = (self.z.total_in() - before) as usize;
            self.input.drain(..used);
            let st = match res {
                Ok(Status::BufError) if self.out.is_empty() => Err(-5),
                Ok(s) => Ok(s),
                Err(_) => Err(-3),
            };
            let st = match st {
                Ok(s) => s,
                Err(code) => {
                    debug(format!("decompress_read: inflate error: {code}"));
                    return Err(PxErr::Code(PXE_PGP_CORRUPT_DATA));
                }
            };
            if st == Status::StreamEnd {
                // The packet must end with the stream: read its terminating packet. (Input
                // already read past the stream's end isn't looked at, as in PostgreSQL.)
                let extra = r.read(below, 1, ctx)?;
                if !extra.is_empty() {
                    debug("decompress_read: extra bytes after end of stream".into());
                    return Err(PxErr::Code(PXE_PGP_CORRUPT_DATA));
                }
                self.eof = true;
            }
        }
    }
}

/// `pgp_decompress_filter` on top of the chain.
pub(super) fn decompress_filter(ctx: &Pgp, r: &mut Reader) -> Px<usize> {
    let zlib = match ctx.compress_algo {
        1 => false,
        2 => true,
        _ => return Err(PxErr::Code(PXE_PGP_UNSUPPORTED_COMPR)),
    };
    Ok(r.push(
        Box::new(Unzip {
            z: Decompress::new(zlib),
            input: Vec::new(),
            out: Vec::new(),
            pos: 0,
            eof: false,
        }),
        0,
    ))
}
