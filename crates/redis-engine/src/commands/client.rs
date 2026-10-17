//! CLIENT: connection introspection and control (LIST, INFO, KILL, PAUSE, UNBLOCK…) and
//! client-side caching (TRACKING, CACHING, GETREDIR, TRACKINGINFO), as in Redis' networking.c.

use super::*;
use crate::client::ReplyMode;
use crate::clients::View;
use crate::cmd;
use crate::Engine;
use crate::clients::Track;
use std::sync::Arc;
use std::sync::atomic::Ordering;

pub static CLIENT: &[Command] = &[
    cmd!("id", 2, NOSCRIPT | LOADING | STALE, 0, 0, 0, id, Connection),
    cmd!("getname", 2, NOSCRIPT | LOADING | STALE, 0, 0, 0, getname, Connection),
    cmd!("setname", 3, NOSCRIPT | LOADING | STALE, 0, 0, 0, setname, Connection),
    cmd!("setinfo", 4, NOSCRIPT | LOADING | STALE, 0, 0, 0, setinfo, Connection),
    cmd!("info", 2, NOSCRIPT | LOADING | STALE, 0, 0, 0, info, Connection),
    cmd!("list", -2, ADMIN | NOSCRIPT | LOADING | STALE, 0, 0, 0, list, Connection),
    cmd!("kill", -3, ADMIN | NOSCRIPT | LOADING | STALE, 0, 0, 0, kill, Connection),
    cmd!("pause", -3, ADMIN | NOSCRIPT | LOADING | STALE, 0, 0, 0, pause, Connection),
    cmd!("unpause", 2, ADMIN | NOSCRIPT | LOADING | STALE, 0, 0, 0, unpause, Connection),
    cmd!("unblock", -3, ADMIN | NOSCRIPT | LOADING | STALE, 0, 0, 0, unblock, Connection),
    cmd!("reply", 3, NOSCRIPT | LOADING | STALE, 0, 0, 0, reply, Connection),
    cmd!("no-evict", 3, ADMIN | NOSCRIPT | LOADING | STALE, 0, 0, 0, no_evict, Connection),
    cmd!("no-touch", 3, NOSCRIPT | LOADING | STALE, 0, 0, 0, no_touch, Connection),
    cmd!("tracking", -3, NOSCRIPT | LOADING | STALE, 0, 0, 0, tracking, Connection),
    cmd!("trackinginfo", 2, NOSCRIPT | LOADING | STALE, 0, 0, 0, trackinginfo, Connection),
    cmd!("caching", 3, NOSCRIPT | LOADING | STALE, 0, 0, 0, caching, Connection),
    cmd!("getredir", 2, NOSCRIPT | LOADING | STALE, 0, 0, 0, getredir, Connection),
    cmd!("help", 2, LOADING | STALE, 0, 0, 0, client_help, Connection),
];

/// `addReplySubcommandSyntaxError`.
fn subcommand_syntax(args: &[Bytes]) -> Error {
    Error::msg(format!(
        "ERR unknown subcommand or wrong number of arguments for '{}'. Try CLIENT HELP.",
        String::from_utf8_lossy(&args[1]).chars().take(128).collect::<String>()
    ))
}

fn id(cx: &mut Cx, _args: &[Bytes]) -> CmdResult {
    Ok(Reply::Int(cx.client.id as i64))
}

fn getname(cx: &mut Cx, _args: &[Bytes]) -> CmdResult {
    Ok(cx.client.name.clone().map_or(Reply::Nil, Reply::Bulk))
}

/// `validateClientName`.
pub fn validate_name(n: &[u8]) -> Result<(), Error> {
    if n.iter().any(|&c| !(b'!'..=b'~').contains(&c)) {
        return Err(Error::new("ERR Client names cannot contain spaces, newlines or special characters."));
    }
    Ok(())
}

fn setname(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    validate_name(&args[2])?;
    cx.client.name = if args[2].is_empty() { None } else { Some(args[2].clone()) };
    Ok(Reply::Ok)
}

