//! Dissect: splitting text on the literal delimiters between `%{key}` fields, as
//! Elasticsearch's `dissect` library does it — for the `dissect(...)` extractor of runtime
//! fields, and reusable by the ingest `dissect` processor.
//!
//! A pattern is an optional leading delimiter, then `%{key}` / delimiter pairs. Key modifiers:
//! `%{+key}` appends to `key` (joined by the append separator; `%{+key/2}` sets the order),
//! `%{?key}` and `%{}` skip, `%{*name}`/`%{&name}` pair a field name with its value, and a
//! trailing `->` swallows repeated delimiters. Parsing walks the input bytes once, matching
//! each delimiter at its first occurrence; consecutive delimiters give empty values (or are
//! skipped after a `->` key). The result is `None` unless every key was matched.

use std::collections::HashMap;

/// Why a dissect pattern is invalid (`DissectException`): the Elasticsearch type
/// (`pattern_parse`, `key_parse`) and message.
#[derive(Clone, Debug, PartialEq)]
pub struct DissectError {
    pub kind: &'static str,
    pub message: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Modifier {
    None,
    AppendWithOrder,
    Append,
    FieldName,
    FieldValue,
    NamedSkip,
}

#[derive(Clone, Debug)]
struct Key {
    modifier: Modifier,
    skip: bool,
    skip_right_padding: bool,
    append_position: i32,
    name: String,
}

impl Key {
    /// `DissectKey(String)`.
    fn parse(key: &str) -> Result<Key, DissectError> {
        let key_err = |reason: &str| DissectError { kind: "key_parse", message: format!("Unable to parse key: {key} Reason: {reason}") };
        let modifier = find_modifier(key).map_err(key_err)?;
        let (strip_padding, padded) = match key.strip_suffix("->") {
            Some(k) => (k, true),
            None => (key, false),
        };
        let mut k = Key { modifier, skip: key.is_empty(), skip_right_padding: padded, append_position: 0, name: String::new() };
        match modifier {
            Modifier::None => {
                k.name = strip_padding.to_string();
                k.skip = k.name.is_empty();
            }
            Modifier::NamedSkip | Modifier::Append | Modifier::FieldName | Modifier::FieldValue => {
                // `([+*&?])(.*?)(->)?$` found anywhere: the name follows the modifier's (last)
                // occurrence that leaves a match to the end
                let pos = strip_padding.find(['+', '*', '&', '?']).unwrap_or(0);
                k.name = strip_padding[pos + 1..].to_string();
                if modifier == Modifier::NamedSkip {
                    k.skip = true;
                }
            }
            Modifier::AppendWithOrder => {
                // `[+](.*?)(/)([0-9]+)(->)?$`
                let Some(pos) = strip_padding.find('+') else { return Err(key_err("The key name could not be determined")) };
                let body = &strip_padding[pos + 1..];
                let slash = body.rfind('/').filter(|&s| !body[s + 1..].is_empty() && body[s + 1..].bytes().all(|b| b.is_ascii_digit()));
                match slash {
                    Some(s) => {
                        k.name = body[..s].to_string();
                        // `Short.valueOf`
                        k.append_position = match body[s + 1..].parse::<i64>() {
                            Ok(v) if (i16::MIN as i64..=i16::MAX as i64).contains(&v) => v as i32,
                            _ => return Err(DissectError { kind: "number_format_exception", message: format!("Value out of range. Value:\"{}\" Radix:10", &body[s + 1..]) }),
                        };
                    }
                    None => return Err(key_err("The key name could not be determined")),
                }
            }
        }
        if k.name.is_empty() && !k.skip {
            return Err(key_err("The key name could not be determined"));
        }
        Ok(k)
    }
}

/// `Modifier.findModifier`: one of `/+*&?` (only `+` followed by `/` may combine).
fn find_modifier(key: &str) -> Result<Modifier, &'static str> {
    let mut modifier = Modifier::None;
    let mut matches = 0;
    for c in key.chars() {
        let m = match c {
            '/' => Modifier::AppendWithOrder,
            '+' => Modifier::Append,
            '*' => Modifier::FieldName,
            '&' => Modifier::FieldValue,
            '?' => Modifier::NamedSkip,
            _ => continue,
        };
        let prior = modifier;
        modifier = m;
        matches += 1;
        if matches > 1 && !(prior == Modifier::Append && modifier == Modifier::AppendWithOrder) {
            return Err("multiple modifiers are not allowed.");
        }
    }
    Ok(modifier)
}

