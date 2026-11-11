//! Postgres's pglz compressor (common/pg_lzcompress.c) with the default strategy, as
//! TOAST uses it: only the compressed length is needed, to lay values out as Postgres
//! does.

const MAX_HISTORY_LISTS: usize = 8192;
const HISTORY_SIZE: usize = 4096;
const MAX_MATCH: usize = 273;
/// PGLZ_strategy_default.
const MIN_INPUT: usize = 32;
const NEED_RATE: usize = 25;
const FIRST_SUCCESS_BY: usize = 1024;
const MATCH_GOOD: usize = 128;
const MATCH_DROP: usize = 10;

#[derive(Clone, Copy, Default)]
struct Entry {
    /// The next entry in this hash's list (0: none).
    next: u16,
    /// The previous one (None: this entry is the list's head).
    prev: Option<u16>,
    hindex: usize,
    pos: usize,
}

struct Hist {
    start: Vec<u16>,
    entries: Vec<Entry>,
    next: usize,
    recycle: bool,
    mask: usize,
}

impl Hist {
    fn idx(&self, s: &[u8], p: usize) -> usize {
        let h = if s.len() - p < 4 {
            s[p] as usize
        } else {
            ((s[p] as usize) << 6)
                ^ ((s[p + 1] as usize) << 4)
                ^ ((s[p + 2] as usize) << 2)
                ^ s[p + 3] as usize
        };
        h & self.mask
    }

    /// pglz_hist_add.
    fn add(&mut self, s: &[u8], p: usize) {
        let hindex = self.idx(s, p);
        let me = self.next;
        if self.recycle {
            let e = self.entries[me];
            match e.prev {
                None => self.start[e.hindex] = e.next,
                Some(pr) => self.entries[pr as usize].next = e.next,
            }
            self.entries[e.next as usize].prev = e.prev;
        }
        let head = self.start[hindex];
        self.entries[me] = Entry {
            next: head,
            prev: None,
            hindex,
            pos: p,
        };
        self.entries[head as usize].prev = Some(me as u16);
        self.start[hindex] = me as u16;
        self.next += 1;
        if self.next > HISTORY_SIZE {
            self.next = 1;
            self.recycle = true;
        }
    }

    /// pglz_find_match: (length, offset) of the best earlier match at `ip`.
    fn find(&self, s: &[u8], ip0: usize) -> Option<(usize, usize)> {
        let end = s.len();
        let mut hent = self.start[self.idx(s, ip0)];
        let (mut len, mut off) = (0usize, 0usize);
        let mut good = MATCH_GOOD;
        while hent != 0 {
            let hp0 = self.entries[hent as usize].pos;
            let thisoff = ip0 - hp0;
            if thisoff >= 0x0fff {
                break;
            }
            let mut thislen = 0;
            let (mut ip, mut hp) = (ip0, hp0);
            if len >= 16 {
                if s[ip0..ip0 + len] == s[hp0..hp0 + len] {
                    thislen = len;
                    ip += len;
                    hp += len;
                    while ip < end && s[ip] == s[hp] && thislen < MAX_MATCH {
                        thislen += 1;
                        ip += 1;
                        hp += 1;
                    }
                }
            } else {
                while ip < end && s[ip] == s[hp] && thislen < MAX_MATCH {
                    thislen += 1;
                    ip += 1;
                    hp += 1;
                }
            }
            if thislen > len {
                len = thislen;
                off = thisoff;
            }
            hent = self.entries[hent as usize].next;
            if hent != 0 {
                if len >= good {
                    break;
                }
                good -= good * MATCH_DROP / 100;
            }
        }
        (len > 2).then_some((len, off))
    }
}

/// The length pglz_compress gives `src` (None: not compressible enough).
pub fn compressed_len(src: &[u8]) -> Option<usize> {
    let slen = src.len();
    if slen < MIN_INPUT || slen > i32::MAX as usize {
        return None;
    }
    let result_max = if slen > i32::MAX as usize / 100 {
        (slen / 100) * (100 - NEED_RATE)
    } else {
        slen * (100 - NEED_RATE) / 100
    };
    let hashsz = match slen {
        0..128 => 512,
        128..256 => 1024,
        256..512 => 2048,
        512..1024 => 4096,
        _ => MAX_HISTORY_LISTS,
    };
    let mut h = Hist {
        start: vec![0; hashsz],
        entries: vec![Entry::default(); HISTORY_SIZE + 1],
        next: 1,
        recycle: false,
        mask: hashsz - 1,
    };
    let mut bp = 0usize;
    let mut ctrl: u8 = 0;
    let mut found = false;
    let mut dp = 0;
    while dp < slen {
        if bp >= result_max {
            return None;
        }
        if !found && bp >= FIRST_SUCCESS_BY {
            return None;
        }
        if ctrl == 0 {
            // A new control byte.
            bp += 1;
            ctrl = 1;
        }
        if let Some((mlen, _)) = h.find(src, dp) {
            ctrl <<= 1;
            bp += if mlen > 17 { 3 } else { 2 };
            for _ in 0..mlen {
                h.add(src, dp);
                dp += 1;
            }
            found = true;
        } else {
            ctrl <<= 1;
            bp += 1;
            h.add(src, dp);
            dp += 1;
        }
    }
    (bp < result_max).then_some(bp)
}

#[cfg(test)]
mod tests {
    use super::compressed_len;

    #[test]
    fn repeats_compress() {
        assert!(compressed_len(&[b'x'; 3000]).is_some_and(|n| n < 100));
        assert_eq!(compressed_len(b"short"), None);
    }
}