fn setinfo(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let v = &args[3];
    let lib_name = eq(&args[2], "lib-name");
    if !lib_name && !eq(&args[2], "lib-ver") {
        return Err(Error::msg(format!("ERR Unrecognized option '{}'", String::from_utf8_lossy(&args[2]))));
    }
    if v.iter().any(|&c| !(b'!'..=b'~').contains(&c)) {
        return Err(Error::msg(format!(
            "ERR {} cannot contain spaces, newlines or special characters.",
            String::from_utf8_lossy(&args[2]).to_lowercase()
        )));
    }
    let v: Option<Arc<str>> = if v.is_empty() { None } else { Some(String::from_utf8_lossy(v).into()) };
    if lib_name {
        cx.client.lib_name = v;
    } else {
        cx.client.lib_ver = v;
    }
    Ok(Reply::Ok)
}

fn info(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let line = line_for_current(cx, args);
    Ok(Reply::Verbatim("txt", Bytes::from(line + "\n")))
}

fn line_for_current(cx: &Cx, args: &[Bytes]) -> String {
    let view = View::of(cx.client);
    let stats = cx.client.conn.clone();
    line(cx.eng, cx.client.id, &view, stats.as_deref(), Some(args), cx.now)
}

/// `getClientType`: 0 normal, 1 replica, 2 pubsub, 3 master.
fn client_type(view: &View, stats: Option<&crate::client::ConnStats>) -> u8 {
    if !view.monitor && stats.is_some_and(|s| s.replica.load(Ordering::Relaxed)) {
        1
    } else if view.subscriptions() > 0 {
        2
    } else {
        0
    }
}

/// `getClientTypeByName`.
fn type_by_name(name: &[u8]) -> Result<u8, Error> {
    let t = String::from_utf8_lossy(name).to_ascii_lowercase();
    Ok(match t.as_str() {
        "normal" => 0,
        "slave" | "replica" => 1,
        "pubsub" => 2,
        "master" => 3,
        _ => return Err(Error::msg(format!("ERR Unknown client type '{}'", String::from_utf8_lossy(name)))),
    })
}

/// What the engine knows about a client beyond its [`View`].
#[derive(Default)]
pub struct Derived<'a> {
    pub track: Option<&'a Track>,
    pub blocked: bool,
    pub dirty_cas: bool,
    pub watch: usize,
}

impl<'a> Derived<'a> {
    pub fn of(eng: &'a Engine, id: u64) -> Self {
        Self {
            track: eng.clients.track(id),
            blocked: eng.is_blocked(id),
            dirty_cas: eng.watches.dirty.contains(&id),
            watch: eng.watches.by_client.get(&id).map_or(0, |l| l.len()),
        }
    }
}

/// The CLIENT LIST / CLIENT INFO line of a registered client.
pub fn line(eng: &Engine, id: u64, v: &View, stats: Option<&crate::client::ConnStats>, current: Option<&[Bytes]>, now: i64) -> String {
    line_with(id, v, &Derived::of(eng, id), stats, current, now)
}

