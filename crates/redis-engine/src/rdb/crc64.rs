//! CRC-64/Jones (reflected, init 0, no final xor): the checksum of RDB files and DUMP payloads,
//! as Redis' `crc64.c`. Slice-by-8.

const POLY: u64 = 0x95ac_9329_ac4b_c9b5;

const fn tables() -> [[u64; 256]; 8] {
    let mut t = [[0u64; 256]; 8];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u64;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { (c >> 1) ^ POLY } else { c >> 1 };
            k += 1;
        }
        t[0][i] = c;
        i += 1;
    }
    let mut i = 0;
    while i < 256 {
        let mut k = 1;
        while k < 8 {
            let prev = t[k - 1][i];
            t[k][i] = (prev >> 8) ^ t[0][(prev & 0xff) as usize];
            k += 1;
        }
        i += 1;
    }
    t
}

static T: [[u64; 256]; 8] = tables();

/// Continue `crc` over `data` (start with 0).
pub fn crc64(mut crc: u64, data: &[u8]) -> u64 {
    let (chunks, rest) = data.as_chunks::<8>();
    for c in chunks {
        let v = crc ^ u64::from_le_bytes(*c);
        crc = T[7][(v & 0xff) as usize]
            ^ T[6][((v >> 8) & 0xff) as usize]
            ^ T[5][((v >> 16) & 0xff) as usize]
            ^ T[4][((v >> 24) & 0xff) as usize]
            ^ T[3][((v >> 32) & 0xff) as usize]
            ^ T[2][((v >> 40) & 0xff) as usize]
            ^ T[1][((v >> 48) & 0xff) as usize]
            ^ T[0][(v >> 56) as usize];
    }
    for &b in rest {
        crc = T[0][((crc ^ b as u64) & 0xff) as usize] ^ (crc >> 8);
    }
    crc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vectors() {
        assert_eq!(crc64(0, b"123456789"), 0xe9c6d914c4b8d9ca);
        let lorem = b"This is a test of the emergency broadcast system.";
        let whole = crc64(0, lorem);
        let split = crc64(crc64(0, &lorem[..13]), &lorem[13..]);
        assert_eq!(whole, split);
        assert_eq!(crc64(0, b""), 0);
    }
}
