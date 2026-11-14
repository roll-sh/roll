//! The `icu_transform` token filter (Elasticsearch's `IcuTransformTokenFilterFactory` over
//! Lucene's `ICUTransformFilter`): ICU transliteration from `ruleset` rules or an `id`.
//!
//! Rules follow ICU's syntax (`TransliteratorParser`): conversion rules `ante { key } post >
//! output ;` in either direction (`>`, `<`, `<>`), variables, quoted and escaped literals,
//! `UnicodeSet`s, anchors, a cursor `|` in the output, `::` stages and a global `:: [filter] ;`.
//! Rules are tried in order at each position and an earlier rule that masks a later one is an
//! error, as in ICU. IDs (also as `::` stages) can be the normalization forms (`NFC`, `NFD`,
//! `NFKC`, `NFKD`, `FCD` excluded), `Lower`, `Upper`, `Null`, `Remove`, with an optional
//! `[filter]`, in compound lists. Not supported: segments, quantifiers and functions in rules,
//! and ICU's data-driven transforms (script conversions such as `Any-Latin`, `Hiragana-Katakana`,
//! `Han-Latin`, `Latin-ASCII`), which need ICU's transliteration rule data.

use std::sync::Arc;

use super::norm::{Data, Normalizer};
use super::uset::USet;
use crate::component::{FilterFactory, normalizing_fn};
use crate::error::AnalysisError;
use crate::registry::{FilterBuilder, Provider};
use crate::stream::{Filter, TokenStream};
use crate::token::Tok;

/// One element of a rule's pattern.
#[derive(Clone)]
enum El {
    Char(char),
    /// A set and its index (masking compares sets by identity, as ICU's stand-ins do).
    Set(Arc<USet>, usize),
}

impl El {
    fn matches(&self, c: char) -> bool {
        match self {
            El::Char(x) => *x == c,
            El::Set(s, _) => s.contains(c as u32),
        }
    }
    fn same(&self, o: &El) -> bool {
        match (self, o) {
            (El::Char(a), El::Char(b)) => a == b,
            (El::Set(_, a), El::Set(_, b)) => a == b,
            _ => false,
        }
    }
}

struct Rule {
    ante: Vec<El>,
    key: Vec<El>,
    post: Vec<El>,
    out: Vec<char>,
    /// Where the cursor goes in the output (`|`), by default after it.
    cursor: usize,
    anchor_start: bool,
    anchor_end: bool,
    source: String,
}

impl Rule {
    fn pattern(&self) -> Vec<&El> {
        self.ante.iter().chain(&self.key).chain(&self.post).collect()
    }

    /// `TransliterationRule.masks`.
    fn masks(&self, r2: &Rule) -> bool {
        let p1 = self.pattern();
        let p2 = r2.pattern();
        let len = p1.len();
        let (left, left2) = (self.ante.len(), r2.ante.len());
        let (right, right2) = (len - left, p2.len() - left2);
        let region = |off: usize| off + len <= p2.len() && (0..len).all(|i| p2[off + i].same(p1[i]));
        let flags = (self.anchor_start, self.anchor_end);
        let flags2 = (r2.anchor_start, r2.anchor_end);
        if left == left2 && right == right2 && self.key.len() <= r2.key.len() && region(0) {
            return flags == flags2 || (!self.anchor_start && !self.anchor_end) || (r2.anchor_start && r2.anchor_end);
        }
        left <= left2 && (right < right2 || (right == right2 && self.key.len() <= r2.key.len())) && region(left2 - left)
    }

    /// Matches at `pos` of `t` (the text being transliterated) within `[0, limit)`.
    fn matches_at(&self, t: &[char], pos: usize, limit: usize) -> bool {
        if self.anchor_start && pos != self.ante.len() {
            return false;
        }
        if pos < self.ante.len() {
            return false;
        }
        let a0 = pos - self.ante.len();
        if !self.ante.iter().enumerate().all(|(i, e)| e.matches(t[a0 + i])) {
            return false;
        }
        let end = pos + self.key.len() + self.post.len();
        if end > limit {
            return false;
        }
        if self.anchor_end && end != limit {
            return false;
        }
        self.key.iter().chain(&self.post).enumerate().all(|(i, e)| e.matches(t[pos + i]))
    }
}

