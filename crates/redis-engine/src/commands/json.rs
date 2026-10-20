//! JSON commands (JSON.*), as the RedisJSON module bundled with Redis 8.10 answers them.
//!
//! The module's conventions are kept: arguments are read in its order (a missing one is an
//! arity error, a non-UTF-8 one "Couldn't parse as UTF-8 string"), keys are opened before or
//! after parsing values exactly where it opens them, a key opened for writing is signalled as
//! modified whatever the command then does, and a write propagates only when the module
//! replicates it (`RM_ReplicateVerbatim`, one dirty change), even if it changed nothing.
//!
//! Paths address values as lists of member names and array indexes. Mutations follow such a
//! list from the root; an element of a typed (unboxed numeric) array is a slot that only
//! number operations and replacement can change.

use super::*;
use crate::cx::KeyMut;
use crate::json::de::parse;
use crate::json::path::{Last, Path, Query, Step, Ty, compile, is_equal, project, select, select_paths};
use crate::json::ser::{Format, compact, plain, to_string};
use crate::json::value::{Arr, MAX_DEPTH, Obj, Tag};
use crate::json::{J, Json, N, Node};
use crate::notify;
use crate::value::{Value, ValueRef};
use crate::cmd;
use std::cmp::Ordering;

pub static COMMANDS: &[Command] = &[
    cmd!("json.get", -2, READONLY | MODULE, 1, 1, 1, get, Module),
    cmd!("json.set", -4, WRITE | DENYOOM | MODULE, 1, 1, 1, set, Module),
    cmd!("json.merge", -4, WRITE | DENYOOM | MODULE, 1, 1, 1, merge_cmd, Module),
    cmd!("json.mset", -4, WRITE | DENYOOM | MODULE, 1, -1, 3, mset, Module),
    cmd!("json.mget", -3, READONLY | MODULE, 1, 1, 1, mget, Module),
    cmd!("json.del", -2, WRITE | MODULE, 1, 1, 1, del, Module),
    cmd!("json.forget", -2, WRITE | MODULE, 1, 1, 1, del, Module),
    cmd!("json.type", -2, READONLY | MODULE, 1, 1, 1, type_cmd, Module),
    cmd!("json.numincrby", 4, WRITE | MODULE, 1, 1, 1, numincrby, Module),
    cmd!("json.nummultby", 4, WRITE | MODULE, 1, 1, 1, nummultby, Module),
    cmd!("json.numpowby", 4, WRITE | MODULE, 1, 1, 1, numpowby, Module),
    cmd!("json.toggle", 3, WRITE | MODULE, 1, 1, 1, toggle, Module),
    cmd!("json.strappend", -3, WRITE | DENYOOM | MODULE, 1, 1, 1, strappend, Module),
    cmd!("json.strlen", -2, READONLY | MODULE, 1, 1, 1, strlen, Module),
    cmd!("json.arrappend", -3, WRITE | DENYOOM | MODULE, 1, 1, 1, arrappend, Module),
    cmd!("json.arrindex", -4, READONLY | MODULE, 1, 1, 1, arrindex, Module),
    cmd!("json.arrinsert", -5, WRITE | DENYOOM | MODULE, 1, 1, 1, arrinsert, Module),
    cmd!("json.arrlen", -2, READONLY | MODULE, 1, 1, 1, arrlen, Module),
    cmd!("json.arrpop", -2, WRITE | MODULE, 1, 1, 1, arrpop, Module),
    cmd!("json.arrtrim", 5, WRITE | MODULE, 1, 1, 1, arrtrim, Module),
    cmd!("json.objkeys", -2, READONLY | MODULE, 1, 1, 1, objkeys, Module),
    cmd!("json.objlen", -2, READONLY | MODULE, 1, 1, 1, objlen, Module),
    cmd!("json.clear", -2, WRITE | MODULE, 1, 1, 1, clear, Module),
    cmd!("json.debug", -2, READONLY | MODULE, keys = debug_keys, debug, Module),
    cmd!("json.resp", -2, READONLY | MODULE, 1, 1, 1, resp, Module),
];

/// JSON.DEBUG's key follows the MEMORY keyword.
fn debug_keys(args: &[Bytes]) -> Vec<usize> {
    (1..args.len().saturating_sub(1)).find(|&i| args[i].eq_ignore_ascii_case(b"memory")).map(|i| vec![i + 1]).unwrap_or_default()
}

const WRONG_KEY_TYPE: Error = Error::new("Existing key has wrong Redis type");
const NO_KEY: Error = Error::new("ERR could not perform this operation on a key that doesn't exist");
const NO_PATH: Error = Error::new("ERR Path does not exist");
const PROJECTION: Error = Error::new("ERR computed/projection expressions are only supported by JSON.GET/JSON.MGET/JSON.RESP");
const RECURSION: Error = Error::new("ERR recursion limit exceeded");
const NUM_OVERFLOW: Error = Error::new("ERR numeric overflow");
const NOT_A_NUMBER: Error = Error::new("ERR result is not a number");
const BAD_OBJECT: Error = Error::new("ERR bad object type");
const NOT_UTF8: Error = Error::new("Couldn't parse as UTF-8 string");
const NOT_INTEGER: Error = Error::new("Couldn't parse as integer");
const AT_ROOT: Error = Error::new("ERR new objects must be created at the root");
const WRONG_REPLY_FORMAT: Error = Error::new("ERR wrong reply format");
const RESP2_FORMAT: Error = Error::new("ERR FORMAT argument is not supported on RESP2");

fn no_path_or(what: &str) -> Error {
    Error::msg(format!("ERR Path does not exist or {what}"))
}

fn expected(what: &str) -> Error {
    Error::msg(format!("WRONGTYPE wrong type of path value - expected {what}"))
}

/// A Rust error passed through the module's `?` (`"ERR {e}"`).
fn err(e: impl std::fmt::Display) -> Error {
    Error::msg(format!("ERR {e}"))
}

// ---------------------------------------------------------------------------------------------
// Arguments
// ---------------------------------------------------------------------------------------------

fn utf8(b: &[u8]) -> Result<&str, Error> {
    std::str::from_utf8(b).map_err(|_| NOT_UTF8)
}

fn integer(b: &[u8]) -> Result<i64, Error> {
    crate::util::parse_i64(b).ok_or(NOT_INTEGER)
}

/// The arguments after the command name, consumed like the module's `NextArg`.
struct Args<'a> {
    name: &'static str,
    rest: std::slice::Iter<'a, Bytes>,
}

impl<'a> Args<'a> {
    fn new(cx: &Cx, args: &'a [Bytes]) -> Self {
        Args { name: cx.cmd.name, rest: args[1..].iter() }
    }

    fn len(&self) -> usize {
        self.rest.len()
    }

    fn next(&mut self) -> Option<&'a Bytes> {
        self.rest.next()
    }

    fn peek(&self) -> Option<&'a Bytes> {
        self.rest.clone().next()
    }

    fn next_arg(&mut self) -> Result<&'a Bytes, Error> {
        let name = self.name;
        self.rest.next().ok_or_else(|| arity_error(name))
    }

    fn next_str(&mut self) -> Result<&'a str, Error> {
        utf8(self.next_arg()?)
    }

    fn next_i64(&mut self) -> Result<i64, Error> {
        integer(self.next_arg()?)
    }

    fn done(&mut self) -> Result<(), Error> {
        match self.rest.next() {
            Some(_) => Err(arity_error(self.name)),
            None => Ok(()),
        }
    }

    /// An optional path: the default (legacy root) when missing or not UTF-8.
    fn path(&mut self) -> Path<'a> {
        self.next_str().map(Path::new).unwrap_or_else(|_| Path::new("."))
    }
}

fn is_root(p: &Path) -> bool {
    p.get() == "$"
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Input {
    Json,
    Bson,
}

fn input_format(s: &str) -> Result<Input, Error> {
    match s {
        "STRING" | "JSON" => Ok(Input::Json),
        "BSON" => Ok(Input::Bson),
        _ => Err(Error::new("ERR wrong format")),
    }
}

/// A value given to a write command.
fn value_of(text: &str, format: Input, fpha: Option<Tag>) -> Result<J, Error> {
    match format {
        Input::Json => parse(text, fpha).map_err(Error::msg),
        Input::Bson => match crate::json::bson::first_value(text.as_bytes()).map_err(Error::msg)? {
            Some(text) => parse(&text, fpha).map_err(Error::msg),
            None => Ok(J::Null),
        },
    }
}

// ---------------------------------------------------------------------------------------------
// Keys
// ---------------------------------------------------------------------------------------------

/// The document of a key looked up for reading (`None` when missing).
fn doc(v: Option<ValueRef<'_>>) -> Result<Option<&J>, Error> {
    match v {
        None => Ok(None),
        Some(ValueRef::Json(j)) => Ok(Some(j.root())),
        Some(_) => Err(WRONG_KEY_TYPE),
    }
}

/// Open a key for writing (`RM_OpenKey` with WRITE): it is signalled as modified when the
/// command ends, changed or not.
fn open<'c>(cx: &'c mut Cx, key: &[u8]) -> Result<Option<KeyMut<'c>>, Error> {
    cx.touch(key);
    match cx.peek(key) {
        None => return Ok(None),
        Some(e) if !matches!(e.value(), ValueRef::Json(_)) => return Err(WRONG_KEY_TYPE),
        Some(_) => {}
    }
    Ok(Some(reopen(cx, key)))
}

