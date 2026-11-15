//! `_source` filtering (`XContentMapValues.filter`): `includes` and `excludes` are glob patterns
//! over dotted field paths, each also matching everything below it (`a` covers `a.b.c`); an
//! object is kept when it matched an include or kept a child; excludes win. Array elements are
//! filtered with the array's path. Key order is the document's.
//!
//! Patterns run as a small NFA over the path's characters (`*` matches any run, dots
//! included), stepped key by key, so filtering a document is one walk with no per-key path
//! strings.
//!
//! Elasticsearch filters stored source bytes with Jackson's `FilterPath` instead
//! (`SourceFilter.filterBytes`) unless an exclude has a wildcard: there includes match path
//! segment by segment, a `*` stays within one segment (`*0` keeps `a0` but neither `o.kw0`
//! nor a dotted key `x.y0`) and `**` spans any number. [`SourceFilter::new`] picks the mode.

use crate::json::{Map, Value};

/// A compiled set of globs, each implicitly followed by an optional `.<anything>`.
#[derive(Clone, Debug)]
pub struct Globs {
    pats: Vec<Vec<u8>>,
    /// `FilterPath` semantics: a single `*` does not match `.`.
    segments: bool,
}

/// An NFA state: positions reached in each pattern, or "everything below matched".
#[derive(Clone, Debug, PartialEq)]
pub enum State {
    /// No pattern can match any more.
    Dead,
    /// A pattern matched, then `.` followed: every continuation matches.
    All,
    /// Live positions `(pattern, offset)`.
    At(Vec<(u16, u16)>),
}

impl Globs {
    pub fn new(patterns: &[String]) -> Globs {
        Globs { pats: patterns.iter().map(|p| p.as_bytes().to_vec()).collect(), segments: false }
    }

    /// Globs whose single `*` matches within one path segment (`FilterPath`); `**` matches
    /// across segments.
    pub fn segment_wise(patterns: &[String]) -> Globs {
        Globs { segments: true, ..Globs::new(patterns) }
    }

    pub fn is_empty(&self) -> bool {
        self.pats.is_empty()
    }

    pub fn start(&self) -> State {
        let mut v = Vec::new();
        for (i, p) in self.pats.iter().enumerate() {
            self.closure(i, 0, p, &mut v);
        }
        if v.is_empty() { State::Dead } else { State::At(v) }
    }

    /// Adds `pos` and the positions reachable by letting `*` match nothing.
    fn closure(&self, i: usize, mut pos: usize, p: &[u8], out: &mut Vec<(u16, u16)>) {
        loop {
            let e = (i as u16, pos as u16);
            if !out.contains(&e) {
                out.push(e);
            }
            if pos < p.len() && p[pos] == b'*' {
                pos += 1;
            } else {
                break;
            }
        }
    }

    pub fn accepts(&self, s: &State) -> bool {
        match s {
            State::Dead => false,
            State::All => true,
            State::At(v) => self.accepts_at(v),
        }
    }

    fn accepts_at(&self, v: &[(u16, u16)]) -> bool {
        v.iter().any(|&(i, pos)| pos as usize == self.pats[i as usize].len())
    }

    pub fn step_byte(&self, s: &State, c: u8) -> State {
        match s {
            State::Dead => State::Dead,
            State::All => State::All,
            State::At(v) => {
                if c == b'.' && self.accepts_at(v) {
                    return State::All;
                }
                let mut out = Vec::new();
                self.step_positions(v, c, &mut out);
                if out.is_empty() { State::Dead } else { State::At(out) }
            }
        }
    }

    /// The positions `v` reaches on `c` (a `.` after a full match is the caller's).
    fn step_positions(&self, v: &[(u16, u16)], c: u8, out: &mut Vec<(u16, u16)>) {
        for &(i, pos) in v {
            let p = &self.pats[i as usize];
            let pos = pos as usize;
            if pos < p.len() {
                if p[pos] == b'*' {
                    let double = (pos + 1 < p.len() && p[pos + 1] == b'*') || (pos > 0 && p[pos - 1] == b'*');
                    if c == b'.' && self.segments && !double {
                        continue;
                    }
                    self.closure(i as usize, pos, p, out);
                } else if p[pos] == c {
                    self.closure(i as usize, pos + 1, p, out);
                }
            }
        }
    }

