//! `search.CLUSTERSET`'s topology parser (`RedisEnterprise_ParseTopology`, `coord/rmr/redise.c`):
//! the replies a malformed topology gets. A standalone server keeps no topology, so a valid one
//! is only accepted.

use super::args::{Ac, AcErr, c_eq};
use bytes::Bytes;
use std::collections::HashMap;

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(crate::util::cstr(b)).into_owned()
}

struct Shard {
    ranges: Vec<(u16, u16)>,
    host: Option<String>,
    port: i64,
    password: Option<String>,
    unix: Option<String>,
    master: bool,
}

/// `MREndpoint_Parse`: `[password@]host:port`, IPv6 hosts in brackets.
fn endpoint(addr: &str) -> Option<(Option<String>, String, i64)> {
    let (password, addr) = match addr.find('@') {
        Some(i) => (Some(addr[..i].to_string()), &addr[i + 1..]),
        None => (None, addr),
    };
    let (opener, addr) = match addr.strip_prefix('[') {
        Some(a) => (true, a),
        None => (false, addr),
    };
    let colon = addr.rfind(':')?;
    let mut host = &addr[..colon];
    if opener {
        host = host.strip_suffix(']')?;
    }
    // `atoi`.
    let digits: String = addr[colon + 1..].trim_start().chars().take_while(|c| c.is_ascii_digit() || *c == '-' || *c == '+').collect();
    let port = digits.parse::<i64>().unwrap_or(0);
    if port <= 0 || port > 0xFFFF {
        return None;
    }
    Some((password, host.to_string(), port))
}

