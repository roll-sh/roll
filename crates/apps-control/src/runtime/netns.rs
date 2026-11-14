//! Who is at an instance's port without going through rolld, from its network namespace's own
//! numbers (`/proc/<pid>/net/…` of its sandbox: readable by anyone, and still readable while
//! the app is frozen). It's how a private caller wakes a sleeping service and keeps it awake:
//! rolld never sees that traffic, and the app sees it only once it runs.
//!
//! Two reads, a cheap one and a dear one. `snmp`'s TCP `PassiveOpens` counts the connections the
//! namespace accepted (a frozen app's backlog and the waker's caller too): 34 µs, so a sleeping
//! instance is watched five times a second. The socket tables (`tcp`, `tcp6`) say who is
//! connected, but for another namespace the kernel walks the whole host's table: 5.6 ms on
//! ai-server, so they're read only when a running instance is about to sleep.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

const LISTEN: u8 = 0x0A;
const ESTABLISHED: u8 = 0x01;
/// The peer has closed; its data may still be unread.
const CLOSE_WAIT: u8 = 0x08;

#[derive(Debug, PartialEq)]
struct Socket {
    local: IpAddr,
    port: u16,
    peer: IpAddr,
    state: u8,
}

/// The rows of one table (`tcp` or `tcp6`): a line that doesn't parse is skipped.
fn sockets(table: &str) -> impl Iterator<Item = Socket> + '_ {
    table.lines().skip(1).filter_map(|line| {
        let mut f = line.split_whitespace();
        let local = f.nth(1)?;
        let remote = f.next()?;
        let state = u8::from_str_radix(f.next()?, 16).ok()?;
        let port = u16::from_str_radix(local.rsplit(':').next()?, 16).ok()?;
        Some(Socket { local: address(local.split(':').next()?)?, port, peer: address(remote.split(':').next()?)?, state })
    })
}

/// The kernel's hex address: four little-endian 32-bit words (IPv6) or one (IPv4).
fn address(hex: &str) -> Option<IpAddr> {
    let word = |i: usize| u32::from_str_radix(hex.get(i * 8..i * 8 + 8)?, 16).ok().map(u32::swap_bytes);
    match hex.len() {
        8 => Some(IpAddr::V4(Ipv4Addr::from(word(0)?))),
        32 => {
            let mut b = [0u8; 16];
            for i in 0..4 {
                b[i * 4..i * 4 + 4].copy_from_slice(&word(i)?.to_be_bytes());
            }
            let v6 = Ipv6Addr::from(b);
            Some(v6.to_ipv4_mapped().map_or(IpAddr::V6(v6), IpAddr::V4))
        }
        _ => None,
    }
}

/// Whether something listens on `port` (the app, or the waker while the app is stopped).
pub fn listening(tcp: &str, tcp6: &str, port: u16) -> bool {
    sockets(tcp).chain(sockets(tcp6)).any(|s| s.port == port && s.state == LISTEN)
}

/// Every address something listens on (what an app that doesn't answer bound instead).
pub fn listeners(tcp: &str, tcp6: &str) -> Vec<std::net::SocketAddr> {
    let mut v: Vec<_> = sockets(tcp).chain(sockets(tcp6)).filter(|s| s.state == LISTEN).map(|s| std::net::SocketAddr::new(s.local, s.port)).collect();
    v.sort();
    v.dedup();
    v
}

/// The connections the namespace has accepted (TCP `PassiveOpens` in its `snmp`).
pub fn passive_opens(snmp: &str) -> Option<u64> {
    let mut rows = snmp.lines().filter(|l| l.starts_with("Tcp:"));
    let (names, values) = (rows.next()?, rows.next()?);
    let at = names.split_whitespace().position(|n| n == "PassiveOpens")?;
    values.split_whitespace().nth(at)?.parse().ok()
}

/// Connections to `port` from peers other than the host (`host`: the gateways of its networks,
/// where rolld's own come from) and loopback: the project's other services.
pub fn private_peers(tcp: &str, tcp6: &str, port: u16, host: &[IpAddr]) -> u32 {
    sockets(tcp).chain(sockets(tcp6)).filter(|s| s.port == port && matches!(s.state, ESTABLISHED | CLOSE_WAIT) && !s.peer.is_loopback() && !host.contains(&s.peer)).count() as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEAD: &str = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n";

    #[test]
    fn accepted_connections_are_read_from_snmp() {
        let snmp = "Ip: Forwarding DefaultTTL\nIp: 1 64\nTcp: RtoAlgorithm RtoMin RtoMax MaxConn ActiveOpens PassiveOpens AttemptFails\nTcp: 1 200 120000 -1 3 7 0\nUdp: InDatagrams\nUdp: 0\n";
        assert_eq!(passive_opens(snmp), Some(7));
        assert_eq!(passive_opens("Ip: x\nIp: 1\n"), None);
    }

    #[test]
    fn only_peers_other_than_the_host_are_private_traffic() {
        let listen = "   0: 00000000:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 1 1 0 100 0 0 10 0\n";
        let from = |peer: &str| format!("   1: 030012AC:1F90 {peer}:D2F0 01 00000000:00000000 00:00000000 00000000     0        0 1 1 0 20 4 28 10 -1\n");
        let gw: IpAddr = "172.18.0.1".parse().unwrap();
        // rolld's proxy and probes (through the gateway) and loopback are not.
        let rolld = format!("{HEAD}{listen}{}{}", from("010012AC"), from("0100007F"));
        assert_eq!(private_peers(&rolld, HEAD, 8080, &[gw]), 0);
        let other = format!("{HEAD}{listen}{}{}", from("050012AC"), from("060012AC"));
        assert_eq!(private_peers(&other, HEAD, 8080, &[gw]), 2);
        assert!(listening(&other, HEAD, 8080) && !listening(&other, HEAD, 9000));
        let loopback = format!("{HEAD}   0: 0100007F:0BB8 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 1 1 0 100 0 0 10 0\n");
        assert_eq!(listeners(&loopback, HEAD), vec!["127.0.0.1:3000".parse().unwrap()]);
        // An IPv4-mapped IPv6 row (busybox listens on `::`).
        let tcp6 = format!("{HEAD}   0: 00000000000000000000000000000000:2328 00000000000000000000000000000000:0000 0A 00000000:00000001 00:00000000 00000000     0        0 2245507849 2 000000009a288b5b 100 0 0 10 0\n");
        assert!(listening(HEAD, &tcp6, 9000));
    }
}
