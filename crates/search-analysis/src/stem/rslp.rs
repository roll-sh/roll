//! RSLP stemmers (Removedor de Sufixos da Lingua Portuguesa): `portuguese_rslp`,
//! `minimal_portuguese`, `galician`, `minimal_galician` (Lucene's `RSLPStemmerBase` and the
//! stemmers built on it).
//!
//! The rules are Lucene's `portuguese.rslp` and `galician.rslp` resource files (in `data/`),
//! parsed once with the same grammar Lucene uses. A step applies its first matching rule; rules
//! carry a minimum stem length and optional exceptions (whole words or suffixes, depending on
//! the step type).

use std::collections::HashMap;
use std::sync::OnceLock;

use super::light::ends_with;
use crate::jregex::{self, JavaRegex};

enum Exceptions {
    None,
    Words(Vec<Vec<char>>),
    Suffixes(Vec<String>),
}

struct Rule {
    suffix: String,
    suffix_len: usize,
    replacement: Vec<char>,
    min: usize,
    exceptions: Exceptions,
}

impl Rule {
    fn matches(&self, s: &[char], len: usize) -> bool {
        if len < self.suffix_len + self.min || !ends_with(s, len, &self.suffix) {
            return false;
        }
        match &self.exceptions {
            Exceptions::None => true,
            Exceptions::Words(words) => !words.iter().any(|w| w.len() == len && w[..] == s[..len]),
            Exceptions::Suffixes(sfx) => !sfx.iter().any(|x| ends_with(s, len, x)),
        }
    }

    fn replace(&self, s: &mut Vec<char>, len: usize) -> usize {
        let start = len - self.suffix_len;
        let end = start + self.replacement.len();
        if s.len() < end {
            s.resize(end, '\0');
        }
        s[start..end].copy_from_slice(&self.replacement);
        end
    }
}

pub(crate) struct Step {
    rules: Vec<Rule>,
    min: usize,
    suffixes: Vec<String>,
}

impl Step {
    /// `Step.apply`: the new length after the first matching rule.
    pub fn apply(&self, s: &mut Vec<char>, len: usize) -> usize {
        if len < self.min {
            return len;
        }
        if !self.suffixes.is_empty() && !self.suffixes.iter().any(|x| ends_with(s, len, x)) {
            return len;
        }
        for r in &self.rules {
            if r.matches(s, len) {
                return r.replace(s, len);
            }
        }
        len
    }
}

fn parse_list(s: &str) -> Vec<String> {
    if s.is_empty() {
        return Vec::new();
    }
    s.split(',')
        .map(|x| {
            let x = x.trim();
            // `parseString`: drop the surrounding quotes.
            let mut it = x.chars();
            it.next();
            it.next_back();
            it.as_str().to_string()
        })
        .collect()
}

/// Parses an `.rslp` description into named steps (`RSLPStemmerBase.parse`).
fn parse(text: &str) -> HashMap<String, Step> {
    static PATTERNS: OnceLock<[JavaRegex; 4]> = OnceLock::new();
    let [header, strip, rep, exc] = PATTERNS.get_or_init(|| {
        let c = |p: &str| jregex::compile(p, 0).expect("valid RSLP pattern");
        [
            c("^\\{\\s*\"([^\"]*)\",\\s*([0-9]+),\\s*(0|1),\\s*\\{(.*)\\},\\s*$"),
            c("^\\{\\s*\"([^\"]*)\",\\s*([0-9]+)\\s*\\}\\s*(,|(\\}\\s*;))$"),
            c("^\\{\\s*\"([^\"]*)\",\\s*([0-9]+),\\s*\"([^\"]*)\"\\}\\s*(,|(\\}\\s*;))$"),
            c("^\\{\\s*\"([^\"]*)\",\\s*([0-9]+),\\s*\"([^\"]*)\",\\s*\\{(.*)\\}\\s*\\}\\s*(,|(\\}\\s*;))$"),
        ]
    });
    let mut lines = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'));
    let mut steps = HashMap::new();
    let mut caps = header.captures();
    let group = |caps: &crate::jregex::Captures, line: &str, i: usize| -> String {
        caps.get(i)
            .map_or(String::new(), |(s, e)| line[s..e].to_string())
    };
    while let Some(h) = lines.next() {
        assert!(
            header.find_at(h, 0, &mut caps),
            "illegal RSLP step header {h:?}"
        );
        let name = group(&caps, h, 1);
        let min: usize = group(&caps, h, 2).parse().unwrap_or(0);
        let ty: u8 = group(&caps, h, 3).parse().unwrap_or(0);
        let suffixes = parse_list(&group(&caps, h, 4));
        let mut rules = Vec::new();
        let (mut cs, mut cr, mut ce) = (strip.captures(), rep.captures(), exc.captures());
        for line in lines.by_ref() {
            // The patterns are anchored, so a match is a whole-line match (`matches()`).
            let rule = if strip.find_at(line, 0, &mut cs) {
                (
                    group(&cs, line, 1),
                    group(&cs, line, 2),
                    String::new(),
                    None,
                )
            } else if rep.find_at(line, 0, &mut cr) {
                (
                    group(&cr, line, 1),
                    group(&cr, line, 2),
                    group(&cr, line, 3),
                    None,
                )
            } else if exc.find_at(line, 0, &mut ce) {
                (
                    group(&ce, line, 1),
                    group(&ce, line, 2),
                    group(&ce, line, 3),
                    Some(parse_list(&group(&ce, line, 4))),
                )
            } else {
                panic!("illegal RSLP rule {line:?}");
            };
            let (suffix, rmin, replacement, exceptions) = rule;
            let exceptions = match exceptions {
                None => Exceptions::None,
                Some(list) if ty == 0 => Exceptions::Suffixes(list),
                Some(list) => Exceptions::Words(list.iter().map(|w| w.chars().collect()).collect()),
            };
            rules.push(Rule {
                suffix_len: suffix.chars().count(),
                suffix,
                replacement: replacement.chars().collect(),
                min: rmin.parse().unwrap_or(0),
                exceptions,
            });
            if line.ends_with(';') {
                break;
            }
        }
        let min = if min == 0 {
            rules
                .iter()
                .map(|r| r.min + r.suffix_len)
                .min()
                .unwrap_or(0)
        } else {
            min
        };
        steps.insert(
            name,
            Step {
                rules,
                min,
                suffixes,
            },
        );
    }
    steps
}

