//! Beider-Morse phonetic matching, ported from Apache Commons Codec 1.15's `PhoneticEngine`,
//! `Rule`, `Lang` and `Languages` (Apache-2.0), with the codec's rule files (`data/bm/`,
//! Apache-2.0). A name type's files are parsed the first time an index uses it and shared by
//! every index on the node.

use std::collections::{BTreeMap, HashMap};
use std::sync::OnceLock;

/// `NameType`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NameType {
    Ashkenazi,
    Generic,
    Sephardic,
}

impl NameType {
    pub(crate) fn prefix(self) -> &'static str {
        match self {
            NameType::Ashkenazi => "ash",
            NameType::Generic => "gen",
            NameType::Sephardic => "sep",
        }
    }
}

/// `RuleType` (`RULES` is internal: the main rules every encoding runs first).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum RuleType {
    Approx,
    Exact,
    Rules,
}

impl RuleType {
    fn name(self) -> &'static str {
        match self {
            RuleType::Approx => "approx",
            RuleType::Exact => "exact",
            RuleType::Rules => "rules",
        }
    }
}

static FILES: &[(&str, &str)] = &[
    ("ash_approx_any", include_str!("data/bm/ash_approx_any.txt")),
    ("ash_approx_common", include_str!("data/bm/ash_approx_common.txt")),
    ("ash_approx_cyrillic", include_str!("data/bm/ash_approx_cyrillic.txt")),
    ("ash_approx_english", include_str!("data/bm/ash_approx_english.txt")),
    ("ash_approx_french", include_str!("data/bm/ash_approx_french.txt")),
    ("ash_approx_german", include_str!("data/bm/ash_approx_german.txt")),
    ("ash_approx_hebrew", include_str!("data/bm/ash_approx_hebrew.txt")),
    ("ash_approx_hungarian", include_str!("data/bm/ash_approx_hungarian.txt")),
    ("ash_approx_polish", include_str!("data/bm/ash_approx_polish.txt")),
    ("ash_approx_romanian", include_str!("data/bm/ash_approx_romanian.txt")),
    ("ash_approx_russian", include_str!("data/bm/ash_approx_russian.txt")),
    ("ash_approx_spanish", include_str!("data/bm/ash_approx_spanish.txt")),
    ("ash_exact_any", include_str!("data/bm/ash_exact_any.txt")),
    ("ash_exact_approx_common", include_str!("data/bm/ash_exact_approx_common.txt")),
    ("ash_exact_common", include_str!("data/bm/ash_exact_common.txt")),
    ("ash_exact_cyrillic", include_str!("data/bm/ash_exact_cyrillic.txt")),
    ("ash_exact_english", include_str!("data/bm/ash_exact_english.txt")),
    ("ash_exact_french", include_str!("data/bm/ash_exact_french.txt")),
    ("ash_exact_german", include_str!("data/bm/ash_exact_german.txt")),
    ("ash_exact_hebrew", include_str!("data/bm/ash_exact_hebrew.txt")),
    ("ash_exact_hungarian", include_str!("data/bm/ash_exact_hungarian.txt")),
    ("ash_exact_polish", include_str!("data/bm/ash_exact_polish.txt")),
    ("ash_exact_romanian", include_str!("data/bm/ash_exact_romanian.txt")),
    ("ash_exact_russian", include_str!("data/bm/ash_exact_russian.txt")),
    ("ash_exact_spanish", include_str!("data/bm/ash_exact_spanish.txt")),
    ("ash_hebrew_common", include_str!("data/bm/ash_hebrew_common.txt")),
    ("ash_lang", include_str!("data/bm/ash_lang.txt")),
    ("ash_languages", include_str!("data/bm/ash_languages.txt")),
    ("ash_rules_any", include_str!("data/bm/ash_rules_any.txt")),
    ("ash_rules_cyrillic", include_str!("data/bm/ash_rules_cyrillic.txt")),
    ("ash_rules_english", include_str!("data/bm/ash_rules_english.txt")),
    ("ash_rules_french", include_str!("data/bm/ash_rules_french.txt")),
    ("ash_rules_german", include_str!("data/bm/ash_rules_german.txt")),
    ("ash_rules_hebrew", include_str!("data/bm/ash_rules_hebrew.txt")),
    ("ash_rules_hungarian", include_str!("data/bm/ash_rules_hungarian.txt")),
    ("ash_rules_polish", include_str!("data/bm/ash_rules_polish.txt")),
    ("ash_rules_romanian", include_str!("data/bm/ash_rules_romanian.txt")),
    ("ash_rules_russian", include_str!("data/bm/ash_rules_russian.txt")),
    ("ash_rules_spanish", include_str!("data/bm/ash_rules_spanish.txt")),
    ("gen_approx_any", include_str!("data/bm/gen_approx_any.txt")),
    ("gen_approx_arabic", include_str!("data/bm/gen_approx_arabic.txt")),
    ("gen_approx_common", include_str!("data/bm/gen_approx_common.txt")),
    ("gen_approx_cyrillic", include_str!("data/bm/gen_approx_cyrillic.txt")),
    ("gen_approx_czech", include_str!("data/bm/gen_approx_czech.txt")),
    ("gen_approx_dutch", include_str!("data/bm/gen_approx_dutch.txt")),
    ("gen_approx_english", include_str!("data/bm/gen_approx_english.txt")),
    ("gen_approx_french", include_str!("data/bm/gen_approx_french.txt")),
    ("gen_approx_german", include_str!("data/bm/gen_approx_german.txt")),
    ("gen_approx_greeklatin", include_str!("data/bm/gen_approx_greeklatin.txt")),
    ("gen_approx_greek", include_str!("data/bm/gen_approx_greek.txt")),
    ("gen_approx_hebrew", include_str!("data/bm/gen_approx_hebrew.txt")),
    ("gen_approx_hungarian", include_str!("data/bm/gen_approx_hungarian.txt")),
    ("gen_approx_italian", include_str!("data/bm/gen_approx_italian.txt")),
    ("gen_approx_polish", include_str!("data/bm/gen_approx_polish.txt")),
    ("gen_approx_portuguese", include_str!("data/bm/gen_approx_portuguese.txt")),
    ("gen_approx_romanian", include_str!("data/bm/gen_approx_romanian.txt")),
    ("gen_approx_russian", include_str!("data/bm/gen_approx_russian.txt")),
    ("gen_approx_spanish", include_str!("data/bm/gen_approx_spanish.txt")),
    ("gen_approx_turkish", include_str!("data/bm/gen_approx_turkish.txt")),
    ("gen_exact_any", include_str!("data/bm/gen_exact_any.txt")),
    ("gen_exact_approx_common", include_str!("data/bm/gen_exact_approx_common.txt")),
    ("gen_exact_arabic", include_str!("data/bm/gen_exact_arabic.txt")),
    ("gen_exact_common", include_str!("data/bm/gen_exact_common.txt")),
    ("gen_exact_cyrillic", include_str!("data/bm/gen_exact_cyrillic.txt")),
    ("gen_exact_czech", include_str!("data/bm/gen_exact_czech.txt")),
    ("gen_exact_dutch", include_str!("data/bm/gen_exact_dutch.txt")),
    ("gen_exact_english", include_str!("data/bm/gen_exact_english.txt")),
    ("gen_exact_french", include_str!("data/bm/gen_exact_french.txt")),
    ("gen_exact_german", include_str!("data/bm/gen_exact_german.txt")),
    ("gen_exact_greeklatin", include_str!("data/bm/gen_exact_greeklatin.txt")),
    ("gen_exact_greek", include_str!("data/bm/gen_exact_greek.txt")),
    ("gen_exact_hebrew", include_str!("data/bm/gen_exact_hebrew.txt")),
    ("gen_exact_hungarian", include_str!("data/bm/gen_exact_hungarian.txt")),
    ("gen_exact_italian", include_str!("data/bm/gen_exact_italian.txt")),
    ("gen_exact_polish", include_str!("data/bm/gen_exact_polish.txt")),
    ("gen_exact_portuguese", include_str!("data/bm/gen_exact_portuguese.txt")),
    ("gen_exact_romanian", include_str!("data/bm/gen_exact_romanian.txt")),
    ("gen_exact_russian", include_str!("data/bm/gen_exact_russian.txt")),
    ("gen_exact_spanish", include_str!("data/bm/gen_exact_spanish.txt")),
    ("gen_exact_turkish", include_str!("data/bm/gen_exact_turkish.txt")),
    ("gen_hebrew_common", include_str!("data/bm/gen_hebrew_common.txt")),
    ("gen_lang", include_str!("data/bm/gen_lang.txt")),
    ("gen_languages", include_str!("data/bm/gen_languages.txt")),
    ("gen_rules_any", include_str!("data/bm/gen_rules_any.txt")),
    ("gen_rules_arabic", include_str!("data/bm/gen_rules_arabic.txt")),
    ("gen_rules_cyrillic", include_str!("data/bm/gen_rules_cyrillic.txt")),
    ("gen_rules_czech", include_str!("data/bm/gen_rules_czech.txt")),
    ("gen_rules_dutch", include_str!("data/bm/gen_rules_dutch.txt")),
    ("gen_rules_english", include_str!("data/bm/gen_rules_english.txt")),
    ("gen_rules_french", include_str!("data/bm/gen_rules_french.txt")),
    ("gen_rules_german", include_str!("data/bm/gen_rules_german.txt")),
    ("gen_rules_greeklatin", include_str!("data/bm/gen_rules_greeklatin.txt")),
    ("gen_rules_greek", include_str!("data/bm/gen_rules_greek.txt")),
    ("gen_rules_hebrew", include_str!("data/bm/gen_rules_hebrew.txt")),
    ("gen_rules_hungarian", include_str!("data/bm/gen_rules_hungarian.txt")),
    ("gen_rules_italian", include_str!("data/bm/gen_rules_italian.txt")),
    ("gen_rules_polish", include_str!("data/bm/gen_rules_polish.txt")),
    ("gen_rules_portuguese", include_str!("data/bm/gen_rules_portuguese.txt")),
    ("gen_rules_romanian", include_str!("data/bm/gen_rules_romanian.txt")),
    ("gen_rules_russian", include_str!("data/bm/gen_rules_russian.txt")),
    ("gen_rules_spanish", include_str!("data/bm/gen_rules_spanish.txt")),
    ("gen_rules_turkish", include_str!("data/bm/gen_rules_turkish.txt")),
    ("sep_approx_any", include_str!("data/bm/sep_approx_any.txt")),
    ("sep_approx_common", include_str!("data/bm/sep_approx_common.txt")),
    ("sep_approx_french", include_str!("data/bm/sep_approx_french.txt")),
    ("sep_approx_hebrew", include_str!("data/bm/sep_approx_hebrew.txt")),
    ("sep_approx_italian", include_str!("data/bm/sep_approx_italian.txt")),
    ("sep_approx_portuguese", include_str!("data/bm/sep_approx_portuguese.txt")),
    ("sep_approx_spanish", include_str!("data/bm/sep_approx_spanish.txt")),
    ("sep_exact_any", include_str!("data/bm/sep_exact_any.txt")),
    ("sep_exact_approx_common", include_str!("data/bm/sep_exact_approx_common.txt")),
    ("sep_exact_common", include_str!("data/bm/sep_exact_common.txt")),
    ("sep_exact_french", include_str!("data/bm/sep_exact_french.txt")),
    ("sep_exact_hebrew", include_str!("data/bm/sep_exact_hebrew.txt")),
    ("sep_exact_italian", include_str!("data/bm/sep_exact_italian.txt")),
    ("sep_exact_portuguese", include_str!("data/bm/sep_exact_portuguese.txt")),
    ("sep_exact_spanish", include_str!("data/bm/sep_exact_spanish.txt")),
    ("sep_hebrew_common", include_str!("data/bm/sep_hebrew_common.txt")),
    ("sep_lang", include_str!("data/bm/sep_lang.txt")),
    ("sep_languages", include_str!("data/bm/sep_languages.txt")),
    ("sep_rules_any", include_str!("data/bm/sep_rules_any.txt")),
    ("sep_rules_french", include_str!("data/bm/sep_rules_french.txt")),
    ("sep_rules_hebrew", include_str!("data/bm/sep_rules_hebrew.txt")),
    ("sep_rules_italian", include_str!("data/bm/sep_rules_italian.txt")),
    ("sep_rules_portuguese", include_str!("data/bm/sep_rules_portuguese.txt")),
    ("sep_rules_spanish", include_str!("data/bm/sep_rules_spanish.txt")),
];

