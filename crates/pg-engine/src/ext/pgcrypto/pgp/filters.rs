//! The decryption side's pull filters (`pgp-decrypt.c`): the prefix check, CFB decryption,
//! the MDC hasher and the combined reader + hasher of context-length MDC packets.

use super::super::px::{PXE_BUG, PXE_PGP_CORRUPT_DATA, Px, PxErr};
use super::cfb::{self, Cfb};
use super::mbuf::{PullOp, Reader};
use super::s2k::sha1;
use super::{Pgp, debug};

struct Pass;

impl PullOp for Pass {}

/// `prefix_filter`: reads and checks the random prefix as it is created (a failed "quick
/// check" is reported at the end, by `pgp_decrypt`).
pub(super) fn prefix_filter(r: &mut Reader, ctx: &mut Pgp) -> Px<usize> {
    let len = cfb::block_size(ctx.cipher_algo);
    if len > 32 {
        return Err(PxErr::Code(PXE_BUG));
    }
    let top = r.top();
    let buf = r.read_max(top, len + 2, ctx)?;
    if buf.len() != len + 2 {
        debug("prefix_init: short read".into());
        return Err(PxErr::Code(PXE_PGP_CORRUPT_DATA));
    }
    if buf[len - 2] != buf[len] || buf[len - 1] != buf[len + 1] {
        debug("prefix_init: corrupt prefix".into());
        ctx.corrupt_prefix = true;
    }
    Ok(r.push(Box::new(Pass), 0))
}

/// `pgp_decrypt_filter`.
pub(super) struct Decrypt {
    pub cfb: Cfb,
}

impl PullOp for Decrypt {
    fn pull(&mut self, r: &mut Reader, below: usize, len: usize, ctx: &mut Pgp) -> Px<Vec<u8>> {
        let d = r.read(below, len, ctx)?;
        if d.is_empty() {
            return Ok(d);
        }
        Ok(self.cfb.decrypt(&d)?)
    }
}

/// `pgp_decrypt_filter` on top of the chain (its reads are 4096 bytes at most).
pub(super) fn decrypt_filter(r: &mut Reader, cfb: Cfb) -> usize {
    r.push(Box::new(Decrypt { cfb }), 4096)
}

/// `mdc_filter`.
struct Mdc;

impl PullOp for Mdc {
    fn pull(&mut self, r: &mut Reader, below: usize, len: usize, ctx: &mut Pgp) -> Px<Vec<u8>> {
        if ctx.use_mdcbuf_filter || ctx.in_mdc_pkt {
            return r.read(below, len, ctx);
        }
        let d = r.read(below, len, ctx)?;
        if d.is_empty() {
            debug("mdc_read: unexpected eof".into());
            return Err(PxErr::Code(PXE_PGP_CORRUPT_DATA));
        }
        if let Some(md) = &mut ctx.mdc_ctx {
            md.update(&d);
        }
        Ok(d)
    }

    fn free(&mut self, ctx: &mut Pgp) {
        if !ctx.use_mdcbuf_filter {
            ctx.mdc_ctx = None;
        }
    }
}

pub(super) fn mdc_filter(r: &mut Reader, ctx: &mut Pgp) -> usize {
    ctx.mdc_ctx = Some(sha1());
    r.push(Box::new(Mdc), 0)
}

/// `mdc_finish`: the MDC packet's hash against the one computed.
pub(super) fn mdc_finish(r: &mut Reader, level: usize, len: usize, ctx: &mut Pgp) -> Px<()> {
    if ctx.use_mdcbuf_filter {
        return Err(PxErr::Code(PXE_BUG));
    }
    if len != 20 {
        return Err(PxErr::Code(PXE_PGP_CORRUPT_DATA));
    }
    ctx.in_mdc_pkt = true;
    let data = r.read_max(level, len, ctx)?;
    if data.is_empty() {
        debug("no mdc".into());
        return Err(PxErr::Code(PXE_PGP_CORRUPT_DATA));
    }
    if data.len() != 20 {
        debug(format!("mdc_finish: read failed, res={}", data.len()));
        return Err(PxErr::Code(PXE_PGP_CORRUPT_DATA));
    }
    let hash = ctx.mdc_ctx.as_mut().map(|m| m.finish()).unwrap_or_default();
    if hash != data {
        debug("mdc_finish: mdc failed".into());
        return Err(PxErr::Code(PXE_PGP_CORRUPT_DATA));
    }
    ctx.mdc_checked = true;
    Ok(())
}

