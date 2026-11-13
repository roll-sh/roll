//! Who is at an instance's port without going through rolld, read from its network
//! namespace's own socket tables (`/proc/<pid>/net/tcp` and `tcp6`, readable by anyone, and
//! still readable while the app is frozen). It's how a private caller wakes a sleeping service
//! and keeps it awake: rolld never sees that traffic, and the app sees it only once it runs.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// The sockets on an instance's port, as its network namespace has them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Callers {
    /// Someone is waiting for the app: a connection in a frozen app's backlog, or one the
    /// waker took while the app was stopped (connected, and nothing listening).
    pub waiting: bool,
    /// Connections from peers that aren't the host (rolld's own come through the network's
    /// gateway): the project's other services.
    pub private: u32,
}

const LISTEN: u8 = 0x0A;
const ESTABLISHED: u8 = 0x01;
/// The peer has closed; its data may still be unread.
const CLOSE_WAIT: u8 = 0x08;

#[derive(Debug, PartialEq)]
struct Socket {
    port: u16,
    peer: IpAddr,
    state: u8,
    rx_queue: u32,
}

/// The rows of one table (`tcp` or `tcp6`): a line that doesn't parse is skipped.
fn sockets(table: &str) -> impl Iterator<Item = Socket> + '_ {
    table.lines().skip(1).filter_map(|line| {
        let mut f = line.split_whitespace();
        let local = f.nth(1)?;
        let remote = f.next()?;
        let state = u8::from_str_radix(f.next()?, 16).ok()?;
        let rx_queue = u32::from_str_radix(f.next()?.split(':').nth(1)?, 16).ok()?;
        let port = u16::from_str_radix(local.rsplit(':').next()?, 16).ok()?;
        Some(Socket { port, peer: address(remote.split(':').next()?)?, state, rx_queue })
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

/// Who's at `port`, given the namespace's `tcp` and `tcp6` tables and the host's addresses on
/// its networks (`host`: their gateways).
pub fn callers(tcp: &str, tcp6: &str, port: u16, host: &[IpAddr]) -> Callers {
    let mut listening = false;
    let (mut backlog, mut held, mut private) = (false, false, 0);
    for s in sockets(tcp).chain(sockets(tcp6)).filter(|s| s.port == port) {
        match s.state {
            LISTEN => {
                listening = true;
                backlog |= s.rx_queue > 0;
            }
            ESTABLISHED | CLOSE_WAIT if !s.peer.is_loopback() => {
                held = true;
                if !host.contains(&s.peer) {
                    private += 1;
                }
            }
            _ => {}
        }
    }
    Callers { waiting: backlog || (held && !listening), private }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEAD: &str = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n";

    #[test]
    fn a_frozen_apps_backlog_is_a_caller_waiting() {
        // Captured from a paused busybox `nc -lk -p 9000` with one caller (port 0x2328).
        let tcp6 = format!(
            "{HEAD}   0: 00000000000000000000000000000000:2328 00000000000000000000000000000000:0000 0A 00000000:00000001 00:00000000 00000000     0        0 2245507849 2 000000009a288b5b 100 0 0 10 0\n   1: 0000000000000000FFFF00000100007F:2328 0000000000000000FFFF00000100007F:89C3 08 00000000:00000004 00:00000000 00000000     0        0 0 1 00000000d45db106 20 4 28 10 -1\n"
        );
        assert_eq!(callers(HEAD, &tcp6, 9000, &[]), Callers { waiting: true, private: 0 });
        // Nobody in the backlog: nobody waiting.
        let idle = tcp6.replace("00000000:00000001", "00000000:00000000");
        assert_eq!(callers(HEAD, &idle.lines().take(2).collect::<Vec<_>>().join("\n"), 9000, &[]), Callers::default());
        assert_eq!(callers(HEAD, &tcp6, 8080, &[]), Callers::default(), "another port");
    }

    #[test]
    fn a_connection_with_nothing_listening_is_the_wakers_caller() {
        // 172.18.0.3:8080 (0x1F90) held by the waker for 172.18.0.5; no LISTEN row.
        let tcp = format!("{HEAD}   0: 030012AC:1F90 050012AC:D2F0 01 00000000:00000000 00:00000000 00000000     0        0 1 1 0 20 4 28 10 -1\n");
        assert_eq!(callers(&tcp, HEAD, 8080, &["172.18.0.1".parse().unwrap()]), Callers { waiting: true, private: 1 });
    }

    #[test]
    fn only_peers_other_than_the_host_are_private_traffic() {
        let listen = "   0: 00000000:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 1 1 0 100 0 0 10 0\n";
        let from = |peer: &str| format!("   1: 030012AC:1F90 {peer}:D2F0 01 00000000:00000000 00:00000000 00000000     0        0 1 1 0 20 4 28 10 -1\n");
        let gw: IpAddr = "172.18.0.1".parse().unwrap();
        // rolld's proxy and probes (through the gateway) and loopback are not.
        let rolld = format!("{HEAD}{listen}{}{}", from("010012AC"), from("0100007F"));
        assert_eq!(callers(&rolld, HEAD, 8080, &[gw]), Callers::default());
        let other = format!("{HEAD}{listen}{}{}", from("050012AC"), from("060012AC"));
        assert_eq!(callers(&other, HEAD, 8080, &[gw]), Callers { waiting: false, private: 2 });
    }
}