fn file(name: &str) -> Option<&'static str> {
    FILES.iter().find(|(n, _)| *n == name).map(|(_, t)| *t)
}

/// Every language a rule file names, as bit positions of a [`LangSet`].
const LANGUAGES: &[&str] = &[
    "any", "arabic", "cyrillic", "czech", "dutch", "english", "french", "german", "greek", "greeklatin",
    "hebrew", "hungarian", "italian", "polish", "portuguese", "romanian", "russian", "spanish", "turkish",
];

fn lang_bit(name: &str) -> Option<u32> {
    LANGUAGES.iter().position(|l| *l == name).map(|i| 1 << i)
}

/// `Languages.LanguageSet`: `NO_LANGUAGES`, `ANY_LANGUAGE` or some languages.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LangSet {
    No,
    Any,
    Some(u32),
}

impl LangSet {
    fn from_bits(bits: u32) -> LangSet {
        if bits == 0 { LangSet::No } else { LangSet::Some(bits) }
    }

    /// `LanguageSet.from` over configured names. Names no rule file knows take bits of their
    /// own (Java keeps them in the set: they never match a phoneme, and a set of just one has
    /// no rules).
    pub(crate) fn from_names<'a>(names: impl IntoIterator<Item = &'a str>) -> LangSet {
        let mut bits = 0;
        let mut unknown = 31;
        for n in names {
            bits |= lang_bit(n).unwrap_or_else(|| {
                let b = 1 << unknown;
                unknown = (unknown - 1).max(LANGUAGES.len() as u32);
                b
            });
        }
        LangSet::from_bits(bits)
    }

    fn is_empty(self) -> bool {
        match self {
            LangSet::No => true,
            LangSet::Any => false,
            LangSet::Some(b) => b == 0,
        }
    }

    fn restrict_to(self, other: LangSet) -> LangSet {
        match (self, other) {
            (LangSet::No, _) => LangSet::No,
            (LangSet::Any, o) => o,
            (LangSet::Some(_), LangSet::No) => LangSet::No,
            (s @ LangSet::Some(_), LangSet::Any) => s,
            (LangSet::Some(a), LangSet::Some(b)) => LangSet::from_bits(a & b),
        }
    }

    fn merge(self, other: LangSet) -> LangSet {
        match (self, other) {
            (LangSet::No, o) | (LangSet::Any, o) => o,
            (s @ LangSet::Some(_), LangSet::No) => s,
            (LangSet::Some(_), LangSet::Any) => LangSet::Any,
            (LangSet::Some(a), LangSet::Some(b)) => LangSet::from_bits(a | b),
        }
    }
}

