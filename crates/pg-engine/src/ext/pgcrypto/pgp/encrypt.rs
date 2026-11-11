//! Encryption (`pgp-encrypt.c`): the session key packet, then the encrypted data packet
//! holding the prefix, an optional compressed packet and the literal data packet, each
//! written by a chain of push filters.

use super::super::digest::Md;
use super::super::px::{PXE_ARGUMENT_ERROR, PXE_BUG, Px, PxErr};
use super::cfb::{self, Cfb};
use super::mbuf::{PushOp, Writer};
use super::s2k::{S2k, sha1};
use super::{Pgp, random};

const STREAM_BLOCK_SHIFT: u32 = 14;

/// `render_newlen`.
pub(super) fn newlen(len: usize) -> Vec<u8> {
    if len <= 191 {
        vec![len as u8]
    } else if len <= 8383 {
        vec![(((len - 192) >> 8) + 192) as u8, ((len - 192) & 255) as u8]
    } else {
        let l = len as u32;
        vec![255, (l >> 24) as u8, (l >> 16) as u8, (l >> 8) as u8, l as u8]
    }
}

/// Streamable packets (`pkt_stream_filter`): full blocks as partial-length chunks, then a
/// final normal-length one.
struct PktStream {
    final_done: bool,
}

const PKT_BLOCK: usize = 1 << STREAM_BLOCK_SHIFT;

impl PushOp for PktStream {
    fn push(&mut self, w: &mut Writer, below: usize, data: &[u8]) -> Px<()> {
        if self.final_done {
            return Err(PxErr::Code(PXE_BUG));
        }
        let hdr = if data.len() == PKT_BLOCK {
            vec![0xE0 | STREAM_BLOCK_SHIFT as u8]
        } else {
            self.final_done = true;
            newlen(data.len())
        };
        w.write(below, &hdr)?;
        w.write(below, data)
    }

    fn flush(&mut self, w: &mut Writer, below: usize) -> Px<()> {
        if !self.final_done {
            w.write(below, &newlen(0))?;
            self.final_done = true;
        }
        Ok(())
    }
}

/// `pgp_create_pkt_writer`: a streamed packet of `tag` on top of the chain.
pub(super) fn pkt_writer(w: &mut Writer, tag: u8) -> Px<usize> {
    let top = w.top();
    w.write(top, &[0xC0 | tag])?;
    Ok(w.push(Box::new(PktStream { final_done: false }), PKT_BLOCK))
}

struct Encrypt {
    cfb: Cfb,
}

impl PushOp for Encrypt {
    fn push(&mut self, w: &mut Writer, below: usize, data: &[u8]) -> Px<()> {
        for chunk in data.chunks(8192) {
            let enc = self.cfb.encrypt(chunk)?;
            w.write(below, &enc)?;
        }
        Ok(())
    }
}

/// The MDC hasher: what passes is hashed; the MDC packet follows it.
struct Mdc {
    md: Md,
}

impl PushOp for Mdc {
    fn push(&mut self, w: &mut Writer, below: usize, data: &[u8]) -> Px<()> {
        self.md.update(data);
        w.write(below, data)
    }

    fn flush(&mut self, w: &mut Writer, below: usize) -> Px<()> {
        let mut pkt = vec![0xD3, 0x14];
        self.md.update(&pkt);
        pkt.extend_from_slice(&self.md.finish());
        w.write(below, &pkt)
    }
}

/// Text conversion: LF to CRLF.
struct Crlf;

impl PushOp for Crlf {
    fn push(&mut self, w: &mut Writer, below: usize, data: &[u8]) -> Px<()> {
        let mut out = Vec::with_capacity(data.len() + data.len() / 16);
        for &b in data {
            if b == b'\n' {
                out.extend_from_slice(b"\r\n");
            } else {
                out.push(b);
            }
        }
        w.write(below, &out)
    }
}