const MDCBUF_LEN: usize = 8192;

/// `mdcbuf_filter`: a context-length packet inside SYMENCRYPTED_DATA_MDC, whose last 22
/// bytes are the MDC packet.
struct MdcBuf {
    eof: bool,
    buf: Vec<u8>,
    pos: usize,
    mdc: Vec<u8>,
}

impl MdcBuf {
    fn load_data(&mut self, src: &[u8], ctx: &mut Pgp) {
        self.buf.extend_from_slice(src);
        if let Some(md) = &mut ctx.mdc_ctx {
            md.update(src);
        }
    }

    fn finish(&mut self, ctx: &mut Pgp) -> Px<()> {
        self.eof = true;
        if self.mdc.len() < 2 || self.mdc[0] != 0xD3 || self.mdc[1] != 0x14 {
            debug("mdcbuf_finish: bad MDC pkt hdr".into());
            return Err(PxErr::Code(PXE_PGP_CORRUPT_DATA));
        }
        let md = ctx.mdc_ctx.as_mut().ok_or(PxErr::Code(PXE_BUG))?;
        md.update(&self.mdc[..2]);
        let hash = md.finish();
        if self.mdc.len() != 22 || hash[..] != self.mdc[2..] {
            debug("mdcbuf_finish: MDC does not match".into());
            return Err(PxErr::Code(PXE_PGP_CORRUPT_DATA));
        }
        Ok(())
    }

    fn refill(&mut self, r: &mut Reader, below: usize, ctx: &mut Pgp) -> Px<()> {
        self.buf.drain(..self.pos);
        self.pos = 0;
        let need = MDCBUF_LEN + 22 - self.buf.len() - self.mdc.len();
        let data = r.read(below, need, ctx)?;
        if data.is_empty() {
            return self.finish(ctx);
        }
        let res = data.len();
        if res >= 22 {
            let held = std::mem::take(&mut self.mdc);
            self.load_data(&held, ctx);
            self.load_data(&data[..res - 22], ctx);
            self.mdc = data[res - 22..].to_vec();
        } else {
            let canmove = self.mdc.len() as isize + res as isize - 22;
            if canmove > 0 {
                let moved: Vec<u8> = self.mdc.drain(..canmove as usize).collect();
                self.load_data(&moved, ctx);
            }
            self.mdc.extend_from_slice(&data);
        }
        Ok(())
    }
}

impl PullOp for MdcBuf {
    fn pull(&mut self, r: &mut Reader, below: usize, len: usize, ctx: &mut Pgp) -> Px<Vec<u8>> {
        let avail = self.buf.len() - self.pos;
        if !self.eof && len > avail {
            self.refill(r, below, ctx)?;
        }
        let avail = self.buf.len() - self.pos;
        let n = len.min(avail);
        let d = self.buf[self.pos..self.pos + n].to_vec();
        self.pos += n;
        Ok(d)
    }

    fn free(&mut self, ctx: &mut Pgp) {
        ctx.mdc_ctx = None;
    }
}

pub(super) fn mdcbuf_filter(r: &mut Reader, ctx: &mut Pgp) -> usize {
    ctx.use_mdcbuf_filter = true;
    r.push(
        Box::new(MdcBuf {
            eof: false,
            buf: Vec::new(),
            pos: 0,
            mdc: Vec::new(),
        }),
        0,
    )
}