#[derive(Clone, Debug)]
struct Phoneme {
    text: String,
    langs: LangSet,
}

/// A context pattern (`Rule.pattern`): the codec's shortcuts for patterns without character
/// classes (literal comparisons, even of regex syntax), its single-class shortcuts, and
/// otherwise the regular expression, searched (`Matcher.find`).
#[derive(Debug)]
enum Pat {
    All,
    Empty,
    Equals(Vec<char>),
    StartsWith(Vec<char>),
    EndsWith(Vec<char>),
    /// One class: (`^`, `$`, chars, negated).
    Box(bool, bool, Vec<char>, bool),
    Regex(Regex),
}

impl Pat {
    fn new(regex: &str) -> Pat {
        let starts = regex.starts_with('^');
        let ends = regex.ends_with('$');
        let mut content = regex;
        if starts {
            content = &content[1..];
        }
        if ends && !content.is_empty() {
            content = &content[..content.len() - 1];
        }
        if !content.contains('[') {
            let lit: Vec<char> = content.chars().collect();
            if starts && ends {
                return if lit.is_empty() { Pat::Empty } else { Pat::Equals(lit) };
            }
            if lit.is_empty() {
                return Pat::All;
            }
            if starts {
                return Pat::StartsWith(lit);
            }
            if ends {
                return Pat::EndsWith(lit);
            }
        } else if content.starts_with('[') && content.ends_with(']') {
            let inner = &content[1..content.len() - 1];
            if !inner.contains('[') {
                let (neg, inner) = match inner.strip_prefix('^') {
                    Some(rest) => (true, rest),
                    None => (false, inner),
                };
                if starts || ends {
                    return Pat::Box(starts, ends, inner.chars().collect(), neg);
                }
            }
        }
        Pat::Regex(Regex::new(regex))
    }

