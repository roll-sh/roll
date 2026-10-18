//! Schema and metadata commands of RediSearch: FT.ALTER (and `_ALTERIFNX`), aliases
//! (FT.ALIASADD/ALIASDEL/ALIASUPDATE/ALIASLIST and the IF[N]X twins), spelling dictionaries
//! (FT.DICTADD/DICTDEL/DICTDUMP), synonym groups (FT.SYNUPDATE/SYNDUMP, FT.SYNADD's refusal) and
//! FT.TAGVALS. The write commands run their `_FT.*` twins as FT.CREATE does; each replication
//! (`RedisModule_Replicate*`) counts as a change.

use super::ft::{not_found, rm_call, scan_if_any, verify_acl};
use super::*;
use crate::search::args::Ac;
use crate::search::error::{Code, QErr};
use crate::search::spec;
use crate::search::tokenize;
use std::collections::BTreeSet;

const NOPERM: &str = "NOPERM User does not have the required permissions to query the index";

fn cstr(b: &[u8]) -> &[u8] {
    crate::util::cstr(b)
}

/// `RedisModule_Replicate(cmd, "v", argv + 1, argc - 1)`.
fn replicate(cx: &mut Cx, name: &'static [u8], args: &[Bytes]) {
    let mut rep = Vec::with_capacity(args.len());
    rep.push(Bytes::from_static(name));
    rep.extend_from_slice(&args[1..]);
    cx.changes(1);
    cx.propagate_as(rep);
}

/// `RedisModule_ReplicateVerbatim`: the inner command as it ran.
fn replicate_verbatim(cx: &mut Cx, inner: &'static [u8], args: &[Bytes]) {
    replicate(cx, inner, args);
}

/// `MastersFanoutCommandHandler` with the index at `pos` (0 = indexless): the arity, then
/// `VERIFY_ACL`, then the twin.
fn fanout(cx: &mut Cx, args: &[Bytes], pos: usize, outer: &str, inner: &str, f: fn(&mut Cx, &[Bytes]) -> CmdResult) -> CmdResult {
    if args.len() < 2 {
        return Err(arity_error(outer));
    }
    if pos > 0 {
        if pos >= args.len() {
            return Err(arity_error(outer));
        }
        verify_acl(cx, &args[pos])?;
    }
    rm_call(cx, inner, |cx| f(cx, args))
}

// ---------------------------------------------------------------------------------------------
// FT.ALTER
// ---------------------------------------------------------------------------------------------

pub(super) fn ft_alter(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    fanout(cx, args, 1, "FT.ALTER", "_FT.ALTER", int_alter)
}

pub(super) fn ft_alterifnx(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    fanout(cx, args, 1, "FT._ALTERIFNX", "_FT._ALTERIFNX", int_alterifnx)
}

pub(super) fn int_alter(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    alter(cx, args, false)
}

pub(super) fn int_alterifnx(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    alter(cx, args, true)
}

/// `AlterIndexInternalCommand`.
fn alter(cx: &mut Cx, args: &[Bytes], if_nx: bool) -> CmdResult {
    if args.len() < 5 {
        return Err(arity_error(if if_nx { "_FT._ALTERIFNX" } else { "_FT.ALTER" }));
    }
    cx.changes(0);
    let Some(name) = cx.eng.search.load(cstr(&args[1]), false) else {
        return Err(not_found(&args[1]));
    };
    let mut ac = Ac::new(&args[2..]);
    let initial_scan = !ac.advance_if("SKIPINITIALSCAN");
    if !ac.advance_if("SCHEMA") {
        return Err(Error::msg("ALTER must be followed by SCHEMA"));
    }
    if !ac.advance_if("ADD") {
        return Err(Error::msg("Unknown action passed to ALTER SCHEMA"));
    }
    if ac.at_end() {
        return Err(Error::msg("No fields provided"));
    }
    if if_nx
        && let Some(first) = ac.peek()
        && cx.eng.search.indexes[&name].spec.field(first).is_some()
    {
        replicate(cx, b"_FT._ALTERIFNX", args);
        return Ok(Reply::Ok);
    }
    let cfg = cx.eng.cfg.clone();
    let ix = cx.eng.search.indexes.get_mut(&name).expect("resolved");
    ix.spec.add_fields(&mut ac, false, &cfg)?;
    ix.fields_added();
    if initial_scan {
        // `IndexSpec_ScanAndReindex`: only when the client's database has keys.
        let db = cx.db_index();
        scan_if_any(cx.eng, db, &name);
    }
    replicate(cx, b"_FT._ALTERIFNX", args);
    Ok(Reply::Ok)
}