    /// [`step_byte`](Self::step_byte) over every byte of `key`, alternating two position
    /// buffers instead of allocating a state per byte.
    pub fn step(&self, s: &State, key: &str) -> State {
        let State::At(v) = s else { return s.clone() };
        let mut cur = v.clone();
        let mut next = Vec::with_capacity(cur.len() + 2);
        for &c in key.as_bytes() {
            if c == b'.' && self.accepts_at(&cur) {
                return State::All;
            }
            next.clear();
            self.step_positions(&cur, c, &mut next);
            if next.is_empty() {
                return State::Dead;
            }
            std::mem::swap(&mut cur, &mut next);
        }
        State::At(cur)
    }

    /// Whether `path` matches (a pattern, or anything below one).
    pub fn matches(&self, path: &str) -> bool {
        self.accepts(&self.step(&self.start(), path))
    }
}

/// Source filtering as `FetchSourceContext` / mapping `_source` includes and excludes.
#[derive(Clone, Debug)]
pub struct SourceFilter {
    includes: Option<Globs>,
    excludes: Globs,
    /// Map filtering (`XContentMapValues.filter`) builds every object it filters as a new
    /// `HashMap`, so those print in Java hash order; objects kept whole, and everything the
    /// byte filter writes, keep the document's order.
    hash_order: bool,
}

impl SourceFilter {
    pub fn new(includes: &[String], excludes: &[String]) -> SourceFilter {
        // `SourceFilter.canFilterBytes`: wildcard excludes filter the parsed map instead.
        let bytes = !excludes.iter().any(|e| e.contains('*'));
        let inc = |p: &[String]| if bytes { Globs::segment_wise(p) } else { Globs::new(p) };
        SourceFilter { includes: (!includes.is_empty()).then(|| inc(includes)), excludes: Globs::new(excludes), hash_order: !bytes }
    }

    /// The filter Elasticsearch applies to a parsed (or patched) source map
    /// (`SourceFilter.filterMap`): `*` spans dots whatever the excludes.
    pub fn over_map(includes: &[String], excludes: &[String]) -> SourceFilter {
        SourceFilter { includes: (!includes.is_empty()).then(|| Globs::new(includes)), excludes: Globs::new(excludes), hash_order: true }
    }

    pub fn is_noop(&self) -> bool {
        self.includes.is_none() && self.excludes.is_empty()
    }

    pub fn apply(&self, m: &Map) -> Map {
        if self.is_noop() {
            return m.clone();
        }
        let inc = self.includes.as_ref().map(|g| g.start()).unwrap_or(State::All);
        let exc = self.excludes.start();
        self.filter_map(m, &inc, &exc)
    }

    fn inc_globs(&self) -> &Globs {
        static EMPTY: std::sync::OnceLock<Globs> = std::sync::OnceLock::new();
        self.includes.as_ref().unwrap_or_else(|| EMPTY.get_or_init(|| Globs::new(&[])))
    }

    fn filter_map(&self, m: &Map, inc: &State, exc: &State) -> Map {
        let ig = self.inc_globs();
        let eg = &self.excludes;
        let mut out = Map::new();
        for (k, v) in m {
            let is = ig.step(inc, k);
            if is == State::Dead {
                continue;
            }
            let es = eg.step(exc, k);
            if es != State::Dead && eg.accepts(&es) {
                continue;
            }
            let mut sub_inc = is.clone();
            let accepted = ig.accepts(&is);
            if accepted {
                if es == State::Dead || eg.step_byte(&es, b'.') == State::Dead {
                    out.insert(k.clone(), v.clone());
                    continue;
                }
                sub_inc = State::All;
            }
            match v {
                Value::Obj(child) => {
                    let si = ig.step_byte(&sub_inc, b'.');
                    if si == State::Dead {
                        continue;
                    }
                    let se = if es == State::Dead { State::Dead } else { eg.step_byte(&es, b'.') };
                    let f = crate::stack::maybe_grow(|| self.filter_map(child, &si, &se));
                    if accepted || !f.is_empty() {
                        out.insert(k.clone(), Value::Obj(f));
                    }
                }
                Value::Arr(a) => {
                    let f = self.filter_arr(a, &sub_inc, &es);
                    if !f.is_empty() {
                        out.insert(k.clone(), Value::Arr(f));
                    }
                }
                leaf => {
                    if accepted && (es == State::Dead || !eg.accepts(&es)) {
                        out.insert(k.clone(), leaf.clone());
                    }
                }
            }
        }
        if self.hash_order { crate::json::java_hash_map(out, 16) } else { out }
    }

