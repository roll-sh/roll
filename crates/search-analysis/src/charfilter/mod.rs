//! Char filters: `html_strip`, `mapping` and `pattern_replace`.
//!
//! Each rewrites the whole text before tokenization and records offset corrections in Lucene's
//! `BaseCharFilter` form ([`OffsetMap`]): at output offset `o` and beyond, add the cumulative diff
//! to get the input offset. The corrections are computed exactly as Lucene computes them (they
//! decide every token offset `_analyze` and highlighting report), in UTF-16 units.

pub(crate) mod html_entities;
pub(crate) mod html_strip;

use std::sync::Arc;

use crate::component::CharFilterFactory;
use crate::error::{AnalysisError, Result};
use crate::jregex::{self, Captures, Finder, JavaRegex, Replacement};
use crate::registry::{CharFilterBuilder, Ctx, Provider, word_list};
use crate::stream::{CharFilter, OffsetMap};
use crate::unicode::utf16_len;

// ---------------------------------------------------------------------------------------------
// mapping

/// Lucene's `NormalizeCharMap`: a trie of the rules' left-hand sides. Matching is greedy (the
/// longest rule matching at a position wins).
#[derive(Debug)]
pub(crate) struct CharMap {
    nodes: Vec<TrieNode>,
    /// Root transitions for ASCII (node index + 1, 0 when absent).
    ascii: [u32; 128],
}

#[derive(Debug, Default)]
struct TrieNode {
    /// The replacement and its UTF-16 length when a rule ends here.
    out: Option<(Box<str>, u32)>,
    /// Sorted transitions.
    next: Vec<(char, u32)>,
}

impl CharMap {
    /// `NormalizeCharMap.Builder.add` for every rule, with its errors.
    pub fn build(rules: &[(String, String)]) -> Result<CharMap> {
        let mut m = CharMap {
            nodes: vec![TrieNode::default()],
            ascii: [0; 128],
        };
        let mut seen = std::collections::HashSet::new();
        for (from, to) in rules {
            if from.is_empty() {
                return Err(AnalysisError::illegal("cannot match the empty string"));
            }
            if !seen.insert(from.as_str()) {
                return Err(AnalysisError::illegal(format!(
                    "match \"{from}\" was already added"
                )));
            }
            let mut n = 0usize;
            for c in from.chars() {
                n = match m.nodes[n].next.binary_search_by_key(&c, |e| e.0) {
                    Ok(i) => m.nodes[n].next[i].1 as usize,
                    Err(i) => {
                        let id = m.nodes.len() as u32;
                        m.nodes.push(TrieNode::default());
                        m.nodes[n].next.insert(i, (c, id));
                        id as usize
                    }
                };
            }
            m.nodes[n].out = Some((to.clone().into_boxed_str(), utf16_len(to) as u32));
        }
        for &(c, id) in &m.nodes[0].next {
            if c.is_ascii() {
                m.ascii[c as usize] = id + 1;
            }
        }
        Ok(m)
    }

    #[inline]
    fn step(&self, node: usize, c: char) -> Option<usize> {
        if node == 0 && c.is_ascii() {
            let id = self.ascii[c as usize];
            return (id != 0).then(|| id as usize - 1);
        }
        let next = &self.nodes[node].next;
        next.binary_search_by_key(&c, |e| e.0)
            .ok()
            .map(|i| next[i].1 as usize)
    }

    /// The longest rule matching at the start of `s`: (bytes, UTF-16 units, replacement).
    #[inline]
    fn longest(&self, s: &str) -> Option<(usize, u32, &str, u32)> {
        let mut node = 0;
        let mut best = None;
        let mut units = 0u32;
        for (i, c) in s.char_indices() {
            let Some(n) = self.step(node, c) else { break };
            node = n;
            units += c.len_utf16() as u32;
            if let Some((out, len)) = &self.nodes[node].out {
                best = Some((i + c.len_utf8(), units, &**out, *len));
            }
            if self.nodes[node].next.is_empty() {
                break;
            }
        }
        best
    }
}

/// `MappingCharFilter`.
pub(crate) struct Mapping {
    map: Arc<CharMap>,
}

