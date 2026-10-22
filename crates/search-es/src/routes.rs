//! The route tables: every API of the vendored REST specs (Elasticsearch's, and OpenSearch's for
//! the opensearch dialect), matched the way Elasticsearch's `PathTrie` matches.
//!
//! `build.rs` turns the specs into static `ApiSpec`s (one array per dialect) and the [`Api`]
//! enum covering both. On first use for a dialect
//! the paths are inserted into a trie of `/`-separated segments where `{name}` segments become
//! a single wildcard child per node. Lookup reproduces Elasticsearch's four matching modes, tried
//! in order — explicit nodes only, wildcard allowed at the root, at the leaf, anywhere — so
//! `/_cat/indices` beats `/{index}/{…}` and `/my-index/_settings` still finds
//! `/{index}/_settings`. A path that matches some route but not with the request's method
//! answers 405 with the allowed methods, like Elasticsearch.
//!
//! Matching allocates only the captured path parameters; children are sorted slices searched
//! by binary search.

use crate::cluster::Dialect;
use std::sync::OnceLock;

include!(concat!(env!("OUT_DIR"), "/spec.rs"));

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Method {
    Get,
    Post,
    Put,
    Delete,
    Head,
    Options,
    Patch,
    Trace,
    Connect,
}

impl Method {
    pub const ALL: [Method; 9] =
        [Method::Get, Method::Post, Method::Put, Method::Delete, Method::Head, Method::Options, Method::Patch, Method::Trace, Method::Connect];

    pub const fn bit(self) -> u16 {
        1 << self as u16
    }

    pub fn parse(s: &str) -> Option<Method> {
        Some(match s {
            "GET" => Method::Get,
            "POST" => Method::Post,
            "PUT" => Method::Put,
            "DELETE" => Method::Delete,
            "HEAD" => Method::Head,
            "OPTIONS" => Method::Options,
            "PATCH" => Method::Patch,
            "TRACE" => Method::Trace,
            "CONNECT" => Method::Connect,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Method::Get => "GET",
            Method::Post => "POST",
            Method::Put => "PUT",
            Method::Delete => "DELETE",
            Method::Head => "HEAD",
            Method::Options => "OPTIONS",
            Method::Patch => "PATCH",
            Method::Trace => "TRACE",
            Method::Connect => "CONNECT",
        }
    }
}

impl std::fmt::Display for Method {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParamType {
    Boolean,
    Time,
    List,
    String,
    Enum,
    Int,
    Long,
    Date,
    Number,
    Double,
    NumberOrString,
    BooleanOrLong,
}

#[derive(Debug)]
pub struct ParamSpec {
    pub name: &'static str,
    pub ty: ParamType,
    pub options: &'static [&'static str],
    pub default: Option<&'static str>,
    pub deprecated: bool,
}

#[derive(Debug)]
pub struct PartSpec {
    pub name: &'static str,
    pub ty: ParamType,
    pub options: &'static [&'static str],
}

#[derive(Debug)]
pub struct PathSpec {
    pub path: &'static str,
    pub methods: u16,
    pub deprecated: bool,
    pub parts: &'static [PartSpec],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodySpec {
    None,
    Optional,
    Required,
}

#[derive(Debug)]
pub struct ApiSpec {
    pub name: &'static str,
    /// Whether the dialect has this API at all.
    pub present: bool,
    pub stability: &'static str,
    pub visibility: &'static str,
    pub body: BodySpec,
    pub accept: &'static [&'static str],
    pub content_type: &'static [&'static str],
    pub paths: &'static [PathSpec],
    /// Sorted by name.
    pub params: &'static [ParamSpec],
}

fn specs(d: Dialect) -> &'static [ApiSpec; API_COUNT] {
    match d {
        Dialect::Elasticsearch => &APIS,
        Dialect::OpenSearch => &OS_APIS,
    }
}