    fn filter_arr(&self, a: &[Value], inc: &State, exc: &State) -> Vec<Value> {
        let ig = self.inc_globs();
        let eg = &self.excludes;
        let is_include = ig.accepts(inc);
        let mut out = Vec::new();
        for v in a {
            match v {
                Value::Obj(m) => {
                    let si = ig.step_byte(inc, b'.');
                    let se = if *exc == State::Dead { State::Dead } else { eg.step_byte(exc, b'.') };
                    let f = crate::stack::maybe_grow(|| self.filter_map(m, &si, &se));
                    if !f.is_empty() {
                        out.push(Value::Obj(f));
                    }
                }
                Value::Arr(inner) => {
                    let f = crate::stack::maybe_grow(|| self.filter_arr(inner, inc, exc));
                    if !f.is_empty() {
                        out.push(Value::Arr(f));
                    }
                }
                leaf if is_include => out.push(leaf.clone()),
                _ => {}
            }
        }
        out
    }

    /// [`apply`](Self::apply) over a source's JSON text, writing the filtered object as compact
    /// JSON to `out` without building a tree: the hits of a filtered search go out as raw JSON
    /// (`json::raw`). The output is byte for byte what writing `apply`'s map gives: values are
    /// copied as they are where the writer would print the same text (plain strings, short
    /// integers, literals) and parsed and written back otherwise (escapes, floats, big
    /// numbers), whitespace dropped. Returns false, leaving `out` unspecified, when `src` is not
    /// one object of plain JSON (invalid UTF-8, comments, trailing content) or the filter is
    /// a map filter (its objects print in hash order): the caller then filters the parsed
    /// tree, which reports any error.
    pub fn filter_bytes(&self, src: &[u8], out: &mut Vec<u8>) -> bool {
        if self.hash_order {
            return false;
        }
        let Ok(s) = std::str::from_utf8(src) else { return false };
        let mut r = Reader { s, b: src, i: 0 };
        r.ws();
        if r.peek() != Some(b'{') {
            return false;
        }
        let inc = self.includes.as_ref().map(|g| g.start()).unwrap_or(State::All);
        let exc = self.excludes.start();
        if self.stream_obj(&mut r, &inc, &exc, out).is_none() {
            return false;
        }
        r.ws();
        r.i == src.len()
    }

    /// [`filter_map`](Self::filter_map) over the object at `r`: writes `{…}` and returns
    /// whether a member was kept.
    fn stream_obj(&self, r: &mut Reader, inc: &State, exc: &State, out: &mut Vec<u8>) -> Option<bool> {
        let ig = self.inc_globs();
        let eg = &self.excludes;
        r.i += 1;
        out.push(b'{');
        r.ws();
        let mut any = false;
        if r.peek()? == b'}' {
            r.i += 1;
            out.push(b'}');
            return Some(false);
        }
        loop {
            r.ws();
            let key = r.string()?;
            r.ws();
            if r.next()? != b':' {
                return None;
            }
            r.ws();
            let mark = out.len();
            let member = |out: &mut Vec<u8>, any: bool| {
                if any {
                    out.push(b',');
                }
                key.write(out);
                out.push(b':');
            };
            let key = &*key.text;
            let is = ig.step(inc, key);
            let es = if is == State::Dead { State::Dead } else { eg.step(exc, key) };
            if is == State::Dead || (es != State::Dead && eg.accepts(&es)) {
                r.skip_value()?;
            } else {
                let accepted = ig.accepts(&is);
                let whole = accepted && (es == State::Dead || eg.step_byte(&es, b'.') == State::Dead);
                let sub_inc = if accepted { State::All } else { is };
                match r.peek()? {
                    _ if whole => {
                        member(out, any);
                        r.copy_value(out)?;
                        any = true;
                    }
                    b'{' => {
                        let si = ig.step_byte(&sub_inc, b'.');
                        if si == State::Dead {
                            r.skip_value()?;
                        } else {
                            let se = if es == State::Dead { State::Dead } else { eg.step_byte(&es, b'.') };
                            member(out, any);
                            let kept = crate::stack::maybe_grow(|| self.stream_obj(r, &si, &se, out))?;
                            if accepted || kept {
                                any = true;
                            } else {
                                out.truncate(mark);
                            }
                        }
                    }
                    b'[' => {
                        member(out, any);
                        if self.stream_arr(r, &sub_inc, &es, out)? {
                            any = true;
                        } else {
                            out.truncate(mark);
                        }
                    }
                    _ if accepted => {
                        member(out, any);
                        r.copy_value(out)?;
                        any = true;
                    }
                    _ => r.skip_value()?,
                }
            }
            r.ws();
            match r.next()? {
                b',' => {}
                b'}' => break,
                _ => return None,
            }
        }
        out.push(b'}');
        Some(any)
    }