// ---------------------------------------------------------------------------------------------
// Aliases
// ---------------------------------------------------------------------------------------------

pub(super) fn ft_aliasadd(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    fanout(cx, args, 2, "FT.ALIASADD", "_FT.ALIASADD", int_aliasadd)
}

pub(super) fn ft_aliasaddifnx(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    fanout(cx, args, 0, "FT._ALIASADDIFNX", "_FT._ALIASADDIFNX", int_aliasaddifnx)
}

pub(super) fn ft_aliasdel(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    fanout(cx, args, 0, "FT.ALIASDEL", "_FT.ALIASDEL", int_aliasdel)
}

pub(super) fn ft_aliasdelifx(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    fanout(cx, args, 0, "FT._ALIASDELIFX", "_FT._ALIASDELIFX", int_aliasdelifx)
}

pub(super) fn ft_aliasupdate(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    fanout(cx, args, 2, "FT.ALIASUPDATE", "_FT.ALIASUPDATE", int_aliasupdate)
}

pub(super) fn int_aliasadd(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    alias_add_command(cx, args, false, "_FT.ALIASADD")
}

pub(super) fn int_aliasaddifnx(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    alias_add_command(cx, args, true, "_FT._ALIASADDIFNX")
}

/// `AliasAddCommandCommon`.
fn alias_add_command(cx: &mut Cx, args: &[Bytes], if_nx: bool, name: &str) -> CmdResult {
    if args.len() != 3 {
        return Err(arity_error(name));
    }
    cx.changes(0);
    alias_add(cx, args, if_nx)?;
    replicate(cx, b"_FT._ALIASADDIFNX", args);
    Ok(Reply::Ok)
}

/// `aliasAddCommon`: the target must be an index (not an alias), the alias no index's name.
fn alias_add(cx: &mut Cx, args: &[Bytes], skip_if_exists: bool) -> Result<(), Error> {
    let Some(target) = cx.eng.search.load(cstr(&args[2]), true) else {
        return Err(QErr::new(Code::NoIndex, "Unknown index name (or name is an alias itself)").into());
    };
    let alias = args[1].clone();
    if cx.eng.search.indexes.contains_key(&alias) {
        return Err(QErr::code(Code::AliasConflict).into());
    }
    if skip_if_exists && cx.eng.search.aliases.get(&alias) == Some(&target) {
        return Ok(());
    }
    Ok(cx.eng.search.add_alias(alias, &target)?)
}

pub(super) fn int_aliasdel(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    if args.len() != 2 {
        return Err(arity_error("_FT.ALIASDEL"));
    }
    alias_del_command(cx, args)
}

pub(super) fn int_aliasdelifx(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    if args.len() != 2 {
        return Err(arity_error("_FT._ALIASDELIFX"));
    }
    if cx.eng.search.load(cstr(&args[1]), false).is_none() {
        return Ok(Reply::Ok);
    }
    alias_del_command(cx, args)
}

/// `AliasDelCommand`: the name must resolve (an index or an alias), then the alias is removed
/// from that index.
fn alias_del_command(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    cx.changes(0);
    let Some(target) = cx.eng.search.load(cstr(&args[1]), false) else {
        return Err(Error::msg("Alias does not exist"));
    };
    cx.eng.search.del_alias(&args[1], &target)?;
    replicate(cx, b"_FT._ALIASDELIFX", args);
    Ok(Reply::Ok)
}

/// `AliasUpdateCommand`: the alias leaves its index (if it has one), then joins the target; on
/// failure it goes back.
pub(super) fn int_aliasupdate(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    if args.len() != 3 {
        return Err(arity_error("_FT.ALIASUPDATE"));
    }
    cx.changes(0);
    let orig = cx.eng.search.load(cstr(&args[1]), false);
    if let Some(o) = &orig {
        cx.eng.search.del_alias(&args[1], o)?;
    }
    if let Err(e) = alias_add(cx, args, false) {
        if let Some(o) = &orig {
            let _ = cx.eng.search.add_alias(args[1].clone(), o);
        }
        return Err(e);
    }
    replicate_verbatim(cx, b"_FT.ALIASUPDATE", args);
    Ok(Reply::Ok)
}

