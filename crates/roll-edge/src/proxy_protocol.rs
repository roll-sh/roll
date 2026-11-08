//! PROXY protocol v1 and v2 (haproxy.org/download/2.9/doc/proxy-protocol.txt): the AWS NLB puts a
//! header in front of every connection so the node sees the client's address.
//!
//! The header is read exactly (nothing after it is consumed), into fixed buffers: a v2 header
//! whose length passes [`MAX_V2_BODY`] is refused rather than allocated.

use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use tokio::io::{AsyncRead, AsyncReadExt};

/// The v2 signature.
pub const V2_SIGNATURE: &[u8; 12] = b"\r\n\r\n\0\r\nQUIT\n";
/// The most a v2 header may carry after its 16 fixed bytes. The NLB sends the addresses (36
/// bytes for IPv6) and a VPC endpoint TLV; anything near this is not from the NLB.
pub const MAX_V2_BODY: usize = 512;
/// A v1 line's limit, CRLF included (the spec's).
pub const MAX_V1_LINE: usize = 107;

/// What the header said.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    /// The client; `None` for `LOCAL` (the load balancer's own health checks) and `UNKNOWN`.
    pub source: Option<SocketAddr>,
    /// The address the client connected to.
    pub destination: Option<SocketAddr>,
}

fn invalid(msg: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}

/// Read one header, v1 or v2.
pub async fn read<R: AsyncRead + Unpin>(r: &mut R) -> io::Result<Header> {
    let mut head = [0u8; 16];
    r.read_exact(&mut head[..5]).await?;
    if &head[..5] == b"PROXY" {
        return read_v1(r).await;
    }
    r.read_exact(&mut head[5..]).await?;
    if &head[..12] != V2_SIGNATURE {
        return Err(invalid("expected a PROXY protocol header"));
    }
    let (ver_cmd, family) = (head[12], head[13]);
    let len = u16::from_be_bytes([head[14], head[15]]) as usize;
    if ver_cmd >> 4 != 2 {
        return Err(invalid("unsupported PROXY protocol version"));
    }
    if len > MAX_V2_BODY {
        return Err(invalid("PROXY v2 header too long"));
    }
    let mut buf = [0u8; MAX_V2_BODY];
    let body = &mut buf[..len];
    r.read_exact(body).await?;
    match ver_cmd & 0xf {
        0 => return Ok(Header { source: None, destination: None }),
        1 => {}
        _ => return Err(invalid("unknown PROXY v2 command")),
    }
    let port = |b: &[u8]| u16::from_be_bytes([b[0], b[1]]);
    Ok(match family >> 4 {
        // TCP or UDP over IPv4: src, dst, sport, dport.
        1 => {
            let a = body.get(..12).ok_or_else(|| invalid("short PROXY v2 IPv4 addresses"))?;
            let ip = |b: &[u8]| IpAddr::V4(Ipv4Addr::new(b[0], b[1], b[2], b[3]));
            Header { source: Some(SocketAddr::new(ip(&a[0..4]), port(&a[8..10]))), destination: Some(SocketAddr::new(ip(&a[4..8]), port(&a[10..12]))) }
        }
        2 => {
            let a = body.get(..36).ok_or_else(|| invalid("short PROXY v2 IPv6 addresses"))?;
            let ip = |b: &[u8]| IpAddr::V6(Ipv6Addr::from(<[u8; 16]>::try_from(b).expect("16 bytes")));
            Header { source: Some(SocketAddr::new(ip(&a[0..16]), port(&a[32..34]))), destination: Some(SocketAddr::new(ip(&a[16..32]), port(&a[34..36]))) }
        }
        // UNSPEC or a Unix socket: no address worth reporting.
        _ => Header { source: None, destination: None },
    })
}

