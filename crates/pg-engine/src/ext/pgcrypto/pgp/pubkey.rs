//! Public and secret keys (`pgp-pubkey.c`) and public-key encrypted session keys
//! (`pgp-pubenc.c`, `pgp-pubdec.c`).

use super::super::px::*;
use super::cfb::{self, Cfb};
use super::filters;
use super::mbuf::Reader;
use super::mpi::Mpi;
use super::packet;
use super::s2k::{S2k, sha1};
use super::{Pgp, debug};

pub(super) const RSA_ENCRYPT_SIGN: u8 = 1;
pub(super) const RSA_ENCRYPT: u8 = 2;
pub(super) const RSA_SIGN: u8 = 3;
pub(super) const ELG_ENCRYPT: u8 = 16;
pub(super) const DSA_SIGN: u8 = 17;

fn code(c: i32) -> PxErr {
    PxErr::Code(c)
}

/// `PGP_PubKey`: the public MPIs (Elgamal p g y, RSA n e, DSA p q g y) and the secret ones
/// (Elgamal x, RSA d p q u, DSA x).
#[derive(Clone, Debug, Default)]
pub(super) struct PubKey {
    pub ver: u8,
    pub time: Vec<u8>,
    pub algo: u8,
    pub public: Vec<Mpi>,
    pub secret: Vec<Mpi>,
    pub key_id: [u8; 8],
    pub can_encrypt: bool,
}

/// `calc_key_id`: the low 8 bytes of the V4 fingerprint.
fn calc_key_id(pk: &mut PubKey) {
    let len = 6 + pk.public.iter().map(|m| 2 + m.bytes()).sum::<usize>();
    let mut md = sha1();
    md.update(&[0x99, (len >> 8) as u8, len as u8]);
    md.update(&[pk.ver]);
    md.update(&pk.time);
    md.update(&[pk.algo]);
    for m in &pk.public {
        m.hash(&mut md);
    }
    let h = md.finish();
    pk.key_id.copy_from_slice(&h[12..20]);
}

/// `_pgp_read_public_key`.
pub(super) fn read_public_key(r: &mut Reader, level: usize, ctx: &mut Pgp) -> Px<PubKey> {
    let mut pk = PubKey {
        ver: r.byte(level, ctx)?,
        ..Default::default()
    };
    if pk.ver != 4 {
        return Err(code(PXE_PGP_NOT_V4_KEYPKT));
    }
    pk.time = r.read_fixed(level, 4, ctx)?;
    pk.algo = r.byte(level, ctx)?;
    let n = match pk.algo {
        DSA_SIGN => 4,
        RSA_SIGN | RSA_ENCRYPT | RSA_ENCRYPT_SIGN => 2,
        ELG_ENCRYPT => 3,
        a => {
            debug(format!("unknown public algo: {a}"));
            return Err(code(PXE_PGP_UNKNOWN_PUBALGO));
        }
    };
    for _ in 0..n {
        pk.public.push(Mpi::read(r, level, ctx)?);
    }
    calc_key_id(&mut pk);
    pk.can_encrypt = matches!(pk.algo, RSA_ENCRYPT | RSA_ENCRYPT_SIGN | ELG_ENCRYPT);
    Ok(pk)
}

/// `check_key_sha1` / `check_key_cksum`.
fn check_key(r: &mut Reader, level: usize, pk: &PubKey, sha: bool, ctx: &mut Pgp) -> Px<()> {
    if sha {
        let got = r.read_fixed(level, 20, ctx)?;
        let mut md = sha1();
        for m in &pk.secret {
            m.hash(&mut md);
        }
        if md.finish() != got {
            debug("key sha1 check failed".into());
            return Err(code(PXE_PGP_KEYPKT_CORRUPT));
        }
        return Ok(());
    }
    let b = r.read_fixed(level, 2, ctx)?;
    let got = ((b[0] as u32) << 8) + b[1] as u32;
    let mine = pk.secret.iter().fold(0, |c, m| m.cksum(c));
    if mine != got {
        debug("key cksum check failed".into());
        return Err(code(PXE_PGP_KEYPKT_CORRUPT));
    }
    Ok(())
}