    /// [`filter_arr`](Self::filter_arr) over the array at `r`: writes `[…]` and returns whether
    /// an element was kept.
    fn stream_arr(&self, r: &mut Reader, inc: &State, exc: &State, out: &mut Vec<u8>) -> Option<bool> {
        let ig = self.inc_globs();
        let eg = &self.excludes;
        let is_include = ig.accepts(inc);
        r.i += 1;
        out.push(b'[');
        r.ws();
        let mut any = false;
        if r.peek()? == b']' {
            r.i += 1;
            out.push(b']');
            return Some(false);
        }
        loop {
            r.ws();
            let mark = out.len();
            if any {
                out.push(b',');
            }
            let kept = match r.peek()? {
                b'{' => {
                    let si = ig.step_byte(inc, b'.');
                    if si == State::Dead {
                        r.skip_value()?;
                        false
                    } else {
                        let se = if *exc == State::Dead { State::Dead } else { eg.step_byte(exc, b'.') };
                        crate::stack::maybe_grow(|| self.stream_obj(r, &si, &se, out))?
                    }
                }
                b'[' => crate::stack::maybe_grow(|| self.stream_arr(r, inc, exc, out))?,
                _ if is_include => {
                    r.copy_value(out)?;
                    true
                }
                _ => {
                    r.skip_value()?;
                    false
                }
            };
            if kept {
                any = true;
            } else {
                out.truncate(mark);
            }
            r.ws();
            match r.next()? {
                b',' => {}
                b']' => break,
                _ => return None,
            }
        }
        out.push(b']');
        Some(any)
    }
}

/// A string of a source's text.
struct Str<'a> {
    /// The token, quotes included.
    token: &'a [u8],
    text: std::borrow::Cow<'a, str>,
}

impl Str<'_> {
    /// Writes the string as the JSON writer would: the token when it has no escapes (the
    /// writer escapes nothing else that a parsed string can hold), else the text re-escaped.
    fn write(&self, out: &mut Vec<u8>) {
        match self.text {
            std::borrow::Cow::Borrowed(_) => out.extend_from_slice(self.token),
            std::borrow::Cow::Owned(ref s) => crate::json::write::write_str(out, s),
        }
    }
}

/// A cursor over a source's JSON text for [`SourceFilter::filter_bytes`]: tokens are checked
/// as far as copying them needs; skipped values only for their structure (the text was parsed
/// when the document was indexed).
struct Reader<'a> {
    s: &'a str,
    b: &'a [u8],
    i: usize,
}

impl<'a> Reader<'a> {
    #[inline]
    fn ws(&mut self) {
        while let Some(b' ' | b'\t' | b'\n' | b'\r') = self.b.get(self.i) {
            self.i += 1;
        }
    }

    #[inline]
    fn peek(&self) -> Option<u8> {
        self.b.get(self.i).copied()
    }

    #[inline]
    fn next(&mut self) -> Option<u8> {
        let c = self.peek()?;
        self.i += 1;
        Some(c)
    }

    /// The end (past the closing quote) of the string starting at `i`, and whether it has an
    /// escape.
    #[inline]
    fn string_end(&self) -> Option<(usize, bool)> {
        let mut j = self.i + 1;
        let mut escaped = false;
        loop {
            j += memchr::memchr2(b'"', b'\\', self.b.get(j..)?)?;
            if self.b[j] == b'"' {
                return Some((j + 1, escaped));
            }
            escaped = true;
            j += 2;
        }
    }