    fn matches(&self, input: &[char]) -> bool {
        match self {
            Pat::All => true,
            Pat::Empty => input.is_empty(),
            Pat::Equals(l) => input == &l[..],
            Pat::StartsWith(l) => input.starts_with(l),
            Pat::EndsWith(l) => input.ends_with(l),
            Pat::Box(starts, ends, chars, neg) => {
                let hit = |c: &char| chars.contains(c) != *neg;
                match (starts, ends) {
                    (true, true) => input.len() == 1 && hit(&input[0]),
                    (true, false) => input.first().is_some_and(hit),
                    _ => input.last().is_some_and(hit),
                }
            }
            Pat::Regex(r) => r.find(input),
        }
    }
}

/// The regular expressions the rule and language files use: literals, `.`, classes and groups
/// of single-character alternatives, optionally anchored. Anything else goes to the `regex`
/// crate.
#[derive(Debug)]
enum Regex {
    Atoms { atoms: Vec<Atom>, start: bool, end: bool },
    Other(regex::Regex),
    /// A pattern Java rejects too (never in the shipped files).
    Never,
}

#[derive(Debug)]
enum Atom {
    Any,
    Set(Vec<char>, bool),
}

impl Atom {
    fn hit(&self, c: char) -> bool {
        match self {
            Atom::Any => true,
            Atom::Set(s, neg) => s.contains(&c) != *neg,
        }
    }
}