/// A transliteration stage.
enum Stage {
    Rules { rules: Vec<Rule>, filter: Option<Arc<USet>> },
    Norm(Normalizer, Option<Arc<USet>>),
    Lower(Option<Arc<USet>>),
    Upper(Option<Arc<USet>>),
    Remove(Option<Arc<USet>>),
    Null,
}

/// A whole transliterator: its stages and a global filter.
pub(crate) struct Transliterator {
    stages: Vec<Stage>,
    filter: Option<Arc<USet>>,
}

fn by_runs(t: &[char], filter: Option<&Arc<USet>>, f: &mut dyn FnMut(&[char]) -> Vec<char>) -> Vec<char> {
    let Some(set) = filter else { return f(t) };
    let mut out = Vec::with_capacity(t.len());
    let mut i = 0;
    while i < t.len() {
        let inside = set.contains(t[i] as u32);
        let j = (i..t.len()).find(|&k| set.contains(t[k] as u32) != inside).unwrap_or(t.len());
        if inside {
            out.extend(f(&t[i..j]));
        } else {
            out.extend_from_slice(&t[i..j]);
        }
        i = j;
    }
    out
}

impl Stage {
    fn apply(&self, t: &[char]) -> Vec<char> {
        match self {
            Stage::Null => t.to_vec(),
            Stage::Remove(f) => by_runs(t, f.as_ref(), &mut |_| Vec::new()),
            Stage::Lower(f) => by_runs(t, f.as_ref(), &mut |r| r.iter().collect::<String>().to_lowercase().chars().collect()),
            Stage::Upper(f) => by_runs(t, f.as_ref(), &mut |r| r.iter().collect::<String>().to_uppercase().chars().collect()),
            Stage::Norm(n, f) => by_runs(t, f.as_ref(), &mut |r| n.normalize(&r.iter().collect::<String>()).chars().collect()),
            Stage::Rules { rules, filter } => by_runs(t, filter.as_ref(), &mut |r| {
                let mut text = r.to_vec();
                let mut pos = 0;
                while pos < text.len() {
                    let limit = text.len();
                    match rules.iter().find(|rule| rule.matches_at(&text, pos, limit)) {
                        Some(rule) => {
                            text.splice(pos..pos + rule.key.len(), rule.out.iter().copied());
                            pos += rule.cursor;
                            if rule.cursor == 0 && rule.key.is_empty() {
                                pos += 1;
                            }
                        }
                        None => pos += 1,
                    }
                }
                text
            }),
        }
    }
}

impl Transliterator {
    pub(crate) fn transliterate(&self, s: &str) -> String {
        let t: Vec<char> = s.chars().collect();
        by_runs(&t, self.filter.as_ref(), &mut |run| {
            let mut cur = run.to_vec();
            for st in &self.stages {
                cur = st.apply(&cur);
            }
            cur
        })
        .into_iter()
        .collect()
    }

    /// `Transliterator.getInstance(id, dir)`: a compound list of the built-in IDs.
    pub(crate) fn from_id(id: &str, forward: bool) -> std::result::Result<Transliterator, String> {
        let mut stages = Vec::new();
        let mut parts: Vec<&str> = id.split(';').map(str::trim).filter(|p| !p.is_empty()).collect();
        if !forward {
            parts.reverse();
        }
        let mut global = None;
        for (i, part) in parts.iter().enumerate() {
            let (filter, name) = split_filter(part)?;
            if name.is_empty() {
                // A leading `[set]` alone is the global filter.
                if i == 0 && filter.is_some() {
                    global = filter;
                    continue;
                }
                return Err(format!("Illegal ID {id}"));
            }
            stages.push(builtin(name, filter, forward).ok_or_else(|| format!("Illegal ID {id}"))?);
        }
        if stages.is_empty() {
            stages.push(Stage::Null);
        }
        Ok(Transliterator { stages, filter: global })
    }