/// A key [`open`] found, again (after something else used the context).
fn reopen<'c>(cx: &'c mut Cx, key: &[u8]) -> KeyMut<'c> {
    let mut k = cx.get_mut(key).expect("opened");
    k.unchanged();
    k
}

/// Store a new document under a missing key (`RM_ModuleTypeSetValue`: a `new` event).
fn create(cx: &mut Cx, key: &[u8], root: J) {
    cx.set(key, Value::Json(Box::new(Json::new(root))), false);
}

/// The module's `apply_changes`: replicate the command as is.
fn replicate(k: &mut KeyMut, event: &'static str) {
    k.notify(notify::MODULE, event);
    k.changes(1);
}

// ---------------------------------------------------------------------------------------------
// Selecting
// ---------------------------------------------------------------------------------------------

fn compiled(path: &str) -> Result<Query<'_>, Error> {
    compile(path).map_err(Error::msg)
}

/// A path that has to address values (not compute them).
fn addressing(path: &str) -> Result<Query<'_>, Error> {
    let q = compiled(path)?;
    if q.is_projection() {
        return Err(PROJECTION);
    }
    Ok(q)
}

fn is_projection(path: &str) -> bool {
    compile(path).is_ok_and(|q| q.is_projection())
}

fn values<'j>(doc: &'j J, path: &str) -> Result<Vec<Node<'j>>, Error> {
    Ok(select(&addressing(path)?, doc))
}

fn first<'j>(doc: &'j J, path: &str) -> Result<Node<'j>, Error> {
    values(doc, path)?.into_iter().next().ok_or(NO_PATH)
}

type Loc = Vec<String>;

fn loc(steps: &[Step]) -> Loc {
    steps.iter().map(|s| s.text().into_owned()).collect()
}

/// Locations of the matches passing `f` (`find_paths`).
fn find(doc: &J, path: &str, f: impl Fn(Node) -> bool) -> Result<Vec<Loc>, Error> {
    let q = addressing(path)?;
    Ok(select_paths(&q, doc).into_iter().filter(|m| f(m.node)).map(|m| loc(&m.path)).collect())
}

/// Every match, with the location of those passing `f` (`find_all_paths`).
fn find_all(doc: &J, path: &str, f: impl Fn(Node) -> bool) -> Result<Vec<Option<Loc>>, Error> {
    let q = addressing(path)?;
    Ok(select_paths(&q, doc).into_iter().map(|m| f(m.node).then(|| loc(&m.path))).collect())
}

/// Every match, kept when it passes `f` (`find_all_values`).
fn find_values<'j>(doc: &'j J, path: &str, f: impl Fn(Node) -> bool) -> Result<Vec<Option<Node<'j>>>, Error> {
    let q = addressing(path)?;
    Ok(select_paths(&q, doc).into_iter().map(|m| f(m.node).then_some(m.node)).collect())
}

fn is(ty: Ty) -> impl Fn(Node) -> bool {
    move |n| n.ty() == ty
}

/// Children before parents, higher indexes before lower ones, so that changing one location
/// never moves another.
fn mutation_order(a: &Loc, b: &Loc) -> Ordering {
    for i in 0.. {
        match (a.get(i), b.get(i)) {
            (None, None) => return Ordering::Equal,
            (Some(_), None) => return Ordering::Less,
            (None, Some(_)) => return Ordering::Greater,
            (Some(x), Some(y)) => {
                let o = match (x.parse::<usize>(), y.parse::<usize>()) {
                    (Err(_), Err(_)) => x.cmp(y),
                    (Ok(_), Err(_)) => Ordering::Greater,
                    (Err(_), Ok(_)) => Ordering::Less,
                    (Ok(i), Ok(j)) => j.cmp(&i),
                };
                if o != Ordering::Equal {
                    return o;
                }
            }
        }
    }
    unreachable!()
}

/// Sort for mutation and keep only the outermost of nested locations.
fn prepare_for_updating(paths: &mut Vec<Loc>) {
    if paths.len() < 2 {
        return;
    }
    paths.sort_by(mutation_order);
    let joined: Vec<String> = paths.iter().map(|p| p.join(",")).collect();
    let nested_in = |path: &str, p: &str| path.starts_with(p) && (path.len() == p.len() || path[p.len()..].starts_with(','));
    // Of the locations containing a path, the outermost sorts first; the path stays only if
    // that is itself.
    let keep: Vec<bool> = joined
        .iter()
        .map(|path| joined.iter().filter(|p| nested_in(path, p)).min_by_key(|p| p.len()).is_some_and(|p| p == path))
        .collect();
    let mut i = 0;
    paths.retain(|_| {
        i += 1;
        keep[i - 1]
    });
}

// ---------------------------------------------------------------------------------------------
// Changing
// ---------------------------------------------------------------------------------------------

/// What a location reaches: a value, or an element of a typed array.
enum At<'a> {
    Val(&'a mut J),
    Slot(&'a mut Arr, usize),
}

/// Follow a location from the root, with the number of steps taken.
fn follow<'a>(root: &'a mut J, path: &[String]) -> Option<(At<'a>, usize)> {
    let mut cur = root;
    for (depth, tok) in path.iter().enumerate() {
        cur = match cur {
            J::Obj(o) => o.get_mut(tok)?,
            J::Arr(a) => {
                let i: usize = tok.parse().ok()?;
                if i >= a.len() {
                    return None;
                }
                if a.tag() != Tag::Het {
                    // A number with no value of its own: nothing can be followed below it.
                    return (depth + 1 == path.len()).then_some((At::Slot(a, i), depth + 1));
                }
                a.het_mut(i).expect("in range")
            }
            _ => return None,
        };
    }
    Some((At::Val(cur), path.len()))
}

fn update<T>(root: &mut J, path: &[String], f: impl FnOnce(At, usize) -> Result<T, Error>) -> Result<T, Error> {
    match follow(root, path) {
        Some((at, depth)) => f(at, depth),
        None => Err(NO_PATH),
    }
}

fn value_at<'a>(at: At<'a>) -> Result<&'a mut J, Error> {
    match at {
        At::Val(v) => Ok(v),
        At::Slot(..) => Err(BAD_OBJECT),
    }
}

fn set_value(root: &mut J, path: &[String], v: J) -> Result<bool, Error> {
    if path.is_empty() {
        *root = v;
        return Ok(true);
    }
    update(root, path, |at, depth| {
        if depth + v.depth() >= MAX_DEPTH {
            return Err(RECURSION);
        }
        match at {
            At::Val(x) => *x = v,
            At::Slot(a, i) => {
                a.remove(i);
                a.insert(i, v);
            }
        }
        Ok(true)
    })
}

fn dict_add(root: &mut J, path: &[String], key: &str, v: J) -> Result<bool, Error> {
    update(root, path, |at, depth| {
        let x = value_at(at)?;
        if depth + 1 + v.depth() >= MAX_DEPTH {
            return Err(RECURSION);
        }
        Ok(match x.as_obj_mut() {
            Some(o) if !o.contains_key(key) => {
                o.insert(key, v);
                true
            }
            _ => false,
        })
    })
}

fn can_merge(doc: &J, patch: &J, depth: usize) -> bool {
    if depth >= MAX_DEPTH {
        return false;
    }
    let (Some(d), Some(p)) = (doc.as_obj(), patch.as_obj()) else {
        return depth + patch.depth() < MAX_DEPTH;
    };
    p.iter().all(|(k, v)| matches!(v, J::Null) || can_merge(d.get(k).unwrap_or(&J::Null), v, depth + 1))
}

/// RFC 7396 merge patch.
fn merge(doc: &mut J, patch: J) {
    let J::Obj(mut patch) = patch else {
        *doc = patch;
        return;
    };
    if doc.as_obj().is_none() {
        *doc = J::Obj(Box::new(Obj::new()));
    }
    let map = doc.as_obj_mut().expect("object");
    for (k, v) in patch.iter_mut() {
        let v = std::mem::take(v);
        if matches!(v, J::Null) {
            map.remove(k);
        } else {
            merge(map.entry_or_null(k), v);
        }
    }
}