    /// The string at `i`. Escapes and raw control characters (which the parser rejects) go
    /// through the parser.
    fn string(&mut self) -> Option<Str<'a>> {
        if self.peek()? != b'"' {
            return None;
        }
        let (end, escaped) = self.string_end()?;
        let token = &self.b[self.i..end];
        let start = self.i;
        self.i = end;
        let body = &token[1..token.len() - 1];
        if !escaped && !body.iter().any(|&c| c < 0x20) {
            return Some(Str { token, text: std::borrow::Cow::Borrowed(&self.s[start + 1..end - 1]) });
        }
        match crate::json::parse(token) {
            Ok(Some(Value::Str(s))) => Some(Str { token, text: std::borrow::Cow::Owned(s) }),
            _ => None,
        }
    }

    /// Copies the value at `i` as compact JSON.
    fn copy_value(&mut self, out: &mut Vec<u8>) -> Option<()> {
        match self.peek()? {
            b'"' => self.string()?.write(out),
            b'{' | b'[' => {
                let close = if self.b[self.i] == b'{' { b'}' } else { b']' };
                out.push(self.b[self.i]);
                self.i += 1;
                self.ws();
                if self.peek()? == close {
                    self.i += 1;
                    out.push(close);
                    return Some(());
                }
                loop {
                    self.ws();
                    if close == b'}' {
                        self.string()?.write(out);
                        self.ws();
                        if self.next()? != b':' {
                            return None;
                        }
                        out.push(b':');
                        self.ws();
                    }
                    crate::stack::maybe_grow(|| self.copy_value(out))?;
                    self.ws();
                    match self.next()? {
                        b',' => out.push(b','),
                        c if c == close => break,
                        _ => return None,
                    }
                }
                out.push(close);
            }
            b'-' | b'0'..=b'9' => {
                let start = self.i;
                let plain = self.number()?;
                let token = &self.b[start..self.i];
                // Integers of up to 18 digits print as written; `-0`, fractions, exponents and
                // longer integers print as the parser reads them.
                if plain && token.len() - usize::from(token[0] == b'-') <= 18 && token != b"-0" {
                    out.extend_from_slice(token);
                } else {
                    match crate::json::parse(token) {
                        Ok(Some(v @ Value::Num(_))) => crate::json::write::write_scalar(out, &v),
                        _ => return None,
                    }
                }
            }
            c => {
                let word: &[u8] = match c {
                    b't' => b"true",
                    b'f' => b"false",
                    b'n' => b"null",
                    _ => return None,
                };
                if !self.b[self.i..].starts_with(word) {
                    return None;
                }
                self.i += word.len();
                out.extend_from_slice(word);
            }
        }
        Some(())
    }

    /// Moves past the JSON number at `i` (`-?(0|[1-9][0-9]*)(.[0-9]+)?([eE][+-]?[0-9]+)?`);
    /// true when it is an integer.
    fn number(&mut self) -> Option<bool> {
        let digits = |r: &mut Self| {
            let start = r.i;
            while let Some(b'0'..=b'9') = r.peek() {
                r.i += 1;
            }
            r.i > start
        };
        if self.peek() == Some(b'-') {
            self.i += 1;
        }
        match self.peek()? {
            b'0' => self.i += 1,
            b'1'..=b'9' => {
                digits(self);
            }
            _ => return None,
        }
        let mut int = true;
        if self.peek() == Some(b'.') {
            self.i += 1;
            int = false;
            if !digits(self) {
                return None;
            }
        }
        if let Some(b'e' | b'E') = self.peek() {
            self.i += 1;
            int = false;
            if let Some(b'+' | b'-') = self.peek() {
                self.i += 1;
            }
            if !digits(self) {
                return None;
            }
        }
        Some(int)
    }

    /// Moves past the value at `i`.
    fn skip_value(&mut self) -> Option<()> {
        match self.peek()? {
            b'"' => self.i = self.string_end()?.0,
            b'{' | b'[' => {
                let mut depth = 0usize;
                loop {
                    match self.next()? {
                        b'"' => {
                            self.i -= 1;
                            self.i = self.string_end()?.0;
                        }
                        b'{' | b'[' => depth += 1,
                        b'}' | b']' => {
                            depth -= 1;
                            if depth == 0 {
                                return Some(());
                            }
                        }
                        b'/' => return None,
                        _ => {}
                    }
                }
            }
            _ => {
                let start = self.i;
                while let Some(c) = self.peek() {
                    if matches!(c, b',' | b'}' | b']' | b' ' | b'\t' | b'\n' | b'\r' | b'/') {
                        break;
                    }
                    self.i += 1;
                }
                if self.i == start {
                    return None;
                }
            }
        }
        Some(())
    }
}

/// Extracts the values at a dotted path of a source map (`XContentMapValues.extractRawValues`):
/// objects are walked through and arrays flattened, and a dotted key in the document (`"a.b":
/// 1`) matches too.
pub fn extract_values<'a>(m: &'a Map, path: &str, out: &mut Vec<&'a Value>) {
    extract_in(m, path, out);
}

/// [`extract_values`] for fields whose value can itself be an array of numbers (`[lon, lat]`
/// points, vectors): such arrays stay whole instead of being flattened.
pub fn extract_values_keeping_coordinates<'a>(m: &'a Map, path: &str, out: &mut Vec<&'a Value>) {
    extract_in_with(m, path, out, true);
}

fn extract_in<'a>(m: &'a Map, path: &str, out: &mut Vec<&'a Value>) {
    extract_in_with(m, path, out, false);
}