impl Regex {
    fn new(src: &str) -> Regex {
        Self::atoms(src).unwrap_or_else(|| match regex::Regex::new(src) {
            Ok(r) => Regex::Other(r),
            Err(_) => Regex::Never,
        })
    }

    fn atoms(src: &str) -> Option<Regex> {
        let mut chars: Vec<char> = src.chars().collect();
        let start = chars.first() == Some(&'^');
        if start {
            chars.remove(0);
        }
        let end = chars.last() == Some(&'$');
        if end {
            chars.pop();
        }
        let mut atoms = Vec::new();
        let mut i = 0;
        while i < chars.len() {
            match chars[i] {
                '.' => {
                    atoms.push(Atom::Any);
                    i += 1;
                }
                '[' => {
                    let close = chars[i + 1..].iter().position(|&c| c == ']')? + i + 1;
                    let mut set = &chars[i + 1..close];
                    let neg = set.first() == Some(&'^');
                    if neg {
                        set = &set[1..];
                    }
                    if set.iter().any(|&c| matches!(c, '[' | '\\' | '-' | '&')) {
                        return None;
                    }
                    atoms.push(Atom::Set(set.to_vec(), neg));
                    i = close + 1;
                }
                '(' => {
                    let close = chars[i + 1..].iter().position(|&c| c == ')')? + i + 1;
                    let body = &chars[i + 1..close];
                    let mut set = Vec::new();
                    for (k, part) in body.split(|&c| c == '|').enumerate() {
                        if part.len() != 1 || matches!(part[0], '[' | '.' | '\\' | '(' | '^' | '$') {
                            return None;
                        }
                        let _ = k;
                        set.push(part[0]);
                    }
                    atoms.push(Atom::Set(set, false));
                    i = close + 1;
                }
                '\\' | '*' | '+' | '?' | '{' | '|' | ')' | ']' | '$' | '^' => return None,
                c => {
                    atoms.push(Atom::Set(vec![c], false));
                    i += 1;
                }
            }
        }
        Some(Regex::Atoms { atoms, start, end })
    }