fn merge_value(root: &mut J, path: &[String], v: J) -> Result<bool, Error> {
    let r = update(root, path, |at, depth| {
        let cur = value_at(at)?;
        if can_merge(cur, &v, depth) {
            merge(cur, v);
            Ok(true)
        } else {
            Ok(false)
        }
    });
    match r {
        Err(e) if e == NO_PATH => Ok(false),
        r => r,
    }
}

fn remove(root: &mut J, path: &[String]) -> bool {
    let Some((tok, parent)) = path.split_last() else { return false };
    let Some((At::Val(target), _)) = follow(root, parent) else { return false };
    match target {
        J::Obj(o) => o.remove(tok).is_some(),
        J::Arr(a) => tok.parse::<usize>().ok().and_then(|i| a.remove(i)).is_some(),
        _ => false,
    }
}

/// A located change of JSON.SET / JSON.MERGE / JSON.MSET: replace a value, or add a member.
enum Update {
    Set(Loc),
    Add(Loc, String),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SetOpt {
    None,
    Nx,
    Xx,
    MergeExisting,
}

fn find_updates(doc: &J, path: &str, opt: SetOpt) -> Result<Vec<Update>, Error> {
    if opt != SetOpt::Nx {
        let q = addressing(path)?;
        let mut found: Vec<Loc> = select_paths(&q, doc).into_iter().map(|m| loc(&m.path)).collect();
        if opt != SetOpt::MergeExisting {
            prepare_for_updating(&mut found);
        }
        if !found.is_empty() {
            return Ok(found.into_iter().map(Update::Set).collect());
        }
    }
    if opt == SetOpt::Xx {
        return Ok(Vec::new());
    }
    find_additions(doc, path)
}

/// Where a missing last member of a static path would go.
fn find_additions(doc: &J, path: &str) -> Result<Vec<Update>, Error> {
    let mut q = addressing(path)?;
    if !q.is_static() {
        return Err(Error::new("Err wrong static path"));
    }
    let size = q.size();
    if size < 1 {
        return Err(Error::new("Err path must end with object key to set"));
    }
    match q.pop_last() {
        Some(Last::Key(key)) if size == 1 => Ok(vec![Update::Add(Vec::new(), key)]),
        Some(Last::Key(key)) => Ok(select_paths(&q, doc).into_iter().map(|m| Update::Add(loc(&m.path), key.clone())).collect()),
        _ => {
            // An index past the end, or an NX on an element that exists.
            if select_paths(&compiled(path)?, doc).is_empty() {
                Err(Error::new("ERR array index out of range"))
            } else {
                Ok(Vec::new())
            }
        }
    }
}

fn apply(root: &mut J, u: Update, v: J) -> bool {
    match u {
        Update::Set(p) => set_value(root, &p, v),
        Update::Add(p, k) => dict_add(root, &p, &k, v),
    }
    .unwrap_or(false)
}

/// Apply updates: whether any, and whether all succeeded.
fn apply_updates(root: &mut J, v: J, mut ups: Vec<Update>) -> (bool, bool) {
    if ups.len() == 1 {
        let ok = apply(root, ups.pop().expect("one"), v);
        return (ok, ok);
    }
    let mut any = false;
    let mut all = !ups.is_empty();
    for u in ups {
        let ok = apply(root, u, v.clone());
        any |= ok;
        all &= ok;
    }
    (any, all)
}

// ---------------------------------------------------------------------------------------------
// Replies
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum ReplyFormat {
    String,
    Strings,
    Expand1,
    Expand,
}

fn reply_format(s: &str) -> Result<ReplyFormat, Error> {
    match s {
        "STRING" => Ok(ReplyFormat::String),
        "STRINGS" => Ok(ReplyFormat::Strings),
        "EXPAND1" => Ok(ReplyFormat::Expand1),
        "EXPAND" => Ok(ReplyFormat::Expand),
        _ => Err(WRONG_REPLY_FORMAT),
    }
}

struct Fmt<'a> {
    format: ReplyFormat,
    text: Format<'a>,
    resp3: bool,
}

impl<'a> Fmt<'a> {
    fn new(resp3: bool, format: ReplyFormat) -> Self {
        Fmt { format, text: Format::default(), resp3 }
    }

    /// Native RESP3 values rather than JSON text.
    fn native(&self) -> bool {
        self.resp3 && !matches!(self.format, ReplyFormat::String | ReplyFormat::Strings)
    }

    fn text<T: serde::Serialize + ?Sized>(&self, v: &T) -> String {
        to_string(v, self.text)
    }
}

fn bulk(s: String) -> Reply {
    Reply::Bulk(Bytes::from(s))
}

fn type_name(n: Node) -> &'static str {
    match n.ty() {
        Ty::Null => "null",
        Ty::Bool => "boolean",
        Ty::Long => "integer",
        Ty::Double if matches!(n.num(), Some(N::U(_))) => "integer",
        Ty::Double => "number",
        Ty::String => "string",
        Ty::Array => "array",
        Ty::Object => "object",
    }
}

fn native(n: Node, f: &Fmt) -> Reply {
    match n.ty() {
        Ty::Null => Reply::Nil,
        Ty::Bool => Reply::Bool(n.bool().unwrap_or_default()),
        Ty::Long => Reply::Int(n.long().unwrap_or_default()),
        Ty::Double => Reply::Double(n.double().unwrap_or_default()),
        Ty::String if f.format == ReplyFormat::Expand => bulk(n.as_str().unwrap_or_default().to_string()),
        Ty::Array if f.format == ReplyFormat::Expand => Reply::Array(n.values().into_iter().map(|v| native(v, f)).collect()),
        Ty::Object if f.format == ReplyFormat::Expand => Reply::Map(
            n.obj()
                .map(|o| o.iter().map(|(k, v)| (bulk(k.to_string()), native(Node::Ref(v), f))).collect())
                .unwrap_or_default(),
        ),
        _ => bulk(f.text(&n)),
    }
}

fn natives<'j>(ns: impl IntoIterator<Item = Node<'j>>, f: &Fmt) -> Reply {
    Reply::Array(ns.into_iter().map(|n| native(n, f)).collect())
}

fn computed_natives(vs: &[J], f: &Fmt) -> Reply {
    natives(vs.iter().map(Node::Ref), f)
}

/// JSON.RESP's form of a value.
fn resp_of(n: Node) -> Reply {
    match n.ty() {
        Ty::Null => Reply::Nil,
        Ty::Bool => Reply::status(if n.bool().unwrap_or_default() { "true" } else { "false" }),
        Ty::Long => Reply::Int(n.long().unwrap_or_default()),
        Ty::Double => Reply::Double(n.double().unwrap_or_default()),
        Ty::String => bulk(n.as_str().unwrap_or_default().to_string()),
        Ty::Array => {
            let mut out = vec![Reply::status("[")];
            out.extend(n.values().into_iter().map(resp_of));
            Reply::Array(out)
        }
        Ty::Object => {
            let mut out = vec![Reply::status("{")];
            for (k, v) in n.obj().into_iter().flat_map(|o| o.iter()) {
                out.push(bulk(k.to_string()));
                out.push(resp_of(Node::Ref(v)));
            }
            Reply::Array(out)
        }
    }
}

/// The first match as text (legacy paths), or a projection's values.
fn text_single(doc: &J, path: &str, f: &Fmt) -> Result<String, Error> {
    let q = compiled(path)?;
    if q.is_projection() {
        return Ok(f.text(&project(&q, doc)));
    }
    let n = select(&q, doc).into_iter().next().ok_or(NO_PATH)?;
    Ok(f.text(&n))
}

/// Every match as a JSON array.
fn text_multi(doc: &J, path: &str, f: &Fmt) -> Result<String, Error> {
    let q = compiled(path)?;
    if q.is_projection() { Ok(f.text(&project(&q, doc))) } else { Ok(f.text(&select(&q, doc))) }
}

enum Values<'j> {
    Single(Node<'j>),
    Multi(Vec<Node<'j>>),
    Computed(Vec<J>),
}

impl serde::Serialize for Values<'_> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Values::Single(n) => n.serialize(s),
            Values::Multi(v) => v.serialize(s),
            Values::Computed(v) => v.serialize(s),
        }
    }
}

/// Several paths answered as one object keyed by path.
struct ByPath<'a, 'j>(Vec<(&'a str, Values<'j>)>);

