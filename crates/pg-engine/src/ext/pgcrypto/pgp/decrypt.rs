//! Decryption (`pgp-decrypt.c`): the session key packets, then the encrypted data packet's
//! packets.

use super::super::px::{PXE_PGP_CORRUPT_DATA, PXE_PGP_NOT_TEXT, PXE_PGP_UNSUPPORTED_COMPR, Px, PxErr};
use super::cfb::{self, Cfb};
use super::filters;
use super::mbuf::Reader;
use super::packet::{self, PKT_CONTEXT};
use super::s2k::{S2k, decode_count};
use super::{Pgp, debug};

fn corrupt() -> PxErr {
    PxErr::Code(PXE_PGP_CORRUPT_DATA)
}

/// `decrypt_key`: a separate session key, encrypted with the S2K key.
fn decrypt_key(ctx: &mut Pgp, src: &[u8]) -> Px<()> {
    let mut c = Cfb::new(ctx.s2k_cipher_algo, &ctx.s2k.key, false, None, ctx.ignore_cipher_failure)?;
    let algo = c.decrypt(&src[..1])?[0];
    let key = c.decrypt(&src[1..])?;
    ctx.sess_key = key;
    ctx.cipher_algo = algo as i32;
    let want = cfb::key_size(algo as i32);
    if want != src.len() - 1 {
        debug(format!("sesskey bad len: algo={algo}, expected={want}, got={}", src.len() - 1));
        return Err(corrupt());
    }
    Ok(())
}

/// `parse_symenc_sesskey`.
fn parse_symenc_sesskey(r: &mut Reader, level: usize, ctx: &mut Pgp) -> Px<()> {
    let ver = r.byte(level, ctx)?;
    ctx.s2k_cipher_algo = r.byte(level, ctx)? as i32;
    if ver != 4 {
        debug("bad key pkt ver".into());
        return Err(corrupt());
    }
    ctx.s2k = S2k::read(r, level, ctx)?;
    ctx.s2k_mode = ctx.s2k.mode as i32;
    ctx.s2k_count = decode_count(ctx.s2k.iter) as i32;
    ctx.s2k_digest_algo = ctx.s2k.digest_algo as i32;
    let key = ctx.sym_key.clone().unwrap_or_default();
    let cipher = ctx.s2k_cipher_algo;
    ctx.s2k.process(cipher, &key)?;
    let p = r.read_max(level, 32 + 2, ctx)?;
    if p.is_empty() {
        ctx.sess_key = ctx.s2k.key.clone();
        ctx.cipher_algo = ctx.s2k_cipher_algo;
        ctx.use_sess_key = false;
        return Ok(());
    }
    if p.len() < 17 || p.len() > 32 + 1 {
        debug("expect key, but bad data".into());
        return Err(corrupt());
    }
    ctx.use_sess_key = true;
    decrypt_key(ctx, &p)
}

/// `copy_crlf`: CRLF to LF (a CR at the end of a chunk waits for the next one).
fn copy_crlf(dst: &mut Vec<u8>, data: &[u8], got_cr: &mut bool) {
    let mut i = 0;
    if *got_cr {
        if data.first() != Some(&b'\n') {
            dst.push(b'\r');
        }
        *got_cr = false;
    }
    while i < data.len() {
        if data[i] == b'\r' {
            if i + 1 < data.len() {
                if data[i + 1] == b'\n' {
                    i += 1;
                }
            } else {
                *got_cr = true;
                break;
            }
        }
        dst.push(data[i]);
        i += 1;
    }
}

/// `parse_literal_data`.
fn parse_literal_data(r: &mut Reader, level: usize, ctx: &mut Pgp, dst: &mut Vec<u8>) -> Px<()> {
    let ty = r.byte(level, ctx)?;
    let mut name_len = r.byte(level, ctx)? as usize;
    while name_len > 0 {
        let d = r.read(level, name_len, ctx)?;
        if d.is_empty() {
            break;
        }
        name_len -= d.len();
    }
    if name_len > 0 {
        debug("parse_literal_data: unexpected eof".into());
        return Err(corrupt());
    }
    if r.read_max(level, 4, ctx)?.len() != 4 {
        debug("parse_literal_data: unexpected eof".into());
        return Err(corrupt());
    }
    if ctx.text_mode && ty != b't' && ty != b'u' {
        debug(format!("parse_literal_data: data type={}", ty as char));
        ctx.unexpected_binary = true;
    }
    ctx.unicode_mode = ty == b'u';
    let mut got_cr = false;
    loop {
        let d = r.read(level, 32 * 1024, ctx)?;
        if d.is_empty() {
            break;
        }
        if ctx.text_mode && ctx.convert_crlf {
            copy_crlf(dst, &d, &mut got_cr);
        } else {
            dst.extend_from_slice(&d);
        }
    }
    if got_cr {
        dst.push(b'\r');
    }
    Ok(())
}