    fn find(&self, input: &[char]) -> bool {
        match self {
            Regex::Atoms { atoms, start, end } => {
                let n = atoms.len();
                if n > input.len() {
                    return false;
                }
                let at = |off: usize| atoms.iter().zip(&input[off..off + n]).all(|(a, &c)| a.hit(c));
                match (start, end) {
                    (true, true) => input.len() == n && at(0),
                    (true, false) => at(0),
                    (false, true) => at(input.len() - n),
                    (false, false) => (0..=input.len() - n).any(at),
                }
            }
            Regex::Other(r) => r.is_match(&input.iter().collect::<String>()),
            Regex::Never => false,
        }
    }
}

#[derive(Debug)]
struct Rule {
    pattern: Vec<char>,
    lctx: Pat,
    rctx: Pat,
    phonemes: Vec<Phoneme>,
}

impl Rule {
    /// `Rule.patternAndContextMatches`.
    fn matches(&self, input: &[char], i: usize) -> bool {
        let ipl = i + self.pattern.len();
        ipl <= input.len()
            && input[i..ipl] == self.pattern[..]
            && self.rctx.matches(&input[ipl..])
            && self.lctx.matches(&input[..i])
    }
}

type RuleMap = HashMap<char, Vec<Rule>>;

fn strip_quotes(s: &str) -> &str {
    let s = s.strip_prefix('"').unwrap_or(s);
    s.strip_suffix('"').unwrap_or(s)
}

fn parse_phoneme(ph: &str) -> Phoneme {
    if let Some(open) = ph.find('[')
        && ph.ends_with(']')
    {
        let langs = LangSet::from_names(ph[open + 1..ph.len() - 1].split('+'));
        return Phoneme { text: ph[..open].to_string(), langs };
    }
    Phoneme { text: ph.to_string(), langs: LangSet::Any }
}

fn parse_phoneme_expr(ph: &str) -> Vec<Phoneme> {
    if let Some(body) = ph.strip_prefix('(').and_then(|b| b.strip_suffix(')')) {
        // `body.split("[|]")`: trailing empty parts dropped.
        let mut parts: Vec<&str> = body.split('|').collect();
        while parts.len() > 1 && parts.last() == Some(&"") {
            parts.pop();
        }
        if parts == [""] && !body.is_empty() {
            parts.clear();
        }
        let mut phs: Vec<Phoneme> = parts.into_iter().map(parse_phoneme).collect();
        if body.starts_with('|') || body.ends_with('|') {
            phs.push(Phoneme { text: String::new(), langs: LangSet::Any });
        }
        return phs;
    }
    vec![parse_phoneme(ph)]
}

/// Lines of a resource without comments (`//` and `/* … */` blocks starting a line).
fn lines(text: &str) -> impl Iterator<Item = &str> {
    let mut in_comment = false;
    text.lines().filter_map(move |raw| {
        if in_comment {
            if raw.ends_with("*/") {
                in_comment = false;
            }
            return None;
        }
        if raw.starts_with("/*") {
            in_comment = true;
            return None;
        }
        let line = raw.split("//").next().unwrap_or("").trim();
        (!line.is_empty()).then_some(line)
    })
}

/// `Rule.parseRules`, with `#include` replacing whole first-character lists (`putAll`).
fn parse_rules(text: &str) -> RuleMap {
    let mut map: RuleMap = HashMap::new();
    for line in lines(text) {
        if let Some(incl) = line.strip_prefix("#include") {
            if let Some(t) = file(incl.trim()) {
                map.extend(parse_rules(t));
            }
            continue;
        }
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() != 4 {
            continue;
        }
        let pattern: Vec<char> = strip_quotes(parts[0]).chars().collect();
        let Some(&key) = pattern.first() else { continue };
        let rule = Rule {
            lctx: Pat::new(&format!("{}$", strip_quotes(parts[1]))),
            rctx: Pat::new(&format!("^{}", strip_quotes(parts[2]))),
            phonemes: parse_phoneme_expr(strip_quotes(parts[3])),
            pattern,
        };
        map.entry(key).or_default().push(rule);
    }
    map
}

