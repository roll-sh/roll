//! The PROXY protocol (HAProxy's v1 text and v2 binary headers), as an AWS NLB sends it when the
//! target group has `proxy_protocol_v2.enabled`: the client's real address arrives before the
//! first byte of the connection's own data.
//!
//! The header is read byte-exactly (nothing after it is consumed), bounded by the protocol's
//! own limits (107 bytes for v1, 16 + 65 535 for v2, and v2 bodies are only skipped, never
//! buffered beyond what the address needs). `LOCAL` commands (health checks from the load
//! balancer itself) and `UNKNOWN` families carry no client address.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use tokio::io::{AsyncRead, AsyncReadExt};

const V2_SIG: [u8; 12] = *b"\r\n\r\n\0\r\nQUIT\n";
const V1_MAX: usize = 107;

/// Reads a PROXY header; `Ok(None)` for one that names no client (LOCAL, UNKNOWN).
pub async fn read_header<R: AsyncRead + Unpin>(r: &mut R) -> anyhow::Result<Option<SocketAddr>> {
    let mut head = [0u8; 16];
    r.read_exact(&mut head[..5]).await?;
    if &head[..5] == b"PROXY" {
        let mut line = Vec::with_capacity(V1_MAX);
        line.extend_from_slice(b"PROXY");
        loop {
            anyhow::ensure!(line.len() < V1_MAX, "PROXY v1 header too long");
            let b = r.read_u8().await?;
            line.push(b);
            if line.ends_with(b"\r\n") {
                break;
            }
        }
        return parse_v1(&line[..line.len() - 2]);
    }
    r.read_exact(&mut head[5..16]).await?;
    anyhow::ensure!(head[..12] == V2_SIG, "no PROXY protocol header");
    anyhow::ensure!(head[12] >> 4 == 2, "unsupported PROXY protocol version {}", head[12] >> 4);
    let command = head[12] & 0x0f;
    let family = head[13];
    let len = u16::from_be_bytes([head[14], head[15]]) as usize;
    // At most 36 bytes of address are needed; the rest (TLVs) is skipped in small steps.
    let need = match family {
        0x11 => 12,
        0x21 => 36,
        _ => 0,
    }
    .min(len);
    let mut addr = [0u8; 36];
    r.read_exact(&mut addr[..need]).await?;
    let mut rest = len - need;
    let mut sink = [0u8; 256];
    while rest > 0 {
        let n = rest.min(sink.len());
        r.read_exact(&mut sink[..n]).await?;
        rest -= n;
    }
    anyhow::ensure!(command <= 1, "unknown PROXY protocol command {command}");
    if command == 0 {
        return Ok(None);
    }
    Ok(match family {
        0x11 if need == 12 => {
            let ip = Ipv4Addr::new(addr[0], addr[1], addr[2], addr[3]);
            Some(SocketAddr::new(IpAddr::V4(ip), u16::from_be_bytes([addr[8], addr[9]])))
        }
        0x21 if need == 36 => {
            let mut a = [0u8; 16];
            a.copy_from_slice(&addr[..16]);
            Some(SocketAddr::new(IpAddr::V6(Ipv6Addr::from(a)), u16::from_be_bytes([addr[32], addr[33]])))
        }
        0x11 | 0x21 => anyhow::bail!("PROXY v2 address block too short"),
        _ => None,
    })
}