fn extract_in_with<'a>(m: &'a Map, path: &str, out: &mut Vec<&'a Value>, coords: bool) {
    // Shorter keys first (`a`, then `a.b`, …, the whole path last), as Elasticsearch walks.
    let bytes = path.as_bytes();
    for (i, &c) in bytes.iter().enumerate() {
        if c == b'.' {
            let (head, rest) = (&path[..i], &path[i + 1..]);
            if let Some(v) = m.get(head) {
                extract_value(v, rest, out, coords);
            }
        }
    }
    if let Some(v) = m.get(path) {
        if coords {
            push_flat_coords(v, out);
        } else {
            push_flat(v, out);
        }
    }
}

fn extract_value<'a>(v: &'a Value, path: &str, out: &mut Vec<&'a Value>, coords: bool) {
    match v {
        Value::Obj(m) => crate::stack::maybe_grow(|| extract_in_with(m, path, out, coords)),
        Value::Arr(a) => {
            for x in a {
                crate::stack::maybe_grow(|| extract_value(x, path, out, coords));
            }
        }
        _ => {}
    }
}

fn push_flat_coords<'a>(v: &'a Value, out: &mut Vec<&'a Value>) {
    match v {
        Value::Arr(a) if a.first().is_some_and(|x| matches!(x, Value::Num(_))) => out.push(v),
        Value::Arr(a) => {
            for x in a {
                push_flat_coords(x, out);
            }
        }
        other => out.push(other),
    }
}

