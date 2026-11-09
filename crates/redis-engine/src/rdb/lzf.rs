//! LZF (liblzf 3.6 as built by Redis: `HLOG` 16, `VERY_FAST`), byte-compatible with
//! `lzf_compress` / `lzf_decompress`.
//!
//! liblzf leaves its hash table uninitialised between calls; we start every call from an empty
//! table (what a fresh process does) without clearing 256 KB each time: entries carry the offset
//! of the call that wrote them and older ones read as empty.

const HLOG: u32 = 16;
const HSIZE: usize = 1 << HLOG;
const MAX_LIT: usize = 1 << 5;
const MAX_OFF: usize = 1 << 13;
const MAX_REF: usize = (1 << 8) + (1 << 3);

/// Compressor state (the hash table), reusable across calls.
pub struct Lzf {
    htab: Box<[u32]>,
    base: u32,
}

impl Default for Lzf {
    fn default() -> Self {
        Self::new()
    }
}

#[inline]
fn idx(h: u32) -> usize {
    ((h >> (3 * 8 - HLOG)).wrapping_sub(h.wrapping_mul(5)) as usize) & (HSIZE - 1)
}

impl Lzf {
    pub fn new() -> Self {
        Lzf { htab: vec![0; HSIZE].into_boxed_slice(), base: 0 }
    }

    /// `lzf_compress(input, out_len)`: the compressed bytes in `out`, or false when they don't
    /// fit in `out_len` bytes (`out` is then unspecified).
    pub fn compress(&mut self, input: &[u8], out_len: usize, out: &mut Vec<u8>) -> bool {
        let in_end = input.len();
        if in_end == 0 || out_len == 0 {
            return false;
        }
        if self.base as u64 + in_end as u64 + 2 >= u32::MAX as u64 {
            self.htab.fill(0);
            self.base = 0;
        }
        let base = self.base;
        self.base += in_end as u32 + 1;
        let htab = &mut self.htab;
        let at = |p: usize| input.get(p).copied().unwrap_or(0) as u32;
        let out_end = out_len;
        out.clear();
        out.resize(out_len, 0);
        let o = &mut out[..];
        let mut ip = 0usize;
        let mut op = 1usize; // start run
        let mut lit = 0usize;
        let mut hval = (at(0) << 8) | at(1);
        while ip + 2 < in_end {
            hval = (hval << 8) | input[ip + 2] as u32;
            let slot = idx(hval);
            let e = htab[slot];
            htab[slot] = base + 1 + ip as u32;
            let r = if e > base { Some((e - base - 1) as usize) } else { None };
            let found = r.filter(|&r| {
                r < ip && ip - r - 1 < MAX_OFF && r > 0 && input[r + 2] == input[ip + 2] && input[r] == input[ip] && input[r + 1] == input[ip + 1]
            });
            if let Some(r) = found {
                let off = ip - r - 1;
                let mut len = 2usize;
                let maxlen = (in_end - ip - len).min(MAX_REF);
                if op + 3 + 1 >= out_end && op - (lit == 0) as usize + 3 + 1 >= out_end {
                    return false;
                }
                o[op - lit - 1] = (lit as u8).wrapping_sub(1);
                op -= (lit == 0) as usize;
                'scan: {
                    if maxlen > 16 {
                        for _ in 0..16 {
                            len += 1;
                            if input[r + len] != input[ip + len] {
                                break 'scan;
                            }
                        }
                    }
                    loop {
                        len += 1;
                        if !(len < maxlen && input[r + len] == input[ip + len]) {
                            break;
                        }
                    }
                }
                len -= 2;
                ip += 1;
                if len < 7 {
                    o[op] = ((off >> 8) + (len << 5)) as u8;
                    op += 1;
                } else {
                    o[op] = ((off >> 8) + (7 << 5)) as u8;
                    o[op + 1] = (len - 7) as u8;
                    op += 2;
                }
                o[op] = off as u8;
                op += 1;
                lit = 0;
                op += 1;
                ip += len + 1;
                if ip + 2 >= in_end {
                    break;
                }
                ip -= 2;
                hval = (at(ip) << 8) | at(ip + 1);
                hval = (hval << 8) | input[ip + 2] as u32;
                htab[idx(hval)] = base + 1 + ip as u32;
                ip += 1;
                hval = (hval << 8) | input[ip + 2] as u32;
                htab[idx(hval)] = base + 1 + ip as u32;
                ip += 1;
            } else {
                if op >= out_end {
                    return false;
                }
                lit += 1;
                o[op] = input[ip];
                op += 1;
                ip += 1;
                if lit == MAX_LIT {
                    o[op - lit - 1] = (lit - 1) as u8;
                    lit = 0;
                    op += 1;
                }
            }
        }
        if op + 3 > out_end {
            return false;
        }
        while ip < in_end {
            lit += 1;
            o[op] = input[ip];
            op += 1;
            ip += 1;
            if lit == MAX_LIT {
                o[op - lit - 1] = (lit - 1) as u8;
                lit = 0;
                op += 1;
            }
        }
        o[op - lit - 1] = (lit as u8).wrapping_sub(1);
        op -= (lit == 0) as usize;
        out.truncate(op);
        true
    }
}

