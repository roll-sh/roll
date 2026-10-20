//! Pub/Sub commands.

use super::*;
use crate::hub::{Kind as SubKind, Sub};
use crate::{cmd, container};

const PS: u64 = PUBSUB | NOSCRIPT | LOADING | STALE | SUBSCRIBED_OK;

pub static COMMANDS: &[Command] = &[
    cmd!("subscribe", -2, PS | DENYOOM, 0, 0, 0, subscribe, PubSub),
    cmd!("psubscribe", -2, PS | DENYOOM, 0, 0, 0, psubscribe, PubSub),
    cmd!("ssubscribe", -2, PS | DENYOOM, 1, -1, 1, ssubscribe, PubSub),
    cmd!("unsubscribe", -1, PS, 0, 0, 0, unsubscribe, PubSub),
    cmd!("punsubscribe", -1, PS, 0, 0, 0, punsubscribe, PubSub),
    cmd!("sunsubscribe", -1, PS, 1, -1, 1, sunsubscribe, PubSub),
    cmd!("publish", 3, PUBSUB | LOADING | STALE | FAST | MAY_REPLICATE, 0, 0, 0, publish, PubSub),
    cmd!("spublish", 3, PUBSUB | LOADING | STALE | FAST | MAY_REPLICATE, 1, 1, 1, spublish, PubSub),
    container!("pubsub", 0, PubSub, PUBSUB_SUBS),
];

static PUBSUB_SUBS: &[Command] = &[
    cmd!("channels", -2, PUBSUB | LOADING | STALE, 0, 0, 0, channels, PubSub),
    cmd!("numsub", -2, PUBSUB | LOADING | STALE, 0, 0, 0, numsub, PubSub),
    cmd!("numpat", 2, PUBSUB | LOADING | STALE, 0, 0, 0, numpat, PubSub),
    cmd!("shardchannels", -2, PUBSUB | LOADING | STALE, 0, 0, 0, shardchannels, PubSub),
    cmd!("shardnumsub", -2, PUBSUB | LOADING | STALE, 0, 0, 0, shardnumsub, PubSub),
    cmd!("help", 2, LOADING | STALE, 0, 0, 0, pubsub_help, PubSub),
];

fn sink(cx: &Cx) -> Result<Sub, Error> {
    let sink = cx.client.sink.clone().ok_or(Error::new("ERR Pub/Sub needs a connection"))?;
    Ok(Sub { sink, resp: cx.client.resp })
}

fn confirm(kind: &'static str, name: Option<&Bytes>, count: usize) -> Reply {
    Reply::Push(vec![
        Reply::bulk(kind),
        name.map_or(Reply::Nil, |n| Reply::Bulk(n.clone())),
        Reply::Int(count as i64),
    ])
}

fn sub_generic(cx: &mut Cx, args: &[Bytes], kind: SubKind) -> CmdResult {
    if cx.client.in_multi() && cx.mode != crate::engine::Mode::Top {
        // SUBSCRIBE inside MULTI is allowed by Redis but pointless; keep its semantics.
    }
    let sub = sink(cx)?;
    let mut out = Vec::new();
    for ch in &args[1..] {
        let (label, newly) = match kind {
            SubKind::Channel => ("subscribe", cx.client.subs.insert(ch.clone())),
            SubKind::Pattern => ("psubscribe", cx.client.psubs.insert(ch.clone())),
            SubKind::Shard => ("ssubscribe", cx.client.ssubs.insert(ch.clone())),
        };
        if newly {
            let k = kind;
            cx.eng.hub.subscribe(k, ch, cx.client.id, sub.clone());
        }
        let count = match kind {
            SubKind::Shard => cx.client.ssubs.len(),
            _ => cx.client.subs.len() + cx.client.psubs.len(),
        };
        out.push(confirm(label, Some(ch), count));
    }
    Ok(Reply::Seq(out))
}