/// A compiled dissect pattern. Immutable; share it across threads.
#[derive(Clone, Debug)]
pub struct Dissect {
    pattern: String,
    leading: String,
    pairs: Vec<(Key, String)>,
    append_separator: String,
    max_results: usize,
    append_count: usize,
    reference_count: usize,
}

impl Dissect {
    /// `DissectParser(pattern, appendSeparator)`.
    pub fn new(pattern: &str, append_separator: Option<&str>) -> Result<Dissect, DissectError> {
        let parse_err = |reason: &str| DissectError { kind: "pattern_parse", message: format!("Unable to parse pattern: {pattern} Reason: {reason}") };
        // `^(.*?)%\{` found repeatedly: the text before the first `%{` — unless it holds a line
        // terminator, which `.` does not cross (then there is no leading delimiter, and the
        // text is skipped over when the keys are read)
        let leading = pattern
            .find("%{")
            .map(|i| &pattern[..i])
            .filter(|l| !l.contains(['\n', '\r', '\u{85}', '\u{2028}', '\u{2029}']))
            .unwrap_or("")
            .to_string();
        let body = &pattern[leading.len()..];
        // `%\{([^}]*?)}(.+?(?=%\{)|.*$)` with DOTALL, found repeatedly
        let mut pairs: Vec<(Key, String)> = Vec::new();
        let mut i = 0;
        while let Some(off) = body[i..].find("%{") {
            let start = i + off;
            let Some(close) = body[start + 2..].find('}').map(|j| start + 2 + j) else { break };
            let key = &body[start + 2..close];
            let after = close + 1;
            // the delimiter: at least one char up to the next `%{`, or everything to the end
            let delim_end = if after < body.len() {
                let first = body[after..].chars().next().map(|c| c.len_utf8()).unwrap_or(0);
                match body[after + first..].find("%{") {
                    Some(j) => after + first + j,
                    None => body.len(),
                }
            } else {
                body.len()
            };
            pairs.push((Key::parse(key)?, body[after..delim_end].to_string()));
            i = delim_end;
            if i >= body.len() {
                break;
            }
        }
        let mut names: Vec<&str> = Vec::new();
        for (k, _) in &pairs {
            if !k.skip && !names.contains(&k.name.as_str()) {
                names.push(&k.name);
            }
        }
        let max_results = names.len();
        if pairs.is_empty() || max_results == 0 {
            return Err(parse_err("Unable to find any keys or delimiters."));
        }
        // a plain key sharing its name with an appended one appends too
        let append_names: Vec<String> =
            pairs.iter().filter(|(k, _)| matches!(k.modifier, Modifier::Append | Modifier::AppendWithOrder)).map(|(k, _)| k.name.clone()).collect();
        let mut distinct_appends = append_names.clone();
        distinct_appends.sort();
        distinct_appends.dedup();
        for (k, _) in &mut pairs {
            if k.modifier == Modifier::None && append_names.contains(&k.name) {
                k.modifier = Modifier::Append;
            }
        }
        // `*name` and `&name` come in pairs
        let mut refs: Vec<(String, Vec<String>)> = Vec::new();
        for (k, _) in &pairs {
            if matches!(k.modifier, Modifier::FieldName | Modifier::FieldValue) {
                match refs.iter_mut().find(|(n, _)| *n == k.name) {
                    Some((_, v)) => v.push(k.name.clone()),
                    None => refs.push((k.name.clone(), vec![k.name.clone()])),
                }
            }
        }
        for (_, v) in &refs {
            if v.len() != 2 {
                return Err(parse_err(&format!(
                    "Found invalid key/reference associations: '{}' Please ensure each '*<key>' is matched with a matching '&<key>",
                    v.join(",")
                )));
            }
        }
        Ok(Dissect {
            pattern: pattern.to_string(),
            leading,
            pairs,
            append_separator: append_separator.unwrap_or("").to_string(),
            max_results,
            append_count: distinct_appends.len(),
            reference_count: refs.len() * 2,
        })
    }