thread_local! {
    static STATE: std::cell::RefCell<Lzf> = std::cell::RefCell::new(Lzf::new());
}

/// [`Lzf::compress`] with a thread-local compressor.
pub fn compress(input: &[u8], out_len: usize, out: &mut Vec<u8>) -> bool {
    STATE.with(|s| s.borrow_mut().compress(input, out_len, out))
}

/// `lzf_decompress`: `input` must expand to exactly `out_len` bytes.
pub fn decompress(input: &[u8], out_len: usize, out: &mut Vec<u8>) -> bool {
    out.clear();
    out.reserve_exact(out_len);
    let mut ip = 0;
    while ip < input.len() {
        let ctrl = input[ip] as usize;
        ip += 1;
        if ctrl < 1 << 5 {
            let n = ctrl + 1;
            if out.len() + n > out_len || ip + n > input.len() {
                return false;
            }
            out.extend_from_slice(&input[ip..ip + n]);
            ip += n;
        } else {
            let mut len = ctrl >> 5;
            if ip >= input.len() {
                return false;
            }
            if len == 7 {
                len += input[ip] as usize;
                ip += 1;
                if ip >= input.len() {
                    return false;
                }
            }
            let back = ((ctrl & 0x1f) << 8) + input[ip] as usize + 1;
            ip += 1;
            let len = len + 2;
            if out.len() + len > out_len || back > out.len() {
                return false;
            }
            let start = out.len() - back;
            if back >= len {
                out.extend_from_within(start..start + len);
            } else {
                for i in 0..len {
                    let b = out[start + i];
                    out.push(b);
                }
            }
        }
    }
    out.len() == out_len
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(data: &[u8]) -> Option<Vec<u8>> {
        let mut c = Vec::new();
        if !compress(data, data.len().saturating_sub(4), &mut c) {
            return None;
        }
        let mut d = Vec::new();
        assert!(decompress(&c, data.len(), &mut d));
        assert_eq!(d, data);
        Some(c)
    }

    #[test]
    fn liblzf_vectors() {
        // Outputs of Redis' lzf_compress (DUMP of these strings, LZF blob part).
        assert_eq!(roundtrip(&[b'a'; 30]).unwrap(), hex("016161e01100016161").unwrap());
        let text = b"hello hello hello hello hello world world world world".repeat(10);
        assert_eq!(roundtrip(&text).unwrap(), hex("0668656c6c6f2068e00e0504776f726c64e00905e00f2e8017e0ff34e1ac08016c64").unwrap());
        assert!(roundtrip(b"abcdefghijklmnopqrstuvwxyz0123456789").is_none());
        let mut rng = crate::util::Rng::seeded(7);
        for n in [0usize, 1, 2, 3, 21, 100, 5000, 70000] {
            let v: Vec<u8> = (0..n).map(|_| b"abcab"[rng.below(5)]).collect();
            roundtrip(&v);
        }
    }

    #[test]
    fn rejects_garbage() {
        let mut out = Vec::new();
        assert!(!decompress(&[0x05, 1, 2], 6, &mut out));
        assert!(!decompress(&[0x20, 0x00], 3, &mut out));
        assert!(!decompress(&[0x00, b'a', 0xe0], 300, &mut out));
        assert!(decompress(&[0x00, b'a', 0x20, 0x00], 4, &mut out));
        assert_eq!(out, b"aaaa");
    }

    fn hex(s: &str) -> Option<Vec<u8>> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok()).collect()
    }
}