impl serde::Serialize for ByPath<'_, '_> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut m = s.serialize_map(Some(self.0.len()))?;
        for (k, v) in &self.0 {
            m.serialize_entry(k, v)?;
        }
        m.end()
    }
}

fn to_json(doc: &J, paths: &[Path], f: &Fmt) -> CmdResult {
    let legacy = paths.iter().all(Path::is_legacy);
    if f.native() {
        let mut out = Vec::with_capacity(paths.len());
        for p in paths {
            let q = compiled(p.get())?;
            out.push(if q.is_projection() { computed_natives(&project(&q, doc), f) } else { natives(select(&q, doc), f) });
        }
        return Ok(Reply::Array(out));
    }
    if paths.len() > 1 {
        let mut missing = false;
        let mut out: Vec<(&str, Values)> = Vec::with_capacity(paths.len());
        for p in paths {
            let Ok(q) = compile(p.get()) else { continue };
            let v = if q.is_projection() {
                Some(Values::Computed(project(&q, doc)))
            } else {
                let found = select(&q, doc);
                if legacy { found.into_iter().next().map(Values::Single) } else { Some(Values::Multi(found)) }
            };
            let Some(v) = v else {
                missing = true;
                continue;
            };
            match out.iter_mut().find(|(k, _)| *k == p.original()) {
                Some(slot) => slot.1 = v,
                None => out.push((p.original(), v)),
            }
        }
        if missing {
            return Err(NO_PATH);
        }
        return Ok(bulk(f.text(&ByPath(out))));
    }
    let p = paths[0].get();
    Ok(bulk(if legacy { text_single(doc, p, f)? } else { text_multi(doc, p, f)? }))
}

// ---------------------------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------------------------

/// JSON.GET key [INDENT s] [NEWLINE s] [SPACE s] [FORMAT f] [path ...]
fn get(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let mut a = Args::new(cx, args);
    let key = a.next_arg()?;
    let mut paths = Vec::with_capacity(1);
    let mut f = Fmt::new(cx.resp() == 3, ReplyFormat::String);
    while let Ok(arg) = a.next_str() {
        if arg.len() > 8 {
            paths.push(Path::new(arg));
        } else if arg.eq_ignore_ascii_case("FORMAT") {
            if !f.resp3 && paths.is_empty() {
                return Err(RESP2_FORMAT);
            }
            let next = a.next_str()?;
            if next.eq_ignore_ascii_case("STRINGS") {
                return Err(WRONG_REPLY_FORMAT);
            }
            f.format = reply_format(next)?;
        } else if arg.eq_ignore_ascii_case("INDENT") {
            f.text.indent = Some(a.next_str()?);
        } else if arg.eq_ignore_ascii_case("NEWLINE") {
            f.text.newline = Some(a.next_str()?);
        } else if arg.eq_ignore_ascii_case("SPACE") {
            f.text.space = Some(a.next_str()?);
        } else if !arg.eq_ignore_ascii_case("NOESCAPE") {
            paths.push(Path::new(arg));
        }
    }
    if paths.is_empty() {
        paths.push(Path::new("."));
    }
    match doc(cx.get(key))? {
        Some(d) => to_json(d, &paths, &f),
        None => Ok(Reply::Nil),
    }
}

/// JSON.SET key path json [NX | XX] [FORMAT f] [FPHA t]
fn set(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    cx.changes(0);
    let mut a = Args::new(cx, args);
    let key = a.next_arg()?;
    let path = Path::new(a.next_str()?);
    let text = a.next_str()?;
    let mut format = Input::Json;
    let mut opt = SetOpt::None;
    let mut fpha = None;
    while let Some(s) = a.next() {
        let s = utf8(s)?;
        if s.eq_ignore_ascii_case("NX") && opt == SetOpt::None {
            opt = SetOpt::Nx;
        } else if s.eq_ignore_ascii_case("XX") && opt == SetOpt::None {
            opt = SetOpt::Xx;
        } else if s.eq_ignore_ascii_case("FORMAT") {
            format = input_format(a.next_str()?)?;
        } else if s.eq_ignore_ascii_case("FPHA") {
            fpha = Some(match a.next_str()?.to_uppercase().as_str() {
                "BF16" => Tag::BF16,
                "FP16" => Tag::F16,
                "FP32" => Tag::F32,
                "FP64" => Tag::F64,
                _ => return Err(Error::new("ERR invalid FPHA type")),
            });
        } else {
            return Err(SYNTAX);
        }
    }
    let found = open(cx, key)?.is_some();
    let v = value_of(text, format, fpha)?;
    match found {
        true => {
            let mut k = reopen(cx, key);
            if is_root(&path) {
                if opt == SetOpt::Nx {
                    return Ok(Reply::Nil);
                }
                *k.json().root_mut() = v;
                replicate(&mut k, "json.set");
                return Ok(Reply::Ok);
            }
            let ups = find_updates(k.json().root(), path.get(), opt)?;
            if ups.is_empty() {
                return Ok(Reply::Nil);
            }
            let (any, all) = apply_updates(k.json().root_mut(), v, ups);
            if any {
                replicate(&mut k, "json.set");
            }
            Ok(if all { Reply::Ok } else { Reply::Nil })
        }
        false if opt == SetOpt::Xx => Ok(Reply::Nil),
        false if is_root(&path) => {
            create(cx, key, v);
            cx.notify(notify::MODULE, "json.set", key);
            cx.changes(1);
            Ok(Reply::Ok)
        }
        false => Err(AT_ROOT),
    }
}

/// JSON.MERGE key path json [FORMAT f]
fn merge_cmd(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    cx.changes(0);
    let mut a = Args::new(cx, args);
    let key = a.next_arg()?;
    let path = Path::new(a.next_str()?);
    let text = a.next_str()?;
    let mut format = Input::Json;
    while let Some(s) = a.next() {
        if utf8(s)?.eq_ignore_ascii_case("FORMAT") {
            format = input_format(a.next_str()?)?;
        } else {
            return Err(SYNTAX);
        }
    }
    let found = open(cx, key)?.is_some();
    let v = value_of(text, format, None)?;
    match found {
        true => {
            let mut k = reopen(cx, key);
            if is_root(&path) {
                merge_value(k.json().root_mut(), &[], v)?;
                replicate(&mut k, "json.merge");
                return Ok(Reply::Ok);
            }
            let mut ups = find_updates(k.json().root(), path.get(), SetOpt::MergeExisting)?;
            let root = k.json().root_mut();
            let mut res = false;
            if ups.len() == 1 {
                res = match ups.pop().expect("one") {
                    Update::Set(p) => merge_value(root, &p, v)?,
                    Update::Add(p, key) => dict_add(root, &p, &key, v)?,
                };
            } else {
                for u in ups {
                    res = match u {
                        Update::Set(p) => merge_value(root, &p, v.clone())?,
                        Update::Add(p, key) => dict_add(root, &p, &key, v.clone())?,
                    } || res;
                }
            }
            if res {
                replicate(&mut k, "json.merge");
                Ok(Reply::Ok)
            } else {
                Ok(Reply::Nil)
            }
        }
        false if is_root(&path) => {
            create(cx, key, v);
            cx.notify(notify::MODULE, "json.merge", key);
            cx.changes(1);
            Ok(Reply::Ok)
        }
        false => Err(AT_ROOT),
    }
}

/// JSON.MSET key path json [key path json ...]: every triple is checked before any is applied.
fn mset(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    cx.changes(0);
    let mut a = Args::new(cx, args);
    if a.len() < 3 {
        return Err(arity_error(cx.cmd.name));
    }
    let mut parsed: Vec<(&Bytes, Option<Vec<Update>>, &str)> = Vec::new();
    while let Ok(key) = a.next_arg() {
        let k = open(cx, key)?;
        let path = Path::new(a.next_str()?);
        let ups = if is_root(&path) {
            None
        } else if let Some(mut k) = k {
            Some(find_updates(k.json().root(), path.get(), SetOpt::None)?)
        } else {
            return Err(AT_ROOT);
        };
        let text = a.next_str()?;
        value_of(text, Input::Json, None)?;
        parsed.push((key, ups, text));
    }
    let mut all_updated = true;
    for (key, ups, text) in parsed {
        let found = open(cx, key)?.is_some();
        let v = value_of(text, Input::Json, None)?;
        let all = match (ups, found) {
            (Some(ups), true) => {
                let mut k = reopen(cx, key);
                let r = if ups.is_empty() { (false, false) } else { apply_updates(k.json().root_mut(), v, ups) };
                if r.0 {
                    k.notify(notify::MODULE, "json.mset");
                }
                r.1
            }
            (Some(_), false) => return Err(NO_KEY),
            (None, true) => {
                let mut k = reopen(cx, key);
                *k.json().root_mut() = v;
                k.notify(notify::MODULE, "json.mset");
                true
            }
            (None, false) => {
                create(cx, key, v);
                cx.notify(notify::MODULE, "json.mset", key);
                true
            }
        };
        all_updated &= all;
    }
    cx.changes(1);
    Ok(if all_updated { Reply::Ok } else { Reply::Nil })
}

