//! `inet`: 4 or 16 address bytes.
//!
//! Cassandra goes through `java.net.InetAddress`: `getByName` accepts the classic IPv4 forms
//! (`127.1`, `1` — one to four decimal parts), IPv6 with brackets or a `%scope`, and resolves
//! host names (we resolve only `localhost`); an IPv4-mapped IPv6 literal becomes a 4-byte
//! address. `getHostAddress` prints IPv6 as eight uncompressed hex groups (`0:0:0:0:0:0:0:1`).
//!
//! ScyllaDB uses `inet_pton` / `inet_ntop`: strict dotted quads and RFC 4291 IPv6, kept as
//! given, printed compressed with glibc's rules (`::1`, `::ffff:1.2.3.4`, `::1.2.3.4`).

use std::net::{Ipv4Addr, Ipv6Addr};

use crate::Dialect;

/// Parses an address literal into its 4 or 16 bytes. `None` if the reference rejects it.
pub fn parse_inet(s: &str, dialect: Dialect) -> Option<Vec<u8>> {
    match dialect {
        Dialect::Scylla => {
            if let Ok(v4) = s.parse::<Ipv4Addr>() {
                return Some(v4.octets().to_vec());
            }
            if s.contains(':')
                && let Ok(v6) = s.parse::<Ipv6Addr>()
            {
                return Some(v6.octets().to_vec());
            }
            None
        }
        Dialect::Cassandra => {
            if s == "localhost" {
                return Some(vec![127, 0, 0, 1]);
            }
            if let Some(v4) = java_ipv4(s) {
                return Some(v4.to_vec());
            }
            let inner = s
                .strip_prefix('[')
                .and_then(|x| x.strip_suffix(']'))
                .unwrap_or(s);
            let addr = inner.split_once('%').map(|(a, _)| a).unwrap_or(inner);
            if !addr.contains(':') {
                return None;
            }
            let v6: Ipv6Addr = addr.parse().ok()?;
            let o = v6.octets();
            if o[..10].iter().all(|&b| b == 0) && o[10] == 0xff && o[11] == 0xff {
                return Some(o[12..].to_vec());
            }
            Some(o.to_vec())
        }
    }
}

/// `IPAddressUtil.textToNumericFormatV4`: `a`, `a.b`, `a.b.c` or `a.b.c.d`, decimal parts, the
/// last part filling the remaining bytes.
fn java_ipv4(s: &str) -> Option<[u8; 4]> {
    if s.is_empty() || s.ends_with('.') {
        return None;
    }
    let parts: Vec<&str> = s.split('.').collect();
    if parts.len() > 4 {
        return None;
    }
    let mut vals = Vec::with_capacity(4);
    for p in &parts {
        if p.is_empty() || p.len() > 10 || !p.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        vals.push(p.parse::<u64>().ok()?);
    }
    let n = vals.len();
    let mut out = [0u8; 4];
    for (i, &v) in vals[..n - 1].iter().enumerate() {
        if v > 255 {
            return None;
        }
        out[i] = v as u8;
    }
    let last = vals[n - 1];
    let free = 4 - (n - 1);
    if last >= 1u64 << (8 * free) {
        return None;
    }
    for k in 0..free {
        out[3 - k] = (last >> (8 * k)) as u8;
    }
    Some(out)
}

/// The address text both references produce for 4 or 16 bytes (`None` for other lengths).
pub fn format_inet(b: &[u8], dialect: Dialect) -> Option<String> {
    match b.len() {
        4 => Some(Ipv4Addr::new(b[0], b[1], b[2], b[3]).to_string()),
        16 => {
            let mut w = [0u16; 8];
            for (i, x) in w.iter_mut().enumerate() {
                *x = u16::from_be_bytes([b[2 * i], b[2 * i + 1]]);
            }
            Some(match dialect {
                Dialect::Cassandra => w
                    .iter()
                    .map(|x| format!("{x:x}"))
                    .collect::<Vec<_>>()
                    .join(":"),
                Dialect::Scylla => glibc_ntop6(&w, b),
            })
        }
        _ => None,
    }
}