    pub fn pattern(&self) -> &str {
        &self.pattern
    }

    /// The number of fields a match yields (the capacity Elasticsearch sizes its result
    /// `HashMap` with, which decides the map's iteration order).
    pub fn max_results(&self) -> usize {
        self.max_results
    }

    /// The fields of `input` (Java `HashMap` semantics are the caller's: the order here is
    /// unspecified), or `None` when the pattern does not match all of it.
    pub fn parse(&self, input: Option<&str>) -> Option<HashMap<String, String>> {
        let mut m = Match::new(self);
        if let Some(text) = input
            && text.encode_utf16().count() > self.leading.encode_utf16().count()
            && text.as_bytes().starts_with(self.leading.as_bytes())
        {
            let input = text.as_bytes();
            let mut it = self.pairs.iter();
            let (mut key, mut delim) = match it.next() {
                Some((k, d)) => (k, d.as_bytes()),
                None => return None,
            };
            let mut i = self.leading.len();
            let mut value_start = i;
            let at = |i: usize, d: &[u8]| (0..d.len()).filter(|&j| i + j < input.len() && input[i + j] == d[j]).count();
            while i < input.len() {
                if !delim.is_empty() && input[i] == delim[0] {
                    let hits = at(i, delim);
                    if hits == delim.len() {
                        m.add(key, String::from_utf8_lossy(&input[value_start..i]).into_owned());
                        i += hits;
                        // consecutive delimiters
                        let mut stop = false;
                        while i < input.len() {
                            if at(i, delim) == delim.len() {
                                i += delim.len();
                                if !key.skip_right_padding {
                                    match it.next() {
                                        Some((k, _)) => {
                                            key = k;
                                            m.add(key, String::new());
                                        }
                                        None => {
                                            stop = true;
                                            break;
                                        }
                                    }
                                }
                            } else {
                                break;
                            }
                        }
                        let _ = stop;
                        match it.next() {
                            Some((k, d)) => {
                                key = k;
                                delim = d.as_bytes();
                            }
                            None => break,
                        }
                        value_start = i;
                    } else {
                        i += 1;
                    }
                } else {
                    i += 1;
                }
            }
            if !m.fully_matched() && delim.is_empty() {
                m.add(key, String::from_utf8_lossy(&input[value_start..]).into_owned());
            }
        }
        let results = m.results();
        (m.fully_matched() && results.len() == self.max_results).then_some(results)
    }
}

/// `DissectMatch`: values by key kind, assembled at the end.
struct Match<'a> {
    d: &'a Dissect,
    matches: usize,
    simple: Vec<(String, String)>,
    references: Vec<(String, Option<String>, Option<String>)>,
    appends: Vec<(String, Vec<(String, i32)>)>,
    implicit_order: i32,
}