    /// `Transliterator.createFromRules(name, rules, dir)`.
    pub(crate) fn from_rules(rules: &str, forward: bool) -> std::result::Result<Transliterator, String> {
        Parser::new(forward).parse(rules)
    }
}

/// `[filter] ID`.
fn split_filter(part: &str) -> std::result::Result<(Option<Arc<USet>>, &str), String> {
    let part = part.trim();
    if !part.starts_with('[') {
        return Ok((None, part));
    }
    let end = set_end(part).ok_or_else(|| format!("Illegal ID {part}"))?;
    let set = USet::parse(&part[..end])?;
    Ok((Some(Arc::new(set)), part[end..].trim()))
}

/// The byte length of the bracketed set at the start of `s` (nested brackets, quotes and
/// escapes skipped).
fn set_end(s: &str) -> Option<usize> {
    let mut depth = 0;
    let mut chars = s.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        match c {
            '\\' => {
                chars.next();
            }
            '[' => depth += 1,
            ']' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i + 1);
                }
            }
            _ => {}
        }
    }
    None
}

fn builtin(name: &str, filter: Option<Arc<USet>>, forward: bool) -> Option<Stage> {
    let lower = name.to_ascii_lowercase();
    let base = lower.strip_prefix("any-").unwrap_or(&lower);
    let norm = |data, compose| Stage::Norm(Normalizer { data, compose, filter: None }, filter.clone());
    Some(match (base, forward) {
        ("null", _) => Stage::Null,
        ("remove", true) => Stage::Remove(filter),
        ("remove", false) => Stage::Null,
        ("nfc", true) | ("nfd", false) => norm(Data::Nfc, true),
        ("nfd", true) | ("nfc", false) => norm(Data::Nfc, false),
        ("nfkc", true) | ("nfkd", false) => norm(Data::Nfkc, true),
        ("nfkd", true) | ("nfkc", false) => norm(Data::Nfkc, false),
        ("lower", true) | ("upper", false) => Stage::Lower(filter),
        ("upper", true) | ("lower", false) => Stage::Upper(filter),
        _ => return None,
    })
}

/// `TransliteratorParser`, for the syntax described in the module docs.
struct Parser {
    forward: bool,
    vars: Vec<(String, Vec<El>)>,
    sets: usize,
}

impl Parser {
    fn new(forward: bool) -> Parser {
        Parser { forward, vars: Vec::new(), sets: 0 }
    }