/// `parse_compressed_data`.
fn parse_compressed_data(r: &mut Reader, level: usize, ctx: &mut Pgp, dst: &mut Vec<u8>) -> Px<()> {
    let ty = r.byte(level, ctx)?;
    ctx.compress_algo = ty as i32;
    match ty {
        0 => process_data_packets(r, level, ctx, dst, false, false),
        1 | 2 => {
            let top = super::compress::decompress_filter(ctx, r)?;
            let res = process_data_packets(r, top, ctx, dst, false, false);
            r.pop(ctx);
            res
        }
        3 => {
            debug("parse_compressed_data: bzip2 unsupported".into());
            ctx.unsupported_compr = true;
            // Discard the data, letting it reach the MDC first.
            while !r.read(level, 32 * 1024, ctx)?.is_empty() {}
            Ok(())
        }
        _ => {
            debug("parse_compressed_data: unknown compr type".into());
            Err(corrupt())
        }
    }
}

/// `process_data_packets`.
fn process_data_packets(r: &mut Reader, src: usize, ctx: &mut Pgp, dst: &mut Vec<u8>, allow_compr: bool, need_mdc: bool) -> Px<()> {
    let mut got_data = false;
    let mut got_mdc = false;
    while let Some((tag, len, ty)) = packet::parse_pkt_hdr(r, src, true, ctx)? {
        if got_mdc {
            debug("process_data_packets: data after mdc".into());
            return Err(corrupt());
        }
        let pkt = if need_mdc && ty == PKT_CONTEXT {
            filters::mdcbuf_filter(r, ctx)
        } else {
            packet::pkt_reader(r, len, ty)
        };
        let res = match tag {
            11 => {
                got_data = true;
                parse_literal_data(r, pkt, ctx, dst)
            }
            8 if !allow_compr => {
                debug("process_data_packets: unexpected compression".into());
                Err(corrupt())
            }
            8 if got_data => {
                debug("process_data_packets: only one cmpr pkt allowed".into());
                Err(corrupt())
            }
            8 => {
                got_data = true;
                parse_compressed_data(r, pkt, ctx, dst)
            }
            19 if !need_mdc => {
                debug("process_data_packets: unexpected MDC".into());
                Err(corrupt())
            }
            19 => filters::mdc_finish(r, pkt, len, ctx).map(|_| got_mdc = true),
            _ => {
                debug(format!("process_data_packets: unexpected pkt tag={tag}"));
                Err(corrupt())
            }
        };
        r.pop(ctx);
        res?;
    }
    if !got_data {
        debug("process_data_packets: no data".into());
        return Err(corrupt());
    }
    if need_mdc && !got_mdc && !ctx.use_mdcbuf_filter {
        debug("process_data_packets: got no mdc".into());
        return Err(corrupt());
    }
    Ok(())
}

/// `parse_symenc_data` / `parse_symenc_mdc_data`.
fn parse_symenc(r: &mut Reader, pkt: usize, ctx: &mut Pgp, dst: &mut Vec<u8>, mdc: bool) -> Px<()> {
    if mdc {
        let ver = r.byte(pkt, ctx)?;
        if ver != 1 {
            debug("parse_symenc_mdc_data: pkt ver != 1".into());
            return Err(corrupt());
        }
    }
    let base = r.top();
    let res = (|| {
        let c = Cfb::new(ctx.cipher_algo, &ctx.sess_key, !mdc, None, ctx.ignore_cipher_failure)?;
        filters::decrypt_filter(r, c);
        if mdc {
            filters::mdc_filter(r, ctx);
        }
        let top = filters::prefix_filter(r, ctx)?;
        process_data_packets(r, top, ctx, dst, true, mdc)
    })();
    r.pop_to(base, ctx);
    res
}

/// `pgp_decrypt`.
pub(super) fn decrypt(ctx: &mut Pgp, src: &[u8]) -> Px<Vec<u8>> {
    let mut r = Reader::new(src);
    let mut dst = Vec::new();
    let mut got_key = false;
    let mut got_data = false;
    while let Some((tag, len, ty)) = packet::parse_pkt_hdr(&mut r, 0, false, ctx)? {
        let pkt = packet::pkt_reader(&mut r, len, ty);
        let res: Px<()> = match tag {
            10 => packet::skip_packet(&mut r, pkt, ctx),
            1 => {
                got_key = true;
                super::pubenc::parse_pubenc_sesskey(&mut r, pkt, ctx)
            }
            3 if got_key => {
                debug("pgp_decrypt: using first of several keys".into());
                Err(corrupt())
            }
            3 => {
                got_key = true;
                parse_symenc_sesskey(&mut r, pkt, ctx)
            }
            9 | 18 if !got_key => {
                debug("pgp_decrypt: have data but no key".into());
                Err(corrupt())
            }
            9 if got_data => {
                debug("pgp_decrypt: got second data packet".into());
                Err(corrupt())
            }
            18 if got_data => {
                debug("pgp_decrypt: several data pkts not supported".into());
                Err(corrupt())
            }
            9 | 18 => {
                got_data = true;
                ctx.disable_mdc = tag == 9;
                parse_symenc(&mut r, pkt, ctx, &mut dst, tag == 18)
            }
            _ => {
                debug(format!("pgp_decrypt: unknown tag: 0x{tag:02x}"));
                Err(corrupt())
            }
        };
        r.pop(ctx);
        res?;
    }
    // A failed prefix check is reported only now (timing attacks).
    if !got_data || ctx.corrupt_prefix {
        return Err(corrupt());
    }
    if ctx.unsupported_compr {
        return Err(PxErr::Code(PXE_PGP_UNSUPPORTED_COMPR));
    }
    if ctx.unexpected_binary {
        return Err(PxErr::Code(PXE_PGP_NOT_TEXT));
    }
    Ok(dst)
}
