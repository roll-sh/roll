//! Client authentication rules, as pg_hba.conf / pg_ident.conf give them: the operator's file
//! (`roll-engine --hba-file`, or rules on the command line with `--hba`), read again by
//! pg_reload_conf / SIGHUP, shown by pg_hba_file_rules and pg_ident_file_mappings. Without
//! any, the server applies one rule to every connection (`--auth`: scram-sha-256 with a
//! password, trust without), as before.
//!
//! The engine listens on TCP only: `host` and `hostnossl` / `hostnogssenc` lines match every
//! connection; `local`, `hostssl` and `hostgssenc` lines none. Methods: trust, reject,
//! scram-sha-256, md5, password. Others (peer, ident, gss, sspi, pam, ldap, radius, cert,
//! oauth) are refused where they'd be chosen.

use std::net::IpAddr;

/// One line of pg_hba.conf, parsed (parse_hba_line), or why it doesn't parse.
#[derive(Clone, Debug, PartialEq)]
pub struct HbaLine {
    pub line: u32,
    pub conn_type: String,
    pub databases: Vec<String>,
    pub users: Vec<String>,
    pub address: Option<String>,
    pub netmask: Option<String>,
    pub method: String,
    pub options: Vec<String>,
    /// (address, prefix bits) for host lines with an address (not all / samehost / samenet).
    net: Option<(IpAddr, u8)>,
    pub error: Option<String>,
}

const METHODS: &[&str] = &[
    "trust", "reject", "md5", "password", "scram-sha-256", "gss", "sspi", "ident", "peer", "pam",
    "ldap", "radius", "cert", "oauth",
];

/// pg_hba.conf's tokens on a line: words, "quoted words" (kept quoted, so `"all"` is a
/// name), commas splitting lists; `#` starts a comment.
fn tokens(l: &str) -> Vec<Vec<String>> {
    let mut fields: Vec<Vec<String>> = Vec::new();
    let mut cur: Vec<String> = Vec::new();
    let mut word = String::new();
    let (mut quoted, mut in_word, mut comma) = (false, false, false);
    for c in l.chars() {
        if c == '"' {
            quoted = !quoted;
            comma = false;
            word.push('"');
            in_word = true;
            continue;
        }
        if quoted {
            word.push(c);
            continue;
        }
        match c {
            '#' => break,
            ',' => {
                cur.push(std::mem::take(&mut word));
                in_word = false;
                comma = true;
            }
            c if c.is_whitespace() => {
                if in_word {
                    cur.push(std::mem::take(&mut word));
                    in_word = false;
                }
                if !comma && !cur.is_empty() {
                    fields.push(std::mem::take(&mut cur));
                }
            }
            c => {
                comma = false;
                word.push(c);
                in_word = true;
            }
        }
    }
    if in_word {
        cur.push(word);
    }
    if !cur.is_empty() {
        fields.push(cur);
    }
    fields
}

fn unquote(t: &str) -> String {
    t.replace('"', "")
}

impl HbaLine {
    fn error(line: u32, msg: impl Into<String>) -> HbaLine {
        HbaLine {
            line,
            conn_type: String::new(),
            databases: Vec::new(),
            users: Vec::new(),
            address: None,
            netmask: None,
            method: String::new(),
            options: Vec::new(),
            net: None,
            error: Some(msg.into()),
        }
    }
}

