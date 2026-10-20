//! Key extraction the way Redis does it (`getKeysFromCommandWithSpecs`): interpret the command's
//! key specs (from the captured metadata), and when they are incomplete, carry variable flags or
//! don't fit the arguments, fall back to the command's own parser (`*GetKeys` in db.c), which
//! also knows the per-key access flags.

use super::{Command, meta};
use crate::Reply;
use bytes::Bytes;
use std::collections::HashMap;
use std::sync::OnceLock;

pub const RO: u16 = 1 << 0;
pub const RW: u16 = 1 << 1;
pub const OW: u16 = 1 << 2;
pub const RM: u16 = 1 << 3;
pub const ACCESS: u16 = 1 << 4;
pub const UPDATE: u16 = 1 << 5;
pub const INSERT: u16 = 1 << 6;
pub const DELETE: u16 = 1 << 7;
pub const NOT_KEY: u16 = 1 << 8;
pub const INCOMPLETE: u16 = 1 << 9;
pub const VARIABLE_FLAGS: u16 = 1 << 10;
pub const PREFIX: u16 = 1 << 11;

const FLAG_NAMES: &[(u16, &str)] = &[
    (RO, "RO"),
    (RW, "RW"),
    (OW, "OW"),
    (RM, "RM"),
    (ACCESS, "access"),
    (UPDATE, "update"),
    (INSERT, "insert"),
    (DELETE, "delete"),
    (NOT_KEY, "not_key"),
    (INCOMPLETE, "incomplete"),
    (VARIABLE_FLAGS, "variable_flags"),
];

/// Flag names as `COMMAND GETKEYSANDFLAGS` prints them.
pub fn flag_names(flags: u16) -> Vec<&'static str> {
    FLAG_NAMES.iter().filter(|(f, _)| flags & f != 0).map(|(_, n)| *n).collect()
}

#[derive(Debug)]
enum Begin {
    Index(i64),
    Keyword(String, i64),
    Unknown,
}

#[derive(Debug)]
enum Find {
    Range { last: i64, step: i64, limit: i64 },
    Keynum { idx: i64, first: i64, step: i64 },
    Unknown,
}

#[derive(Debug)]
pub struct Spec {
    flags: u16,
    begin: Begin,
    find: Find,
}

fn field<'a>(m: &'a Reply, name: &str) -> Option<&'a Reply> {
    match m {
        Reply::Map(pairs) => pairs.iter().find(|(k, _)| matches!(k, Reply::Bulk(b) if &b[..] == name.as_bytes())).map(|(_, v)| v),
        _ => None,
    }
}

fn int(m: &Reply, name: &str) -> i64 {
    match field(m, name) {
        Some(Reply::Int(n)) => *n,
        _ => 0,
    }
}

fn text(r: Option<&Reply>) -> String {
    match r {
        Some(Reply::Bulk(b)) => String::from_utf8_lossy(b).into_owned(),
        Some(Reply::Status(s)) => s.clone(),
        _ => String::new(),
    }
}

fn parse(spec: &Reply) -> Spec {
    let mut flags = 0;
    if let Some(Reply::Set(v) | Reply::Array(v)) = field(spec, "flags") {
        for f in v {
            let name = text(Some(f));
            if let Some((bit, _)) = FLAG_NAMES.iter().find(|(_, n)| n.eq_ignore_ascii_case(&name)) {
                flags |= bit;
            }
        }
    }
    let begin = match field(spec, "begin_search") {
        Some(bs) => {
            let s = field(bs, "spec").cloned().unwrap_or(Reply::Nil);
            match text(field(bs, "type")).as_str() {
                "index" => Begin::Index(int(&s, "index")),
                "keyword" => Begin::Keyword(text(field(&s, "keyword")), int(&s, "startfrom")),
                _ => Begin::Unknown,
            }
        }
        None => Begin::Unknown,
    };
    let find = match field(spec, "find_keys") {
        Some(fk) => {
            let s = field(fk, "spec").cloned().unwrap_or(Reply::Nil);
            match text(field(fk, "type")).as_str() {
                "range" => Find::Range { last: int(&s, "lastkey"), step: int(&s, "keystep"), limit: int(&s, "limit") },
                "keynum" => Find::Keynum { idx: int(&s, "keynumidx"), first: int(&s, "firstkey"), step: int(&s, "keystep") },
                _ => Find::Unknown,
            }
        }
        None => Find::Unknown,
    };
    Spec { flags, begin, find }
}

/// The key specs of a command by full name.
pub fn specs(full: &str) -> &'static [Spec] {
    static SPECS: OnceLock<HashMap<String, Vec<Spec>>> = OnceLock::new();
    let all = SPECS.get_or_init(|| {
        meta::names_all()
            .map(|n| {
                let specs = match meta::get(n).map(|m| m.key_specs()) {
                    Some(Reply::Array(v) | Reply::Set(v)) => v.iter().map(parse).collect(),
                    _ => Vec::new(),
                };
                (n.clone(), specs)
            })
            .collect()
    });
    all.get(&full.to_ascii_lowercase()).map_or(&[], Vec::as_slice)
}