/// JSON.MGET key [key ...] path: a key that can't answer is nil.
fn mget(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    if args.len() < 3 {
        return Err(arity_error(cx.cmd.name));
    }
    let path = Path::new(utf8(&args[args.len() - 1])?);
    let f = Fmt::new(cx.resp() == 3, ReplyFormat::String);
    let mut out = Vec::with_capacity(args.len() - 2);
    for key in &args[1..args.len() - 1] {
        let r = match doc(cx.get(key)) {
            Ok(Some(d)) => {
                let s = if path.is_legacy() { text_single(d, path.get(), &f) } else { text_multi(d, path.get(), &f) };
                s.map_or(Reply::Nil, bulk)
            }
            _ => Reply::Nil,
        };
        out.push(r);
    }
    Ok(Reply::Array(out))
}

/// JSON.DEL / JSON.FORGET key [path]: the key goes when its root does or ends up empty.
fn del(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    cx.changes(0);
    let mut a = Args::new(cx, args);
    let key = a.next_arg()?;
    let path = match a.next() {
        None => Path::new("."),
        Some(s) => Path::new(utf8(s)?),
    };
    let Some(mut k) = open(cx, key)? else { return Ok(Reply::Int(0)) };
    let deleted = if is_root(&path) {
        1
    } else {
        let mut paths = find(k.json().root(), path.get(), |_| true)?;
        prepare_for_updating(&mut paths);
        let root = k.json().root_mut();
        paths.iter().filter(|p| remove(root, p)).count() as i64
    };
    if deleted > 0 {
        let empty = match k.json().root() {
            J::Arr(a) => a.is_empty(),
            J::Obj(o) => o.is_empty(),
            _ => false,
        };
        k.changes(1);
        drop(k);
        if empty || is_root(&path) {
            cx.delete(key);
        }
        cx.notify(notify::MODULE, "json.del", key);
    }
    Ok(Reply::Int(deleted))
}

/// JSON.TYPE key [path]
fn type_cmd(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let mut a = Args::new(cx, args);
    let key = a.next_arg()?;
    let path = a.path();
    let resp3 = cx.resp() == 3;
    let found = cx.get(key);
    let v = if path.is_legacy() {
        if is_projection(path.get()) {
            return Err(PROJECTION);
        }
        match doc(found)? {
            Some(d) => first(d, path.get()).map_or(Reply::Nil, |n| bulk(type_name(n).to_string())),
            None => Reply::Nil,
        }
    } else {
        match doc(found)? {
            Some(d) => Reply::Array(values(d, path.get())?.into_iter().map(|n| bulk(type_name(n).to_string())).collect()),
            None => Reply::Nil,
        }
    };
    Ok(if resp3 { Reply::Array(vec![v]) } else { v })
}

#[derive(Clone, Copy)]
enum NumOp {
    Incr,
    Mult,
    Pow,
}

impl NumOp {
    fn int(self, a: i128, b: i128) -> Option<i128> {
        match self {
            NumOp::Incr => a.checked_add(b),
            NumOp::Mult => a.checked_mul(b),
            NumOp::Pow => a.checked_pow(u32::try_from(b).ok()?),
        }
    }

    fn float(self, a: f64, b: f64) -> f64 {
        match self {
            NumOp::Incr => a + b,
            NumOp::Mult => a * b,
            NumOp::Pow => crate::musl_math::pow(a, b),
        }
    }

    fn event(self) -> &'static str {
        match self {
            NumOp::Incr => "json.numincrby",
            NumOp::Mult => "json.nummultby",
            NumOp::Pow => "json.numpowby",
        }
    }
}

fn int_result(r: i128) -> Result<N, Error> {
    if let Ok(v) = i64::try_from(r) {
        Ok(N::I(v))
    } else {
        u64::try_from(r).map(N::U).map_err(|_| NUM_OVERFLOW)
    }
}

/// Apply a number operation at one location; the number as stored.
fn num_op(root: &mut J, path: &[String], text: &str, op: NumOp) -> Result<N, Error> {
    let input: serde_json::Value = serde_json::from_str(text).map_err(err)?;
    let serde_json::Value::Number(input) = input else { return Err(Error::new("bad input number")) };
    let in_f = input.as_f64().ok_or(NOT_A_NUMBER)?;
    let in_i = input.as_i64();
    update(root, path, |at, _| match at {
        At::Val(v) => {
            let cur = v.as_num().ok_or(NOT_A_NUMBER)?;
            let new = match (cur, in_i) {
                (N::I(a), Some(b)) => N::I(op.int(a as i128, b as i128).and_then(|r| i64::try_from(r).ok()).ok_or(NUM_OVERFLOW)?),
                _ => N::float(op.float(cur.to_f64_lossy(), in_f)).ok_or(NOT_A_NUMBER)?,
            };
            *v = J::Num(new);
            Ok(new)
        }
        At::Slot(a, i) => {
            let cur = a.get(i).and_then(|n| n.num()).expect("typed element");
            match a.tag() {
                Tag::F16 | Tag::BF16 => {
                    let new = op.float(cur.to_f64_lossy(), in_f);
                    if !new.is_finite() {
                        return Err(NOT_A_NUMBER);
                    }
                    let narrowed = if a.tag() == Tag::F16 {
                        let h = half::f16::from_f64(new);
                        h.is_finite().then(|| f64::from(h))
                    } else {
                        let h = half::bf16::from_f64(new);
                        h.is_finite().then(|| f64::from(h))
                    };
                    replace_slot(a, i, narrowed, new)
                }
                Tag::F32 | Tag::F64 => {
                    let new = op.float(cur.to_f64_lossy(), in_f);
                    if !new.is_finite() {
                        return Err(NUM_OVERFLOW);
                    }
                    let narrowed = if a.tag() == Tag::F32 {
                        let s = new as f32;
                        s.is_finite().then_some(s as f64)
                    } else {
                        Some(new)
                    };
                    replace_slot(a, i, narrowed, new)
                }
                _ => {
                    let operand = in_i.map(i128::from).or_else(|| input.as_u64().map(i128::from));
                    match operand {
                        Some(b) => {
                            let cur = match cur {
                                N::I(x) => x as i128,
                                N::U(x) => x as i128,
                                N::F(x) => x as i128,
                            };
                            let r = op.int(cur, b).ok_or(NUM_OVERFLOW)?;
                            let new = int_result(r)?;
                            if !a.set_exact(i, new) {
                                a.remove(i);
                                a.insert(i, J::Num(new));
                            }
                            Ok(new)
                        }
                        None => {
                            let new = op.float(cur.to_f64_lossy(), in_f);
                            a.remove(i);
                            // A NaN or infinity goes in as null, and then can't be answered.
                            a.insert(i, N::float(new).map_or(J::Null, J::Num));
                            N::float(new).ok_or(NOT_A_NUMBER)
                        }
                    }
                }
            }
        }
    })
}

/// Store a float result in a float array element: in place when the array's type holds it
/// (`narrowed`), else as a double the array is promoted for.
fn replace_slot(a: &mut Arr, i: usize, narrowed: Option<f64>, new: f64) -> Result<N, Error> {
    match narrowed {
        Some(x) => {
            a.set_float(i, x);
            Ok(N::F(x))
        }
        None => {
            a.remove(i);
            a.insert(i, J::Num(N::F(new)));
            Ok(N::F(new))
        }
    }
}