/// FT.ALIASLIST index: the index's aliases (by index name only).
pub(super) fn ft_aliaslist(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    if args.len() != 2 {
        return Err(arity_error("FT.ALIASLIST"));
    }
    let Some(name) = cx.eng.search.load(cstr(&args[1]), true) else {
        return Err(not_found(&args[1]));
    };
    let ix = &cx.eng.search.indexes[&name];
    if !ix.spec.rule.prefixes.iter().all(|p| cx.client.user.can_access_prefix(cstr(p))) {
        return Err(Error::msg(NOPERM));
    }
    Ok(Reply::Set(ix.aliases.iter().map(|a| Reply::Bulk(a.clone())).collect()))
}

// ---------------------------------------------------------------------------------------------
// Spelling dictionaries
// ---------------------------------------------------------------------------------------------

/// `strToRunesN`: UTF-8 decoded to 16-bit runes, up to a NUL.
pub(crate) fn runes(s: &[u8]) -> Vec<u16> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < s.len() {
        let (c, n) = tokenize::utf8_read(&s[i..]);
        i += n.max(1);
        if c == 0 {
            break;
        }
        out.push(c as u16);
    }
    out
}

/// `runesToStr`.
pub(crate) fn runes_to_str(r: &[u16]) -> Vec<u8> {
    let mut out = Vec::with_capacity(r.len());
    for &c in r {
        tokenize::utf8_write(c as u32, &mut out);
    }
    out
}

pub(super) fn ft_dictadd(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    fanout(cx, args, 0, "FT.DICTADD", "_FT.DICTADD", int_dictadd)
}

pub(super) fn ft_dictdel(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    fanout(cx, args, 0, "FT.DICTDEL", "_FT.DICTDEL", int_dictdel)
}

/// `DictAddCommand` (`Dictionary_Add`): terms new to the dictionary's trie are counted.
pub(super) fn int_dictadd(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    if args.len() < 3 {
        return Err(arity_error("_FT.DICTADD"));
    }
    let name = Bytes::copy_from_slice(cstr(&args[1]));
    let dicts = &mut cx.eng.search.dicts;
    if !dicts.contains_key(&name) {
        dicts.add(name.clone(), Default::default());
    }
    let d = dicts.find_mut(&name).expect("just added");
    let mut added = 0;
    for v in &args[2..] {
        // `Trie_InsertStringBuffer`: at most 512 bytes, 1..255 runes.
        if v.len() > 512 {
            continue;
        }
        let r = runes(v);
        if r.is_empty() || r.len() >= 256 {
            continue;
        }
        // `Trie_Insert(…, 1, incr 1, …)`: a term already there scores one more.
        match d.get_mut(&r) {
            Some(score) => *score += 1.0,
            None => {
                d.insert(r, 1.0);
                added += 1;
            }
        }
    }
    replicate_verbatim(cx, b"_FT.DICTADD", args);
    Ok(Reply::Int(added))
}

/// `DictDelCommand` (`Dictionary_Del`): an emptied dictionary goes.
pub(super) fn int_dictdel(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    if args.len() < 3 {
        return Err(arity_error("_FT.DICTDEL"));
    }
    let name = cstr(&args[1]);
    let mut deleted = 0;
    if let Some(d) = cx.eng.search.dicts.find_mut(name) {
        for v in &args[2..] {
            let r = runes(v);
            if r.len() > 256 {
                continue;
            }
            if d.remove(&r).is_some() {
                deleted += 1;
            }
        }
        if d.is_empty() {
            cx.eng.search.dicts.delete(name);
        }
    }
    replicate_verbatim(cx, b"_FT.DICTDEL", args);
    Ok(Reply::Int(deleted))
}

/// FT.DICTDUMP dict: its terms in trie (rune) order.
pub(super) fn ft_dictdump(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    if args.len() != 2 {
        return Err(arity_error("FT.DICTDUMP"));
    }
    let Some(d) = cx.eng.search.dicts.find(cstr(&args[1])) else {
        return Ok(Reply::Set(Vec::new()));
    };
    Ok(Reply::Set(d.keys().map(|r| Reply::Bulk(Bytes::from(runes_to_str(r)))).collect()))
}

// ---------------------------------------------------------------------------------------------
// Synonyms
// ---------------------------------------------------------------------------------------------

/// FT.SYNADD: gone since 2.0.
pub(super) fn ft_synadd(_cx: &mut Cx, _args: &[Bytes]) -> CmdResult {
    Err(Error::msg("No longer supported, use FT.SYNUPDATE"))
}

pub(super) fn ft_synupdate(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    fanout(cx, args, 1, "FT.SYNUPDATE", "_FT.SYNUPDATE", int_synupdate)
}

const MAX_SYNONYM_TERMS: usize = 1_000_000;
const MAX_SYNONYM_GROUP_IDS: usize = 4096;