/// Whether the command can have keys at all (`doesCommandHaveKeys`).
pub fn has_keys(cmd: &Command, full: &str) -> bool {
    let specs = specs(full);
    if specs.is_empty() && !meta::exists(full) {
        return cmd.first_key > 0 || cmd.keys_fn.is_some();
    }
    proc_name(full).is_some() || specs.iter().any(|s| s.flags & NOT_KEY == 0)
}

/// `getKeysUsingKeySpecs` with GET_KEYSPEC_DEFAULT: `None` when a spec is invalid or incomplete.
fn using_specs(specs: &[Spec], args: &[Bytes], cmd: &Command) -> Option<Vec<(usize, u16)>> {
    let argc = args.len() as i64;
    let mut out = Vec::new();
    for s in specs {
        if s.flags & NOT_KEY != 0 {
            continue;
        }
        let mut first = match &s.begin {
            Begin::Index(p) => *p,
            Begin::Keyword(kw, from) => {
                let (start, end) = if *from > 0 { (*from, argc - 1) } else { (argc + from, 1) };
                let mut found = 0;
                let mut i = start;
                while i != end {
                    if i >= argc || i < 1 {
                        break;
                    }
                    if args[i as usize].eq_ignore_ascii_case(kw.as_bytes()) {
                        found = i + 1;
                        break;
                    }
                    i = if start <= end { i + 1 } else { i - 1 };
                }
                if found == 0 {
                    continue;
                }
                found
            }
            Begin::Unknown => return None,
        };
        let (last, step) = match s.find {
            Find::Range { last, step, limit } => {
                let l = if last >= 0 {
                    first + last
                } else if limit == 0 {
                    argc + last
                } else {
                    first + ((argc - first) / limit + last)
                };
                (l, step)
            }
            Find::Keynum { idx, first: f, step } => {
                let at = first + idx;
                if at >= argc || at < 0 {
                    return None;
                }
                let n = crate::util::parse_i64(&args[at as usize]).filter(|n| *n >= 0)?;
                first += f;
                (first + (n - 1) * step, step)
            }
            Find::Unknown => return None,
        };
        if last >= argc || last < first || first >= argc {
            return None;
        }
        let mut i = first;
        while i <= last {
            if i >= argc || i < first {
                if cmd.arity < 0 {
                    i += step.max(1);
                    continue;
                }
                return None;
            }
            out.push((i as usize, s.flags));
            i += step.max(1);
        }
        if s.flags & INCOMPLETE != 0 {
            return None;
        }
    }
    Some(out)
}

/// The command's own key parser, if Redis has one.
fn proc_name(full: &str) -> Option<&'static str> {
    Some(match full {
        "bitfield" => "bitfield",
        "migrate" => "migrate",
        "sort" => "sort",
        "sort_ro" => "sort_ro",
        "georadius" | "georadiusbymember" => "georadius",
        "pfmerge" => "pfmerge",
        "lmpop" => "lmpop",
        "blmpop" | "bzmpop" => "blmpop",
        "zmpop" => "zmpop",
        "eval" | "evalsha" | "eval_ro" | "evalsha_ro" => "eval",
        "fcall" | "fcall_ro" => "eval",
        "sdiffcard" | "sintercard" | "sunioncard" => "card",
        "zdiff" | "zinter" | "zintercard" | "zunion" => "zunion",
        "zdiffstore" | "zinterstore" | "zunionstore" => "zunionstore",
        "xread" | "xreadgroup" => "xread",
        "delex" => "delex",
        "set" => "set",
        _ => return None,
    })
}

/// `genericGetKeys`: a count at `count_at` (parsed like atoi), then keys from `first`.
fn generic(store: usize, count_at: usize, first: usize, step: usize, args: &[Bytes]) -> Vec<(usize, u16)> {
    if count_at >= args.len() {
        return Vec::new();
    }
    let n = atoi(&args[count_at]);
    if n < 1 || n > ((args.len() as i64 - first as i64) / step as i64) {
        return Vec::new();
    }
    let mut out: Vec<(usize, u16)> = (0..n as usize).map(|i| (first + i * step, 0)).collect();
    if store > 0 {
        out.push((store, 0));
    }
    out
}

fn atoi(a: &[u8]) -> i64 {
    let s = a.iter().position(|c| !c.is_ascii_whitespace()).map_or(&a[a.len()..], |p| &a[p..]);
    let (neg, digits) = match s.first() {
        Some(b'-') => (true, &s[1..]),
        Some(b'+') => (false, &s[1..]),
        _ => (false, s),
    };
    let mut v: i64 = 0;
    for &c in digits.iter().take_while(|c| c.is_ascii_digit()) {
        v = v.wrapping_mul(10).wrapping_add((c - b'0') as i64);
    }
    let v = if neg { -v } else { v };
    v as i32 as i64
}

fn eq(a: &[u8], b: &str) -> bool {
    a.eq_ignore_ascii_case(b.as_bytes())
}