/// The CLIENT LIST / CLIENT INFO line (`catClientInfoString`). `current` carries the
/// arguments of the command being run when this is the calling client.
pub fn line_with(id: u64, v: &View, d: &Derived, stats: Option<&crate::client::ConnStats>, current: Option<&[Bytes]>, now: i64) -> String {
    let track = d.track;
    let blocked = d.blocked || stats.is_some_and(|s| s.postponed.load(Ordering::Relaxed));
    let mut flags = String::new();
    if v.monitor {
        flags.push('O');
    } else if stats.is_some_and(|s| s.replica.load(Ordering::Relaxed)) {
        flags.push('S');
    }
    if v.subscriptions() > 0 {
        flags.push('P');
    }
    if v.multi >= 0 {
        flags.push('x');
    }
    if blocked {
        flags.push('b');
    }
    if let Some(t) = track {
        flags.push('t');
        if t.broken {
            flags.push('R');
        }
        if t.bcast {
            flags.push('B');
        }
    }
    if d.dirty_cas {
        flags.push('d');
    }
    if stats.is_some_and(|s| s.killed.load(Ordering::Relaxed)) {
        flags.push('c');
    }
    if v.readonly {
        flags.push('r');
    }
    if v.no_evict {
        flags.push('e');
    }
    if v.no_touch {
        flags.push('T');
    }
    if flags.is_empty() {
        flags.push('N');
    }
    let load = |f: fn(&crate::client::ConnStats) -> u64| stats.map_or(0, f);
    // The command being run is still in the query buffer, parsed into argv.
    let (cur_qbuf, argv_mem) = current.map_or((0, 0), |a| {
        let mut b = bytes::BytesMut::new();
        redis_proto::encode_command(a, &mut b);
        (b.len() as u64, a.iter().map(|x| x.len() as u64).sum::<u64>())
    });
    let qbuf = load(|s| s.qbuf.load(Ordering::Relaxed)) + cur_qbuf;
    let qbuf_cap = load(|s| s.qbuf_cap.load(Ordering::Relaxed)).max(qbuf);
    let qbuf_free = qbuf_cap - qbuf;
    let omem = load(|s| s.obuf.load(Ordering::Relaxed));
    let rbs: u64 = 16384;
    let tot_mem = qbuf_cap + rbs + argv_mem + v.multi_mem as u64 + omem + 1024;
    let idle = if current.is_some() {
        0
    } else {
        stats.map_or(0, |s| (now / 1000 - s.last_interaction.load(Ordering::Relaxed)).max(0))
    };
    let fd = stats.map_or(-1, |s| s.fd.load(Ordering::Relaxed));
    let watch = d.watch;
    let redir = track.map_or(-1, |t| t.redirect as i64);
    format!(
        "id={id} addr={} laddr={} fd={fd} name={} age={} idle={idle} flags={flags} db={} sub={} psub={} ssub={} multi={} watch={watch} qbuf={qbuf} qbuf-free={qbuf_free} argv-mem={argv_mem} multi-mem={} rbs={rbs} rbp={rbs} obl=0 oll=0 omem={omem} omem-shared=0 omem-unshared={omem} tot-mem={tot_mem} events=r cmd={} user={} redir={redir} resp={} lib-name={} lib-ver={} io-thread=0 tot-net-in={} tot-net-out={} tot-cmds={} read-events={} avg-pipeline-len-sum={} avg-pipeline-len-cnt={}",
        v.addr,
        v.laddr,
        v.name.as_ref().map(|n| String::from_utf8_lossy(n).into_owned()).unwrap_or_default(),
        now / 1000 - v.created_ms / 1000,
        v.db,
        v.sub,
        v.psub,
        v.ssub,
        v.multi,
        v.multi_mem,
        v.last_cmd,
        v.user.name,
        v.resp,
        v.lib_name.as_deref().unwrap_or(""),
        v.lib_ver.as_deref().unwrap_or(""),
        load(|s| s.net_in.load(Ordering::Relaxed)),
        load(|s| s.net_out.load(Ordering::Relaxed)),
        load(|s| s.cmds.load(Ordering::Relaxed)),
        load(|s| s.read_events.load(Ordering::Relaxed)),
        load(|s| s.pipeline_sum.load(Ordering::Relaxed)),
        load(|s| s.pipeline_cnt.load(Ordering::Relaxed)),
    )
}

