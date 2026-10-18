//! The legacy document commands: FT.ADD and FT.SAFEADD (`RSAddDocumentCommand`: an HSET of the
//! fields with the rule's score, language and payload fields), FT.GET and FT.MGET
//! (`Document_ReplyAllFields`: an HGETALL without those fields) and FT.DEL (a DEL of any key).

use super::ft::{not_found, rm_call, verify_acl};
use super::*;
use crate::engine::{Engine, Mode};
use crate::search::args::{Ac, c_eq};
use crate::search::error::{Code, QErr};
use crate::search::spec;
use crate::util::cstr;
use crate::value::Value;

const NOPERM: &str = "NOPERM User does not have the required permissions to query the index";

/// `Indexes_LoadIndexSpecUnsafe` (counting a use), then `ACLUserMayAccessIndex`.
fn open(cx: &mut Cx, name: &[u8]) -> Result<Bytes, Error> {
    let Some(real) = cx.eng.search.load(cstr(name), false) else { return Err(not_found(name)) };
    let ix = &cx.eng.search.indexes[&real];
    if !ix.spec.rule.prefixes.iter().all(|p| cx.client.user.can_access_prefix(cstr(p))) {
        return Err(Error::msg(NOPERM));
    }
    Ok(real)
}

fn call(cx: &mut Cx, args: Vec<Bytes>) -> Reply {
    cx.call(&args, Mode::Exec)
}

/// `Document_ReplyAllFields`: the hash's fields but the rule's language, score and payload
/// fields (case-insensitively), empty values as nulls; an empty array for a missing key or
/// another type.
fn reply_all_fields(cx: &mut Cx, index: &Bytes, key: &Bytes) -> Reply {
    let pairs: Vec<(Reply, Reply)> = match call(cx, vec![Bytes::from_static(b"HGETALL"), key.clone()]) {
        Reply::Array(items) => {
            let mut it = items.into_iter();
            let mut out = Vec::new();
            while let (Some(f), Some(v)) = (it.next(), it.next()) {
                out.push((f, v));
            }
            out
        }
        Reply::Map(m) => m,
        _ => return Reply::Array(Vec::new()),
    };
    let rule = &cx.eng.search.indexes[index].spec.rule;
    let special = |f: &[u8]| {
        [&rule.lang_field, &rule.score_field, &rule.payload_field].iter().any(|x| x.as_deref().is_some_and(|x| x.as_bytes().eq_ignore_ascii_case(f)))
    };
    let mut out = Vec::with_capacity(pairs.len() * 2);
    for (f, v) in pairs {
        let (Reply::Bulk(f), Reply::Bulk(v)) = (f, v) else { continue };
        if special(&f) {
            continue;
        }
        out.push(Reply::Bulk(f));
        out.push(if v.is_empty() { Reply::Nil } else { Reply::Bulk(v) });
    }
    Reply::Array(out)
}

/// FT.GET index key (`GetSingleDocumentCommand`): null when the index doesn't hold the key.
pub(super) fn ft_get(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    if args.len() != 3 {
        return Err(arity_error("FT.GET"));
    }
    let real = open(cx, &args[1])?;
    if cx.eng.search.indexes[&real].docs.get_id(&args[2]) == 0 {
        return Ok(Reply::Nil);
    }
    Ok(reply_all_fields(cx, &real, &args[2]))
}

/// FT.MGET index key... (`MGetCommandHandler`: `VERIFY_ACL`, then `_FT.MGET`'s
/// `GetDocumentsCommand`).
pub(super) fn ft_mget(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    if args.len() < 3 {
        return Err(arity_error("FT.MGET"));
    }
    verify_acl(cx, &args[1])?;
    rm_call(cx, "_FT.MGET", |cx| {
        let Some(real) = cx.eng.search.load(cstr(&args[1]), false) else { return Err(not_found(&args[1])) };
        let mut out = Vec::with_capacity(args.len() - 2);
        for key in &args[2..] {
            if cx.eng.search.indexes[&real].docs.get_id(key) == 0 {
                out.push(Reply::Nil);
            } else {
                out.push(reply_all_fields(cx, &real, key));
            }
        }
        Ok(Reply::Array(out))
    })
}

/// FT.DEL index key [DD] (`DeleteCommand`): a replicated DEL of the key, indexed or not; 1 when
/// it deleted it.
pub(super) fn ft_del(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    if !(3..=4).contains(&args.len()) {
        return Err(arity_error("FT.DEL"));
    }
    open(cx, &args[1])?;
    let r = call(cx, vec![Bytes::from_static(b"DEL"), args[2].clone()]);
    cx.no_propagate();
    Ok(Reply::Int(matches!(r, Reply::Int(1)) as i64))
}

/// FT.ADD / FT.SAFEADD's options (`parseDocumentOptions`).
#[derive(Default)]
struct AddOpts<'a> {
    replace: bool,
    partial: bool,
    nocreate: bool,
    payload: Option<&'a Bytes>,
    language: Option<&'a Bytes>,
    cond: Option<&'a Bytes>,
    fields: &'a [Bytes],
}

fn add_err(msg: impl Into<String>) -> QErr {
    QErr::new(Code::AddArgs, msg)
}

