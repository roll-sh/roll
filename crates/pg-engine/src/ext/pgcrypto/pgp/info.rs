//! pgp_key_id() (`pgp-info.c`): the key a key or message is for.

use super::super::px::*;
use super::mbuf::Reader;
use super::pubkey::{ELG_ENCRYPT, RSA_ENCRYPT, RSA_ENCRYPT_SIGN, read_public_key};
use super::{Pgp, packet};

/// `read_pubkey_keyid`: an encryption subkey's id (None for other keys).
fn read_pubkey_keyid(r: &mut Reader, pkt: usize, ctx: &mut Pgp) -> Px<Option<[u8; 8]>> {
    let pk = read_public_key(r, pkt, ctx)?;
    packet::skip_packet(r, pkt, ctx)?;
    Ok(matches!(pk.algo, ELG_ENCRYPT | RSA_ENCRYPT | RSA_ENCRYPT_SIGN).then_some(pk.key_id))
}

/// `read_pubenc_keyid`.
fn read_pubenc_keyid(r: &mut Reader, pkt: usize, ctx: &mut Pgp) -> Px<[u8; 8]> {
    let ver = r.byte(pkt, ctx)?;
    if ver != 3 {
        // (PostgreSQL returns -1 here, which no error code has.)
        return Err(PxErr::Code(-1));
    }
    let id: [u8; 8] = r.read_fixed(pkt, 8, ctx)?.try_into().expect("8 bytes");
    packet::skip_packet(r, pkt, ctx)?;
    Ok(id)
}

/// `pgp_get_keyid`.
pub(super) fn key_id(data: &[u8]) -> Px<String> {
    let mut ctx = Pgp::default();
    let ctx = &mut ctx;
    let mut r = Reader::new(data);
    let (mut got_pub_key, mut got_symenc_key, mut got_pubenc_key) = (0, 0, 0);
    let mut got_main_key = false;
    let mut keyid = [0u8; 8];
    while let Some((tag, len, ty)) = packet::parse_pkt_hdr(&mut r, 0, false, ctx)? {
        let pkt = packet::pkt_reader(&mut r, len, ty);
        let mut got_data = false;
        let res: Px<()> = match tag {
            5 | 6 if got_main_key => Err(PxErr::Code(PXE_PGP_MULTIPLE_KEYS)),
            5 | 6 => {
                got_main_key = true;
                packet::skip_packet(&mut r, pkt, ctx)
            }
            7 | 14 => read_pubkey_keyid(&mut r, pkt, ctx).map(|id| {
                if let Some(id) = id {
                    keyid = id;
                    got_pub_key += 1;
                }
            }),
            1 => {
                got_pubenc_key += 1;
                read_pubenc_keyid(&mut r, pkt, ctx).map(|id| keyid = id)
            }
            9 | 18 => {
                got_data = true;
                Ok(())
            }
            3 => {
                got_symenc_key += 1;
                packet::skip_packet(&mut r, pkt, ctx)
            }
            2 | 10 | 12 | 13 | 17 | 61 => packet::skip_packet(&mut r, pkt, ctx),
            _ => Err(PxErr::Code(PXE_PGP_CORRUPT_DATA)),
        };
        r.pop(ctx);
        res?;
        if got_data {
            break;
        }
    }
    let mut res = 0;
    if got_pub_key > 0 && got_pubenc_key > 0 {
        res = PXE_PGP_CORRUPT_DATA;
    }
    if got_pub_key > 1 || got_pubenc_key > 1 {
        res = PXE_PGP_MULTIPLE_KEYS;
    }
    if res < 0 {
        return Err(PxErr::Code(res));
    }
    if got_pubenc_key > 0 || got_pub_key > 0 {
        if keyid == [0u8; 8] {
            return Ok("ANYKEY".into());
        }
        return Ok(keyid.iter().map(|b| format!("{b:02X}")).collect());
    }
    if got_symenc_key > 0 {
        return Ok("SYMKEY".into());
    }
    Err(PxErr::Code(PXE_PGP_NO_USABLE_KEY))
}