pub(crate) fn portuguese() -> &'static HashMap<String, Step> {
    static S: OnceLock<HashMap<String, Step>> = OnceLock::new();
    S.get_or_init(|| parse(include_str!("data/portuguese.rslp")))
}

pub(crate) fn galician() -> &'static HashMap<String, Step> {
    static S: OnceLock<HashMap<String, Step>> = OnceLock::new();
    S.get_or_init(|| parse(include_str!("data/galician.rslp")))
}

fn step<'a>(steps: &'a HashMap<String, Step>, name: &str) -> &'a Step {
    &steps[name]
}

/// `PortugueseStemmer` (`portuguese_rslp`).
pub(crate) fn portuguese_rslp(s: &mut Vec<char>, mut len: usize) -> usize {
    let st = portuguese();
    len = step(st, "Plural").apply(s, len);
    len = step(st, "Adverb").apply(s, len);
    len = step(st, "Feminine").apply(s, len);
    len = step(st, "Augmentative").apply(s, len);
    let old = len;
    len = step(st, "Noun").apply(s, len);
    if len == old {
        let old = len;
        len = step(st, "Verb").apply(s, len);
        if len == old {
            len = step(st, "Vowel").apply(s, len);
        }
    }
    for c in s[..len].iter_mut() {
        *c = match *c {
            'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' => 'a',
            'ç' => 'c',
            'è' | 'é' | 'ê' | 'ë' => 'e',
            'ì' | 'í' | 'î' | 'ï' => 'i',
            'ñ' => 'n',
            'ò' | 'ó' | 'ô' | 'õ' | 'ö' => 'o',
            'ù' | 'ú' | 'û' | 'ü' => 'u',
            'ý' | 'ÿ' => 'y',
            c => c,
        };
    }
    len
}

/// `PortugueseMinimalStemmer` (`minimal_portuguese`): plurals only.
pub(crate) fn portuguese_minimal(s: &mut Vec<char>, len: usize) -> usize {
    step(portuguese(), "Plural").apply(s, len)
}

/// `GalicianStemmer` (`galician` stemmer and analyzer).
pub(crate) fn galician_stem(s: &mut Vec<char>, mut len: usize) -> usize {
    let st = galician();
    len = step(st, "Plural").apply(s, len);
    len = step(st, "Unification").apply(s, len);
    len = step(st, "Adverb").apply(s, len);
    loop {
        let old = len;
        len = step(st, "Augmentative").apply(s, len);
        if len == old {
            break;
        }
    }
    let old = len;
    len = step(st, "Noun").apply(s, len);
    if len == old {
        len = step(st, "Verb").apply(s, len);
    }
    len = step(st, "Vowel").apply(s, len);
    for c in s[..len].iter_mut() {
        *c = match *c {
            'á' => 'a',
            'é' | 'ê' => 'e',
            'í' => 'i',
            'ó' => 'o',
            'ú' => 'u',
            c => c,
        };
    }
    len
}

/// `GalicianMinimalStemmer` (`minimal_galician`).
pub(crate) fn galician_minimal(s: &mut Vec<char>, len: usize) -> usize {
    step(galician(), "Plural").apply(s, len)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(f: fn(&mut Vec<char>, usize) -> usize, w: &str) -> String {
        let mut s: Vec<char> = w.chars().collect();
        let n = s.len();
        let n = f(&mut s, n);
        s[..n].iter().collect()
    }

    #[test]
    fn rules_parse_and_apply() {
        assert_eq!(portuguese().len(), 7);
        assert_eq!(galician().len(), 7);
        assert_eq!(run(portuguese_minimal, "balões"), "balão");
        assert_eq!(run(portuguese_minimal, "lápis"), "lápis");
        assert_eq!(run(portuguese_minimal, "casas"), "casa");
        assert_eq!(run(portuguese_rslp, "felizmente"), "feliz");
    }
}