fn list(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let mut ty = None;
    let mut ids: Option<Vec<u64>> = None;
    if args.len() == 4 && eq(&args[2], "type") {
        ty = Some(type_by_name(&args[3])?);
    } else if args.len() > 3 && eq(&args[2], "id") {
        let mut v = Vec::new();
        for a in &args[3..] {
            v.push(crate::util::parse_i64(a).ok_or(Error::new("ERR Invalid client ID"))? as u64);
        }
        ids = Some(v);
    } else if args.len() != 2 {
        return Err(SYNTAX);
    }
    let me = cx.client.id;
    let mut out = String::new();
    let mut push = |cx: &Cx, id: u64| {
        if id == me {
            out.push_str(&line_for_current(cx, args));
        } else if let Some(e) = cx.eng.clients.by_id.get(&id) {
            out.push_str(&line(cx.eng, id, &e.view, Some(&e.stats), None, cx.now));
        } else {
            return;
        }
        out.push('\n');
    };
    match ids {
        Some(ids) => {
            for id in ids {
                if id == me || cx.eng.clients.by_id.contains_key(&id) {
                    push(cx, id);
                }
            }
        }
        None => {
            let mut all: Vec<u64> = cx.eng.clients.by_id.keys().copied().collect();
            if !all.contains(&me) {
                all.push(me);
                all.sort_unstable();
            }
            for id in all {
                let t = if id == me {
                    client_type(&View::of(cx.client), cx.client.conn.as_deref())
                } else {
                    let e = &cx.eng.clients.by_id[&id];
                    client_type(&e.view, Some(&e.stats))
                };
                if ty.is_none_or(|ty| ty == t) {
                    push(cx, id);
                }
            }
        }
    }
    Ok(Reply::Verbatim("txt", Bytes::from(out)))
}

fn kill(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let (mut addr, mut laddr, mut user, mut ty, mut id, mut max_age) = (None, None, None, None, 0u64, 0i64);
    let mut skipme = true;
    if args.len() == 3 {
        addr = Some(args[2].clone());
        skipme = false;
    } else {
        let mut i = 2;
        while i < args.len() {
            let more = i + 1 < args.len();
            let o = &args[i];
            if !more {
                return Err(SYNTAX);
            }
            let v = &args[i + 1];
            if eq(o, "id") {
                id = match crate::util::parse_i64(v) {
                    Some(n) if n >= 1 => n as u64,
                    _ => return Err(Error::new("ERR client-id should be greater than 0")),
                };
            } else if eq(o, "maxage") {
                let n = crate::util::parse_i64(v).ok_or(Error::new("ERR maxage is not an integer or out of range"))?;
                if n <= 0 {
                    return Err(Error::new("ERR maxage should be greater than 0"));
                }
                max_age = n;
            } else if eq(o, "type") {
                ty = Some(type_by_name(v)?);
            } else if eq(o, "addr") {
                addr = Some(v.clone());
            } else if eq(o, "laddr") {
                laddr = Some(v.clone());
            } else if eq(o, "user") {
                let u = cx.eng.acl.read().user(v).cloned();
                user = Some(u.ok_or_else(|| Error::msg(format!("ERR No such user '{}'", String::from_utf8_lossy(v))))?);
            } else if eq(o, "skipme") {
                skipme = if eq(v, "yes") {
                    true
                } else if eq(v, "no") {
                    false
                } else {
                    return Err(SYNTAX);
                };
            } else {
                return Err(SYNTAX);
            }
            i += 2;
        }
    }
    let me = cx.client.id;
    let now = cx.now;
    let mut killed = 0;
    let mut targets: Vec<(u64, View, u8)> =
        cx.eng.clients.by_id.iter().filter(|(i, _)| **i != me).map(|(i, e)| (*i, e.view.clone(), client_type(&e.view, Some(&e.stats)))).collect();
    let mine = View::of(cx.client);
    let my_type = client_type(&mine, cx.client.conn.as_deref());
    targets.push((me, mine, my_type));
    targets.sort_by_key(|(i, _, _)| *i);
    for (cid, v, vtype) in targets {
        if addr.as_ref().is_some_and(|a| a[..] != *v.addr.as_bytes())
            || laddr.as_ref().is_some_and(|a| a[..] != *v.laddr.as_bytes())
            || ty.is_some_and(|t| t != vtype)
            || (id != 0 && cid != id)
            || user.as_ref().is_some_and(|u| !Arc::ptr_eq(u, &v.user))
            || (cid == me && skipme)
            || (max_age != 0 && now / 1000 - v.created_ms / 1000 < max_age)
        {
            continue;
        }
        if cid == me {
            // Closed after this reply.
            if let Some(s) = &cx.client.conn {
                s.killed.store(true, Ordering::Relaxed);
            }
        } else if let Some(e) = cx.eng.clients.by_id.get(&cid) {
            e.stats.killed.store(true, Ordering::Relaxed);
            if let Some(sink) = &e.sink {
                sink.kill();
            }
        } else {
            continue;
        }
        killed += 1;
    }
    if args.len() == 3 {
        return if killed == 0 { Err(Error::new("ERR No such client")) } else { Ok(Reply::Ok) };
    }
    Ok(Reply::Int(killed))
}