fn by_proc(name: &str, args: &[Bytes]) -> Vec<(usize, u16)> {
    let argc = args.len();
    match name {
        "eval" => generic(0, 2, 3, 1, args),
        "lmpop" | "zmpop" => generic(0, 1, 2, 1, args),
        "blmpop" => generic(0, 2, 3, 1, args),
        "card" | "zunion" => generic(0, 1, 2, 1, args),
        "zunionstore" => generic(1, 2, 3, 1, args),
        "sort_ro" => vec![(1, RO | ACCESS)],
        "sort" => {
            let mut out = vec![(1, RO | ACCESS)];
            let mut store = None;
            let mut i = 2;
            while i < argc {
                if eq(&args[i], "limit") {
                    i += 2;
                } else if eq(&args[i], "get") || eq(&args[i], "by") {
                    i += 1;
                } else if eq(&args[i], "store") && i + 1 < argc {
                    store = Some(i + 1);
                    i += 1;
                }
                i += 1;
            }
            out.extend(store.map(|s| (s, OW | UPDATE)));
            out
        }
        "pfmerge" => {
            let mut out = vec![(1, RW | ACCESS | INSERT)];
            out.extend((2..argc).map(|i| (i, RO | ACCESS)));
            out
        }
        "migrate" => {
            let (mut first, mut num) = (3, 1);
            if argc > 6 {
                let mut i = 6;
                while i < argc {
                    if eq(&args[i], "keys") {
                        if !args[3].is_empty() {
                            num = 0;
                        } else {
                            first = i + 1;
                            num = argc - first;
                        }
                        break;
                    }
                    if eq(&args[i], "auth") {
                        i += 1;
                    } else if eq(&args[i], "auth2") {
                        i += 2;
                    }
                    i += 1;
                }
            }
            (first..first + num).map(|i| (i, RW | ACCESS | DELETE)).collect()
        }
        "georadius" => {
            let mut stored = None;
            let mut i = 5;
            while i < argc {
                if (eq(&args[i], "store") || eq(&args[i], "storedist")) && i + 1 < argc {
                    stored = Some(i + 1);
                    i += 1;
                }
                i += 1;
            }
            let mut out = vec![(1, RO | ACCESS)];
            out.extend(stored.map(|s| (s, OW | UPDATE)));
            out
        }
        "xread" => {
            let mut streams = None;
            let mut i = 1;
            while i < argc {
                let a = &args[i];
                if ["block", "count", "maxcount", "maxsize", "claim"].iter().any(|o| eq(a, o)) {
                    i += 1;
                } else if eq(a, "group") {
                    i += 2;
                } else if eq(a, "noack") {
                } else if eq(a, "streams") {
                    streams = Some(i);
                    break;
                } else {
                    break;
                }
                i += 1;
            }
            let Some(pos) = streams else { return Vec::new() };
            let num = argc - pos - 1;
            if num == 0 || !num.is_multiple_of(2) {
                return Vec::new();
            }
            (pos + 1..argc - num / 2).map(|i| (i, RO | ACCESS)).collect()
        }
        "set" => {
            let (mut actual, mut logical) = (OW, UPDATE);
            for a in args.iter().skip(3) {
                if eq(a, "get") {
                    actual = RW;
                    logical |= ACCESS;
                } else if ["ifeq", "ifne", "ifdeq", "ifdne"].iter().any(|o| eq(a, o)) {
                    actual = RW;
                }
            }
            vec![(1, actual | logical)]
        }
        "delex" => {
            let mut actual = RM;
            if args.iter().skip(2).any(|a| ["ifeq", "ifne", "ifdeq", "ifdne"].iter().any(|o| eq(a, o))) {
                actual = RW;
            }
            vec![(1, actual | DELETE)]
        }
        "bitfield" => {
            let mut readonly = true;
            let mut i = 2;
            while i < argc {
                let rem = argc - i - 1;
                let a = &args[i];
                if eq(a, "get") && rem >= 2 {
                    i += 2;
                } else if (eq(a, "set") || eq(a, "incrby")) && rem >= 3 {
                    readonly = false;
                    break;
                } else if eq(a, "overflow") && rem >= 1 {
                    i += 1;
                } else {
                    readonly = false;
                    break;
                }
                i += 1;
            }
            vec![(1, if readonly { RO | ACCESS } else { RW | ACCESS | UPDATE })]
        }
        _ => Vec::new(),
    }
}

/// Key positions in `args` with their access flags, for the resolved command `cmd` (`full` is
/// its `container|sub` name).
pub fn keys_with_flags(cmd: &Command, full: &str, args: &[Bytes]) -> Vec<(usize, u16)> {
    let specs = specs(full);
    if specs.is_empty() && !meta::exists(full) {
        return cmd.key_positions(args).into_iter().map(|i| (i, 0)).collect();
    }
    let has_keyspec = specs.iter().any(|s| s.flags & NOT_KEY == 0);
    let varflags = specs.iter().any(|s| s.flags & VARIABLE_FLAGS != 0);
    if has_keyspec && !varflags && let Some(keys) = using_specs(specs, args, cmd) {
        return keys;
    }
    match proc_name(full) {
        Some(p) => by_proc(p, args),
        None => Vec::new(),
    }
}