impl Api {
    /// The Elasticsearch spec of this API (OpenSearch's for an API only OpenSearch has).
    pub fn spec(self) -> &'static ApiSpec {
        let s = &APIS[self as usize];
        if s.present { s } else { &OS_APIS[self as usize] }
    }

    /// This API's spec in a dialect (`present: false` when the dialect lacks it).
    pub fn spec_in(self, d: Dialect) -> &'static ApiSpec {
        &specs(d)[self as usize]
    }

    pub fn name(self) -> &'static str {
        API_NAMES[self as usize]
    }

    pub fn by_name(name: &str) -> Option<Api> {
        API_NAMES.binary_search(&name).ok().map(|i| ALL_APIS[i])
    }

    /// The spec of query parameter `name`, if this API declares it.
    pub fn param(self, name: &str) -> Option<&'static ParamSpec> {
        let ps = self.spec().params;
        ps.binary_search_by(|p| p.name.cmp(name)).ok().map(|i| &ps[i])
    }

    /// The spec of query parameter `name` in a dialect's spec of this API.
    pub fn param_in(self, d: Dialect, name: &str) -> Option<&'static ParamSpec> {
        let ps = self.spec_in(d).params;
        ps.binary_search_by(|p| p.name.cmp(name)).ok().map(|i| &ps[i])
    }
}

/// One matched route.
#[derive(Debug, Clone, PartialEq)]
pub struct RouteMatch {
    pub api: Api,
    /// The path pattern as written in the spec, e.g. `/{index}/_mapping`.
    pub pattern: &'static str,
    /// Decoded path parameters in path order.
    pub params: Vec<(&'static str, String)>,
}

impl RouteMatch {
    pub fn param(&self, name: &str) -> Option<&str> {
        self.params.iter().find(|(n, _)| *n == name).map(|(_, v)| v.as_str())
    }
}

/// The outcome of routing a request.
#[derive(Debug, PartialEq)]
pub enum Routed {
    Found(RouteMatch),
    /// The path exists with other methods (405, or 200 for `OPTIONS`).
    MethodNotAllowed(Vec<Method>),
    /// No route has this path (400 "no handler found").
    NotFound,
}

#[derive(Clone, Copy)]
struct Handler {
    api: Api,
    pattern: &'static str,
}

#[derive(Default)]
struct Node {
    /// Sorted by key.
    children: Vec<(&'static str, u32)>,
    wildcard: Option<u32>,
    /// One handler per method.
    value: Option<Box<[Option<Handler>; 9]>>,
}

struct Trie {
    nodes: Vec<Node>,
    root_value: Option<Box<[Option<Handler>; 9]>>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    ExplicitOnly,
    WildcardRoot,
    WildcardLeaf,
    WildcardAny,
}

const MODES: [Mode; 4] = [Mode::ExplicitOnly, Mode::WildcardRoot, Mode::WildcardLeaf, Mode::WildcardAny];

fn is_wildcard(seg: &str) -> bool {
    seg.len() >= 2 && seg.starts_with('{') && seg.ends_with('}')
}

/// Java's `String.split("/")`: trailing empty strings removed.
fn split_path(path: &str) -> Vec<&str> {
    let mut v: Vec<&str> = path.split('/').collect();
    while v.last().is_some_and(|s| s.is_empty()) {
        v.pop();
    }
    v
}

impl Trie {
    fn build(d: Dialect) -> Trie {
        let mut t = Trie { nodes: vec![Node::default()], root_value: None };
        for api in ALL_APIS {
            for p in api.spec_in(d).paths {
                for m in Method::ALL {
                    if p.methods & m.bit() != 0 {
                        t.insert(p.path, m, Handler { api, pattern: p.path });
                    }
                }
            }
        }
        // Children were pushed unsorted; sort once.
        for n in &mut t.nodes {
            n.children.sort_by(|a, b| a.0.cmp(b.0));
        }
        t
    }