fn unblock(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    if args.len() > 4 {
        return Err(subcommand_syntax(args));
    }
    let mut error = false;
    if args.len() == 4 {
        if eq(&args[3], "timeout") {
            error = false;
        } else if eq(&args[3], "error") {
            error = true;
        } else {
            return Err(Error::new("ERR CLIENT UNBLOCK reason should be TIMEOUT or ERROR"));
        }
    }
    let target = int(&args[2])? as u64;
    let Some(sink) = cx.eng.blocking.waiters.get(&target).and_then(|w| w.client.sink.clone()) else { return Ok(Reply::Int(0)) };
    let resp = cx.eng.blocking.waiters[&target].client.resp;
    match cx.eng.unblock(target, error) {
        Some(r) => {
            sink.unblock(r.to_bytes(resp));
            Ok(Reply::Int(1))
        }
        None => Ok(Reply::Int(0)),
    }
}

fn pause(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    if args.len() > 4 {
        return Err(subcommand_syntax(args));
    }
    let mut all = true;
    if args.len() == 4 {
        if eq(&args[3], "write") {
            all = false;
        } else if !eq(&args[3], "all") {
            return Err(Error::new("ERR CLIENT PAUSE mode must be WRITE or ALL"));
        }
    }
    let t = crate::util::parse_i64(&args[2]).ok_or(Error::new("ERR timeout is not an integer or out of range"))?;
    if t < 0 {
        return Err(Error::new("ERR timeout is negative"));
    }
    let end = if t > 0 { cx.now + t } else { 0 };
    let old = cx.eng.clients.pause.map_or(0, |(e, _)| e);
    cx.eng.clients.pause = Some((old.max(end), all));
    Ok(Reply::Ok)
}

fn unpause(cx: &mut Cx, _args: &[Bytes]) -> CmdResult {
    cx.eng.clients.pause = None;
    Ok(Reply::Ok)
}

fn reply(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let mode = if eq(&args[2], "on") {
        ReplyMode::On
    } else if eq(&args[2], "off") {
        ReplyMode::Off
    } else if eq(&args[2], "skip") {
        // SKIP while OFF keeps it off.
        if cx.client.reply_mode == ReplyMode::Off { ReplyMode::Off } else { ReplyMode::Skip }
    } else {
        return Err(SYNTAX);
    };
    cx.client.reply_mode = mode;
    Ok(Reply::Ok)
}

fn on_off(a: &[u8]) -> Result<bool, Error> {
    if eq(a, "on") {
        Ok(true)
    } else if eq(a, "off") {
        Ok(false)
    } else {
        Err(SYNTAX)
    }
}

fn no_evict(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    cx.client.no_evict = on_off(&args[2])?;
    Ok(Reply::Ok)
}

fn no_touch(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    cx.client.no_touch = on_off(&args[2])?;
    Ok(Reply::Ok)
}