fn parse_v1(line: &[u8]) -> anyhow::Result<Option<SocketAddr>> {
    let text = std::str::from_utf8(line)?;
    let mut parts = text.split(' ');
    anyhow::ensure!(parts.next() == Some("PROXY"), "bad PROXY v1 header");
    match parts.next() {
        Some("UNKNOWN") => Ok(None),
        Some(fam @ ("TCP4" | "TCP6")) => {
            let src: IpAddr = parts.next().ok_or_else(|| anyhow::anyhow!("PROXY v1: no source"))?.parse()?;
            let _dst: IpAddr = parts.next().ok_or_else(|| anyhow::anyhow!("PROXY v1: no destination"))?.parse()?;
            let sport: u16 = parts.next().ok_or_else(|| anyhow::anyhow!("PROXY v1: no source port"))?.parse()?;
            let _dport: u16 = parts.next().ok_or_else(|| anyhow::anyhow!("PROXY v1: no destination port"))?.parse()?;
            anyhow::ensure!(parts.next().is_none(), "PROXY v1: trailing fields");
            anyhow::ensure!((fam == "TCP4") == src.is_ipv4(), "PROXY v1: address family mismatch");
            Ok(Some(SocketAddr::new(src, sport)))
        }
        _ => anyhow::bail!("bad PROXY v1 family"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn read(bytes: &[u8]) -> (anyhow::Result<Option<SocketAddr>>, Vec<u8>) {
        let mut r = bytes;
        let got = read_header(&mut r).await;
        (got, r.to_vec())
    }

    #[tokio::test]
    async fn v1() {
        let (a, rest) = read(b"PROXY TCP4 203.0.113.7 10.0.0.1 51234 9200\r\nGET / HTTP/1.1\r\n").await;
        assert_eq!(a.unwrap(), Some("203.0.113.7:51234".parse().unwrap()));
        assert_eq!(rest, b"GET / HTTP/1.1\r\n", "nothing after the header is consumed");
        let (a, _) = read(b"PROXY TCP6 2001:db8::1 2001:db8::2 4000 443\r\n").await;
        assert_eq!(a.unwrap(), Some("[2001:db8::1]:4000".parse().unwrap()));
        assert_eq!(read(b"PROXY UNKNOWN\r\n").await.0.unwrap(), None);
        assert!(read(b"PROXY TCP4 1.2.3.4 5.6.7.8 1 2 3\r\n").await.0.is_err());
        assert!(read(b"PROXY TCP4 ::1 ::1 1 2\r\n").await.0.is_err());
        assert!(read(b"PROXY TCP4 1.2.3.4 5.6.7.8 99999 2\r\n").await.0.is_err());
        let long = format!("PROXY TCP4 {}\r\n", "1".repeat(200));
        assert!(read(long.as_bytes()).await.0.is_err());
        assert!(read(b"GET / HTTP/1.1\r\n\r\n").await.0.is_err());
    }

    fn v2(command: u8, family: u8, body: &[u8]) -> Vec<u8> {
        let mut v = V2_SIG.to_vec();
        v.push(0x20 | command);
        v.push(family);
        v.extend_from_slice(&(body.len() as u16).to_be_bytes());
        v.extend_from_slice(body);
        v
    }

    #[tokio::test]
    async fn v2_headers() {
        let mut body = vec![203, 0, 113, 7, 10, 0, 0, 1];
        body.extend_from_slice(&51234u16.to_be_bytes());
        body.extend_from_slice(&9200u16.to_be_bytes());
        // An NLB appends TLVs (the VPC endpoint id): skipped.
        body.extend_from_slice(&[0xea, 0, 5, 1, 2, 3, 4, 5]);
        let mut bytes = v2(1, 0x11, &body);
        bytes.extend_from_slice(b"rest");
        let (a, rest) = read(&bytes).await;
        assert_eq!(a.unwrap(), Some("203.0.113.7:51234".parse().unwrap()));
        assert_eq!(rest, b"rest");
        let mut body6 = vec![0u8; 36];
        body6[0] = 0x20;
        body6[1] = 0x01;
        body6[15] = 9;
        body6[32..34].copy_from_slice(&443u16.to_be_bytes());
        assert_eq!(read(&v2(1, 0x21, &body6)).await.0.unwrap(), Some("[2001::9]:443".parse().unwrap()));
        // LOCAL (health checks) and UNIX sockets name no client.
        assert_eq!(read(&v2(0, 0x11, &body)).await.0.unwrap(), None);
        assert_eq!(read(&v2(1, 0x31, &[0u8; 216])).await.0.unwrap(), None);
        // A short address block, a bad version, a truncated body.
        assert!(read(&v2(1, 0x11, &[1, 2, 3])).await.0.is_err());
        let mut bad = v2(1, 0x11, &body);
        bad[12] = 0x31;
        assert!(read(&bad).await.0.is_err());
        let mut cut = v2(1, 0x11, &body);
        cut.truncate(20);
        assert!(read(&cut).await.0.is_err());
        // A large TLV area is skipped without buffering it.
        let mut big = body.clone();
        big.resize(60_000, 0);
        let (a, _) = read(&v2(1, 0x11, &big)).await;
        assert!(a.unwrap().is_some());
    }
}