/// `write_symenc_sesskey`.
fn write_symenc_sesskey(ctx: &mut Pgp, w: &mut Writer) -> Px<()> {
    let mut pkt = vec![4, ctx.s2k_cipher_algo as u8, ctx.s2k.mode, ctx.s2k.digest_algo];
    if ctx.s2k.mode > 0 {
        pkt.extend_from_slice(&ctx.s2k.salt);
    }
    if ctx.s2k.mode == 3 {
        pkt.push(ctx.s2k.iter);
    }
    if ctx.use_sess_key {
        let mut c = Cfb::new(ctx.s2k_cipher_algo, &ctx.s2k.key, false, None, false)?;
        pkt.extend_from_slice(&c.encrypt(&[ctx.cipher_algo as u8])?);
        pkt.extend_from_slice(&c.encrypt(&ctx.sess_key)?);
    }
    w.write(0, &[0xC0 | 3])?;
    w.write(0, &newlen(pkt.len()))?;
    w.write(0, &pkt)
}

/// `pgp_encrypt`.
pub(super) fn encrypt(ctx: &mut Pgp, data: &[u8]) -> Px<Vec<u8>> {
    if ctx.sym_key.is_none() && ctx.pub_key.is_none() {
        return Err(PxErr::Code(PXE_ARGUMENT_ERROR));
    }
    let mut w = Writer::default();
    if let Some(key) = ctx.sym_key.clone() {
        if ctx.s2k_cipher_algo < 0 {
            ctx.s2k_cipher_algo = ctx.cipher_algo;
        }
        ctx.s2k = S2k::fill(ctx.s2k_mode, ctx.s2k_digest_algo, ctx.s2k_count, &mut random)?;
        ctx.s2k.process(ctx.s2k_cipher_algo, &key)?;
    }
    if ctx.use_sess_key || ctx.pub_key.is_some() {
        ctx.sess_key = vec![0u8; cfb::key_size(ctx.cipher_algo)];
        random(&mut ctx.sess_key);
    } else {
        ctx.sess_key = ctx.s2k.key.clone();
    }
    if ctx.pub_key.is_some() {
        super::pubenc::write_pubenc_sesskey(ctx, &mut w)?;
    } else {
        write_symenc_sesskey(ctx, &mut w)?;
    }
    // The encrypted data packet.
    let mut top = pkt_writer(&mut w, if ctx.disable_mdc { 9 } else { 18 })?;
    if !ctx.disable_mdc {
        w.write(top, &[1])?;
    }
    let c = Cfb::new(ctx.cipher_algo, &ctx.sess_key, ctx.disable_mdc, None, false)?;
    top = w.push(Box::new(Encrypt { cfb: c }), 8192);
    if !ctx.disable_mdc {
        top = w.push(Box::new(Mdc { md: sha1() }), 0);
    }
    // The prefix: a random block, its last two bytes repeated.
    let bs = cfb::block_size(ctx.cipher_algo);
    let mut prefix = vec![0u8; bs];
    random(&mut prefix);
    prefix.push(prefix[bs - 2]);
    prefix.push(prefix[bs - 1]);
    w.write(top, &prefix)?;
    if ctx.compress_algo > 0 && ctx.compress_level > 0 {
        top = pkt_writer(&mut w, 8)?;
        w.write(top, &[ctx.compress_algo as u8])?;
        super::compress::compress_filter(ctx, &mut w)?;
    }
    // The literal data packet: type, no file name, the time.
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as u32);
    let ty = match (ctx.text_mode, ctx.unicode_mode) {
        (true, true) => b'u',
        (true, false) => b't',
        (false, _) => b'b',
    };
    top = pkt_writer(&mut w, 11)?;
    w.write(top, &[ty, 0, (t >> 24) as u8, (t >> 16) as u8, (t >> 8) as u8, t as u8])?;
    if ctx.text_mode && ctx.convert_crlf {
        top = w.push(Box::new(Crlf), 0);
    }
    w.write(top, data)?;
    w.flush(top)?;
    Ok(std::mem::take(&mut w.out))
}
