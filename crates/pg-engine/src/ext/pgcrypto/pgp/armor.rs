//! ASCII armor (`pgp-armor.c`): base64 with CRC-24, the BEGIN / END lines and the armor
//! headers.

use super::super::px::{PXE_PGP_CORRUPT_ARMOR, Px, PxErr, elog};

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// `pg_base64_encode`: lines of 76 characters.
fn b64_encode(src: &[u8], dst: &mut Vec<u8>) {
    let start = dst.len();
    let mut lend = start + 76;
    let mut pos: i32 = 2;
    let mut buf: u32 = 0;
    for &s in src {
        buf |= (s as u32) << (pos << 3);
        pos -= 1;
        if pos < 0 {
            for sh in [18, 12, 6, 0] {
                dst.push(B64[((buf >> sh) & 0x3f) as usize]);
            }
            pos = 2;
            buf = 0;
        }
        if dst.len() >= lend {
            dst.push(b'\n');
            lend = dst.len() + 76;
        }
    }
    if pos != 2 {
        dst.push(B64[((buf >> 18) & 0x3f) as usize]);
        dst.push(B64[((buf >> 12) & 0x3f) as usize]);
        dst.push(if pos == 0 { B64[((buf >> 6) & 0x3f) as usize] } else { b'=' });
        dst.push(b'=');
    }
}

/// `pg_base64_decode`.
fn b64_decode(src: &[u8]) -> Px<Vec<u8>> {
    let mut out = Vec::new();
    let (mut buf, mut pos, mut end) = (0u32, 0, 0);
    for &c in src {
        let b = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => {
                if end == 0 {
                    end = match pos {
                        2 => 1,
                        3 => 2,
                        _ => return Err(PxErr::Code(PXE_PGP_CORRUPT_ARMOR)),
                    };
                }
                0
            }
            b' ' | b'\t' | b'\n' | b'\r' => continue,
            _ => return Err(PxErr::Code(PXE_PGP_CORRUPT_ARMOR)),
        };
        buf = (buf << 6) + b as u32;
        pos += 1;
        if pos == 4 {
            out.push((buf >> 16) as u8);
            if end == 0 || end > 1 {
                out.push((buf >> 8) as u8);
            }
            if end == 0 || end > 2 {
                out.push(buf as u8);
            }
            buf = 0;
            pos = 0;
        }
    }
    if pos != 0 {
        return Err(PxErr::Code(PXE_PGP_CORRUPT_ARMOR));
    }
    Ok(out)
}

/// CRC-24 (RFC 2440).
fn crc24(data: &[u8]) -> u32 {
    let mut crc: u32 = 0x00b7_04ce;
    for &d in data {
        crc ^= (d as u32) << 16;
        for _ in 0..8 {
            crc <<= 1;
            if crc & 0x100_0000 != 0 {
                crc ^= 0x0186_4cfb;
            }
        }
    }
    crc & 0xff_ffff
}

/// `pgp_armor_encode`.
pub(super) fn encode(src: &[u8], headers: &[(String, String)]) -> Vec<u8> {
    let crc = crc24(src);
    let mut dst = b"-----BEGIN PGP MESSAGE-----\n".to_vec();
    for (k, v) in headers {
        dst.extend_from_slice(format!("{k}: {v}\n").as_bytes());
    }
    dst.push(b'\n');
    b64_encode(src, &mut dst);
    if dst.last() != Some(&b'\n') {
        dst.push(b'\n');
    }
    dst.push(b'=');
    for sh in [18, 12, 6, 0] {
        dst.push(B64[((crc >> sh) & 0x3f) as usize]);
    }
    dst.extend_from_slice(b"\n-----END PGP MESSAGE-----\n");
    dst
}

fn find(data: &[u8], from: usize, s: &[u8]) -> Option<usize> {
    data.get(from..)?.windows(s.len()).position(|w| w == s).map(|p| p + from)
}