impl CharFilter for Mapping {
    fn filter(&mut self, input: &str, out: &mut String, map: &mut OffsetMap) -> Result<()> {
        let m = &*self.map;
        let mut p = 0usize;
        let mut input_off = 0i64;
        out.reserve(input.len());
        while p < input.len() {
            let rest = &input[p..];
            if let Some((bytes, units, rep, rep_len)) = m.longest(rest) {
                p += bytes;
                input_off += units as i64;
                let diff = units as i64 - rep_len as i64;
                if diff != 0 {
                    let prev = map.last_diff() as i64;
                    if diff > 0 {
                        map.add((input_off - diff - prev) as u32, (prev + diff) as i32);
                    } else {
                        // A longer replacement: the extra characters map back to the same input
                        // offset.
                        let output_start = input_off - prev;
                        for extra in 0..-diff {
                            map.add((output_start + extra) as u32, (prev - extra - 1) as i32);
                        }
                    }
                }
                out.push_str(rep);
            } else {
                let c = rest.chars().next().unwrap_or('\0');
                out.push(c);
                p += c.len_utf8();
                input_off += c.len_utf16() as i64;
            }
        }
        Ok(())
    }
}

/// `MappingCharFilterFactory.parseString`: `\\ \" \n \t \r \b \f \uXXXX` (other escaped
/// characters stand for themselves).
fn parse_mapping_string(s: &str) -> Result<String> {
    let units: Vec<u16> = s.encode_utf16().collect();
    let mut out: Vec<u16> = Vec::with_capacity(units.len());
    let invalid = || {
        AnalysisError::other(
            "runtime_exception",
            500,
            format!("Invalid escaped char in [{s}]"),
        )
    };
    let mut i = 0;
    while i < units.len() {
        let mut c = units[i];
        i += 1;
        if c == b'\\' as u16 {
            if i >= units.len() {
                return Err(invalid());
            }
            c = units[i];
            i += 1;
            c = match c {
                0x5C => 0x5C,
                0x22 => 0x22,
                0x6E => 0x0A,
                0x74 => 0x09,
                0x72 => 0x0D,
                0x62 => 0x08,
                0x66 => 0x0C,
                0x75 => {
                    if i + 3 >= units.len() {
                        return Err(invalid());
                    }
                    let hex = String::from_utf16_lossy(&units[i..i + 4]);
                    i += 4;
                    // `Integer.parseInt(hex, 16)` then a cast to `char`.
                    match i32::from_str_radix(&hex, 16) {
                        Ok(v) => v as u16,
                        Err(_) => {
                            return Err(AnalysisError::other(
                                "number_format_exception",
                                400,
                                format!("For input string: \"{hex}\" under radix 16"),
                            ));
                        }
                    }
                }
                other => other,
            };
        }
        out.push(c);
    }
    Ok(String::from_utf16_lossy(&out))
}

fn java_trim(s: &str) -> &str {
    s.trim_matches(|c: char| c <= ' ')
}

/// Parses `mappings` rules (`MappingCharFilterFactory.parseRules`).
pub(crate) fn parse_mapping_rules(rules: &[String]) -> Result<Vec<(String, String)>> {
    static RULE: std::sync::OnceLock<JavaRegex> = std::sync::OnceLock::new();
    let re =
        RULE.get_or_init(|| jregex::compile("(.*)\\s*=>\\s*(.*)\\s*$", 0).expect("valid pattern"));
    let mut caps = re.captures();
    let mut out = Vec::with_capacity(rules.len());
    for rule in rules {
        if !re.find_at(rule, 0, &mut caps) {
            return Err(AnalysisError::other(
                "runtime_exception",
                500,
                format!("Invalid Mapping Rule : [{rule}]"),
            ));
        }
        let g = |i| caps.get(i).map_or("", |(s, e)| &rule[s..e]);
        let lhs = parse_mapping_string(java_trim(g(1)))?;
        let rhs = parse_mapping_string(java_trim(g(2)))?;
        out.push((lhs, rhs));
    }
    Ok(out)
}

// ---------------------------------------------------------------------------------------------
// pattern_replace

/// `PatternReplaceCharFilter`.
pub(crate) struct PatternReplace {
    re: Arc<JavaRegex>,
    rep: Arc<Replacement>,
    caps: Captures,
    finder: Finder,
}