async fn read_v1<R: AsyncRead + Unpin>(r: &mut R) -> io::Result<Header> {
    let mut line = [0u8; MAX_V1_LINE];
    line[..5].copy_from_slice(b"PROXY");
    let mut n = 5;
    loop {
        if n == MAX_V1_LINE {
            return Err(invalid("PROXY v1 header too long"));
        }
        line[n] = r.read_u8().await?;
        n += 1;
        if line[..n].ends_with(b"\r\n") {
            break;
        }
    }
    let text = std::str::from_utf8(&line[..n - 2]).map_err(|_| invalid("PROXY v1 header isn't text"))?;
    let parts: Vec<&str> = text.split(' ').collect();
    let addr = |ip: &str, port: &str| -> io::Result<SocketAddr> {
        Ok(SocketAddr::new(ip.parse().map_err(|_| invalid("bad PROXY v1 address"))?, port.parse().map_err(|_| invalid("bad PROXY v1 port"))?))
    };
    match parts.as_slice() {
        ["PROXY", "TCP4" | "TCP6", src, dst, sport, dport] => Ok(Header { source: Some(addr(src, sport)?), destination: Some(addr(dst, dport)?) }),
        ["PROXY", "UNKNOWN", ..] => Ok(Header { source: None, destination: None }),
        _ => Err(invalid("bad PROXY v1 header")),
    }
}

/// A v2 `PROXY` header for a TCP connection from `source` to `destination` (tests, and hops
/// that pass a client's address on).
pub fn encode_v2(source: SocketAddr, destination: SocketAddr) -> Vec<u8> {
    let mut out = V2_SIGNATURE.to_vec();
    out.push(0x21);
    match (source, destination) {
        (SocketAddr::V4(s), SocketAddr::V4(d)) => {
            out.extend_from_slice(&[0x11, 0, 12]);
            out.extend_from_slice(&s.ip().octets());
            out.extend_from_slice(&d.ip().octets());
            out.extend_from_slice(&s.port().to_be_bytes());
            out.extend_from_slice(&d.port().to_be_bytes());
        }
        (s, d) => {
            let v6 = |a: SocketAddr| match a.ip() {
                IpAddr::V4(v4) => v4.to_ipv6_mapped(),
                IpAddr::V6(v6) => v6,
            };
            out.extend_from_slice(&[0x21, 0, 36]);
            out.extend_from_slice(&v6(s).octets());
            out.extend_from_slice(&v6(d).octets());
            out.extend_from_slice(&s.port().to_be_bytes());
            out.extend_from_slice(&d.port().to_be_bytes());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn v1_v2_local_and_bounds() {
        let mut r: &[u8] = b"PROXY TCP4 203.0.113.7 10.0.0.1 51234 6379\r\nPING\r\n";
        let h = read(&mut r).await.unwrap();
        assert_eq!(h.source, Some("203.0.113.7:51234".parse().unwrap()));
        assert_eq!(h.destination, Some("10.0.0.1:6379".parse().unwrap()));
        assert_eq!(r, b"PING\r\n", "nothing after the header is consumed");
        let mut r: &[u8] = b"PROXY UNKNOWN\r\n";
        assert_eq!(read(&mut r).await.unwrap().source, None);

        for (src, dst) in [("198.51.100.9:8080", "10.0.0.1:443"), ("[2001:db8::1]:5555", "[2001:db8::2]:5432")] {
            let mut h = encode_v2(src.parse().unwrap(), dst.parse().unwrap());
            h.extend_from_slice(b"rest");
            let mut r: &[u8] = &h;
            let got = read(&mut r).await.unwrap();
            assert_eq!((got.source, got.destination), (Some(src.parse().unwrap()), Some(dst.parse().unwrap())));
            assert_eq!(r, b"rest");
        }
        // LOCAL: the NLB's health check.
        let mut h = V2_SIGNATURE.to_vec();
        h.extend_from_slice(&[0x20, 0x00, 0, 0]);
        assert_eq!(read(&mut &h[..]).await.unwrap().source, None);

        // A length past the cap is refused without reading (or allocating) it.
        let mut h = V2_SIGNATURE.to_vec();
        h.extend_from_slice(&[0x21, 0x11, 0xff, 0xff]);
        assert!(read(&mut &h[..]).await.is_err());
        // Addresses shorter than the family needs, an over-long v1 line, no header at all.
        let mut h = V2_SIGNATURE.to_vec();
        h.extend_from_slice(&[0x21, 0x11, 0, 4, 1, 2, 3, 4]);
        assert!(read(&mut &h[..]).await.is_err());
        let long = format!("PROXY TCP4 {}\r\n", "1".repeat(200));
        assert!(read(&mut long.as_bytes()).await.is_err());
        assert!(read(&mut &b"GET / HTTP/1.1\r\n\r\n"[..]).await.is_err());
    }
}