/// parse_hba_line: one line's fields (None: nothing on it).
fn parse_hba_line(line: u32, l: &str) -> Option<HbaLine> {
    let f = tokens(l);
    let first = f.first()?.first()?.clone();
    let mut it = f.into_iter().skip(1);
    let conn_type = first;
    if !matches!(conn_type.as_str(), "local" | "host" | "hostssl" | "hostnossl" | "hostgssenc" | "hostnogssenc") {
        return Some(HbaLine::error(line, format!("invalid connection type \"{conn_type}\"")));
    }
    let Some(databases) = it.next() else {
        return Some(HbaLine::error(line, "end-of-line before database specification"));
    };
    let Some(users) = it.next() else {
        return Some(HbaLine::error(line, "end-of-line before role specification"));
    };
    let (mut address, mut netmask, mut net) = (None, None, None);
    if conn_type != "local" {
        let Some(a) = it.next().and_then(|v| v.into_iter().next()) else {
            return Some(HbaLine::error(line, "end-of-line before IP address specification"));
        };
        match a.as_str() {
            "all" | "samehost" | "samenet" => address = Some(a),
            _ if a.contains('/') => match parse_net(&a, None) {
                Ok((ip, bits)) => {
                    address = Some(ip.to_string());
                    netmask = Some(mask_text(ip, bits));
                    net = Some((ip, bits));
                }
                Err(e) => return Some(HbaLine::error(line, e)),
            },
            _ if a.parse::<IpAddr>().is_ok() => {
                let Some(m) = it.next().and_then(|v| v.into_iter().next()) else {
                    return Some(HbaLine::error(line, "end-of-line before netmask specification"));
                };
                match parse_net(&a, Some(&m)) {
                    Ok((ip, bits)) => {
                        address = Some(a);
                        netmask = Some(m);
                        net = Some((ip, bits));
                    }
                    Err(e) => return Some(HbaLine::error(line, e)),
                }
            }
            // (A host name: matched by reverse lookup in PostgreSQL; never here.)
            _ => address = Some(a),
        }
    }
    let Some(method) = it.next().and_then(|v| v.into_iter().next()) else {
        return Some(HbaLine::error(line, "end-of-line before authentication method"));
    };
    if !METHODS.contains(&method.as_str()) {
        return Some(HbaLine::error(line, format!("invalid authentication method \"{method}\"")));
    }
    let options: Vec<String> = it.flatten().collect();
    if let Some(o) = options.iter().find(|o| !o.contains('=')) {
        return Some(HbaLine::error(line, format!("authentication option not in name=value format: {o}")));
    }
    Some(HbaLine { line, conn_type, databases, users, address, netmask, method, options, net, error: None })
}

fn mask_text(ip: IpAddr, bits: u8) -> String {
    match ip {
        IpAddr::V4(_) => {
            let m = if bits == 0 { 0 } else { u32::MAX << (32 - bits as u32) };
            std::net::Ipv4Addr::from(m).to_string()
        }
        IpAddr::V6(_) => {
            let m = if bits == 0 { 0 } else { u128::MAX << (128 - bits as u32) };
            std::net::Ipv6Addr::from(m).to_string()
        }
    }
}

/// pg_hba.conf's rules.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Hba {
    pub lines: Vec<HbaLine>,
}

impl Hba {
    pub fn parse(text: &str) -> Hba {
        Hba {
            lines: text.lines().enumerate().filter_map(|(i, l)| parse_hba_line(i as u32 + 1, l)).collect(),
        }
    }

    pub fn has_errors(&self) -> bool {
        self.lines.iter().any(|l| l.error.is_some())
    }

    /// check_hba: the first rule matching a connection from `peer` (`local`: through the
    /// engine's stand-in for the Unix socket, `mark_local`) for `user` to `database`
    /// (`member(role)`: whether `user` is a member of `role`).
    pub fn choose(
        &self,
        peer: Option<IpAddr>,
        local: bool,
        // (A physical replication connection matches only `replication` (check_db).)
        physical_repl: bool,
        database: &str,
        user: &str,
        member: &dyn Fn(&str) -> bool,
    ) -> Option<&HbaLine> {
        self.lines.iter().filter(|l| l.error.is_none()).find(|l| {
            let type_ok = match l.conn_type.as_str() {
                "local" => local,
                "host" | "hostnossl" | "hostnogssenc" => !local,
                _ => false,
            };
            if !type_ok {
                return false;
            }
            let addr_ok = local || match (l.address.as_deref(), l.net, peer) {
                (Some("all"), ..) => true,
                (Some("samehost" | "samenet"), _, Some(p)) => p.is_loopback(),
                (_, Some(net), Some(p)) => in_net(p, net),
                _ => false,
            };
            addr_ok
                && l.databases.iter().any(|d| match d.as_str() {
                    _ if physical_repl => d == "replication",
                    "all" => true,
                    "sameuser" => database == user,
                    "samerole" | "samegroup" => member(database),
                    "replication" => false,
                    d => unquote(d) == database,
                })
                && l.users.iter().any(|u| match u.as_str() {
                    "all" => true,
                    u if u.starts_with('+') => member(&u[1..]),
                    u => unquote(u) == user,
                })
        })
    }
}

/// The client ends of the engine's connections to itself through libpq's default socket
/// (dblink / postgres_fdw without a host, `--allow-outbound local`): the server takes them as
/// `local` connections, as PostgreSQL's Unix socket ones.
static LOCAL_PEERS: std::sync::Mutex<Vec<std::net::SocketAddr>> = std::sync::Mutex::new(Vec::new());

/// A connection from `client` (its local address) stands for a Unix socket one.
pub fn mark_local(client: std::net::SocketAddr) {
    let mut l = LOCAL_PEERS.lock().unwrap_or_else(|e| e.into_inner());
    l.push(client);
}