/// glibc `inet_ntop(AF_INET6)`: the longest run of two or more zero groups becomes `::`; an
/// IPv4-compatible (`::a.b.c.d`) or IPv4-mapped (`::ffff:a.b.c.d`) address ends in dotted quad.
fn glibc_ntop6(w: &[u16; 8], b: &[u8]) -> String {
    let (mut best, mut best_len) = (usize::MAX, 0);
    let mut i = 0;
    while i < 8 {
        if w[i] == 0 {
            let start = i;
            while i < 8 && w[i] == 0 {
                i += 1;
            }
            if i - start > best_len {
                best = start;
                best_len = i - start;
            }
        } else {
            i += 1;
        }
    }
    if best_len < 2 {
        best = usize::MAX;
    }
    let mut out = String::with_capacity(40);
    let mut i = 0;
    while i < 8 {
        if i == best {
            if i == 0 {
                out.push(':');
            }
            out.push(':');
            i += best_len;
            continue;
        }
        if i == 6 && best == 0 && (best_len == 6 || (best_len == 5 && w[5] == 0xffff)) {
            out.push_str(&format!("{}.{}.{}.{}", b[12], b[13], b[14], b[15]));
            break;
        }
        out.push_str(&format!("{:x}", w[i]));
        i += 1;
        if i < 8 {
            out.push(':');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const C: Dialect = Dialect::Cassandra;
    const S: Dialect = Dialect::Scylla;

    fn round(s: &str, d: Dialect) -> String {
        format_inet(&parse_inet(s, d).unwrap_or_else(|| panic!("{s}")), d).unwrap()
    }

    #[test]
    fn cassandra_forms() {
        // CAST(ip AS text) on cassandra:5.0.9.
        for (s, out) in [
            ("1.2.3.4", "1.2.3.4"),
            ("127.1", "127.0.0.1"),
            ("1", "0.0.0.1"),
            ("1.2.65535", "1.2.255.255"),
            ("010.1.1.1", "10.1.1.1"),
            ("localhost", "127.0.0.1"),
            ("::1", "0:0:0:0:0:0:0:1"),
            ("[::1]", "0:0:0:0:0:0:0:1"),
            ("::", "0:0:0:0:0:0:0:0"),
            ("fe80::1:2", "fe80:0:0:0:0:0:1:2"),
            ("fe80::1%1", "fe80:0:0:0:0:0:0:1"),
            ("::ffff:1.2.3.4", "1.2.3.4"),
            ("::1.2.3.4", "0:0:0:0:0:0:102:304"),
            ("1:2:3:4:5:6:7:8", "1:2:3:4:5:6:7:8"),
        ] {
            assert_eq!(round(s, C), out, "{s}");
        }
        assert_eq!(parse_inet("::ffff:1.2.3.4", C).unwrap().len(), 4);
        for bad in [
            "1.2.3.256",
            "not an ip",
            "0x7f.1",
            "1.2.3.4.5",
            "",
            "1.",
            "1.2.16777216",
            ":::",
        ] {
            assert!(parse_inet(bad, C).is_none(), "{bad}");
        }
    }

    #[test]
    fn scylla_forms() {
        for (s, out) in [
            ("1.2.3.4", "1.2.3.4"),
            ("::1", "::1"),
            ("::", "::"),
            ("fe80::1:2", "fe80::1:2"),
            ("::ffff:1.2.3.4", "::ffff:1.2.3.4"),
            ("::1.2.3.4", "::1.2.3.4"),
            ("1:2:3:4:5:6:7:8", "1:2:3:4:5:6:7:8"),
            ("ff::", "ff::"),
            ("1:0:0:2:0:0:0:3", "1:0:0:2::3"),
            ("1:0:2:3:4:5:6:7", "1:0:2:3:4:5:6:7"),
            ("::2", "::2"),
        ] {
            assert_eq!(round(s, S), out, "{s}");
        }
        assert_eq!(parse_inet("::ffff:1.2.3.4", S).unwrap().len(), 16);
        for bad in [
            "127.1",
            "1",
            "[::1]",
            "1.2.65535",
            "localhost",
            "0x7f.1",
            "010.1.1.1",
            "1.2.3.256",
            "fe80::1%1",
        ] {
            assert!(parse_inet(bad, S).is_none(), "{bad}");
        }
        assert_eq!(format_inet(&[1, 2, 3], S), None);
    }
}