impl<'a> Match<'a> {
    fn new(d: &'a Dissect) -> Match<'a> {
        Match { d, matches: 0, simple: Vec::new(), references: Vec::new(), appends: Vec::new(), implicit_order: -1000 }
    }

    fn add(&mut self, key: &Key, value: String) {
        self.matches += 1;
        if key.skip {
            return;
        }
        match key.modifier {
            Modifier::None => match self.simple.iter_mut().find(|(k, _)| *k == key.name) {
                Some((_, v)) => *v = value,
                None => self.simple.push((key.name.clone(), value)),
            },
            Modifier::Append | Modifier::AppendWithOrder => {
                let order = if key.modifier == Modifier::Append {
                    self.implicit_order += 1;
                    self.implicit_order - 1
                } else {
                    key.append_position
                };
                match self.appends.iter_mut().find(|(k, _)| *k == key.name) {
                    Some((_, v)) => v.push((value, order)),
                    None => self.appends.push((key.name.clone(), vec![(value, order)])),
                }
            }
            Modifier::FieldName | Modifier::FieldValue => {
                let slot = match self.references.iter().position(|(k, _, _)| *k == key.name) {
                    Some(p) => p,
                    None => {
                        self.references.push((key.name.clone(), None, None));
                        self.references.len() - 1
                    }
                };
                if key.modifier == Modifier::FieldName {
                    self.references[slot].1 = Some(value);
                } else {
                    self.references[slot].2 = Some(value);
                }
            }
            Modifier::NamedSkip => {}
        }
    }

    fn fully_matched(&self) -> bool {
        self.matches == self.d.pairs.len()
    }

    fn results(&self) -> HashMap<String, String> {
        let mut out = HashMap::new();
        for (k, v) in &self.simple {
            out.insert(k.clone(), v.clone());
        }
        let _ = (self.d.append_count, self.d.reference_count);
        for (_, name, value) in &self.references {
            // `results.put(v.getKey(), v.getValue())` — a missing half is a null key or value
            out.insert(name.clone().unwrap_or_else(|| "null".into()), value.clone().unwrap_or_default());
        }
        for (k, values) in &self.appends {
            let mut v = values.clone();
            // `Collections.sort` is stable
            v.sort_by_key(|(_, o)| *o);
            out.insert(k.clone(), v.into_iter().map(|(s, _)| s).collect::<Vec<_>>().join(&self.d.append_separator));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(p: &str, sep: Option<&str>, input: &str) -> Option<Vec<(String, String)>> {
        let d = Dissect::new(p, sep).unwrap();
        d.parse(Some(input)).map(|m| {
            let mut v: Vec<(String, String)> = m.into_iter().collect();
            v.sort();
            v
        })
    }

    fn kv(pairs: &[(&str, &str)]) -> Option<Vec<(String, String)>> {
        Some(pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect())
    }

    #[test]
    fn modifiers() {
        assert_eq!(parse("%{a} %{b}", None, "x y"), kv(&[("a", "x"), ("b", "y")]));
        assert_eq!(parse("%{a} %{+a}", None, "x y"), kv(&[("a", "xy")]));
        assert_eq!(parse("%{a} %{+a}", Some("-"), "x y"), kv(&[("a", "x-y")]));
        assert_eq!(parse("%{?skip} %{b}", None, "x y"), kv(&[("b", "y")]));
        assert_eq!(parse("%{*k} %{&k}", None, "key val"), kv(&[("key", "val")]));
        assert_eq!(parse("%{a->} %{b}", None, "x      y"), kv(&[("a", "x"), ("b", "y")]));
        assert_eq!(parse("%{+a/2} %{+a/1}", None, "x y"), kv(&[("a", "yx")]));
        assert_eq!(parse("%{a} %{a}", None, "x y"), kv(&[("a", "y")]));
        assert_eq!(parse("[%{ts}] %{level}: %{msg}", None, "[2021-01-01] WARN: disk 90% full"), kv(&[("level", "WARN"), ("msg", "disk 90% full"), ("ts", "2021-01-01")]));
        assert_eq!(parse("%{a},%{b},%{c}", None, "1,,3"), kv(&[("a", "1"), ("b", ""), ("c", "3")]));
    }

    #[test]
    fn mismatches_and_errors() {
        assert_eq!(parse("%{a} %{b}", None, "nospace"), None);
        assert_eq!(parse("%{a}", None, ""), None);
        assert_eq!(Dissect::new("no keys", None).unwrap_err(), DissectError { kind: "pattern_parse", message: "Unable to parse pattern: no keys Reason: Unable to find any keys or delimiters.".into() });
        assert_eq!(Dissect::new("%{*a} %{b}", None).unwrap_err().message, "Unable to parse pattern: %{*a} %{b} Reason: Found invalid key/reference associations: 'a' Please ensure each '*<key>' is matched with a matching '&<key>");
        assert_eq!(Dissect::new("%{+?a}", None).unwrap_err().message, "Unable to parse key: +?a Reason: multiple modifiers are not allowed.");
    }
}
