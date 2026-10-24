//! Packet headers and the packet reader (`pgp-decrypt.c`'s first part).

use super::super::px::{PXE_PGP_CORRUPT_DATA, Px, PxErr};
use super::mbuf::{PullOp, Reader};
use super::{Pgp, debug};

pub(super) const PKT_NORMAL: u8 = 1;
pub(super) const PKT_STREAM: u8 = 2;
pub(super) const PKT_CONTEXT: u8 = 3;

const MAX_CHUNK: i64 = 16 * 1024 * 1024;

fn corrupt() -> PxErr {
    PxErr::Code(PXE_PGP_CORRUPT_DATA)
}

/// `parse_new_len`: the length and the packet type (normal or a stream's chunk).
pub(super) fn parse_new_len(r: &mut Reader, level: usize, ctx: &mut Pgp) -> Px<(usize, u8)> {
    let b = r.byte(level, ctx)?;
    let mut ty = PKT_NORMAL;
    let len: i64 = match b {
        0..=191 => b as i64,
        192..=223 => {
            let b2 = r.byte(level, ctx)?;
            (((b as i64) - 192) << 8) + 192 + b2 as i64
        }
        255 => {
            let mut l: i32 = 0;
            for _ in 0..4 {
                l = (l << 8) | r.byte(level, ctx)? as i32;
            }
            l as i64
        }
        _ => {
            ty = PKT_STREAM;
            1i64 << (b & 0x1F)
        }
    };
    if !(0..=MAX_CHUNK).contains(&len) {
        debug("parse_new_len: weird length".into());
        return Err(corrupt());
    }
    Ok((len as usize, ty))
}

fn parse_old_len(r: &mut Reader, level: usize, lentype: u8, ctx: &mut Pgp) -> Px<usize> {
    let mut len: i32 = r.byte(level, ctx)? as i32;
    let more = match lentype {
        1 => 1,
        2 => 3,
        _ => 0,
    };
    for _ in 0..more {
        len = (len << 8) | r.byte(level, ctx)? as i32;
    }
    if !(0..=MAX_CHUNK as i32).contains(&len) {
        debug("parse_old_len: weird length".into());
        return Err(corrupt());
    }
    Ok(len as usize)
}

/// `pgp_parse_pkt_hdr`: (tag, length, packet type), None at the end of the input.
pub(super) fn parse_pkt_hdr(r: &mut Reader, level: usize, allow_ctx: bool, ctx: &mut Pgp) -> Px<Option<(u8, usize, u8)>> {
    let p = r.read(level, 1, ctx)?;
    let Some(&h) = p.first() else {
        return Ok(None);
    };
    if h & 0x80 == 0 {
        debug("pgp_parse_pkt_hdr: not pkt hdr".into());
        return Err(corrupt());
    }
    if h & 0x40 != 0 {
        let (len, ty) = parse_new_len(r, level, ctx)?;
        return Ok(Some((h & 0x3f, len, ty)));
    }
    let lentype = h & 3;
    let tag = (h >> 2) & 0x0F;
    if lentype == 3 {
        if !allow_ctx {
            return Err(corrupt());
        }
        return Ok(Some((tag, 0, PKT_CONTEXT)));
    }
    Ok(Some((tag, parse_old_len(r, level, lentype, ctx)?, PKT_NORMAL)))
}

/// The packet reader (`pktreader_filter`): a packet's body, chunk by chunk.
struct PktReader {
    ty: u8,
    len: usize,
}

impl PullOp for PktReader {
    fn pull(&mut self, r: &mut Reader, below: usize, len: usize, ctx: &mut Pgp) -> Px<Vec<u8>> {
        if self.ty == PKT_CONTEXT {
            return r.read(below, len, ctx);
        }
        while self.len == 0 {
            if self.ty == PKT_NORMAL {
                return Ok(Vec::new());
            }
            let (l, t) = parse_new_len(r, below, ctx)?;
            self.len = l;
            self.ty = t;
        }
        let d = r.read(below, len.min(self.len), ctx)?;
        self.len -= d.len();
        Ok(d)
    }
}

/// `pgp_create_pkt_reader` on top of the chain.
pub(super) fn pkt_reader(r: &mut Reader, len: usize, ty: u8) -> usize {
    r.push(Box::new(PktReader { ty, len }), 0)
}

/// `pgp_skip_packet`.
pub(super) fn skip_packet(r: &mut Reader, level: usize, ctx: &mut Pgp) -> Px<()> {
    while !r.read(level, 32 * 1024, ctx)?.is_empty() {}
    Ok(())
}

/// `pgp_expect_packet_end`.
pub(super) fn expect_packet_end(r: &mut Reader, level: usize, ctx: &mut Pgp) -> Px<()> {
    if !r.read(level, 32 * 1024, ctx)?.is_empty() {
        debug("pgp_expect_packet_end: got data".into());
        return Err(corrupt());
    }
    Ok(())
}