fn num_cmd(cx: &mut Cx, args: &[Bytes], op: NumOp) -> CmdResult {
    cx.changes(0);
    let mut a = Args::new(cx, args);
    let key = a.next_arg()?;
    let path = Path::new(a.next_str()?);
    let number = a.next_str()?;
    let resp3 = cx.resp() == 3;
    let mut k = open(cx, key)?.ok_or(NO_KEY)?;
    if !resp3 && path.is_legacy() {
        let paths = find(k.json().root(), path.get(), |n| matches!(n.ty(), Ty::Long | Ty::Double))?;
        if paths.is_empty() {
            return Err(no_path_or("does not contains a number"));
        }
        let mut last = N::I(0);
        for p in &paths {
            last = num_op(k.json().root_mut(), p, number, op)?;
        }
        replicate(&mut k, op.event());
        return Ok(bulk(compact(&last)));
    }
    let paths = find_all(k.json().root(), path.get(), |n| matches!(n.ty(), Ty::Long | Ty::Double))?;
    let mut res = Vec::with_capacity(paths.len());
    let mut changed = false;
    for p in paths {
        res.push(match p {
            Some(p) => {
                changed = true;
                Some(num_op(k.json().root_mut(), &p, number, op)?)
            }
            None => None,
        });
    }
    if changed {
        replicate(&mut k, op.event());
    }
    if resp3 {
        Ok(Reply::Array(
            res.into_iter()
                .map(|n| match n {
                    None => Reply::Nil,
                    Some(N::I(v)) => Reply::Int(v),
                    Some(n) => Reply::Double(n.to_f64_lossy()),
                })
                .collect(),
        ))
    } else {
        Ok(bulk(compact(&res)))
    }
}

fn numincrby(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    num_cmd(cx, args, NumOp::Incr)
}

fn nummultby(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    num_cmd(cx, args, NumOp::Mult)
}

fn numpowby(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    num_cmd(cx, args, NumOp::Pow)
}

fn toggle_at(root: &mut J, path: &[String]) -> Result<bool, Error> {
    update(root, path, |at, _| match value_at(at)? {
        J::Bool(b) => {
            *b = !*b;
            Ok(*b)
        }
        _ => Err(expected("bool")),
    })
}

/// JSON.TOGGLE key path
fn toggle(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    cx.changes(0);
    let mut a = Args::new(cx, args);
    let key = a.next_arg()?;
    let path = Path::new(a.next_str()?);
    let mut k = open(cx, key)?.ok_or(NO_KEY)?;
    if path.is_legacy() {
        let paths = find(k.json().root(), path.get(), is(Ty::Bool))?;
        if paths.is_empty() {
            return Err(no_path_or("not a bool"));
        }
        let mut res = false;
        for p in &paths {
            res = toggle_at(k.json().root_mut(), p)?;
        }
        replicate(&mut k, "json.toggle");
        return Ok(bulk(res.to_string()));
    }
    let paths = find_all(k.json().root(), path.get(), is(Ty::Bool))?;
    let mut res = Vec::with_capacity(paths.len());
    let mut changed = false;
    for p in paths {
        res.push(match p {
            Some(p) => {
                changed = true;
                Reply::Int(toggle_at(k.json().root_mut(), &p)? as i64)
            }
            None => Reply::Nil,
        });
    }
    if changed {
        replicate(&mut k, "json.toggle");
    }
    Ok(Reply::Array(res))
}

fn str_append(root: &mut J, path: &[String], json: &str) -> Result<usize, Error> {
    let serde_json::Value::String(s) = serde_json::from_str(json).map_err(err)? else { return Err(expected("string")) };
    update(root, path, |at, _| match value_at(at)? {
        J::Str(cur) => {
            let joined = [&**cur, s.as_str()].concat();
            let n = joined.len();
            *cur = joined.into_boxed_str();
            Ok(n)
        }
        _ => Err(expected("string")),
    })
}

/// JSON.STRAPPEND key [path] json-string
fn strappend(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    cx.changes(0);
    let mut a = Args::new(cx, args);
    let key = a.next_arg()?;
    let first_arg = a.next_str()?;
    let (path, json) = match a.next_arg() {
        Ok(v) => (Path::new(first_arg), utf8(v)?),
        Err(_) => (Path::new("."), first_arg),
    };
    let mut k = open(cx, key)?.ok_or(NO_KEY)?;
    if path.is_legacy() {
        let paths = find(k.json().root(), path.get(), is(Ty::String))?;
        if paths.is_empty() {
            return Err(no_path_or("not a string"));
        }
        let mut res = 0;
        for p in &paths {
            res = str_append(k.json().root_mut(), p, json)?;
        }
        replicate(&mut k, "json.strappend");
        return Ok(Reply::Int(res as i64));
    }
    let paths = find_all(k.json().root(), path.get(), is(Ty::String))?;
    let mut res = Vec::with_capacity(paths.len());
    let mut changed = false;
    for p in paths {
        res.push(match p {
            Some(p) => {
                changed = true;
                Reply::Int(str_append(k.json().root_mut(), &p, json)? as i64)
            }
            None => Reply::Nil,
        });
    }
    if changed {
        replicate(&mut k, "json.strappend");
    }
    Ok(Reply::Array(res))
}

/// JSON.STRLEN key [path]
fn strlen(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let mut a = Args::new(cx, args);
    let key = a.next_arg()?;
    let path = a.path();
    let found = doc(cx.get(key))?;
    if path.is_legacy() {
        let Some(d) = found else { return Ok(Reply::Nil) };
        return match first(d, path.get())?.as_str() {
            Some(s) => Ok(Reply::Int(s.len() as i64)),
            None => Err(expected("string")),
        };
    }
    let d = found.ok_or(NO_KEY)?;
    Ok(Reply::Array(
        find_values(d, path.get(), is(Ty::String))?
            .into_iter()
            .map(|v| v.and_then(|n| n.as_str()).map_or(Reply::Nil, |s| Reply::Int(s.len() as i64)))
            .collect(),
    ))
}

/// Values to add to arrays, parsed before the key is opened.
fn array_values(a: &mut Args) -> Result<Vec<J>, Error> {
    if a.peek().is_none() {
        return Err(arity_error(a.name));
    }
    let mut out = Vec::with_capacity(a.len());
    while let Some(v) = a.next() {
        out.push(value_of(utf8(v)?, Input::Json, None)?);
    }
    Ok(out)
}

fn too_deep(items: &[J], depth: usize) -> bool {
    items.iter().any(|v| depth + 1 + v.depth() >= MAX_DEPTH)
}

fn arr_append(root: &mut J, path: &[String], items: Vec<J>) -> Result<usize, Error> {
    update(root, path, |at, depth| {
        let arr = value_at(at)?.as_arr_mut().ok_or_else(|| expected("array"))?;
        if too_deep(&items, depth) {
            return Err(RECURSION);
        }
        arr.extend(items);
        Ok(arr.len())
    })
}

/// JSON.ARRAPPEND key path json [json ...]
fn arrappend(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    cx.changes(0);
    let mut a = Args::new(cx, args);
    let key = a.next_arg()?;
    let path = Path::new(a.next_str()?);
    let items = array_values(&mut a)?;
    let mut k = open(cx, key)?.ok_or(NO_KEY)?;
    if path.is_legacy() {
        let mut paths = find(k.json().root(), path.get(), is(Ty::Array))?;
        if paths.is_empty() {
            return Err(no_path_or("not an array"));
        }
        let res = if paths.len() == 1 {
            arr_append(k.json().root_mut(), &paths.pop().expect("one"), items)?
        } else {
            let mut res = 0;
            for p in &paths {
                res = arr_append(k.json().root_mut(), p, items.clone())?;
            }
            res
        };
        replicate(&mut k, "json.arrappend");
        return Ok(Reply::Int(res as i64));
    }
    let paths = find_all(k.json().root(), path.get(), is(Ty::Array))?;
    let mut res = Vec::with_capacity(paths.len());
    let mut changed = false;
    for p in paths {
        res.push(match p {
            Some(p) => {
                changed = true;
                Reply::Int(arr_append(k.json().root_mut(), &p, items.clone())? as i64)
            }
            None => Reply::Nil,
        });
    }
    if changed {
        replicate(&mut k, "json.arrappend");
    }
    Ok(Reply::Array(res))
}

fn normalize_start(start: i64, len: i64) -> i64 {
    if start < 0 { 0.max(len.wrapping_add(start)) } else { start.min(len - 1) }
}

enum Search {
    At(i64),
    Missing,
    NotArray,
}

fn index_of(n: Node, v: &J, start: i64, end: i64) -> Search {
    let Some(arr) = n.arr() else { return Search::NotArray };
    let len = arr.len() as i64;
    if len == 0 {
        return Search::Missing;
    }
    let start = normalize_start(start, len);
    let end = match end {
        0 => len,
        e if e < 0 => 0.max(len.wrapping_add(e)),
        e => e.min(len),
    };
    if end < start {
        return Search::Missing;
    }
    for i in start..end {
        let Some(x) = arr.get(i as usize) else { return Search::Missing };
        if is_equal(x, Node::Ref(v)) {
            return Search::At(i);
        }
    }
    Search::Missing
}