/// `SynUpdateCommand`: the lowercased terms join the group (`~id` per term), all or nothing.
pub(super) fn int_synupdate(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    if args.len() < 4 {
        return Err(arity_error("_FT.SYNUPDATE"));
    }
    let Some(name) = cx.eng.search.load(cstr(&args[1]), false) else {
        return Err(not_found(&args[1]));
    };
    let id = cstr(&args[2]);
    let (initial_scan, offset) = if crate::search::args::c_eq(&args[3], "SKIPINITIALSCAN") { (false, 4) } else { (true, 3) };
    let ix = cx.eng.search.indexes.get_mut(&name).expect("resolved");
    let map = ix.synonyms.get_or_insert_with(Default::default);
    let lowered: Vec<Bytes> = args[offset..].iter().map(|t| Bytes::from(tokenize::lowercase(t))).map(|t| Bytes::copy_from_slice(cstr(&t))).collect();
    let mut tagged = b"~".to_vec();
    tagged.extend_from_slice(id);
    let tagged = Bytes::from(tagged);
    let mut pending = BTreeSet::new();
    for t in &lowered {
        match map.get(t) {
            Some(ids) => {
                if !ids.contains(&tagged) && ids.len() >= MAX_SYNONYM_GROUP_IDS {
                    return Err(Error::msg("Maximum group IDs per term limit reached"));
                }
            }
            None => {
                pending.insert(t.clone());
            }
        }
    }
    if map.len() + pending.len() > MAX_SYNONYM_TERMS {
        return Err(Error::msg("Maximum synonym terms limit reached"));
    }
    for t in lowered {
        if map.find(&t).is_none() {
            map.add(t.clone(), Vec::new());
        }
        let ids = map.find_mut(&t).expect("present");
        if !ids.contains(&tagged) {
            ids.push(tagged.clone());
        }
    }
    if initial_scan {
        let db = cx.db_index();
        scan_if_any(cx.eng, db, &name);
    }
    replicate_verbatim(cx, b"_FT.SYNUPDATE", args);
    Ok(Reply::Ok)
}

/// FT.SYNDUMP index: each term with its groups (in the synonym dict's order).
pub(super) fn ft_syndump(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    if args.len() != 2 {
        return Err(arity_error("FT.SYNDUMP"));
    }
    let Some(name) = cx.eng.search.load(cstr(&args[1]), false) else {
        return Err(not_found(&args[1]));
    };
    let ix = &cx.eng.search.indexes[&name];
    if !ix.spec.rule.prefixes.iter().all(|p| cx.client.user.can_access_prefix(cstr(p))) {
        return Err(Error::msg(NOPERM));
    }
    let Some(map) = &ix.synonyms else { return Ok(Reply::Map(Vec::new())) };
    Ok(Reply::Map(
        map.iter().map(|(t, ids)| (Reply::Bulk(t.clone()), Reply::Array(ids.iter().map(|i| Reply::Bulk(i.slice(1..))).collect()))).collect(),
    ))
}

// ---------------------------------------------------------------------------------------------
// FT.TAGVALS
// ---------------------------------------------------------------------------------------------

/// FT.TAGVALS index field (`TagValsCommandHandler`: `VERIFY_ACL`, then `_FT.TAGVALS`).
pub(super) fn ft_tagvals(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    if args.len() < 3 {
        return Err(arity_error("FT.TAGVALS"));
    }
    verify_acl(cx, &args[1])?;
    rm_call(cx, "_FT.TAGVALS", |cx| int_tagvals(cx, args))
}

/// `TagValsCommand`: every value the tag field's index holds (values outlive their documents).
pub(super) fn int_tagvals(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    if args.len() != 3 {
        return Err(arity_error("_FT.TAGVALS"));
    }
    let Some(name) = cx.eng.search.load(cstr(&args[1]), false) else {
        return Err(not_found(&args[1]));
    };
    let ix = &cx.eng.search.indexes[&name];
    let Some(fi) = ix.spec.fields.iter().position(|f| f.name.as_ref() == args[2].as_ref()) else {
        return Err(QErr::new(Code::BadAttr, "No such field").into());
    };
    if !ix.spec.fields[fi].is(spec::T_TAG) {
        return Err(QErr::new(Code::BadAttr, "Not a tag field").into());
    }
    let Some(tags) = &ix.fields[fi].tag else { return Ok(Reply::Set(Vec::new())) };
    Ok(Reply::Set(tags.values.keys().into_iter().map(|k| Reply::Bulk(Bytes::from(k))).collect()))
}