    /// Splits the rules into statements at unquoted `;`.
    fn statements(rules: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut cur = String::new();
        let mut quoted = false;
        let mut depth = 0;
        let mut chars = rules.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '\\' if !quoted => {
                    cur.push(c);
                    if let Some(n) = chars.next() {
                        cur.push(n);
                    }
                }
                '\'' => {
                    quoted = !quoted;
                    cur.push(c);
                }
                '[' if !quoted => {
                    depth += 1;
                    cur.push(c);
                }
                ']' if !quoted => {
                    depth -= 1;
                    cur.push(c);
                }
                '#' if !quoted && depth == 0 => {
                    for n in chars.by_ref() {
                        if n == '\n' || n == '\r' {
                            break;
                        }
                    }
                }
                ';' if !quoted && depth == 0 => {
                    out.push(std::mem::take(&mut cur));
                }
                c => cur.push(c),
            }
        }
        out.push(cur);
        out.into_iter().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()
    }

    fn parse(mut self, rules: &str) -> std::result::Result<Transliterator, String> {
        let mut stages: Vec<Stage> = Vec::new();
        let mut pending: Vec<Rule> = Vec::new();
        let mut global = None;
        let mut first = true;
        for st in Self::statements(rules) {
            if let Some(id) = st.strip_prefix("::") {
                let id = id.trim();
                if !pending.is_empty() {
                    stages.push(Self::rule_stage(std::mem::take(&mut pending))?);
                }
                let (filter, name) = split_filter(id)?;
                if name.is_empty() && first {
                    global = filter;
                } else {
                    // `:: ID (reverse) ;`: the reverse ID for the other direction.
                    let (fwd, rev) = match name.find('(') {
                        Some(i) => (name[..i].trim(), name[i + 1..].trim_end_matches(')').trim()),
                        None => (name, ""),
                    };
                    let chosen = if self.forward || rev.is_empty() { fwd } else { rev };
                    if !chosen.is_empty() {
                        stages.push(builtin(chosen, filter, self.forward || !rev.is_empty()).ok_or_else(|| format!("Illegal ID {chosen}"))?);
                    }
                }
                first = false;
                continue;
            }
            first = false;
            if let Some(rest) = st.strip_prefix('$')
                && let Some(eq) = rest.find('=')
                && !rest[..eq].contains(['>', '<', '{', '}'])
                && rest[..eq].trim().chars().all(|c| c.is_alphanumeric() || c == '_')
            {
                let name = rest[..eq].trim().to_string();
                let value = self.elements(rest[eq + 1..].trim())?;
                self.vars.push((name, value.0));
                continue;
            }
            if let Some(rule) = self.rule(&st)? {
                pending.push(rule);
            }
        }
        if !pending.is_empty() {
            stages.push(Self::rule_stage(pending)?);
        }
        if !self.forward {
            stages.reverse();
        }
        if stages.is_empty() {
            stages.push(Stage::Null);
        }
        Ok(Transliterator { stages, filter: global })
    }

    fn rule_stage(rules: Vec<Rule>) -> std::result::Result<Stage, String> {
        for i in 0..rules.len() {
            for j in i + 1..rules.len() {
                if rules[i].masks(&rules[j]) {
                    return Err(format!("Rule {{{}}} masks {{{}}}", rules[i].source, rules[j].source));
                }
            }
        }
        Ok(Stage::Rules { rules, filter: None })
    }

    /// One conversion rule, or None when it belongs to the other direction.
    fn rule(&mut self, st: &str) -> std::result::Result<Option<Rule>, String> {
        let (op_at, op_len, op) = Self::find_op(st).ok_or_else(|| format!("No operator pos=0 in {st}"))?;
        let (lhs, rhs) = (st[..op_at].trim(), st[op_at + op_len..].trim());
        let (pat, out) = match (op, self.forward) {
            ('>', true) | ('=', true) => (lhs, rhs),
            ('<', false) | ('=', false) => (rhs, lhs),
            _ => return Ok(None),
        };
        let source = format!("{lhs} {} {rhs};", match op {
            '>' => ">",
            '<' => "<",
            _ => "<>",
        });
        let (ante, key, post, anchor_start, anchor_end) = self.pattern(pat)?;
        let (out_els, cursor) = self.elements(out)?;
        let mut out_chars = Vec::new();
        for e in out_els {
            match e {
                El::Char(c) => out_chars.push(c),
                El::Set(..) => return Err(format!("Illegal character in replacement: {st}")),
            }
        }
        let cursor = cursor.unwrap_or(out_chars.len());
        Ok(Some(Rule { ante, key, post, cursor, out: out_chars, anchor_start, anchor_end, source }))
    }

    /// The rule operator outside quotes and sets: its byte position, length and kind.
    fn find_op(st: &str) -> Option<(usize, usize, char)> {
        let mut quoted = false;
        let mut depth = 0;
        let b: Vec<(usize, char)> = st.char_indices().collect();
        let mut k = 0;
        while k < b.len() {
            let (i, c) = b[k];
            match c {
                '\\' if !quoted => k += 1,
                '\'' => quoted = !quoted,
                '[' if !quoted => depth += 1,
                ']' if !quoted => depth -= 1,
                '<' | '←' if !quoted && depth == 0 => {
                    if let Some(&(_, n)) = b.get(k + 1)
                        && n == '>'
                    {
                        return Some((i, 2, '='));
                    }
                    return Some((i, c.len_utf8(), '<'));
                }
                '↔' if !quoted && depth == 0 => return Some((i, c.len_utf8(), '=')),
                '>' | '→' if !quoted && depth == 0 => return Some((i, c.len_utf8(), '>')),
                _ => {}
            }
            k += 1;
        }
        None
    }

    #[allow(clippy::type_complexity)]
    fn pattern(&mut self, p: &str) -> std::result::Result<(Vec<El>, Vec<El>, Vec<El>, bool, bool), String> {
        let mut p = p.trim();
        let mut anchor_start = false;
        let mut anchor_end = false;
        if let Some(r) = p.strip_prefix('^') {
            anchor_start = true;
            p = r;
        }
        if p.ends_with('$') && !p.ends_with("\\$") {
            let body = &p[..p.len() - 1];
            // `$` ends the pattern unless it starts a variable name.
            if !body.ends_with('$') {
                anchor_end = true;
                p = body;
            }
        }
        let (ante_s, rest) = match Self::split_unquoted(p, '{') {
            Some((a, r)) => (a, r),
            None => ("", p),
        };
        let (key_s, post_s) = match Self::split_unquoted(rest, '}') {
            Some((k, q)) => (k, q),
            None => (rest, ""),
        };
        let ante = self.elements(ante_s)?.0;
        let key = self.elements(key_s)?.0;
        let post = self.elements(post_s)?.0;
        Ok((ante, key, post, anchor_start, anchor_end))
    }

    fn split_unquoted(s: &str, sep: char) -> Option<(&str, &str)> {
        let mut quoted = false;
        let mut depth = 0;
        let mut esc = false;
        for (i, c) in s.char_indices() {
            if esc {
                esc = false;
                continue;
            }
            match c {
                '\\' if !quoted => esc = true,
                '\'' => quoted = !quoted,
                '[' if !quoted => depth += 1,
                ']' if !quoted => depth -= 1,
                c if c == sep && !quoted && depth == 0 => return Some((&s[..i], &s[i + c.len_utf8()..])),
                _ => {}
            }
        }
        None
    }

    /// Literals, sets and variables; the position of a cursor `|`.
    fn elements(&mut self, s: &str) -> std::result::Result<(Vec<El>, Option<usize>), String> {
        let mut out = Vec::new();
        let mut cursor = None;
        let chars: Vec<(usize, char)> = s.char_indices().collect();
        let mut k = 0;
        while k < chars.len() {
            let (i, c) = chars[k];
            match c {
                c if c.is_whitespace() => {}
                '\'' => {
                    k += 1;
                    if k < chars.len() && chars[k].1 == '\'' {
                        out.push(El::Char('\''));
                    } else {
                        while k < chars.len() {
                            if chars[k].1 == '\'' {
                                if k + 1 < chars.len() && chars[k + 1].1 == '\'' {
                                    out.push(El::Char('\''));
                                    k += 2;
                                    continue;
                                }
                                break;
                            }
                            out.push(El::Char(chars[k].1));
                            k += 1;
                        }
                    }
                }
                '\\' => {
                    let (ch, used) = Self::escape(&s[i..])?;
                    out.push(El::Char(ch));
                    let end = i + used;
                    while k + 1 < chars.len() && chars[k + 1].0 < end {
                        k += 1;
                    }
                }
                '[' => {
                    let end = set_end(&s[i..]).ok_or_else(|| format!("Unmatched [ in {s}"))?;
                    let src = self.substitute_vars(&s[i..i + end]);
                    let set = USet::parse(&src)?;
                    out.push(El::Set(Arc::new(set), self.sets));
                    self.sets += 1;
                    while k + 1 < chars.len() && chars[k + 1].0 < i + end {
                        k += 1;
                    }
                }
                '$' => {
                    let name: String = chars[k + 1..].iter().map(|&(_, c)| c).take_while(|c| c.is_alphanumeric() || *c == '_').collect();
                    if name.is_empty() {
                        return Err(format!("Undefined variable in {s}"));
                    }
                    let v = self.vars.iter().rev().find(|(n, _)| *n == name).ok_or_else(|| format!("Undefined variable ${name}"))?;
                    out.extend(v.1.iter().cloned());
                    k += name.chars().count();
                }
                '|' => cursor = Some(out.len()),
                '(' | ')' | '*' | '+' | '?' | '&' | '@' => {
                    return Err(format!("Unsupported syntax {c} in {s}"));
                }
                c => out.push(El::Char(c)),
            }
            k += 1;
        }
        Ok((out, cursor))
    }

    /// `\uXXXX`, `\UXXXXXXXX`, `\x{…}`, `\xXX` or an escaped character; and the bytes used.
    fn escape(s: &str) -> std::result::Result<(char, usize), String> {
        let b = s.as_bytes();
        let hex = |from: usize, n: usize| -> Option<(char, usize)> {
            let h = s.get(from..from + n)?;
            char::from_u32(u32::from_str_radix(h, 16).ok()?).map(|c| (c, from + n))
        };
        match b.get(1) {
            Some(b'u') => hex(2, 4).ok_or_else(|| format!("Invalid escape {s}")),
            Some(b'U') => hex(2, 8).ok_or_else(|| format!("Invalid escape {s}")),
            Some(b'x') if b.get(2) == Some(&b'{') => {
                let close = s.find('}').ok_or_else(|| format!("Invalid escape {s}"))?;
                char::from_u32(u32::from_str_radix(&s[3..close], 16).map_err(|e| e.to_string())?)
                    .map(|c| (c, close + 1))
                    .ok_or_else(|| format!("Invalid escape {s}"))
            }
            Some(b'x') => hex(2, 2).ok_or_else(|| format!("Invalid escape {s}")),
            Some(_) => {
                let c = s[1..].chars().next().unwrap_or('\\');
                let c = match c {
                    't' => '\t',
                    'n' => '\n',
                    'r' => '\r',
                    c => c,
                };
                Ok((c, 1 + s[1..].chars().next().map_or(0, char::len_utf8)))
            }
            None => Ok(('\\', 1)),
        }
    }

    /// Variables inside a set pattern are replaced by their text (single characters).
    fn substitute_vars(&self, set: &str) -> String {
        let mut out = String::new();
        let mut chars = set.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '$' && chars.peek().is_some_and(|n| n.is_alphanumeric() || *n == '_') {
                let mut name = String::new();
                while let Some(&n) = chars.peek() {
                    if n.is_alphanumeric() || n == '_' {
                        name.push(n);
                        chars.next();
                    } else {
                        break;
                    }
                }
                match self.vars.iter().rev().find(|(n, _)| *n == name) {
                    Some((_, v)) => {
                        for e in v {
                            if let El::Char(c) = e {
                                out.push_str(&format!("\\U{:08x}", *c as u32));
                            }
                        }
                    }
                    None => {
                        out.push('$');
                        out.push_str(&name);
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    }
}

/// `ICUTransformFilter`.
struct TransformFilter(Arc<Transliterator>);

impl Filter for TransformFilter {
    fn next(&mut self, input: &mut dyn TokenStream, t: &mut Tok) -> bool {
        if !input.next(t) {
            return false;
        }
        t.term = self.0.transliterate(&t.term);
        true
    }
}

pub(crate) fn provider() -> Option<Provider<FilterBuilder>> {
    let build: FilterBuilder = |_, name, s| {
        let id = s.get("id");
        let ruleset = s.get("ruleset");
        let forward = s.get("dir").is_none_or(|d| d == "forward");
        if id.is_some() && ruleset.is_some() {
            return Err(AnalysisError::illegal(format!("icu_transform filter [{name}] must not specify both [id] and [ruleset]")));
        }
        let t = match ruleset {
            Some(r) => Transliterator::from_rules(&r, forward).map_err(|e| {
                AnalysisError::illegal(format!("icu_transform filter [{name}] has invalid ruleset: {e}")).with_cause(AnalysisError::illegal(e))
            })?,
            None => Transliterator::from_id(id.as_deref().unwrap_or("Null"), forward).map_err(AnalysisError::illegal)?,
        };
        let t = Arc::new(t);
        Ok(normalizing_fn(move || Box::new(TransformFilter(t.clone()))) as Arc<dyn FilterFactory>)
    };
    Some(Provider { build, requires_settings: false })
}
