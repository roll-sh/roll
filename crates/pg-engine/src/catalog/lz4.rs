//! liblz4's block compressor (LZ4_compress_default: acceleration 1, a 4096-entry hash
//! table), as TOAST's lz4 compression calls it: only the compressed length is needed, to
//! lay values out as Postgres does.

const MINMATCH: usize = 4;
const LASTLITERALS: usize = 5;
const MFLIMIT: usize = 12;
const MIN_LENGTH: usize = MFLIMIT + 1;
const SKIP_TRIGGER: u32 = 6;
const HASHLOG: u32 = 12;
const RUN_MASK: usize = 15;
const ML_MASK: usize = 15;
/// LZ4_64Klimit: below it the table holds 16-bit positions (byU16).
const LIMIT_64K: usize = 65536 + (MFLIMIT - 1);
const DISTANCE_MAX: usize = 65535;

fn read32(s: &[u8], p: usize) -> u32 {
    u32::from_le_bytes([s[p], s[p + 1], s[p + 2], s[p + 3]])
}

fn read64(s: &[u8], p: usize) -> u64 {
    let mut b = [0u8; 8];
    let n = (s.len() - p).min(8);
    b[..n].copy_from_slice(&s[p..p + n]);
    u64::from_le_bytes(b)
}

/// The length of a literal run's or match's extra length bytes.
fn extra(len: usize, mask: usize) -> usize {
    if len >= mask { (len - mask) / 255 + 1 } else { 0 }
}

/// LZ4_compress_default's output length for `src` (None: it doesn't fit the bound,
/// which can't happen; kept for symmetry with pglz).
pub fn compressed_len(src: &[u8]) -> Option<usize> {
    let n = src.len();
    let small = n < LIMIT_64K;
    // LZ4_hashPosition: 4-byte hash into 13 bits (byU16), else 5-byte into 12 (64-bit).
    let hash = |p: usize| -> usize {
        if small {
            (read32(src, p).wrapping_mul(2_654_435_761) >> (32 - (HASHLOG + 1))) as usize
        } else {
            ((read64(src, p) << 24).wrapping_mul(889_523_592_379) >> (64 - HASHLOG)) as usize
        }
    };
    let mut table = vec![0usize; 1 << (HASHLOG + 1)];
    let mut op = 0usize;
    let mut anchor = 0usize;
    'body: {
        if n < MIN_LENGTH {
            break 'body;
        }
        let mflimit_plus_one = n - MFLIMIT + 1;
        let matchlimit = n - LASTLITERALS;
        let count = |mut a: usize, mut b: usize| -> usize {
            let start = a;
            while a < matchlimit && src[a] == src[b] {
                a += 1;
                b += 1;
            }
            a - start
        };
        let mut ip = 0usize;
        table[hash(ip)] = ip;
        ip += 1;
        let mut forward_h = hash(ip);
        loop {
            // Find a match.
            let mut m;
            let mut forward_ip = ip;
            let mut step = 1usize;
            let mut search = 1u32 << SKIP_TRIGGER;
            loop {
                let h = forward_h;
                let current = forward_ip;
                let match_index = table[h];
                ip = forward_ip;
                forward_ip += step;
                step = (search >> SKIP_TRIGGER) as usize;
                search += 1;
                if forward_ip > mflimit_plus_one {
                    break 'body;
                }
                m = match_index;
                forward_h = hash(forward_ip);
                table[h] = current;
                if !small && match_index + DISTANCE_MAX < current {
                    continue;
                }
                if read32(src, m) == read32(src, ip) {
                    break;
                }
            }
            // Catch up.
            while ip > anchor && m > 0 && src[ip - 1] == src[m - 1] {
                ip -= 1;
                m -= 1;
            }
            // The literals.
            let lit = ip - anchor;
            op += 1 + extra(lit, RUN_MASK) + lit;
            loop {
                // Offset and match length.
                op += 2;
                let code = count(ip + MINMATCH, m + MINMATCH);
                ip += code + MINMATCH;
                op += extra(code, ML_MASK);
                anchor = ip;
                if ip >= mflimit_plus_one {
                    break 'body;
                }
                table[hash(ip - 2)] = ip - 2;
                // An immediate next match?
                let h = hash(ip);
                let match_index = table[h];
                table[h] = ip;
                if (small || match_index + DISTANCE_MAX >= ip)
                    && read32(src, match_index) == read32(src, ip)
                {
                    m = match_index;
                    op += 1;
                    continue;
                }
                break;
            }
            ip += 1;
            forward_h = hash(ip);
        }
    }
    let last = n - anchor;
    op += 1 + extra(last, RUN_MASK) + last;
    Some(op)
}
