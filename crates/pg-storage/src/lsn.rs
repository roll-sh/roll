//! Log sequence numbers and WAL segment naming (16 MiB segments, the Postgres default).

use std::fmt;
use std::str::FromStr;

pub const SEG_SIZE: u64 = 16 * 1024 * 1024;
const SEGS_PER_XLOGID: u64 = 0x1_0000_0000 / SEG_SIZE;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Lsn(pub u64);

impl Lsn {
    pub fn segno(self) -> u64 {
        self.0 / SEG_SIZE
    }
    pub fn segment_start(self) -> Lsn {
        Lsn(self.0 - self.0 % SEG_SIZE)
    }
}

impl fmt::Display for Lsn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:X}/{:X}", self.0 >> 32, self.0 & 0xffff_ffff)
    }
}

impl FromStr for Lsn {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> anyhow::Result<Self> {
        let (hi, lo) = s.trim().split_once('/').ok_or_else(|| anyhow::anyhow!("bad LSN {s:?}"))?;
        Ok(Lsn((u64::from_str_radix(hi, 16)? << 32) | u64::from_str_radix(lo, 16)?))
    }
}

/// `000000010000000000000003` style WAL file name.
pub fn segment_file(timeline: u32, segno: u64) -> String {
    format!("{timeline:08X}{:08X}{:08X}", segno / SEGS_PER_XLOGID, segno % SEGS_PER_XLOGID)
}

pub fn parse_segment_file(name: &str) -> Option<(u32, u64)> {
    if name.len() != 24 || !name.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let tli = u32::from_str_radix(&name[0..8], 16).ok()?;
    let hi = u64::from_str_radix(&name[8..16], 16).ok()?;
    let lo = u64::from_str_radix(&name[16..24], 16).ok()?;
    Some((tli, hi * SEGS_PER_XLOGID + lo))
}

pub fn history_file(timeline: u32) -> String {
    format!("{timeline:08X}.history")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lsn_roundtrip() {
        let l: Lsn = "16/B374D848".parse().unwrap();
        assert_eq!(l.0, 0x16_B374_D848);
        assert_eq!(l.to_string(), "16/B374D848");
    }

    #[test]
    fn segment_names() {
        let l: Lsn = "0/3000060".parse().unwrap();
        assert_eq!(segment_file(1, l.segno()), "000000010000000000000003");
        let l: Lsn = "1/0".parse().unwrap();
        assert_eq!(segment_file(2, l.segno()), "000000020000000100000000");
        assert_eq!(parse_segment_file("000000020000000100000000"), Some((2, l.segno())));
        assert_eq!(parse_segment_file("00000002.history"), None);
    }
}