/// `find_header`: where the BEGIN (or END) line starts and its length.
fn find_header(data: &[u8], from: usize, is_end: bool) -> Px<(usize, usize)> {
    let sep: &[u8] = if is_end { b"-----END" } else { b"-----BEGIN" };
    let bad = || PxErr::Code(PXE_PGP_CORRUPT_ARMOR);
    let mut p = from;
    let start = loop {
        let at = find(data, p, sep).ok_or_else(bad)?;
        if at == from || data[at - 1] == b'\n' {
            break at;
        }
        p = at + sep.len();
    };
    let mut p = start + sep.len();
    while p < data.len() && data[p] != b'-' {
        if data[p] < b' ' {
            return Err(bad());
        }
        p += 1;
    }
    if data.len() - p < 5 || &data[p..p + 5] != b"-----" {
        return Err(bad());
    }
    p += 5;
    if p < data.len() {
        if data[p] != b'\n' && data[p] != b'\r' {
            return Err(bad());
        }
        if data[p] == b'\r' {
            p += 1;
        }
        if p < data.len() && data[p] == b'\n' {
            p += 1;
        }
    }
    Ok((start, p - start))
}

/// The armor's body: after the header lines (each up to an empty line), and where the
/// END line starts.
fn body(src: &[u8]) -> Px<(usize, usize, usize)> {
    let bad = || PxErr::Code(PXE_PGP_CORRUPT_ARMOR);
    let (s, l) = find_header(src, 0, false)?;
    let armor_start = s + l;
    let (armor_end, _) = find_header(src, armor_start, true)?;
    let mut p = armor_start;
    while p < armor_end && src[p] != b'\n' && src[p] != b'\r' {
        p += src[p..armor_end].iter().position(|&c| c == b'\n').ok_or_else(bad)? + 1;
    }
    Ok((armor_start, p, armor_end))
}

/// `pgp_armor_decode`.
pub(super) fn decode(src: &[u8]) -> Px<Vec<u8>> {
    let bad = || PxErr::Code(PXE_PGP_CORRUPT_ARMOR);
    let (_, base64_start, armor_end) = body(src)?;
    let eq = (base64_start..=armor_end).rev().find(|&p| src[p] == b'=').ok_or_else(bad)?;
    let crc_bytes = b64_decode(&src[eq + 1..(eq + 5).min(src.len())])?;
    if crc_bytes.len() != 3 {
        return Err(bad());
    }
    let crc = ((crc_bytes[0] as u32) << 16) | ((crc_bytes[1] as u32) << 8) | crc_bytes[2] as u32;
    // (base64_end is the byte before the '='.)
    let data = b64_decode(&src[base64_start..eq.saturating_sub(1).max(base64_start)])?;
    if crc24(&data) != crc {
        return Err(bad());
    }
    Ok(data)
}

/// `pgp_extract_armor_headers`.
pub(super) fn headers(src: &[u8]) -> Px<Vec<(Vec<u8>, Vec<u8>)>> {
    let (armor_start, base64_start, _) = body(src)?;
    let lines = src[armor_start..base64_start].iter().filter(|&&c| c == b'\n').count();
    let mut out = Vec::new();
    let mut rest = &src[armor_start..base64_start];
    while let Some(eol) = rest.iter().position(|&c| c == b'\n') {
        let mut line = &rest[..eol];
        rest = &rest[eol + 1..];
        if line.last() == Some(&b'\r') {
            line = &line[..line.len() - 1];
        }
        let colon = line.windows(2).position(|w| w == b": ").ok_or(PxErr::Code(PXE_PGP_CORRUPT_ARMOR))?;
        if out.len() >= lines {
            return Err(elog("unexpected number of armor header lines"));
        }
        out.push((line[..colon].to_vec(), line[colon + 2..].to_vec()));
    }
    if out.len() != lines {
        return Err(elog("unexpected number of armor header lines"));
    }
    Ok(out)
}