/// A name type's rules and language guesser.
struct Tables {
    languages: u32,
    lang_rules: Vec<(Regex, u32, bool)>,
    rules: HashMap<(RuleType, &'static str), RuleMap>,
}

fn tables(nt: NameType) -> &'static Tables {
    static T: [OnceLock<Tables>; 3] = [OnceLock::new(), OnceLock::new(), OnceLock::new()];
    T[nt as usize].get_or_init(|| {
        let p = nt.prefix();
        let mut languages = 0;
        let mut names: Vec<&'static str> = Vec::new();
        for line in file(&format!("{p}_languages")).unwrap_or("").lines() {
            let line = line.trim();
            if let Some(i) = LANGUAGES.iter().position(|l| *l == line) {
                languages |= 1 << i;
                names.push(LANGUAGES[i]);
            }
        }
        let mut lang_rules = Vec::new();
        for line in lines(file(&format!("{p}_lang")).unwrap_or("")) {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() != 3 {
                continue;
            }
            let mut bits = 0;
            for l in parts[1].split('+') {
                bits |= lang_bit(l).unwrap_or(0);
            }
            lang_rules.push((Regex::new(parts[0]), bits, parts[2] == "true"));
        }
        let mut rules = HashMap::new();
        for rt in [RuleType::Approx, RuleType::Exact, RuleType::Rules] {
            let mut langs = names.clone();
            if rt != RuleType::Rules {
                langs.push("common");
            }
            for l in langs {
                if let Some(t) = file(&format!("{p}_{}_{l}", rt.name())) {
                    rules.insert((rt, l), parse_rules(t));
                }
            }
        }
        Tables { languages, lang_rules, rules }
    })
}

/// `PhoneticEngine` with `concat` (as Elasticsearch builds it) and at most 20 phonemes.
pub(crate) struct Engine {
    pub name_type: NameType,
    pub rule_type: RuleType,
}

const MAX_PHONEMES: usize = 20;

/// Java's `HashSet` iteration order of `PhoneticEngine.NAME_PREFIXES` for generic names (the
/// first prefix the input starts with wins).
const GENERIC_PREFIXES: &[&str] =
    &["da", "dal", "de", "del", "dela", "de la", "della", "des", "di", "do", "dos", "du", "van", "von"];

impl Engine {
    /// `Lang.guessLanguages`.
    fn guess(&self, input: &str) -> LangSet {
        let t = tables(self.name_type);
        let text: Vec<char> = input.to_lowercase().chars().collect();
        let mut langs = t.languages;
        for (re, bits, accept) in &t.lang_rules {
            if re.find(&text) {
                if *accept {
                    langs &= bits;
                } else {
                    langs &= !bits;
                }
            }
        }
        match LangSet::from_bits(langs) {
            LangSet::No => LangSet::Any,
            l => l,
        }
    }