fn tracking(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let (mut redirect, mut bcast, mut optin, mut optout, mut noloop) = (0i64, false, false, false, false);
    let mut prefixes: Vec<Bytes> = Vec::new();
    let mut j = 3;
    while j < args.len() {
        let more = j + 1 < args.len();
        let a = &args[j];
        if eq(a, "redirect") && more {
            j += 1;
            if redirect != 0 {
                return Err(Error::new("ERR A client can only redirect to a single other client"));
            }
            redirect = int(&args[j])?;
            if redirect as u64 != cx.client.id && !cx.eng.clients.by_id.contains_key(&(redirect as u64)) {
                return Err(Error::new("ERR The client ID you want redirect to does not exist"));
            }
        } else if eq(a, "bcast") {
            bcast = true;
        } else if eq(a, "optin") {
            optin = true;
        } else if eq(a, "optout") {
            optout = true;
        } else if eq(a, "noloop") {
            noloop = true;
        } else if eq(a, "prefix") && more {
            j += 1;
            prefixes.push(args[j].clone());
        } else {
            return Err(SYNTAX);
        }
        j += 1;
    }
    let id = cx.client.id;
    if eq(&args[2], "on") {
        if !bcast && !prefixes.is_empty() {
            return Err(Error::new("ERR PREFIX option requires BCAST mode to be enabled"));
        }
        if let Some(t) = cx.eng.clients.track(id)
            && t.bcast != bcast
        {
            return Err(Error::new(
                "ERR You can't switch BCAST mode on/off before disabling tracking for this client, and then re-enabling it with a different mode.",
            ));
        }
        if bcast && (optin || optout) {
            return Err(Error::new("ERR OPTIN and OPTOUT are not compatible with BCAST"));
        }
        if optin && optout {
            return Err(Error::new("ERR You can't specify both OPTIN mode and OPTOUT mode"));
        }
        if let Some(t) = cx.eng.clients.track(id)
            && ((optin && t.optout) || (optout && t.optin))
        {
            return Err(Error::new(
                "ERR You can't switch OPTIN/OPTOUT mode before disabling tracking for this client, and then re-enabling it with a different mode.",
            ));
        }
        if bcast && let Some(e) = cx.eng.clients.prefix_collision(id, &prefixes) {
            return Err(Error::msg(e));
        }
        if !cx.eng.clients.by_id.contains_key(&id) {
            // Not a registered connection (embedded use): track it anyway.
            let mut c = cx.client.clone();
            c.conn.get_or_insert_with(Default::default);
            cx.eng.clients.register(&c);
        }
        let opts = crate::clients::Track { bcast, optin, optout, noloop, redirect: redirect as u64, prefixes: prefixes.into_iter().collect(), ..Default::default() };
        cx.eng.clients.enable_tracking(id, opts);
    } else if eq(&args[2], "off") {
        cx.eng.clients.disable_tracking(id);
    } else {
        return Err(SYNTAX);
    }
    Ok(Reply::Ok)
}

fn caching(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let id = cx.client.id;
    let Some(t) = cx.eng.clients.track_mut(id) else {
        return Err(Error::new("ERR CLIENT CACHING can be called only when the client is in tracking mode with OPTIN or OPTOUT mode enabled"));
    };
    if eq(&args[2], "yes") {
        if !t.optin {
            return Err(Error::new("ERR CLIENT CACHING YES is only valid when tracking is enabled in OPTIN mode."));
        }
    } else if eq(&args[2], "no") {
        if !t.optout {
            return Err(Error::new("ERR CLIENT CACHING NO is only valid when tracking is enabled in OPTOUT mode."));
        }
    } else {
        return Err(SYNTAX);
    }
    t.caching = true;
    Ok(Reply::Ok)
}

fn getredir(cx: &mut Cx, _args: &[Bytes]) -> CmdResult {
    Ok(Reply::Int(cx.eng.clients.track(cx.client.id).map_or(-1, |t| t.redirect as i64)))
}