/// `process_secret_key`: the public part, then the secret MPIs (decrypted with `key` when
/// they are protected) and their checksum.
fn process_secret_key(r: &mut Reader, pkt: usize, key: Option<&[u8]>, ctx: &mut Pgp) -> Px<PubKey> {
    let mut pk = read_public_key(r, pkt, ctx)?;
    let hide = r.byte(pkt, ctx)?;
    let level = match hide {
        254 | 255 => {
            let key = key.ok_or(code(PXE_PGP_NEED_SECRET_PSW))?;
            let cipher = r.byte(pkt, ctx)? as i32;
            let mut s2k = S2k::read(r, pkt, ctx)?;
            s2k.process(cipher, key)?;
            let bs = cfb::block_size(cipher);
            if bs == 0 {
                debug(format!("unknown cipher algo={cipher}"));
                return Err(code(PXE_PGP_UNSUPPORTED_CIPHER));
            }
            let iv = r.read_fixed(pkt, bs, ctx)?;
            let c = Cfb::new(cipher, &s2k.key, false, Some(&iv), false)?;
            filters::decrypt_filter(r, c)
        }
        0 => pkt,
        _ => {
            debug("unknown hide type".into());
            return Err(code(PXE_PGP_KEYPKT_CORRUPT));
        }
    };
    let res = (|| {
        let n = match pk.algo {
            RSA_SIGN | RSA_ENCRYPT | RSA_ENCRYPT_SIGN => 4,
            _ => 1,
        };
        for _ in 0..n {
            pk.secret.push(Mpi::read(r, level, ctx)?);
        }
        check_key(r, level, &pk, hide == 254, ctx)?;
        packet::expect_packet_end(r, level, ctx)
    })();
    r.pop_to(pkt, ctx);
    res.map(|_| pk)
}

/// `internal_read_key`: the one encryption subkey of a key (`pubtype` 0: public, 1: secret).
fn read_key(r: &mut Reader, psw: Option<&[u8]>, pubtype: i32, ctx: &mut Pgp) -> Px<PubKey> {
    let mut enc_key: Option<PubKey> = None;
    let mut got_main_key = false;
    while let Some((tag, len, ty)) = packet::parse_pkt_hdr(r, 0, false, ctx)? {
        let pkt = packet::pkt_reader(r, len, ty);
        let res: Px<Option<PubKey>> = match tag {
            6 | 5 if got_main_key => Err(code(PXE_PGP_MULTIPLE_KEYS)),
            6 | 5 => {
                got_main_key = true;
                packet::skip_packet(r, pkt, ctx).map(|_| None)
            }
            14 if pubtype != 0 => Err(code(PXE_PGP_EXPECT_SECRET_KEY)),
            14 => read_public_key(r, pkt, ctx).map(Some),
            7 if pubtype != 1 => Err(code(PXE_PGP_EXPECT_PUBLIC_KEY)),
            7 => process_secret_key(r, pkt, psw, ctx).map(Some),
            2 | 10 | 12 | 13 | 17 | 61 => packet::skip_packet(r, pkt, ctx).map(|_| None),
            _ => {
                debug(format!("unknown/unexpected packet: {tag}"));
                Err(code(PXE_PGP_UNEXPECTED_PKT))
            }
        };
        r.pop(ctx);
        if let Some(pk) = res?
            && pk.can_encrypt
        {
            if enc_key.is_some() {
                return Err(code(PXE_PGP_MULTIPLE_SUBKEYS));
            }
            enc_key = Some(pk);
        }
    }
    enc_key.ok_or(code(PXE_PGP_NO_USABLE_KEY))
}

/// `pgp_set_pubkey`.
pub(super) fn set_pubkey(ctx: &mut Pgp, key: &[u8], psw: Option<&[u8]>, pubtype: i32) -> Px<()> {
    let mut r = Reader::new(key);
    let pk = read_key(&mut r, psw, pubtype, ctx)?;
    ctx.pub_key = Some(pk);
    Ok(())
}