    fn rule_map(&self, rt: RuleType, langs: LangSet) -> Option<&'static RuleMap> {
        let lang = match langs {
            LangSet::Some(b) if b.count_ones() == 1 => *LANGUAGES.get(b.trailing_zeros() as usize)?,
            _ => "any",
        };
        tables(self.name_type).rules.get(&(rt, lang))
    }

    /// `PhoneticEngine.encode(input)`: languages guessed from the input.
    pub(crate) fn encode_guess(&self, input: &str) -> Option<String> {
        let langs = self.guess(input);
        self.encode(input, langs)
    }

    /// `PhoneticEngine.encode(input, languageSet)`; None where Java fails (no rules for a
    /// language).
    pub(crate) fn encode(&self, input: &str, langs: LangSet) -> Option<String> {
        let rules = self.rule_map(RuleType::Rules, langs)?;
        let t = tables(self.name_type);
        let final1 = t.rules.get(&(self.rule_type, "common"))?;
        let final2 = self.rule_map(self.rule_type, langs)?;
        let input = input.to_lowercase().replace('-', " ");
        let input = input.trim_matches(|c: char| c <= ' ');
        if self.name_type == NameType::Generic {
            if let Some(rest) = input.strip_prefix("d'") {
                return Some(format!("({})-({})", self.encode_guess(rest)?, self.encode_guess(&format!("d{rest}"))?));
            }
            for l in GENERIC_PREFIXES {
                if let Some(rest) = input.strip_prefix(l).and_then(|r| r.strip_prefix(' ')) {
                    return Some(format!("({})-({})", self.encode_guess(rest)?, self.encode_guess(&format!("{l}{rest}"))?));
                }
            }
        }
        let words = input.split([' ', '\t', '\n', '\u{b}', '\u{c}', '\r']).filter(|w| !w.is_empty());
        let words2: Vec<String> = match self.name_type {
            NameType::Sephardic => {
                const P: &[&str] = &[
                    "al", "el", "da", "dal", "de", "del", "dela", "de la", "della", "des", "di", "do", "dos", "du", "van", "von",
                ];
                let mut v = Vec::new();
                for w in words {
                    let mut parts: Vec<&str> = w.split('\'').collect();
                    while parts.last() == Some(&"") {
                        parts.pop();
                    }
                    v.push(parts.last()?.to_string());
                }
                v.retain(|w| !P.contains(&w.as_str()));
                v
            }
            NameType::Ashkenazi => {
                const P: &[&str] = &["bar", "ben", "da", "de", "van", "von"];
                words.filter(|w| !P.contains(w)).map(str::to_string).collect()
            }
            NameType::Generic => words.map(str::to_string).collect(),
        };
        let input: Vec<char> = words2.join(" ").chars().collect();
        let mut phonemes = vec![Phoneme { text: String::new(), langs }];
        let mut i = 0;
        while i < input.len() {
            i = apply(rules, &input, &mut phonemes, i).0;
        }
        let phonemes = apply_final(phonemes, final1);
        let phonemes = apply_final(phonemes, final2);
        let mut out = String::new();
        for p in &phonemes {
            // `makeString`: the separator only after something (an empty first phoneme vanishes).
            if !out.is_empty() {
                out.push('|');
            }
            out.push_str(&p.text);
        }
        Some(out)
    }
}

/// `RulesApplication.invoke`: the next position and whether a rule matched.
fn apply(rules: &RuleMap, input: &[char], phonemes: &mut Vec<Phoneme>, i: usize) -> (usize, bool) {
    if let Some(list) = rules.get(&input[i]) {
        for rule in list {
            if rule.matches(input, i) {
                let mut next = Vec::with_capacity(MAX_PHONEMES);
                'expr: for left in phonemes.iter() {
                    for right in &rule.phonemes {
                        let langs = left.langs.restrict_to(right.langs);
                        if !langs.is_empty() {
                            next.push(Phoneme { text: format!("{}{}", left.text, right.text), langs });
                            if next.len() >= MAX_PHONEMES {
                                break 'expr;
                            }
                        }
                    }
                }
                *phonemes = next;
                return (i + rule.pattern.len(), true);
            }
        }
    }
    (i + 1, false)
}

/// `PhoneticEngine.applyFinalRules`: sorted, merged by text.
fn apply_final(phonemes: Vec<Phoneme>, rules: &RuleMap) -> Vec<Phoneme> {
    if rules.is_empty() {
        return phonemes;
    }
    let mut merged: BTreeMap<Vec<u16>, (String, LangSet)> = BTreeMap::new();
    for ph in phonemes {
        let text: Vec<char> = ph.text.chars().collect();
        let mut sub = vec![Phoneme { text: String::new(), langs: ph.langs }];
        let mut i = 0;
        while i < text.len() {
            let (next, found) = apply(rules, &text, &mut sub, i);
            if !found {
                for p in sub.iter_mut() {
                    p.text.push(text[i]);
                }
            }
            i = next;
        }
        for p in sub {
            let key: Vec<u16> = p.text.encode_utf16().collect();
            match merged.remove(&key) {
                Some((t, old)) => {
                    merged.insert(key, (t, old.merge(p.langs)));
                }
                None => {
                    merged.insert(key, (p.text, p.langs));
                }
            }
        }
    }
    merged.into_values().map(|(text, langs)| Phoneme { text, langs }).collect()
}