pub fn parse_topology(argv: &[Bytes]) -> Result<(), String> {
    let mut ac = Ac::new(argv);
    let _ = ac.advance();
    let err = |ac: &Ac, msg: String| format!("{msg} at offset {}", ac.offset);
    let cur = |ac: &Ac| ac.peek().map(|b| text(b));
    if argv.len() == 3 {
        // The short form: AUTH <password>, the rest from the cluster API (which a standalone
        // server doesn't have).
        if !ac.advance_if("AUTH") {
            return Err(err(&ac, format!("Expected `AUTH` but got `{}`", cur(&ac).unwrap_or_else(|| "(nil)".into()))));
        }
        if ac.get().is_err() {
            return Err(err(&ac, "Missing value for AUTH".into()));
        }
        return Err(err(&ac, "Failed to parse topology using module API".into()));
    }
    let bad_or_missing = |ac: &mut Ac, arg: &str, rc: Result<(), AcErr>| -> String {
        match rc {
            Err(AcErr::NoArg) => err(ac, format!("Missing value for {arg}")),
            rc => {
                if rc.is_ok() {
                    ac.offset -= 1;
                }
                let val = cur(ac).unwrap_or_default();
                let e = err(ac, format!("Bad value for {arg}: {val}"));
                if rc.is_ok() {
                    ac.offset += 1;
                }
                e
            }
        }
    };
    let mut my_id: Option<&Bytes> = None;
    let mut num_ranges = 0u32;
    let mut num_slots = 16384u32;
    while !ac.at_end() {
        if ac.advance_if("MYID") {
            my_id = ac.take();
        } else if ac.advance_if("HASHFUNC") {
            let Some(f) = ac.take() else {
                return Err(err(&ac, "Missing value for HASHFUNC".into()));
            };
            if !c_eq(f, "CRC12") && !c_eq(f, "CRC16") {
                return Err(err(&ac, format!("Bad value for HASHFUNC: {}", text(f))));
            }
        } else if ac.advance_if("NUMSLOTS") {
            match ac.get_u32(super::args::GE1) {
                Ok(n) if n > 16384 => return Err(err(&ac, format!("Bad value for NUMSLOTS: {n}"))),
                Ok(n) => num_slots = n,
                Err(e) => return Err(bad_or_missing(&mut ac, "NUMSLOTS", Err(e))),
            }
        } else if ac.advance_if("RANGES") {
            match ac.get_u32(super::args::GE1) {
                Ok(n) => num_ranges = n,
                Err(e) => return Err(bad_or_missing(&mut ac, "RANGES", Err(e))),
            }
            break;
        } else if ac.advance_if("HASREPLICATION") {
        } else {
            return Err(err(&ac, format!("Unexpected argument: `{}`", cur(&ac).unwrap_or_default())));
        }
    }
    let Some(my_id) = my_id else {
        return Err(err(&ac, "Missing value for MYID".into()));
    };
    let mut shards: HashMap<Vec<u8>, Shard> = HashMap::new();
    for _ in 0..num_ranges {
        let mut sh = Shard { ranges: Vec::new(), host: None, port: 0, password: None, unix: None, master: false };
        if !ac.advance_if("SHARD") {
            return Err(err(&ac, format!("Expected `SHARD` but got `{}`", cur(&ac).unwrap_or_else(|| "(nil)".into()))));
        }
        let Ok(id_raw) = ac.get() else {
            return Err(err(&ac, "Missing value for SHARD".into()));
        };
        let id = text(id_raw);
        while !ac.at_end() {
            if ac.advance_if("SLOTRANGE") {
                if !sh.ranges.is_empty() {
                    return Err(err(&ac, format!("Multiple SLOTRANGE specified for shard `{id}`")));
                }
                let start = match ac.get_u16(0) {
                    Ok(v) => v,
                    Err(e) => return Err(bad_or_missing(&mut ac, "SLOTRANGE start", Err(e))),
                };
                let end = match ac.get_u16(0) {
                    Ok(v) if (v as u32) < num_slots => v,
                    Ok(_) => return Err(bad_or_missing(&mut ac, "SLOTRANGE end", Ok(()))),
                    Err(e) => return Err(bad_or_missing(&mut ac, "SLOTRANGE end", Err(e))),
                };
                if start > end {
                    return Err(err(&ac, format!("Bad values for SLOTRANGE: {start}, {end}")));
                }
                sh.ranges.push((start, end));
            } else if ac.advance_if("ADDR") {
                let Some(addr) = ac.take() else {
                    return Err(err(&ac, "Missing value for ADDR".into()));
                };
                if sh.host.is_some() {
                    return Err(err(&ac, format!("Multiple ADDR specified for shard `{id}`")));
                }
                let addr = text(addr);
                let Some((pw, host, port)) = endpoint(&addr) else {
                    return Err(err(&ac, format!("Bad value for ADDR: {addr}")));
                };
                sh.password = pw;
                sh.host = Some(host);
                sh.port = port;
            } else if ac.advance_if("UNIXADDR") {
                if sh.unix.is_some() {
                    return Err(err(&ac, format!("Multiple UNIXADDR specified for shard `{id}`")));
                }
                let lit = |u: &[u8], l: &str| u.len() == l.len() && u.eq_ignore_ascii_case(l.as_bytes());
                match ac.peek() {
                    Some(u) if !lit(u, "MASTER") && !lit(u, "SHARD") => {
                        sh.unix = Some(text(u));
                        let _ = ac.advance();
                    }
                    _ => return Err(err(&ac, "Missing value for UNIXADDR".into())),
                }
            } else if ac.advance_if("MASTER") {
                sh.master = true;
            } else {
                break;
            }
        }
        match shards.get_mut(id_raw.as_ref()) {
            None => {
                if sh.host.is_none() {
                    return Err(err(&ac, "Missing value for ADDR".into()));
                }
                shards.insert(id_raw.to_vec(), sh);
            }
            Some(existing) => {
                if let Some(h) = &sh.host
                    && (Some(h) != existing.host.as_ref() || sh.password != existing.password || sh.port != existing.port)
                {
                    return Err(err(&ac, format!("Conflicting ADDR for shard `{id}`")));
                }
                if let Some(u) = &sh.unix
                    && existing.unix.as_ref() != Some(u)
                {
                    return Err(err(&ac, format!("Conflicting UNIXADDR for shard `{id}`")));
                }
                if let Some(&r) = sh.ranges.first() {
                    if existing.ranges.last().is_some_and(|l| l.1 as u32 + 1 >= r.0 as u32) {
                        return Err(err(&ac, format!("SLOTRANGE out of order for shard `{id}`")));
                    }
                    existing.ranges.push(r);
                }
            }
        }
    }
    if let Some(extra) = cur(&ac) {
        return Err(err(&ac, format!("Expected end of command but got `{extra}`")));
    }
    // Our shard among the masters with slots (by C string), else at least a known shard.
    let mine = shards.iter().any(|(k, s)| s.master && !s.ranges.is_empty() && crate::util::cstr(k) == crate::util::cstr(my_id));
    if !mine && !shards.contains_key(my_id.as_ref()) {
        return Err(err(&ac, format!("MYID `{}` does not correspond to any shard", text(my_id))));
    }
    Ok(())
}
