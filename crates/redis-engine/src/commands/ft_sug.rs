//! RediSearch's auto-complete dictionaries (`suggest.c`): FT.SUGADD, FT.SUGGET, FT.SUGDEL and
//! FT.SUGLEN over a score-sorted rune trie stored as the module's `trietype0` key type.
//!
//! The module's key conventions hold: a key opened for writing is signalled as modified whatever
//! the command then does, a new trie is stored like `RM_ModuleTypeSetValue` (a `new` event, no
//! other), an emptied one is removed like `RM_DeleteKey` (no event), and only the module's
//! `RM_ReplicateVerbatim` counts a change (FT.SUGDEL replicates before it even looks at the key).

use super::bloom::{double, reopen};
use super::*;
use crate::search::args::{Ac, AcErr, COALESCE, GE1};
use crate::search::error::{Code, QErr};
use crate::search::levenshtein::MatchMode;
use crate::search::trie::{SortMode, TRIE_ERR_PAYLOAD_OVERFLOW, TRIE_MAX_PREFIX, Trie};
use crate::value::Value;

/// What `RM_OpenKey` finds: nothing, another type, or a trie.
enum Found {
    Empty,
    Other,
    Trie,
}

/// `RM_OpenKey(READ | WRITE)`: an uncounted lookup, the key signalled as modified.
fn open_write(cx: &mut Cx, key: &[u8]) -> Found {
    cx.touch(key);
    match cx.peek(key) {
        None => Found::Empty,
        Some(e) if matches!(e.value, Value::Sug(_)) => Found::Trie,
        Some(_) => Found::Other,
    }
}

/// FT.SUGADD key string score [INCR] [PAYLOAD payload]
pub(super) fn sugadd(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    if args.len() < 4 || args.len() > 7 {
        return Err(arity_error("FT.SUGADD"));
    }
    let mut incr = false;
    let mut payload: Option<&Bytes> = None;
    let mut ac = Ac::new(&args[4..]);
    while let Some(s) = ac.take() {
        let name = crate::util::cstr(s);
        if name.eq_ignore_ascii_case(b"INCR") {
            incr = true;
        } else if name.eq_ignore_ascii_case(b"PAYLOAD") {
            match ac.get() {
                Ok(p) => payload = Some(p),
                Err(e) => return Err(Error::msg(format!("Invalid payload: {}", e.strerror()))),
            }
        } else {
            return Err(Error::msg(format!("Unknown argument `{}`", String::from_utf8_lossy(name))));
        }
    }
    cx.changes(0);
    let key = &args[1];
    let found = open_write(cx, key);
    if let Found::Other = found {
        return Err(WRONGTYPE);
    }
    let Some(score) = double(&args[3]) else {
        return Err(Error::msg("ERR invalid score"));
    };
    if let Found::Empty = found {
        cx.set(key, Value::Sug(Box::new(Trie::new(SortMode::Score))), false);
    }
    let mut k = reopen(cx, key);
    let t = k.sug();
    let rc = t.insert(&args[2], score, incr, payload.map(|p| p.as_ref()), 0);
    let size = t.size() as i64;
    drop(k);
    if rc == TRIE_ERR_PAYLOAD_OVERFLOW {
        return Err(Error::msg("Payload too large"));
    }
    cx.changes(1);
    Ok(Reply::Int(size))
}

/// FT.SUGLEN key
pub(super) fn suglen(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    if args.len() != 2 {
        return Err(arity_error("FT.SUGLEN"));
    }
    match cx.get(&args[1]) {
        None => Ok(Reply::Int(0)),
        Some(Value::Sug(t)) => Ok(Reply::Int(t.size() as i64)),
        Some(_) => Err(WRONGTYPE),
    }
}

/// FT.SUGDEL key string
pub(super) fn sugdel(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    if args.len() != 3 {
        return Err(arity_error("FT.SUGDEL"));
    }
    cx.changes(1);
    let key = &args[1];
    match open_write(cx, key) {
        Found::Other => return Err(WRONGTYPE),
        Found::Empty => return Ok(Reply::Int(0)),
        Found::Trie => {}
    }
    let mut k = reopen(cx, key);
    let t = k.sug();
    let deleted = t.delete(&args[2]);
    let empty = t.size() == 0;
    drop(k);
    if empty {
        cx.delete(key);
    }
    Ok(Reply::Int(deleted as i64))
}

/// FT.SUGGET key prefix [FUZZY] [MAX num] [WITHSCORES] [TRIM] [OPTIMIZE] [WITHPAYLOADS]
pub(super) fn sugget(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    if args.len() < 3 || args.len() > 10 {
        return Err(arity_error("FT.SUGGET"));
    }
    let prefix = &args[2];
    if prefix.len() > TRIE_MAX_PREFIX * 2 {
        return Err(Error::msg("Invalid query length"));
    }
    let (mut fuzzy, mut with_scores, mut trim, mut optimize, mut with_payloads) = (false, false, false, false, false);
    let mut num = 5u32;
    // `AC_ParseArgSpec`.
    let mut ac = Ac::new(&args[3..]);
    while let Some(s) = ac.peek() {
        let flag = match s.to_ascii_uppercase().as_slice() {
            b"FUZZY" => Some(&mut fuzzy),
            b"WITHSCORES" => Some(&mut with_scores),
            b"OPTIMIZE" => Some(&mut optimize),
            b"TRIM" => Some(&mut trim),
            b"WITHPAYLOADS" => Some(&mut with_payloads),
            b"MAX" => None,
            _ => {
                let arg = String::from_utf8_lossy(crate::util::cstr(s)).into_owned();
                return Err(QErr::user(Code::ParseArgs, "Unrecognized argument", format!(": {arg}")).into());
            }
        };
        let _ = ac.advance();
        match flag {
            Some(f) => *f = true,
            None => match ac.get_u32(COALESCE | GE1) {
                Ok(n) => num = n,
                Err(e) => return Err(max_error(e)),
            },
        }
    }
    let max_dist = if fuzzy { 1 } else { 0 };
    let t = match cx.get(&args[1]) {
        None => return Ok(Reply::Array(Vec::new())),
        Some(Value::Sug(t)) => t,
        Some(_) => return Err(WRONGTYPE),
    };
    let Some(res) = t.collect_fuzzy(prefix, num as usize, max_dist, MatchMode::Prefix, trim, optimize) else {
        return Err(Error::msg("Invalid query"));
    };
    let mut out = Vec::with_capacity(res.len() * (1 + with_scores as usize + with_payloads as usize));
    for e in res {
        out.push(Reply::Bulk(Bytes::from(e.str)));
        if with_scores {
            out.push(Reply::Double(e.score as f64));
        }
        if with_payloads {
            out.push(e.payload.map_or(Reply::Nil, |p| Reply::Bulk(Bytes::from(p))));
        }
    }
    Ok(Reply::Array(out))
}

fn max_error(e: AcErr) -> Error {
    QErr::user(Code::ParseArgs, "MAX", format!(": {}", e.strerror())).into()
}