fn parse_add<'a>(ac: &mut Ac<'a>) -> Result<AddOpts<'a>, QErr> {
    let mut o = AddOpts::default();
    let mut found = false;
    while !ac.at_end() {
        let flag = ["REPLACE", "PARTIAL", "NOCREATE"].into_iter().position(|n| ac.advance_if(n));
        match flag {
            Some(0) => o.replace = true,
            Some(1) => o.partial = true,
            Some(_) => o.nocreate = true,
            None => {
                let Some(name) = ["PAYLOAD", "LANGUAGE", "IF"].into_iter().find(|n| ac.advance_if(n)) else {
                    let s = ac.take().expect("not at the end");
                    if c_eq(s, "FIELDS") {
                        if !ac.remaining().is_multiple_of(2) {
                            return Err(add_err("Fields must be specified in FIELD VALUE pairs"));
                        }
                        o.fields = ac.rest();
                        found = true;
                        break;
                    }
                    return Err(QErr::user(Code::AddArgs, "Unknown keyword", format!(" `{}` provided", String::from_utf8_lossy(s))));
                };
                let v = ac.get().map_err(|e| add_err(format!("Parsing error for document option {name}: {}", e.strerror())))?;
                match name {
                    "PAYLOAD" => o.payload = Some(v),
                    "LANGUAGE" => o.language = Some(v),
                    _ => o.cond = Some(v),
                }
            }
        }
    }
    if !found {
        return Err(add_err("No field list found"));
    }
    if let Some(l) = o.language
        && spec::find_language(l).is_none()
    {
        return Err(add_err("Unsupported language"));
    }
    Ok(o)
}

/// FT.ADD / FT.SAFEADD index key score [REPLACE [PARTIAL]] [NOCREATE] [PAYLOAD p]
/// [LANGUAGE l] [IF expr] FIELDS field value... (`RSAddDocumentCommand`, `RS_AddDocument`,
/// `Redis_SaveDocument`): OK, NOADD when IF is unmet, or the error.
pub(super) fn ft_add(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let name = if c_eq(&args[0], "FT.SAFEADD") { "FT.SAFEADD" } else { "FT.ADD" };
    if args.len() < 4 {
        return Err(arity_error(name));
    }
    let real = open(cx, &args[1])?;
    let key = &args[2];
    let mut ac = Ac::new(&args[3..]);
    let score = match ac.get_double(0) {
        Err(_) => return Err(add_err("Could not parse document score").into()),
        Ok(s) if !(0.0..=1.0).contains(&s) => return Err(add_err("Score must be between 0 and 1").into()),
        Ok(s) => s,
    };
    let o = parse_add(&mut ac)?;

    let db = cx.client.db;
    let exists = match super::ft::live(cx.eng, db, key).map(|e| &e.value) {
        None => false,
        Some(Value::Hash(_)) => true,
        Some(_) => return Err(QErr::code(Code::RedisKeyType).into()),
    };
    if !exists && o.nocreate {
        return Err(QErr::new(Code::NoDoc, "Document does not exist").into());
    }
    if exists && !o.replace {
        return Err(QErr::code(Code::DocExists).into());
    }
    if exists && let Some(cond) = o.cond {
        let eng: &Engine = cx.eng;
        match super::ft_aggregate::eval_if(eng, db, &eng.search.indexes[&real], key, cond) {
            Ok(true) => {}
            Ok(false) => return Ok(Reply::Status("NOADD".into())),
            Err(e) if e.code == Code::NoPropVal => return Ok(Reply::Status("NOADD".into())),
            Err(e) => return Err(e.into()),
        }
    }
    let start = cx.eng.effects.len();
    if exists && o.replace && !o.partial {
        // `RedisModule_Call(ctx, "DEL", "s", key)`: not replicated.
        call(cx, vec![Bytes::from_static(b"DEL"), key.clone()]);
        cx.eng.effects.truncate(start);
    }
    let mut hset = Vec::with_capacity(o.fields.len() + 8);
    hset.push(Bytes::from_static(b"HSET"));
    hset.push(key.clone());
    hset.extend(o.fields.iter().cloned());
    let rule = &mut cx.eng.search.indexes.get_mut(&real).expect("loaded").spec.rule;
    if score != 1.0 || o.partial {
        hset.push(Bytes::from_static(b"__score"));
        hset.push(args[3].clone());
        rule.score_field.get_or_insert_with(|| "__score".into());
    }
    if let Some(l) = o.language {
        hset.push(Bytes::from_static(b"__language"));
        hset.push(l.clone());
        rule.lang_field.get_or_insert_with(|| "__language".into());
    }
    if let Some(p) = o.payload {
        hset.push(Bytes::from_static(b"__payload"));
        hset.push(p.clone());
        rule.payload_field.get_or_insert_with(|| "__payload".into());
    }
    // `RedisModule_Call(ctx, "HSET", "!v", ...)`: replicated, its reply ignored (without a
    // field, the call fails its arity check before running).
    if hset.len() >= 4 {
        call(cx, hset);
    }
    cx.no_propagate();
    if matches!(cx.mode, Mode::Top | Mode::Unblock) {
        super::multi::wrap_effects(cx, start);
    }
    Ok(Reply::Status("OK".into()))
}
