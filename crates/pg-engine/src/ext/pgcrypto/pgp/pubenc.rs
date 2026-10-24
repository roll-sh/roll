//! Public-key encrypted session keys (`pgp-pubenc.c`, `pgp-pubdec.c`).

use super::super::px::*;
use super::mbuf::{Reader, Writer};
use super::mpi::{self, Mpi};
use super::packet;
use super::pubkey::{ELG_ENCRYPT, RSA_ENCRYPT, RSA_ENCRYPT_SIGN};
use super::{Pgp, debug, random};

fn code(c: i32) -> PxErr {
    PxErr::Code(c)
}

/// `create_secmsg`: the algorithm, session key and checksum, padded per EME-PKCS1-v1_5 to
/// `full_bytes`.
fn create_secmsg(ctx: &Pgp, full_bytes: usize) -> Px<Mpi> {
    let klen = ctx.sess_key.len();
    let cksum: u32 = ctx.sess_key.iter().map(|&b| b as u32).sum();
    let mut msg = vec![ctx.cipher_algo as u8];
    msg.extend_from_slice(&ctx.sess_key);
    msg.extend([(cksum >> 8) as u8, cksum as u8]);
    let pad_len = full_bytes as isize - 2 - (klen + 3) as isize;
    if pad_len < 8 {
        return Err(code(PXE_BUG));
    }
    let mut buf = vec![0u8; 1 + pad_len as usize];
    buf[0] = 0x02;
    random(&mut buf[1..]);
    // The padding must not contain zero bytes.
    for b in buf[1..].iter_mut() {
        while *b == 0 {
            let mut x = [0u8; 1];
            random(&mut x);
            *b = x[0];
        }
    }
    buf.push(0);
    buf.extend_from_slice(&msg);
    Ok(Mpi::create(&buf, full_bytes * 8 - 6))
}

/// `pgp_write_pubenc_sesskey`.
pub(super) fn write_pubenc_sesskey(ctx: &mut Pgp, w: &mut Writer) -> Px<()> {
    let Some(pk) = ctx.pub_key.clone() else {
        debug("no pubkey?\n".into());
        return Err(code(PXE_BUG));
    };
    let pkt = super::encrypt::pkt_writer(w, 1)?;
    w.write(pkt, &[3])?;
    w.write(pkt, &pk.key_id)?;
    w.write(pkt, &[pk.algo])?;
    match pk.algo {
        ELG_ENCRYPT => {
            let (p, g, y) = (&pk.public[0], &pk.public[1], &pk.public[2]);
            let m = create_secmsg(ctx, p.bytes().saturating_sub(1))?;
            let (c1, c2) = mpi::elgamal_encrypt(p, g, y, &m)?;
            c1.write(w, pkt)?;
            c2.write(w, pkt)?;
        }
        RSA_ENCRYPT | RSA_ENCRYPT_SIGN => {
            let (n, e) = (&pk.public[0], &pk.public[1]);
            let m = create_secmsg(ctx, n.bytes().saturating_sub(1))?;
            mpi::rsa_encrypt(n, e, &m)?.write(w, pkt)?;
        }
        _ => {}
    }
    w.flush(pkt)?;
    w.pop();
    Ok(())
}

/// `check_eme_pkcs1_v15`: where the message starts after the padding.
fn check_eme(data: &[u8]) -> Option<usize> {
    if data.len() < 10 || data[0] != 2 {
        return None;
    }
    let zero = data[1..].iter().position(|&b| b == 0)? + 1;
    (zero > 8).then_some(zero + 1)
}

/// `control_cksum`.
fn control_cksum(msg: &[u8]) -> Px<()> {
    if msg.len() < 3 {
        return Err(code(PXE_PGP_WRONG_KEY));
    }
    let n = msg.len();
    let mine = msg[1..n - 2].iter().map(|&b| b as u32).sum::<u32>() & 0xFFFF;
    let got = ((msg[n - 2] as u32) << 8) + msg[n - 1] as u32;
    if mine != got {
        debug("pubenc cksum failed".into());
        return Err(code(PXE_PGP_WRONG_KEY));
    }
    Ok(())
}

/// `pgp_parse_pubenc_sesskey`.
pub(super) fn parse_pubenc_sesskey(r: &mut Reader, pkt: usize, ctx: &mut Pgp) -> Px<()> {
    let Some(pk) = ctx.pub_key.clone() else {
        debug("no pubkey?".into());
        return Err(code(PXE_BUG));
    };
    let ver = r.byte(pkt, ctx)?;
    if ver != 3 {
        debug(format!("unknown pubenc_sesskey pkt ver={ver}"));
        return Err(code(PXE_PGP_CORRUPT_DATA));
    }
    let key_id = r.read_fixed(pkt, 8, ctx)?;
    if key_id != [0u8; 8] && key_id != pk.key_id {
        debug("key_id's does not match".into());
        return Err(code(PXE_PGP_WRONG_KEY));
    }
    let algo = r.byte(pkt, ctx)?;
    let m = match algo {
        ELG_ENCRYPT => {
            if pk.algo != ELG_ENCRYPT {
                return Err(code(PXE_PGP_WRONG_KEY));
            }
            let c1 = Mpi::read(r, pkt, ctx)?;
            let c2 = Mpi::read(r, pkt, ctx)?;
            mpi::elgamal_decrypt(&pk.public[0], &pk.secret[0], &c1, &c2)?
        }
        RSA_ENCRYPT | RSA_ENCRYPT_SIGN => {
            if pk.algo != RSA_ENCRYPT && pk.algo != RSA_ENCRYPT_SIGN {
                return Err(code(PXE_PGP_WRONG_KEY));
            }
            let c = Mpi::read(r, pkt, ctx)?;
            mpi::rsa_decrypt(&pk.public[0], &pk.secret[0], &c)?
        }
        _ => return Err(code(PXE_PGP_UNKNOWN_PUBALGO)),
    };
    let Some(start) = check_eme(&m.data) else {
        debug("check_eme_pkcs1_v15 failed".into());
        return Err(code(PXE_PGP_WRONG_KEY));
    };
    let msg = &m.data[start..];
    control_cksum(msg)?;
    let sess_key_len = msg.len() - 3;
    if sess_key_len > 32 {
        debug(format!("incorrect session key length={sess_key_len}"));
        return Err(code(PXE_PGP_KEY_TOO_BIG));
    }
    ctx.cipher_algo = msg[0] as i32;
    ctx.sess_key = msg[1..1 + sess_key_len].to_vec();
    packet::expect_packet_end(r, pkt, ctx)
}