/// Whether the connection from `peer` was marked local (and forget it).
pub fn take_local(peer: std::net::SocketAddr) -> bool {
    let mut l = LOCAL_PEERS.lock().unwrap_or_else(|e| e.into_inner());
    match l.iter().position(|a| *a == peer) {
        Some(i) => {
            l.swap_remove(i);
            true
        }
        None => false,
    }
}

/// One line of pg_ident.conf: map name, system user name, database user name.
#[derive(Clone, Debug, PartialEq)]
pub struct IdentLine {
    pub line: u32,
    pub map: String,
    pub sys: String,
    pub pg: String,
    pub error: Option<String>,
}

pub fn parse_ident(text: &str) -> Vec<IdentLine> {
    text.lines()
        .enumerate()
        .filter_map(|(i, l)| {
            let f: Vec<String> = tokens(l).into_iter().filter_map(|v| v.into_iter().next()).collect();
            let line = i as u32 + 1;
            match f.as_slice() {
                [] => None,
                [map, sys, pg, ..] => Some(IdentLine { line, map: map.clone(), sys: sys.clone(), pg: pg.clone(), error: None }),
                _ => Some(IdentLine {
                    line,
                    map: String::new(),
                    sys: String::new(),
                    pg: String::new(),
                    error: Some("missing entry at end of line".into()),
                }),
            }
        })
        .collect()
}

fn parse_net(addr: &str, mask: Option<&str>) -> Result<(IpAddr, u8), String> {
    let (a, bits) = match addr.split_once('/') {
        Some((a, b)) => (a, Some(b)),
        None => (addr, None),
    };
    let ip: IpAddr = a.parse().map_err(|_| format!("invalid IP address \"{a}\""))?;
    let max = if ip.is_ipv4() { 32 } else { 128 };
    let bits = match (bits, mask) {
        (Some(b), _) => b
            .parse::<u8>()
            .ok()
            .filter(|b| *b <= max)
            .ok_or_else(|| format!("invalid CIDR mask in address \"{addr}\""))?,
        (None, Some(m)) => {
            let m: IpAddr = m.parse().map_err(|_| format!("invalid IP mask \"{m}\""))?;
            match m {
                IpAddr::V4(v) => u32::from(v).leading_ones() as u8,
                IpAddr::V6(v) => u128::from(v).leading_ones() as u8,
            }
        }
        (None, None) => return Err("specifying both host name and CIDR mask is invalid".into()),
    };
    Ok((ip, bits))
}

fn in_net(peer: IpAddr, (net, bits): (IpAddr, u8)) -> bool {
    let peer = match (peer, net) {
        (IpAddr::V6(v), IpAddr::V4(_)) => v.to_ipv4_mapped().map_or(peer, IpAddr::V4),
        _ => peer,
    };
    match (peer, net) {
        (IpAddr::V4(p), IpAddr::V4(n)) => {
            let m = if bits == 0 { 0 } else { u32::MAX << (32 - bits as u32) };
            u32::from(p) & m == u32::from(n) & m
        }
        (IpAddr::V6(p), IpAddr::V6(n)) => {
            let m = if bits == 0 { 0 } else { u128::MAX << (128 - bits as u32) };
            u128::from(p) & m == u128::from(n) & m
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hba_rules() {
        let h = Hba::parse("# c\nlocal all all trust\nhost all all 127.0.0.1/32 trust\nhost db1,\"all\" +grp ::1 ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff md5\nhost all all all scram-sha-256\nhostx a b\n");
        assert_eq!(h.lines.len(), 5);
        assert_eq!(h.lines[1].netmask.as_deref(), Some("255.255.255.255"));
        assert_eq!(h.lines[2].databases, vec!["db1".to_string(), "\"all\"".to_string()]);
        assert_eq!(h.lines[4].error.as_deref(), Some("invalid connection type \"hostx\""));
        let no = |_: &str| false;
        let lo: IpAddr = "127.0.0.1".parse().unwrap();
        let far: IpAddr = "10.1.2.3".parse().unwrap();
        assert_eq!(h.choose(Some(lo), false, false, "x", "u", &no).map(|l| l.method.as_str()), Some("trust"));
        assert_eq!(h.choose(Some(far), false, false, "x", "u", &no).map(|l| l.method.as_str()), Some("scram-sha-256"));
        assert_eq!(h.choose(Some(far), true, false, "x", "u", &no).map(|l| l.line), Some(2));
        assert_eq!(h.choose(Some(lo), false, true, "postgres", "u", &no).map(|l| l.line), None);
        let six: IpAddr = "::1".parse().unwrap();
        assert_eq!(h.choose(Some(six), false, false, "all", "u", &|r| r == "grp").map(|l| l.method.as_str()), Some("md5"));
    }
}