fn trackinginfo(cx: &mut Cx, _args: &[Bytes]) -> CmdResult {
    let t = cx.eng.clients.track(cx.client.id);
    let mut flags = vec![Reply::bulk(if t.is_some() { "on" } else { "off" })];
    if let Some(t) = t {
        if t.bcast {
            flags.push(Reply::bulk("bcast"));
        }
        if t.optin {
            flags.push(Reply::bulk("optin"));
            if t.caching {
                flags.push(Reply::bulk("caching-yes"));
            }
        }
        if t.optout {
            flags.push(Reply::bulk("optout"));
            if t.caching {
                flags.push(Reply::bulk("caching-no"));
            }
        }
        if t.noloop {
            flags.push(Reply::bulk("noloop"));
        }
        if t.broken {
            flags.push(Reply::bulk("broken_redirect"));
        }
    }
    Ok(Reply::Map(vec![
        (Reply::bulk("flags"), Reply::Set(flags)),
        (Reply::bulk("redirect"), Reply::Int(t.map_or(-1, |t| t.redirect as i64))),
        (Reply::bulk("prefixes"), Reply::Array(t.map_or(Vec::new(), |t| t.prefixes.iter().cloned().map(Reply::Bulk).collect()))),
    ]))
}

fn client_help(_cx: &mut Cx, _args: &[Bytes]) -> CmdResult {
    Ok(help(&[
        "CLIENT <subcommand> [<arg> [value] [opt] ...]. Subcommands are:",
        "CACHING (YES|NO)",
        "    Enable/disable tracking of the keys for next command in OPTIN/OPTOUT modes.",
        "GETREDIR",
        "    Return the client ID we are redirecting to when tracking is enabled.",
        "GETNAME",
        "    Return the name of the current connection.",
        "ID",
        "    Return the ID of the current connection.",
        "INFO",
        "    Return information about the current client connection.",
        "KILL <ip:port>",
        "    Kill connection made from <ip:port>.",
        "KILL <option> <value> [<option> <value> [...]]",
        "    Kill connections. Options are:",
        "    * ADDR (<ip:port>|<unixsocket>:0)",
        "      Kill connections made from the specified address",
        "    * LADDR (<ip:port>|<unixsocket>:0)",
        "      Kill connections made to specified local address",
        "    * TYPE (NORMAL|MASTER|REPLICA|PUBSUB)",
        "      Kill connections by type.",
        "    * USER <username>",
        "      Kill connections authenticated by <username>.",
        "    * SKIPME (YES|NO)",
        "      Skip killing current connection (default: yes).",
        "    * ID <client-id>",
        "      Kill connections by client id.",
        "    * MAXAGE <maxage>",
        "      Kill connections older than the specified age.",
        "LIST [options ...]",
        "    Return information about client connections. Options:",
        "    * TYPE (NORMAL|MASTER|REPLICA|PUBSUB)",
        "      Return clients of specified type.",
        "UNPAUSE",
        "    Stop the current client pause, resuming traffic.",
        "PAUSE <timeout> [WRITE|ALL]",
        "    Suspend all, or just write, clients for <timeout> milliseconds.",
        "REPLY (ON|OFF|SKIP)",
        "    Control the replies sent to the current connection.",
        "SETNAME <name>",
        "    Assign the name <name> to the current connection.",
        "SETINFO <option> <value>",
        "    Set client meta attr. Options are:",
        "    * LIB-NAME: the client lib name.",
        "    * LIB-VER: the client lib version.",
        "UNBLOCK <clientid> [TIMEOUT|ERROR]",
        "    Unblock the specified blocked client.",
        "TRACKING (ON|OFF) [REDIRECT <id>] [BCAST] [PREFIX <prefix> [...]]",
        "         [OPTIN] [OPTOUT] [NOLOOP]",
        "    Control server assisted client side caching.",
        "TRACKINGINFO",
        "    Report tracking status for the current connection.",
        "NO-EVICT (ON|OFF)",
        "    Protect current client connection from eviction.",
        "NO-TOUCH (ON|OFF)",
        "    Will not touch LRU/LFU stats when this mode is on.",
        "HELP",
        "    Print this help.",
    ]))
}