    fn insert(&mut self, path: &'static str, m: Method, h: Handler) {
        let segs = split_path(path);
        let segs: &[&'static str] = if segs.first() == Some(&"") { &segs[1..] } else { &segs };
        if segs.is_empty() {
            let v = self.root_value.get_or_insert_with(|| Box::new([None; 9]));
            v[m as usize].get_or_insert(h);
            return;
        }
        let mut cur = 0usize;
        for seg in segs {
            let next = if is_wildcard(seg) {
                match self.nodes[cur].wildcard {
                    Some(n) => n as usize,
                    None => {
                        self.nodes.push(Node::default());
                        let n = self.nodes.len() - 1;
                        self.nodes[cur].wildcard = Some(n as u32);
                        n
                    }
                }
            } else {
                match self.nodes[cur].children.iter().find(|(k, _)| k == seg) {
                    Some((_, n)) => *n as usize,
                    None => {
                        self.nodes.push(Node::default());
                        let n = self.nodes.len() - 1;
                        self.nodes[cur].children.push((seg, n as u32));
                        n
                    }
                }
            };
            cur = next;
        }
        let v = self.nodes[cur].value.get_or_insert_with(|| Box::new([None; 9]));
        // The first API declaring a path+method wins (the spec has no real duplicates).
        v[m as usize].get_or_insert(h);
    }

    fn child(&self, node: usize, token: &str) -> Option<usize> {
        if token == "*" {
            return self.nodes[node].wildcard.map(|n| n as usize);
        }
        let c = &self.nodes[node].children;
        c.binary_search_by(|(k, _)| (*k).cmp(token)).ok().map(|i| c[i].1 as usize)
    }

    /// Port of `PathTrie.TrieNode.retrieve`. `index` counts from 1 like Elasticsearch's (the
    /// leading empty segment is index 0).
    fn retrieve(&self, node: usize, path: &[&str], index: usize, mode: Mode) -> Option<usize> {
        if index >= path.len() {
            return None;
        }
        let token = path[index];
        let mut used_wildcard;
        let mut next = match self.child(node, token) {
            Some(n) => {
                used_wildcard = token == "*";
                n
            }
            None => {
                let allowed = match mode {
                    Mode::WildcardAny => true,
                    Mode::WildcardRoot => index == 1,
                    Mode::WildcardLeaf => index + 1 == path.len(),
                    Mode::ExplicitOnly => false,
                };
                if !allowed {
                    return None;
                }
                used_wildcard = true;
                self.nodes[node].wildcard? as usize
            }
        };
        if !used_wildcard {
            let wc = self.nodes[node].wildcard;
            if index + 1 == path.len() && self.nodes[next].value.is_none() && !matches!(mode, Mode::ExplicitOnly | Mode::WildcardRoot) {
                if let Some(w) = wc {
                    next = w as usize;
                    used_wildcard = true;
                }
            } else if index == 1 && self.nodes[next].value.is_none() && mode == Mode::WildcardRoot
                && let Some(w) = wc
            {
                next = w as usize;
                used_wildcard = true;
            }
        }
        if index == path.len() - 1 {
            return self.nodes[next].value.as_ref().map(|_| next);
        }
        let found = self.retrieve(next, path, index + 1, mode);
        if found.is_none() && !used_wildcard && mode != Mode::ExplicitOnly
            && let Some(w) = self.nodes[node].wildcard
        {
            return self.retrieve(w as usize, path, index + 1, mode);
        }
        found
    }

    fn handlers_at(&self, leaf: Option<usize>) -> Option<&[Option<Handler>; 9]> {
        match leaf {
            None => self.root_value.as_deref(),
            Some(n) => self.nodes[n].value.as_deref(),
        }
    }

    /// All matches for `raw_path`, one per mode (with duplicates), as leaves (`None` = root).
    fn all(&self, segs: &[&str]) -> Vec<Option<usize>> {
        if segs.is_empty() || (segs.len() == 1 && segs[0].is_empty()) {
            return if self.root_value.is_some() { vec![None] } else { vec![] };
        }
        let start = if segs[0].is_empty() { 1 } else { 0 };
        if start == 1 && segs.len() == 1 {
            return if self.root_value.is_some() { vec![None] } else { vec![] };
        }
        let mut out = Vec::with_capacity(4);
        for mode in MODES {
            // Elasticsearch counts the first real segment as index 1.
            let found = if start == 1 {
                self.retrieve(0, segs, 1, mode)
            } else {
                let mut shifted = Vec::with_capacity(segs.len() + 1);
                shifted.push("");
                shifted.extend_from_slice(segs);
                self.retrieve(0, &shifted, 1, mode)
            };
            if let Some(f) = found {
                out.push(Some(f));
            }
        }
        out
    }
}

fn trie(d: Dialect) -> &'static Trie {
    static ES: OnceLock<Trie> = OnceLock::new();
    static OS: OnceLock<Trie> = OnceLock::new();
    match d {
        Dialect::Elasticsearch => ES.get_or_init(|| Trie::build(Dialect::Elasticsearch)),
        Dialect::OpenSearch => OS.get_or_init(|| Trie::build(Dialect::OpenSearch)),
    }
}

/// Routes `raw_path` (not percent-decoded, without the query string) with Elasticsearch's routes.
pub fn route(method: Method, raw_path: &str) -> Routed {
    route_in(Dialect::Elasticsearch, method, raw_path)
}

/// Routes `raw_path` with a dialect's routes.
pub fn route_in(d: Dialect, method: Method, raw_path: &str) -> Routed {
    let t = trie(d);
    let segs = split_path(raw_path);
    let leaves = t.all(&segs);
    let mut valid = 0u16;
    for leaf in &leaves {
        if let Some(hs) = t.handlers_at(*leaf) {
            for m in Method::ALL {
                if hs[m as usize].is_some() {
                    valid |= m.bit();
                }
            }
        }
    }
    for leaf in &leaves {
        let Some(hs) = t.handlers_at(*leaf) else { continue };
        if let Some(h) = hs[method as usize] {
            return Routed::Found(RouteMatch { api: h.api, pattern: h.pattern, params: extract(h.pattern, &segs) });
        }
        if valid & method.bit() == 0 {
            break;
        }
    }
    if valid != 0 && valid & method.bit() == 0 {
        return Routed::MethodNotAllowed(Method::ALL.into_iter().filter(|m| valid & m.bit() != 0).collect());
    }
    Routed::NotFound
}

/// The methods any route of the dialect accepts for `raw_path` (for `OPTIONS` and 405 `Allow`).
pub fn allowed_methods(d: Dialect, raw_path: &str) -> Vec<Method> {
    let t = trie(d);
    let segs = split_path(raw_path);
    let mut valid = 0u16;
    for leaf in t.all(&segs) {
        if let Some(hs) = t.handlers_at(leaf) {
            for m in Method::ALL {
                if hs[m as usize].is_some() {
                    valid |= m.bit();
                }
            }
        }
    }
    Method::ALL.into_iter().filter(|m| valid & m.bit() != 0).collect()
}

fn extract(pattern: &'static str, segs: &[&str]) -> Vec<(&'static str, String)> {
    let pat = split_path(pattern);
    let pat: Vec<&'static str> = if pat.first() == Some(&"") { pat[1..].to_vec() } else { pat };
    let segs: &[&str] = if segs.first() == Some(&"") { &segs[1..] } else { segs };
    let mut out = Vec::new();
    for (p, s) in pat.iter().zip(segs) {
        if is_wildcard(p) {
            out.push((&p[1..p.len() - 1], percent_decode(s, false)));
        }
    }
    out
}

/// `RestUtils.decodeComponent`: percent-decoding (and `+` as space for query strings), with
/// Elasticsearch's errors for malformed escapes (`%%` is a literal `%`).
pub fn decode_component(s: &str, plus_as_space: bool) -> Result<String, String> {
    if !s.bytes().any(|b| b == b'%' || (plus_as_space && b == b'+')) {
        return Ok(s.to_string());
    }
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let nibble = |c: u8| (c as char).to_digit(16).map(|d| d as u8);
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' => {
                if i + 1 == b.len() {
                    return Err(format!("unterminated escape sequence at end of string: {s}"));
                }
                if b[i + 1] == b'%' {
                    out.push(b'%');
                    i += 2;
                    continue;
                }
                if i + 2 == b.len() {
                    return Err(format!("partial escape sequence at end of string: {s}"));
                }
                match (nibble(b[i + 1]), nibble(b[i + 2])) {
                    (Some(h), Some(l)) => out.push(h * 16 + l),
                    _ => {
                        // Java indexes chars; the escape itself is ASCII or reported as found.
                        let at = s[..i].encode_utf16().count();
                        let shown: String = s[i + 1..].chars().take(2).collect();
                        return Err(format!("invalid escape sequence `%{shown}' at index {at} of: {s}"));
                    }
                }
                i += 3;
            }
            b'+' if plus_as_space => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    Ok(String::from_utf8_lossy(&out).into_owned())
}

/// [`decode_component`], keeping malformed escapes as they are (for text that was already
/// validated, or that only feeds messages).
pub fn percent_decode(s: &str, plus_as_space: bool) -> String {
    if let Ok(d) = decode_component(s, plus_as_space) {
        return d;
    }
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => {
                let hex = |c: u8| (c as char).to_digit(16);
                match (hex(b[i + 1]), hex(b[i + 2])) {
                    (Some(h), Some(l)) => {
                        out.push((h * 16 + l) as u8);
                        i += 3;
                    }
                    _ => {
                        out.push(b'%');
                        i += 1;
                    }
                }
            }
            b'+' if plus_as_space => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn found(m: Method, p: &str) -> (Api, Vec<(&'static str, String)>) {
        match route(m, p) {
            Routed::Found(r) => (r.api, r.params),
            other => panic!("{p}: {other:?}"),
        }
    }

    #[test]
    fn spec_loaded() {
        const { assert!(API_COUNT > 500) };
        assert_eq!(Api::by_name("indices.create"), Some(Api::IndicesCreate));
        assert_eq!(Api::IndicesCreate.param("timeout").unwrap().ty, ParamType::Time);
        assert_eq!(Api::Info.spec().paths[0].path, "/");
    }

    #[test]
    fn explicit_beats_wildcard() {
        assert_eq!(found(Method::Get, "/_cat/indices").0, Api::CatIndices);
        assert_eq!(found(Method::Get, "/_cluster/health").0, Api::ClusterHealth);
        let (api, params) = found(Method::Get, "/my-index/_settings");
        assert_eq!(api, Api::IndicesGetSettings);
        assert_eq!(params, vec![("index", "my-index".to_string())]);
        assert_eq!(found(Method::Get, "/_cat/indices/a,b").1, vec![("index", "a,b".to_string())]);
    }

    #[test]
    fn index_routes() {
        assert_eq!(found(Method::Put, "/idx").0, Api::IndicesCreate);
        assert_eq!(found(Method::Get, "/idx").0, Api::IndicesGet);
        assert_eq!(found(Method::Head, "/idx").0, Api::IndicesExists);
        assert_eq!(found(Method::Delete, "/idx").0, Api::IndicesDelete);
        assert_eq!(found(Method::Get, "/idx/").0, Api::IndicesGet);
        assert_eq!(found(Method::Get, "/").0, Api::Info);
        assert_eq!(found(Method::Head, "/").0, Api::Ping);
        let (api, p) = found(Method::Get, "/idx/_doc/my%2Fid");
        assert_eq!(api, Api::Get);
        assert_eq!(p, vec![("index", "idx".to_string()), ("id", "my/id".to_string())]);
        assert_eq!(found(Method::Get, "/*/_search").0, Api::Search);
        assert_eq!(found(Method::Post, "/_search").0, Api::Search);
    }

    #[test]
    fn method_not_allowed_and_not_found() {
        assert_eq!(route(Method::Delete, "/_cluster/health"), Routed::MethodNotAllowed(vec![Method::Get]));
        assert_eq!(route(Method::Head, "/_cluster/health"), Routed::MethodNotAllowed(vec![Method::Get]));
        assert_eq!(route(Method::Get, "/_nope/x/y"), Routed::NotFound);
    }

    #[test]
    fn decode() {
        assert_eq!(percent_decode("a%2Cb", false), "a,b");
        assert_eq!(percent_decode("a+b", true), "a b");
        assert_eq!(percent_decode("a+b", false), "a+b");
        assert_eq!(percent_decode("%", false), "%");
        assert_eq!(percent_decode("%zz", false), "%zz");
        assert_eq!(percent_decode("%C3%A9", false), "é");
        assert_eq!(percent_decode("a%%b", false), "a%b");
        assert_eq!(decode_component("/idx-%", false).unwrap_err(), "unterminated escape sequence at end of string: /idx-%");
        assert_eq!(decode_component("%a", true).unwrap_err(), "partial escape sequence at end of string: %a");
        assert_eq!(
            decode_component("/idx-%zz/_settings", false).unwrap_err(),
            "invalid escape sequence `%zz' at index 5 of: /idx-%zz/_settings"
        );
    }
}