impl CharFilter for PatternReplace {
    fn filter(&mut self, input: &str, out: &mut String, map: &mut OffsetMap) -> Result<()> {
        self.finder.reset();
        let groups = self.rep.uses_groups();
        let mut cumulative: i64 = 0;
        // Byte position after the previous match, and the UTF-16 length of the input before it.
        let mut last_end = 0usize;
        let mut last_end16 = 0i64;
        let mut out16 = 0i64;
        let mut tmp = String::new();
        while let Some((s, e)) = self.finder.next(&self.re, input, &mut self.caps, groups) {
            let skipped = &input[last_end..s];
            let skipped16 = utf16_len(skipped) as i64;
            let start16 = last_end16 + skipped16;
            let group16 = utf16_len(&input[s..e]) as i64;
            out.push_str(skipped);
            out16 += skipped16;
            let length_before = out16;
            tmp.clear();
            self.rep.append(input, &self.caps, &mut tmp)?;
            let rep16 = utf16_len(&tmp) as i64;
            out.push_str(&tmp);
            out16 += rep16;
            if group16 != rep16 {
                if rep16 < group16 {
                    cumulative += group16 - rep16;
                    map.add((length_before + rep16) as u32, cumulative as i32);
                } else {
                    for i in group16..rep16 {
                        cumulative -= 1;
                        map.add((length_before + i) as u32, cumulative as i32);
                    }
                }
            }
            last_end = e;
            last_end16 = start16 + group16;
        }
        out.push_str(&input[last_end..]);
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------------
// Factories.

struct Factory<F> {
    make: F,
    normalizing: bool,
}

impl<F: Fn() -> Box<dyn CharFilter> + Send + Sync> CharFilterFactory for Factory<F> {
    fn create(&self) -> Box<dyn CharFilter> {
        (self.make)()
    }
    fn normalizing(&self) -> bool {
        self.normalizing
    }
}

fn factory<F: Fn() -> Box<dyn CharFilter> + Send + Sync + 'static>(
    normalizing: bool,
    make: F,
) -> Arc<dyn CharFilterFactory> {
    Arc::new(Factory { make, normalizing })
}

pub(crate) fn html_strip(escaped: Vec<String>) -> Arc<dyn CharFilterFactory> {
    let escaped = Arc::new(escaped);
    factory(false, move || {
        Box::new(html_strip::HtmlStrip::new(&escaped))
    })
}

pub(crate) fn mapping(rules: &[(String, String)]) -> Result<Arc<dyn CharFilterFactory>> {
    let map = Arc::new(CharMap::build(rules)?);
    Ok(factory(true, move || {
        Box::new(Mapping { map: map.clone() })
    }))
}

pub(crate) fn pattern_replace(re: JavaRegex, replacement: &str) -> Arc<dyn CharFilterFactory> {
    let rep = Arc::new(Replacement::parse(replacement, &re));
    let re = Arc::new(re);
    factory(true, move || {
        Box::new(PatternReplace {
            re: re.clone(),
            rep: rep.clone(),
            caps: re.captures(),
            finder: Finder::new(),
        })
    })
}

/// Lucene's `PersianCharFilter` (the Persian analyzer's reader): zero-width non-joiners become
/// spaces, one for one, so offsets are unchanged.
struct PersianChars;

impl CharFilter for PersianChars {
    fn filter(&mut self, input: &str, out: &mut String, _map: &mut OffsetMap) -> Result<()> {
        out.extend(input.chars().map(|c| if c == '\u{200C}' { ' ' } else { c }));
        Ok(())
    }
}

pub(crate) fn persian_char_filter() -> Arc<dyn CharFilterFactory> {
    factory(false, || Box::new(PersianChars))
}

/// Char filter types (`index.analysis.char_filter.*.type`).
pub(crate) fn char_filter_provider(ty: &str) -> Option<Provider<CharFilterBuilder>> {
    let p = |build: CharFilterBuilder, requires_settings: bool| {
        Some(Provider {
            build,
            requires_settings,
        })
    };
    match ty {
        "html_strip" => p(
            |_, _, s| Ok(html_strip(s.list_or_empty("escaped_tags"))),
            false,
        ),
        "mapping" => p(
            |_, _, s| {
                let Some(rules) = word_list(s, "mappings")? else {
                    return Err(AnalysisError::illegal(
                        "mapping requires either `mappings` or `mappings_path` to be configured",
                    ));
                };
                mapping(&parse_mapping_rules(&rules)?)
            },
            true,
        ),
        "pattern_replace" => p(
            |_, name, s| {
                let Some(pattern) = s.get("pattern").filter(|p| !p.is_empty()) else {
                    return Err(AnalysisError::illegal(format!(
                        "pattern is missing for [{name}] char filter of type 'pattern_replace'"
                    )));
                };
                let flags = match s.get("flags") {
                    Some(f) => jregex::flags_from_string(&f)?,
                    None => 0,
                };
                let re = jregex::compile(&pattern, flags)?;
                Ok(pattern_replace(
                    re,
                    &s.get("replacement").unwrap_or_default(),
                ))
            },
            true,
        ),
        _ => None,
    }
}

/// Pre-configured char filters.
pub(crate) fn prebuilt_char_filter(
    _ctx: &Ctx,
    name: &str,
) -> Option<Result<Arc<dyn CharFilterFactory>>> {
    match name {
        "html_strip" => Some(Ok(html_strip(Vec::new()))),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(f: &mut dyn CharFilter, s: &str) -> (String, Vec<u32>) {
        let mut out = String::new();
        let mut map = OffsetMap::default();
        f.filter(s, &mut out, &mut map).unwrap();
        let n = utf16_len(&out) as u32;
        (out, (0..=n).map(|o| map.correct(o)).collect())
    }

    fn rules(r: &[&str]) -> Vec<(String, String)> {
        parse_mapping_rules(&r.iter().map(|s| s.to_string()).collect::<Vec<_>>()).unwrap()
    }

    #[test]
    fn mapping_rules_and_offsets() {
        assert_eq!(
            rules(&["a => b", "\\u0020=>_", "x=>", "a => b => c"]),
            vec![
                ("a".into(), "b".into()),
                (" ".into(), "_".into()),
                ("x".into(), "".into()),
                ("a => b".into(), "c".into())
            ]
        );
        let err = parse_mapping_rules(&["ab".to_string()]).unwrap_err();
        assert_eq!(
            (err.kind, err.reason.as_str()),
            ("runtime_exception", "Invalid Mapping Rule : [ab]")
        );
        assert_eq!(
            parse_mapping_rules(&["a=>\\u00".to_string()])
                .unwrap_err()
                .reason,
            "Invalid escaped char in [\\u00]"
        );
        assert_eq!(
            CharMap::build(&rules(&[" => b"])).unwrap_err().reason,
            "cannot match the empty string"
        );
        assert_eq!(
            CharMap::build(&rules(&["a=>b", "a=>c"]))
                .unwrap_err()
                .reason,
            "match \"a\" was already added"
        );

        let map = Arc::new(
            CharMap::build(&rules(&["ph => f", "a => ", ":) => _happy_", "p => P"])).unwrap(),
        );
        let mut f = Mapping { map };
        let (out, offs) = run(&mut f, "phat :)");
        assert_eq!(out, "ft _happy_");
        // "ph" -> "f": output 1 is input 2; "a" removed; ":)" -> 7 chars all from input 5.
        assert_eq!(offs[..3], [0, 3, 4]);
        assert_eq!(offs[3], 5);
        assert_eq!(*offs.last().unwrap(), 7);
    }

    #[test]
    fn pattern_replace_offsets() {
        let re = jregex::compile("o", 0).unwrap();
        let mut f = pattern_replace(re, "OOO").create();
        let (out, offs) = run(&mut *f, "foo");
        assert_eq!(out, "fOOOOOO");
        assert_eq!(offs, vec![0, 1, 1, 1, 2, 2, 2, 3]);
        let re = jregex::compile("\\s+", 0).unwrap();
        let mut f = pattern_replace(re, " ").create();
        let (out, offs) = run(&mut *f, "a   b");
        assert_eq!(out, "a b");
        assert_eq!(offs, vec![0, 1, 4, 5]);
        let re = jregex::compile("(a)", 0).unwrap();
        let mut f = pattern_replace(re, "$2").create();
        let mut out = String::new();
        let e = f
            .filter("a", &mut out, &mut OffsetMap::default())
            .unwrap_err();
        assert_eq!(
            (e.kind, e.reason.as_str()),
            ("index_out_of_bounds_exception", "No group 2")
        );
        // No match: no error.
        assert!(
            f.filter("b", &mut String::new(), &mut OffsetMap::default())
                .is_ok()
        );
    }
}
