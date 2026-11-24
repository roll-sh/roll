//! LZ4 block decompression as Lucene writes it (`org.apache.lucene.util.compress.LZ4`):
//! token, literals, a two-byte little-endian match offset, match length; output may start after
//! a dictionary already in the buffer, which matches can reach back into.

use crate::input::Input;
use crate::{Result, corrupt};

const MIN_MATCH: usize = 4;

/// Decompresses exactly `out.len() - start` bytes into `out[start..]`, `out[..start]` being
/// the dictionary. Every literal and match is checked against `out`, and the stream must end
/// exactly at its end.
pub fn decompress(inp: &mut Input, out: &mut [u8], start: usize) -> Result<()> {
    let end = out.len();
    let mut at = start;
    // As Lucene's do-while: even an empty output has its token (no literals).
    loop {
        let token = inp.u8()? as usize;
        let mut literal = token >> 4;
        if literal == 0x0F {
            loop {
                let b = inp.u8()? as usize;
                literal += b;
                if b != 0xFF {
                    break;
                }
            }
        }
        if literal > end - at {
            return corrupt("LZ4 literals run past the block");
        }
        out[at..at + literal].copy_from_slice(inp.bytes(literal)?);
        at += literal;
        if at >= end {
            return Ok(());
        }
        let dec = inp.u16()? as usize;
        if dec == 0 {
            return corrupt("LZ4 offset 0 is invalid");
        }
        if dec > at {
            return corrupt("LZ4 match before the start of the buffer");
        }
        let mut len = token & 0x0F;
        if len == 0x0F {
            loop {
                let b = inp.u8()? as usize;
                len += b;
                if b != 0xFF {
                    break;
                }
            }
        }
        len += MIN_MATCH;
        if len > end - at {
            return corrupt("LZ4 match runs past the block");
        }
        let from = at - dec;
        if dec >= len {
            out.copy_within(from..from + len, at);
        } else {
            for i in 0..len {
                out[at + i] = out[from + i];
            }
        }
        at += len;
        if at >= end {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literals_matches_and_bounds() {
        // "abcabcabca": 3 literals, then a 7-byte overlapping match at distance 3.
        let data = [0x33, b'a', b'b', b'c', 3, 0];
        let mut out = vec![0u8; 10];
        decompress(&mut Input::new(&data), &mut out, 0).unwrap();
        assert_eq!(&out, b"abcabcabca");
        // The same match reaching into a dictionary.
        let mut out = b"abc".to_vec();
        out.resize(10, 0);
        decompress(&mut Input::new(&[0x03, 3, 0]), &mut out, 3).unwrap();
        assert_eq!(&out, b"abcabcabca");
        // An empty block still has its token.
        let mut inp = Input::new(&[0x00]);
        decompress(&mut inp, &mut [], 0).unwrap();
        assert_eq!(inp.remaining(), 0);
        // A match before the buffer, a literal past it, a truncated stream.
        assert!(decompress(&mut Input::new(&[0x03, 9, 0]), &mut [0; 7], 0).is_err());
        assert!(decompress(&mut Input::new(&[0x50, 1, 2, 3, 4, 5]), &mut [0; 3], 0).is_err());
        assert!(decompress(&mut Input::new(&[0x33, b'a']), &mut [0; 10], 0).is_err());
    }
}