fn push_flat<'a>(v: &'a Value, out: &mut Vec<&'a Value>) {
    match v {
        Value::Arr(a) => {
            for x in a {
                push_flat(x, out);
            }
        }
        other => out.push(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn src(s: &str) -> Map {
        match crate::json::parse(s.as_bytes()).unwrap().unwrap() {
            Value::Obj(m) => m,
            _ => panic!(),
        }
    }

    fn f(s: &str, inc: &[&str], exc: &[&str]) -> String {
        let inc: Vec<String> = inc.iter().map(|s| s.to_string()).collect();
        let exc: Vec<String> = exc.iter().map(|s| s.to_string()).collect();
        Value::Obj(SourceFilter::new(&inc, &exc).apply(&src(s))).to_json()
    }

    #[test]
    fn map_filtering_spans_dots() {
        // Elasticsearch 9.5.5: `_source: ["*0"]` over bytes keeps `a0` only; over a parsed
        // source (patched vectors, `fields`) `*` crosses dots.
        let doc = r#"{"o":{"kw0":1,"x":2},"a0":1,"n":[{"kw0":1,"z":2}]}"#;
        assert_eq!(f(doc, &["*0"], &[]), r#"{"a0":1}"#);
        let m = SourceFilter::over_map(&["*0".to_string()], &[]).apply(&src(doc));
        assert_eq!(Value::Obj(m).to_json(), r#"{"n":[{"kw0":1}],"o":{"kw0":1},"a0":1}"#);
    }

    #[test]
    fn includes_and_excludes_like_elasticsearch() {
        let d = r#"{"a":{"b":1,"c":2},"ab":3,"d":[{"e":1,"f":2},{"e":3}],"g":[1,2]}"#;
        assert_eq!(f(d, &["a"], &[]), r#"{"a":{"b":1,"c":2}}"#);
        assert_eq!(f(d, &["a.b"], &[]), r#"{"a":{"b":1}}"#);
        assert_eq!(f(d, &["a*"], &[]), r#"{"a":{"b":1,"c":2},"ab":3}"#);
        assert_eq!(f(d, &["d.e"], &[]), r#"{"d":[{"e":1},{"e":3}]}"#);
        assert_eq!(f(d, &[], &["a.b", "d"]), r#"{"a":{"c":2},"ab":3,"g":[1,2]}"#);
        assert_eq!(f(d, &["a"], &["a.c"]), r#"{"a":{"b":1}}"#);
        assert_eq!(f(d, &["*.e"], &[]), r#"{"d":[{"e":1},{"e":3}]}"#);
        assert_eq!(f(d, &["g"], &[]), r#"{"g":[1,2]}"#);
        assert_eq!(f(d, &["x"], &[]), "{}");
        // An include that names an object keeps it even when the exclude empties it.
        assert_eq!(f(r#"{"a":{"b":1}}"#, &["a"], &["a.b"]), r#"{"a":{}}"#);
    }

    /// Answers of Elasticsearch 9.5.5 for one document: includes match segment by segment
    /// (`FilterPath`), unless an exclude has a wildcard, which filters the map with globs that
    /// cross dots.
    #[test]
    fn wildcard_includes_stay_within_a_segment() {
        let d = r#"{"a0":1,"o":{"kw0":"x","b":2},"n":[{"text0":"t","z":1}],"p":{"q":{"r0":3}},"x.y0":5}"#;
        assert_eq!(f(d, &["*0"], &[]), r#"{"a0":1}"#);
        assert_eq!(f(d, &["*kw0"], &[]), "{}");
        assert_eq!(f(d, &["*.kw0"], &[]), r#"{"o":{"kw0":"x"}}"#);
        assert_eq!(f(d, &["*y0"], &[]), "{}");
        assert_eq!(f(d, &["x*"], &[]), r#"{"x.y0":5}"#);
        assert_eq!(f(d, &["p.*0"], &[]), "{}");
        assert_eq!(f(d, &["p.**"], &[]), r#"{"p":{"q":{"r0":3}}}"#);
        assert_eq!(f(d, &["*.*0"], &[]), r#"{"o":{"kw0":"x"},"n":[{"text0":"t"}],"x.y0":5}"#);
        assert_eq!(f(d, &["o"], &["b"]), r#"{"o":{"kw0":"x","b":2}}"#);
        // Wildcard excludes: map filtering, `*` crosses dots for both, and every filtered
        // object is a new `HashMap` (Java hash order).
        assert_eq!(f(d, &["*"], &["*0"]), r#"{"p":{"q":{}},"n":[{"z":1}],"o":{"b":2}}"#);
        assert_eq!(f(d, &[], &["*.r0"]), r#"{"p":{"q":{}},"x.y0":5,"n":[{"text0":"t","z":1}],"a0":1,"o":{"b":2,"kw0":"x"}}"#);
        assert_eq!(f(d, &["o"], &["*b"]), r#"{"o":{"kw0":"x"}}"#);
        // Objects the map filter keeps whole keep the document's order.
        let d = r#"{"p":{"zz":1,"b":2,"a":3,"c":4},"o":{"zz":1,"b":2,"a":3,"n":[{"zz":1,"y":1,"x":2}]},"zq":1,"aa":2}"#;
        assert_eq!(f(d, &[], &["o.z*"]), r#"{"p":{"zz":1,"b":2,"a":3,"c":4},"aa":2,"zq":1,"o":{"a":3,"b":2,"n":[{"zz":1,"y":1,"x":2}]}}"#);
        assert_eq!(f(d, &[], &["*.zz"]), r#"{"p":{"a":3,"b":2,"c":4},"aa":2,"zq":1,"o":{"a":3,"b":2,"n":[{"x":2,"y":1}]}}"#);
        assert_eq!(f(d, &["o.*", "p"], &["*.zz"]), r#"{"p":{"a":3,"b":2,"c":4},"o":{"a":3,"b":2,"n":[{"x":2,"y":1}]}}"#);
    }

    #[test]
    fn extracts_dotted_and_nested_values() {
        let d = src(r#"{"a":{"b":[1,{"c":2}]},"a.b":3,"x":[{"y":1},{"y":[2,3]}]}"#);
        let mut out = Vec::new();
        extract_values(&d, "a.b", &mut out);
        assert_eq!(out.len(), 3);
        let mut out = Vec::new();
        extract_values(&d, "x.y", &mut out);
        assert_eq!(out.iter().map(|v| v.to_json()).collect::<Vec<_>>(), vec!["1", "2", "3"]);
    }

    fn streamed(filter: &SourceFilter, doc: &str) -> Option<String> {
        let mut out = Vec::new();
        filter.filter_bytes(doc.as_bytes(), &mut out).then(|| String::from_utf8(out).unwrap())
    }

    #[test]
    fn streaming_matches_the_examples() {
        let d = r#"{"a0":1,"o":{"kw0":"x","b":2},"n":[{"text0":"t","z":1}],"p":{"q":{"r0":3}},"x.y0":5}"#;
        for (inc, exc) in [(&["*0"][..], &[][..]), (&["*.*0"], &[]), (&["o"], &["b"]), (&["p.**"], &[]), (&["o"], &["o.b"])] {
            let (i, e): (Vec<String>, Vec<String>) = (inc.iter().map(|s| s.to_string()).collect(), exc.iter().map(|s| s.to_string()).collect());
            assert_eq!(streamed(&SourceFilter::new(&i, &e), d).unwrap(), f(d, inc, exc), "{inc:?} {exc:?}");
        }
        // Whitespace goes, numbers and escaped strings print as the writer prints them.
        let doc = "{ \"a\" : 1.50 , \"b\":\"x\\u00e9\\/\" ,\"c\": [ -0, 1E+2, 12345678901234567890 ], \"d\" : { } }";
        let all = SourceFilter::new(&["*".to_string()], &[]);
        assert_eq!(streamed(&all, doc).unwrap(), r#"{"a":1.5,"b":"xé/","c":[0,100.0,12345678901234567890],"d":{}}"#);
        // Anything but one object of plain JSON is left to the parser.
        for bad in ["[1]", "{\"a\":1} x", "{\"a\":/* c */1}", "{\"a\":01}", "{\"a\":1-2}", "{\"a\":tru}", "{\"a\":\"\u{1}\"}", "{\"a\" 1}", "{\"a\":1"] {
            assert_eq!(streamed(&all, bad), None, "{bad}");
        }
        assert!(!all.filter_bytes(b"{\"a\":\"\xff\"}", &mut Vec::new()));
    }

    /// Random documents and patterns: the streaming filter writes exactly what writing the
    /// filtered tree gives.
    #[test]
    fn streaming_matches_the_tree_on_random_documents() {
        use rand::rngs::StdRng;
        use rand::{Rng, SeedableRng};
        const KEYS: [&str; 7] = ["a", "b", "ab", "a.b", "x\\u0041", "é", "c0"];
        const LEAVES: [&str; 26] = [
            "0", "-0", "12", "-345", "1.50", "1e2", "1E+2", "-0.0", "123456789012345678", "1234567890123456789",
            "12345678901234567890", "-9223372036854775809", "0.1e-400", "1e400", "3.4028235e38", "0.30000000000000004",
            "true", "false", "null", "\"plain\"", "\"\"", "\"a\\nb\"", "\"\\u00e9\\/\"", "\"é\\\"\"", "\"\\ud83d\\ude00\"", "\"\\u001f\"",
        ];
        const PATTERNS: [&str; 16] = ["a", "b", "ab", "a.b", "*", "**", "a*", "*b", "xA", "é", "*.b", "a.*", "c0", "*0", "a.**", "b.a"];
        fn ws(rng: &mut StdRng, out: &mut String) {
            out.push_str(["", "", "", " ", "\n  ", "\t"][rng.gen_range(0..6)]);
        }
        fn value(rng: &mut StdRng, depth: u32, out: &mut String) {
            ws(rng, out);
            match if depth == 0 { 2 } else { rng.gen_range(0..4) } {
                0 => {
                    out.push('{');
                    let mut keys = KEYS.to_vec();
                    let n = rng.gen_range(0..5);
                    for i in 0..n {
                        let k = keys.swap_remove(rng.gen_range(0..keys.len()));
                        if i > 0 {
                            out.push(',');
                        }
                        ws(rng, out);
                        out.push_str(&format!("\"{k}\""));
                        ws(rng, out);
                        out.push(':');
                        value(rng, depth - 1, out);
                    }
                    ws(rng, out);
                    out.push('}');
                }
                1 => {
                    out.push('[');
                    for i in 0..rng.gen_range(0..4) {
                        if i > 0 {
                            out.push(',');
                        }
                        value(rng, depth - 1, out);
                    }
                    ws(rng, out);
                    out.push(']');
                }
                _ => out.push_str(LEAVES[rng.gen_range(0..LEAVES.len())]),
            }
            ws(rng, out);
        }
        let mut rng = StdRng::seed_from_u64(7);
        let mut checked = 0;
        for _ in 0..30_000 {
            let mut doc = String::from("{");
            let mut keys = KEYS.to_vec();
            for i in 0..rng.gen_range(0..6) {
                let k = keys.swap_remove(rng.gen_range(0..keys.len()));
                if i > 0 {
                    doc.push(',');
                }
                doc.push_str(&format!("\"{k}\":"));
                value(&mut rng, 4, &mut doc);
            }
            doc.push('}');
            let pick = |rng: &mut StdRng, n: usize| (0..rng.gen_range(0..=n)).map(|_| PATTERNS[rng.gen_range(0..PATTERNS.len())].to_string()).collect::<Vec<_>>();
            let (inc, exc) = (pick(&mut rng, 3), pick(&mut rng, 2));
            let tree = src(&doc);
            let filter = SourceFilter::new(&inc, &exc);
            let want = Value::Obj(filter.apply(&tree)).to_json();
            let got = streamed(&filter, &doc);
            if exc.iter().any(|e| e.contains('*')) {
                assert_eq!(got, None, "map filters are left to the tree");
                continue;
            }
            assert_eq!(got.as_deref(), Some(want.as_str()), "{doc} {inc:?} {exc:?}");
            checked += usize::from(want.len() > 2);
        }
        assert!(checked > 5_000, "{checked}");
    }
}