fn unsub_generic(cx: &mut Cx, args: &[Bytes], kind: SubKind) -> CmdResult {
    let label = match kind {
        SubKind::Channel => "unsubscribe",
        SubKind::Pattern => "punsubscribe",
        SubKind::Shard => "sunsubscribe",
    };
    let targets: Vec<Bytes> = if args.len() > 1 {
        args[1..].to_vec()
    } else {
        match kind {
            SubKind::Channel => cx.client.subs.iter().cloned().collect(),
            SubKind::Pattern => cx.client.psubs.iter().cloned().collect(),
            SubKind::Shard => cx.client.ssubs.iter().cloned().collect(),
        }
    };
    let id = cx.client.id;
    let mut out = Vec::new();
    for ch in &targets {
        let set = match kind {
            SubKind::Channel => &mut cx.client.subs,
            SubKind::Pattern => &mut cx.client.psubs,
            SubKind::Shard => &mut cx.client.ssubs,
        };
        if set.remove(ch) {
            let k = kind;
            cx.eng.hub.unsubscribe(k, ch, id);
        }
        let count = match kind {
            SubKind::Shard => cx.client.ssubs.len(),
            _ => cx.client.subs.len() + cx.client.psubs.len(),
        };
        out.push(confirm(label, Some(ch), count));
    }
    if targets.is_empty() {
        let count = match kind {
            SubKind::Shard => cx.client.ssubs.len(),
            _ => cx.client.subs.len() + cx.client.psubs.len(),
        };
        out.push(confirm(label, None, count));
    }
    Ok(Reply::Seq(out))
}

pub fn subscribe(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    sub_generic(cx, args, SubKind::Channel)
}
pub fn psubscribe(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    sub_generic(cx, args, SubKind::Pattern)
}
pub fn ssubscribe(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    sub_generic(cx, args, SubKind::Shard)
}
pub fn unsubscribe(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    unsub_generic(cx, args, SubKind::Channel)
}
pub fn punsubscribe(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    unsub_generic(cx, args, SubKind::Pattern)
}
pub fn sunsubscribe(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    unsub_generic(cx, args, SubKind::Shard)
}

/// Remove every subscription a client holds (RESET, disconnect).
pub fn unsubscribe_all(cx: &mut Cx) {
    let id = cx.client.id;
    for ch in cx.client.subs.drain() {
        cx.eng.hub.unsubscribe(SubKind::Channel, &ch, id);
    }
    for ch in cx.client.psubs.drain() {
        cx.eng.hub.unsubscribe(SubKind::Pattern, &ch, id);
    }
    for ch in cx.client.ssubs.drain() {
        cx.eng.hub.unsubscribe(SubKind::Shard, &ch, id);
    }
}

pub fn publish(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    Ok(Reply::Int(cx.eng.hub.publish(&args[1], &args[2]) as i64))
}

pub fn spublish(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    Ok(Reply::Int(cx.eng.hub.spublish(&args[1], &args[2]) as i64))
}

pub fn channels(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    if args.len() > 3 {
        return Err(arity_error("pubsub|channels"));
    }
    let list = cx.eng.hub.channels(SubKind::Channel, args.get(2).map(|b| &b[..]));
    Ok(Reply::Array(list.into_iter().map(Reply::Bulk).collect()))
}

pub fn shardchannels(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    if args.len() > 3 {
        return Err(arity_error("pubsub|shardchannels"));
    }
    let list = cx.eng.hub.channels(SubKind::Shard, args.get(2).map(|b| &b[..]));
    Ok(Reply::Array(list.into_iter().map(Reply::Bulk).collect()))
}

pub fn numsub(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let mut out = Vec::new();
    for ch in &args[2..] {
        out.push((Reply::Bulk(ch.clone()), Reply::Int(cx.eng.hub.numsub(SubKind::Channel, ch) as i64)));
    }
    Ok(Reply::Map(out))
}

pub fn shardnumsub(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let mut out = Vec::new();
    for ch in &args[2..] {
        out.push((Reply::Bulk(ch.clone()), Reply::Int(cx.eng.hub.numsub(SubKind::Shard, ch) as i64)));
    }
    Ok(Reply::Map(out))
}

pub fn numpat(cx: &mut Cx, _args: &[Bytes]) -> CmdResult {
    Ok(Reply::Int(cx.eng.hub.numpat() as i64))
}

pub fn pubsub_help(_cx: &mut Cx, _args: &[Bytes]) -> CmdResult {
    Ok(help(&[
        "PUBSUB <subcommand> [<arg> [value] [opt] ...]. Subcommands are:",
        "CHANNELS [<pattern>]",
        "    Return the currently active channels matching a <pattern> (default: '*').",
        "NUMPAT",
        "    Return number of subscriptions to patterns.",
        "NUMSUB [<channel> ...]",
        "    Return the number of subscribers for the specified channels, excluding",
        "    pattern subscriptions(default: no channels).",
        "SHARDCHANNELS [<pattern>]",
        "    Return the currently active shard level channels matching a <pattern> (default: '*').",
        "SHARDNUMSUB [<shardchannel> ...]",
        "    Return the number of subscribers for the specified shard level channel(s)",
        "HELP",
        "    Print this help.",
    ]))
}