/// JSON.ARRINDEX key path value [start [stop]]
fn arrindex(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let mut a = Args::new(cx, args);
    let key = a.next_arg()?;
    let path = Path::new(a.next_str()?);
    let text = a.next_str()?;
    let start = a.next().map_or(Ok(0), |v| integer(v))?;
    let end = a.next().map_or(Ok(0), |v| integer(v))?;
    a.done()?;
    let found = cx.get(key);
    let v: serde_json::Value = serde_json::from_str(text).map_err(err)?;
    let v = plain(Node::Ref(&serde_to_j(v)));
    let d = doc(found)?.ok_or(NO_PATH)?;
    if path.is_legacy() {
        return match index_of(first(d, path.get())?, &v, start, end) {
            Search::NotArray => Err(expected("array")),
            Search::At(i) => Ok(Reply::Int(i)),
            Search::Missing => Ok(Reply::Int(-1)),
        };
    }
    Ok(Reply::Array(
        values(d, path.get())?
            .into_iter()
            .map(|n| match index_of(n, &v, start, end) {
                Search::NotArray => Reply::Nil,
                Search::At(i) => Reply::Int(i),
                Search::Missing => Reply::Int(-1),
            })
            .collect(),
    ))
}

/// A serde value as a document value (heterogeneous arrays, like serde's own).
fn serde_to_j(v: serde_json::Value) -> J {
    use serde_json::Value as V;
    match v {
        V::Null => J::Null,
        V::Bool(b) => J::Bool(b),
        V::Number(n) => J::Num(match (n.as_i64(), n.as_u64()) {
            (Some(i), _) => N::I(i),
            (None, Some(u)) => N::U(u),
            _ => N::F(n.as_f64().unwrap_or_default()),
        }),
        V::String(s) => J::str(&s),
        V::Array(items) => J::Arr(Box::new(Arr::het_from(items.into_iter().map(serde_to_j).collect()))),
        V::Object(m) => {
            let mut o = Obj::with_capacity(m.len());
            for (k, v) in m {
                o.insert(&k, serde_to_j(v));
            }
            J::Obj(Box::new(o))
        }
    }
}

fn arr_insert(root: &mut J, path: &[String], items: &[J], idx: i64) -> Result<usize, Error> {
    update(root, path, |at, depth| {
        let arr = value_at(at)?.as_arr_mut().ok_or_else(|| expected("array"))?;
        let len = arr.len() as i64;
        let idx = if idx < 0 { len.wrapping_add(idx) } else { idx };
        if !(0..=len).contains(&idx) {
            return Err(Error::new("ERR index out of bounds"));
        }
        if too_deep(items, depth) {
            return Err(RECURSION);
        }
        arr.extend(items.to_vec());
        arr.rotate_right_from(idx as usize, items.len());
        Ok(arr.len())
    })
}

/// Matches in mutation order, each with its place in the reply.
fn in_mutation_order(paths: Vec<Option<Loc>>) -> Vec<(usize, Loc)> {
    let mut indexed: Vec<(usize, Loc)> = paths.into_iter().enumerate().filter_map(|(i, p)| p.map(|p| (i, p))).collect();
    if indexed.len() >= 2 {
        indexed.sort_by(|a, b| mutation_order(&a.1, &b.1));
    }
    indexed
}

/// JSON.ARRINSERT key path index json [json ...]
fn arrinsert(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    cx.changes(0);
    let mut a = Args::new(cx, args);
    let key = a.next_arg()?;
    let path = Path::new(a.next_str()?);
    let index = a.next_i64()?;
    let items = array_values(&mut a)?;
    let mut k = open(cx, key)?.ok_or(NO_KEY)?;
    if path.is_legacy() {
        let mut paths = find(k.json().root(), path.get(), is(Ty::Array))?;
        if paths.is_empty() {
            return Err(no_path_or("not an array"));
        }
        paths.sort_by(mutation_order);
        let mut res = 0;
        for p in &paths {
            res = arr_insert(k.json().root_mut(), p, &items, index)?;
        }
        replicate(&mut k, "json.arrinsert");
        return Ok(Reply::Int(res as i64));
    }
    let paths = find_all(k.json().root(), path.get(), is(Ty::Array))?;
    let mut res = vec![Reply::Nil; paths.len()];
    let mut changed = false;
    for (i, p) in in_mutation_order(paths) {
        changed = true;
        res[i] = Reply::Int(arr_insert(k.json().root_mut(), &p, &items, index)? as i64);
    }
    if changed {
        replicate(&mut k, "json.arrinsert");
    }
    Ok(Reply::Array(res))
}

/// JSON.ARRLEN key [path]
fn arrlen(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let mut a = Args::new(cx, args);
    let key = a.next_arg()?;
    let path = a.path();
    let legacy = path.is_legacy();
    let d = match doc(cx.get(key))? {
        Some(d) => d,
        None if legacy => return Ok(Reply::Nil),
        None => return Err(NO_KEY),
    };
    let found = find_values(d, path.get(), is(Ty::Array))?;
    if legacy && found.is_empty() {
        return Err(NO_PATH);
    }
    let mut res = Vec::with_capacity(found.len());
    for v in found {
        let r = match v.and_then(|n| n.child_count()) {
            Some(n) => Reply::Int(n as i64),
            None if legacy => return Err(no_path_or("not an array")),
            None => Reply::Nil,
        };
        if legacy {
            return Ok(r);
        }
        res.push(r);
    }
    Ok(Reply::Array(res))
}

fn arr_pop(root: &mut J, path: &[String], index: i64) -> Result<Option<J>, Error> {
    update(root, path, |at, _| {
        let arr = value_at(at)?.as_arr_mut().ok_or_else(|| expected("array"))?;
        if arr.is_empty() {
            return Ok(None);
        }
        let i = normalize_start(index, arr.len() as i64) as usize;
        Ok(arr.remove(i))
    })
}

/// JSON.ARRPOP key [FORMAT f] [path [index]]
fn arrpop(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    cx.changes(0);
    let mut a = Args::new(cx, args);
    let key = a.next_arg()?;
    let mut f = Fmt::new(cx.resp() == 3, ReplyFormat::Strings);
    let path_arg = match a.next() {
        Some(arg) if utf8(arg)?.eq_ignore_ascii_case("FORMAT") => match a.next_str() {
            Ok(next) => {
                f.format = reply_format(next)?;
                if f.format == ReplyFormat::String {
                    return Err(WRONG_REPLY_FORMAT);
                }
                if !f.resp3 {
                    return Err(RESP2_FORMAT);
                }
                a.next()
            }
            Err(_) => Some(arg),
        },
        other => other,
    };
    let (path, index) = match path_arg {
        None => (Path::new("."), i64::MAX),
        Some(s) => (Path::new(utf8(s)?), a.next_i64().unwrap_or(-1)),
    };
    // `open_key_write` doesn't check the type: a wrong one shows after the format check.
    let k = open(cx, key);
    if path.is_legacy() && f.format != ReplyFormat::Strings {
        return Err(Error::new("Legacy paths are supported only with FORMAT STRINGS"));
    }
    let mut k = k?.ok_or(NO_KEY)?;
    if path.is_legacy() {
        let mut paths = find(k.json().root(), path.get(), is(Ty::Array))?;
        if paths.is_empty() {
            return Err(no_path_or("not an array"));
        }
        paths.sort_by(mutation_order);
        let mut res = Reply::Nil;
        for p in &paths {
            res = arr_pop(k.json().root_mut(), p, index)?.map_or(Reply::Nil, |v| bulk(compact(&v)));
        }
        replicate(&mut k, "json.arrpop");
        return Ok(res);
    }
    let paths = find_all(k.json().root(), path.get(), is(Ty::Array))?;
    let mut res = vec![Reply::Nil; paths.len()];
    let mut changed = false;
    for (i, p) in in_mutation_order(paths) {
        if let Some(v) = arr_pop(k.json().root_mut(), &p, index)? {
            changed = true;
            res[i] = if f.native() { native(Node::Ref(&v), &f) } else { bulk(compact(&v)) };
        }
    }
    if changed {
        replicate(&mut k, "json.arrpop");
    }
    Ok(Reply::Array(res))
}

/// An index into an array of `len` (`ArrayIndex::normalize`).
fn normalize(i: i64, len: i64) -> usize {
    let i = if i < 0 {
        len.wrapping_sub(len.min(i.wrapping_neg()))
    } else if len > 0 {
        (len - 1).min(i)
    } else {
        0
    };
    i as usize
}

fn arr_trim(root: &mut J, path: &[String], start: i64, stop: i64) -> Result<usize, Error> {
    update(root, path, |at, _| {
        let arr = value_at(at)?.as_arr_mut().ok_or_else(|| expected("array"))?;
        let len = arr.len() as i64;
        let stop = normalize(stop, len);
        let start = if start < 0 || start < len { normalize(start, len) } else { stop + 1 };
        let range = if start > stop || len == 0 { 0..0 } else { start..stop + 1 };
        arr.rotate_left(range.start);
        arr.truncate(range.end - range.start);
        Ok(arr.len())
    })
}

