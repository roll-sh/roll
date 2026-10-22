//! `_source` filtering (`XContentMapValues.filter`): `includes` and `excludes` are glob patterns
//! over dotted field paths, each also matching everything below it (`a` covers `a.b.c`); an
//! object is kept when it matched an include or kept a child; excludes win. Array elements are
//! filtered with the array's path. Key order is the document's.
//!
//! Patterns run as a small NFA over the path's characters (`*` matches any run, dots
//! included), stepped key by key, so filtering a document is one walk with no per-key path
//! strings.

use crate::json::{Map, Value};

/// A compiled set of globs, each implicitly followed by an optional `.<anything>`.
#[derive(Clone, Debug)]
pub struct Globs {
    pats: Vec<Vec<u8>>,
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
        Globs { pats: patterns.iter().map(|p| p.as_bytes().to_vec()).collect() }
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
            State::At(v) => v.iter().any(|&(i, pos)| pos as usize == self.pats[i as usize].len()),
        }
    }

    pub fn step_byte(&self, s: &State, c: u8) -> State {
        match s {
            State::Dead => State::Dead,
            State::All => State::All,
            State::At(v) => {
                if c == b'.' && self.accepts(s) {
                    return State::All;
                }
                let mut out = Vec::new();
                for &(i, pos) in v {
                    let p = &self.pats[i as usize];
                    let pos = pos as usize;
                    if pos < p.len() {
                        if p[pos] == b'*' {
                            self.closure(i as usize, pos, p, &mut out);
                        } else if p[pos] == c {
                            self.closure(i as usize, pos + 1, p, &mut out);
                        }
                    }
                }
                if out.is_empty() { State::Dead } else { State::At(out) }
            }
        }
    }

    pub fn step(&self, s: &State, key: &str) -> State {
        let mut cur = s.clone();
        for &c in key.as_bytes() {
            cur = self.step_byte(&cur, c);
            if cur == State::Dead {
                break;
            }
        }
        cur
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
}

impl SourceFilter {
    pub fn new(includes: &[String], excludes: &[String]) -> SourceFilter {
        SourceFilter { includes: (!includes.is_empty()).then(|| Globs::new(includes)), excludes: Globs::new(excludes) }
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
        out
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
}
