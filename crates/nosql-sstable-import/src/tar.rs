//! A minimal reader of (ustar / GNU) tar archives, for ScyllaDB Manager's legacy
//! `schema.tar.gz` (one CQL file per keyspace) and the test fixtures. Regular files only;
//! sizes are checked against the archive's length.

use anyhow::{Result, bail};

/// `(path, contents)` of every regular file.
pub fn entries(raw: &[u8]) -> Result<Vec<(String, Vec<u8>)>> {
    let mut out = Vec::new();
    let mut at = 0usize;
    let mut long_name: Option<String> = None;
    while at + 512 <= raw.len() {
        let h = &raw[at..at + 512];
        if h.iter().all(|b| *b == 0) {
            break;
        }
        let sum: u32 = h.iter().enumerate().map(|(i, b)| if (148..156).contains(&i) { b' ' as u32 } else { *b as u32 }).sum();
        let stored = octal(&h[148..156])?;
        if stored != sum as u64 {
            bail!("corrupt tar archive: header checksum at {at}");
        }
        let size = octal(&h[124..136])?;
        let kind = h[156];
        let field = |r: std::ops::Range<usize>| -> String {
            let f = &h[r];
            let end = f.iter().position(|b| *b == 0).unwrap_or(f.len());
            String::from_utf8_lossy(&f[..end]).into_owned()
        };
        let mut name = field(0..100);
        let prefix = field(345..500);
        if &h[257..262] == b"ustar" && !prefix.is_empty() {
            name = format!("{prefix}/{name}");
        }
        let start = at + 512;
        let end = start.checked_add(size as usize).filter(|e| *e <= raw.len()).ok_or_else(|| anyhow::anyhow!("corrupt tar archive: entry {name} of {size} bytes is truncated"))?;
        let body = &raw[start..end];
        match kind {
            b'0' | 0 => {
                let path = long_name.take().unwrap_or(name);
                out.push((path, body.to_vec()));
            }
            b'L' => long_name = Some(String::from_utf8_lossy(body).trim_end_matches('\0').to_string()),
            _ => {
                long_name = None;
            }
        }
        at = start + (size as usize).div_ceil(512) * 512;
    }
    Ok(out)
}

fn octal(f: &[u8]) -> Result<u64> {
    let s: String = f.iter().take_while(|b| **b != 0).map(|b| *b as char).collect();
    let s = s.trim();
    if s.is_empty() {
        return Ok(0);
    }
    u64::from_str_radix(s, 8).map_err(|_| anyhow::anyhow!("corrupt tar archive: bad number {s:?}"))
}

/// Gunzip (bounded: at most `max` bytes out).
pub fn gunzip(raw: &[u8], max: u64) -> Result<Vec<u8>> {
    use std::io::Read;
    let mut out = Vec::new();
    flate2::read::MultiGzDecoder::new(raw).take(max + 1).read_to_end(&mut out)?;
    if out.len() as u64 > max {
        bail!("compressed file expands past {max} bytes");
    }
    Ok(out)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Build a tar archive (tests).
    pub fn build(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        for (name, body) in files {
            let mut h = [0u8; 512];
            h[..name.len()].copy_from_slice(name.as_bytes());
            h[100..107].copy_from_slice(b"0000644");
            h[124..135].copy_from_slice(format!("{:011o}", body.len()).as_bytes());
            h[156] = b'0';
            h[257..263].copy_from_slice(b"ustar\0");
            h[148..156].copy_from_slice(b"        ");
            let sum: u32 = h.iter().map(|b| *b as u32).sum();
            h[148..155].copy_from_slice(format!("{sum:06o}\0").as_bytes());
            out.extend_from_slice(&h);
            out.extend_from_slice(body);
            out.resize(out.len().div_ceil(512) * 512, 0);
        }
        out.extend_from_slice(&[0u8; 1024]);
        out
    }

    #[test]
    fn reads_entries() {
        let t = build(&[("a/ks.cql", b"CREATE KEYSPACE ks;"), ("b", b"")]);
        let e = entries(&t).unwrap();
        assert_eq!(e, vec![("a/ks.cql".to_string(), b"CREATE KEYSPACE ks;".to_vec()), ("b".to_string(), vec![])]);
        let mut bad = t.clone();
        bad[0] = b'x';
        assert!(entries(&bad).is_err());
        assert!(entries(&t[..520]).is_err());
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        std::io::Write::write_all(&mut gz, &t).unwrap();
        let gz = gz.finish().unwrap();
        assert_eq!(gunzip(&gz, 1 << 20).unwrap(), t);
        assert!(gunzip(&gz, 100).is_err());
    }
}