/// JSON.ARRTRIM key path start stop
fn arrtrim(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    cx.changes(0);
    let mut a = Args::new(cx, args);
    let key = a.next_arg()?;
    let path = Path::new(a.next_str()?);
    let start = a.next_i64()?;
    let stop = a.next_i64()?;
    let mut k = open(cx, key)?.ok_or(NO_KEY)?;
    if path.is_legacy() {
        let mut paths = find(k.json().root(), path.get(), is(Ty::Array))?;
        if paths.is_empty() {
            return Err(no_path_or("not an array"));
        }
        paths.sort_by(mutation_order);
        let mut res = 0;
        for p in &paths {
            res = arr_trim(k.json().root_mut(), p, start, stop)?;
        }
        replicate(&mut k, "json.arrtrim");
        return Ok(Reply::Int(res as i64));
    }
    let paths = find_all(k.json().root(), path.get(), is(Ty::Array))?;
    let mut res = vec![Reply::Nil; paths.len()];
    let mut changed = false;
    for (i, p) in in_mutation_order(paths) {
        changed = true;
        res[i] = Reply::Int(arr_trim(k.json().root_mut(), &p, start, stop)? as i64);
    }
    if changed {
        replicate(&mut k, "json.arrtrim");
    }
    Ok(Reply::Array(res))
}

fn keys_of(n: Node) -> Option<Reply> {
    n.obj().map(|o| Reply::Array(o.keys().map(|k| bulk(k.to_string())).collect()))
}

/// JSON.OBJKEYS key [path]
fn objkeys(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let mut a = Args::new(cx, args);
    let key = a.next_arg()?;
    let path = a.path();
    let found = cx.get(key);
    if path.is_legacy() {
        if is_projection(path.get()) {
            return Err(PROJECTION);
        }
        let Some(d) = doc(found)? else { return Ok(Reply::Nil) };
        return match first(d, path.get()) {
            Ok(n) => keys_of(n).ok_or_else(|| no_path_or("not an object")),
            Err(_) => Ok(Reply::Nil),
        };
    }
    let d = doc(found)?.ok_or(NO_KEY)?;
    Ok(Reply::Array(
        find_values(d, path.get(), is(Ty::Object))?.into_iter().map(|v| v.and_then(keys_of).unwrap_or(Reply::Nil)).collect(),
    ))
}

/// JSON.OBJLEN key [path]
fn objlen(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let mut a = Args::new(cx, args);
    let key = a.next_arg()?;
    let path = a.path();
    let found = doc(cx.get(key))?;
    if path.is_legacy() {
        let Some(d) = found else { return Ok(Reply::Nil) };
        if is_projection(path.get()) {
            return Err(PROJECTION);
        }
        return match first(d, path.get()) {
            Ok(n) => n.obj().map(|o| Reply::Int(o.len() as i64)).ok_or_else(|| expected("object")),
            Err(_) => Ok(Reply::Nil),
        };
    }
    let d = found.ok_or_else(|| no_path_or("not an object"))?;
    Ok(Reply::Array(
        find_values(d, path.get(), is(Ty::Object))?
            .into_iter()
            .map(|v| v.and_then(|n| n.obj()).map_or(Reply::Nil, |o| Reply::Int(o.len() as i64)))
            .collect(),
    ))
}

fn clear_at(root: &mut J, path: &[String]) -> Result<i64, Error> {
    update(root, path, |at, _| {
        Ok(match value_at(at)? {
            J::Obj(o) => {
                o.clear();
                1
            }
            J::Arr(a) => {
                a.clear();
                1
            }
            v @ J::Num(_) => {
                *v = J::Num(N::I(0));
                1
            }
            _ => 0,
        })
    })
}

/// JSON.CLEAR key [path]: empties containers and zeroes numbers.
fn clear(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    cx.changes(0);
    let mut a = Args::new(cx, args);
    if a.len() < 1 || a.len() > 2 {
        return Err(arity_error(cx.cmd.name));
    }
    let key = a.next_arg()?;
    let path = a.path();
    let mut k = open(cx, key)?.ok_or(NO_KEY)?;
    let paths = find(k.json().root(), path.get(), |n| match n.ty() {
        Ty::Array | Ty::Object => n.child_count().is_some_and(|l| l > 0),
        Ty::Long => n.long().is_some_and(|v| v != 0),
        Ty::Double => n.double().is_some_and(|v| v != 0.0),
        _ => false,
    })?;
    let mut cleared = 0;
    for p in &paths {
        cleared += clear_at(k.json().root_mut(), p)?;
    }
    if cleared > 0 {
        replicate(&mut k, "json.clear");
    }
    Ok(Reply::Int(cleared))
}

/// What a value occupies (`IValue::mem_allocated` plus its own slot).
fn memory(n: Node) -> i64 {
    (match n {
        Node::Ref(j) => j.mem(),
        Node::Num(v) => v.mem(),
    } + 8) as i64
}

/// JSON.DEBUG MEMORY key [path] | DEFRAG_INFO | HELP
fn debug(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let mut a = Args::new(cx, args);
    match a.next_str()?.to_uppercase().as_str() {
        "MEMORY" => {
            let key = a.next_arg()?;
            let path = a.path();
            let found = doc(cx.get(key))?;
            if path.is_legacy() {
                Ok(Reply::Int(match found {
                    Some(d) => memory(first(d, path.get())?),
                    None => 0,
                }))
            } else {
                Ok(Reply::Array(match found {
                    Some(d) => values(d, path.get())?.into_iter().map(|n| Reply::Int(memory(n))).collect(),
                    None => Vec::new(),
                }))
            }
        }
        "DEFRAG_INFO" => Ok(Reply::Map(
            ["defrag_ended", "defrag_started", "keys_defrag"].into_iter().map(|k| (bulk(k.to_string()), Reply::Int(0))).collect(),
        )),
        "HELP" => Ok(Reply::Array(vec![
            bulk("MEMORY <key> [path] - reports memory usage".to_string()),
            bulk("HELP                - this message".to_string()),
        ])),
        _ => Err(Error::new("ERR unknown subcommand - try `JSON.DEBUG HELP`")),
    }
}

/// JSON.RESP key [path]
fn resp(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let mut a = Args::new(cx, args);
    let key = a.next_arg()?;
    let path = match a.next() {
        None => Path::new("."),
        Some(s) => Path::new(utf8(s)?),
    };
    let Some(d) = doc(cx.get(key))? else { return Ok(Reply::Nil) };
    let q = compiled(path.get())?;
    if q.is_projection() {
        return Ok(Reply::Array(project(&q, d).iter().map(|v| resp_of(Node::Ref(v))).collect()));
    }
    let found = select(&q, d);
    if path.is_legacy() {
        Ok(resp_of(found.into_iter().next().ok_or(NO_PATH)?))
    } else {
        Ok(Reply::Array(found.into_iter().map(resp_of).collect()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn locs(v: &[&str]) -> Vec<Loc> {
        v.iter().map(|s| if s.is_empty() { Vec::new() } else { s.split(',').map(str::to_string).collect() }).collect()
    }

    #[test]
    fn nested_locations_keep_the_outermost() {
        let mut p = locs(&["a", "a,b", "a,b,0", "c,1", "c,10", "c,2"]);
        prepare_for_updating(&mut p);
        assert_eq!(p, locs(&["a", "c,10", "c,2", "c,1"]));
        let mut p: Vec<Loc> = (0..20).map(|i| vec![i.to_string()]).collect();
        prepare_for_updating(&mut p);
        assert_eq!(p, (0..20).rev().map(|i| vec![i.to_string()]).collect::<Vec<_>>());
    }

    #[test]
    fn trim_indexes() {
        assert_eq!(normalize(-6, 5), 0);
        assert_eq!(normalize(-2, 5), 3);
        assert_eq!(normalize(6, 5), 4);
        assert_eq!(normalize(1, 0), 0);
        assert_eq!(normalize_start(-1, 3), 2);
        assert_eq!(normalize_start(i64::MAX, 3), 2);
    }

    #[test]
    fn merge_patch() {
        let mut d = parse(r#"{"a":1,"b":{"c":2,"d":3}}"#, None).unwrap();
        merge(&mut d, parse(r#"{"a":null,"b":{"c":null,"e":[1]},"f":"x"}"#, None).unwrap());
        assert_eq!(compact(&d), r#"{"b":{"d":3,"e":[1]},"f":"x"}"#);
    }
}
