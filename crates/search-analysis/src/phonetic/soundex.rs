//! Soundex, Refined Soundex, NYSIIS, Caverphone 1 and 2, Cologne phonetics and Daitch-Mokotoff
//! Soundex, ported from Apache Commons Codec 1.15 (Apache-2.0). An encoder that throws in Java
//! (a letter outside A-Z for Soundex) returns None here, and the filter keeps the token.

use std::collections::HashMap;
use std::sync::OnceLock;

use crate::unicode;

use super::metaphone::upper;

/// `SoundexUtils.clean`: letters only, upper-cased.
fn clean(s: &str) -> Vec<char> {
    let letters: String = s.chars().filter(|&c| unicode::is_letter(c)).collect();
    upper(&letters)
}

/// `Soundex.soundex` (US English mapping, H and W ignored).
pub(crate) fn soundex(s: &str) -> Option<String> {
    const MAP: &[u8; 26] = b"01230120022455012623010202";
    let map = |c: char| -> Option<char> {
        let i = (c as u32).checked_sub('A' as u32)? as usize;
        MAP.get(i).map(|&b| b as char)
    };
    let s = clean(s);
    if s.is_empty() {
        return Some(String::new());
    }
    let mut out = ['0'; 4];
    let mut count = 0;
    out[count] = s[0];
    count += 1;
    let mut last = map(s[0])?;
    for &ch in &s[1..] {
        if count >= out.len() {
            break;
        }
        if ch == 'H' || ch == 'W' {
            continue;
        }
        let digit = map(ch)?;
        if digit != '0' && digit != last {
            out[count] = digit;
            count += 1;
        }
        last = digit;
    }
    Some(out.iter().collect())
}

/// `RefinedSoundex.soundex`.
pub(crate) fn refined_soundex(s: &str) -> Option<String> {
    const MAP: &[u8; 26] = b"01360240043788015936020505";
    let s = clean(s);
    if s.is_empty() {
        return Some(String::new());
    }
    let mut out = String::new();
    out.push(s[0]);
    let mut last = '*';
    for &c in &s {
        // `getMappingCode`: non-letters map to 0; a letter outside A-Z throws.
        let current = if !unicode::is_letter(c) {
            '\0'
        } else {
            let i = (unicode::to_upper(c) as u32).checked_sub('A' as u32)? as usize;
            *MAP.get(i)? as char
        };
        if current == last {
            continue;
        }
        if current != '\0' {
            out.push(current);
        }
        last = current;
    }
    Some(out)
}

/// `Nysiis.nysiis` (strict: at most 6 characters).
pub(crate) fn nysiis(s: &str) -> String {
    let mut s = clean(s);
    if s.is_empty() {
        return String::new();
    }
    let starts = |s: &[char], p: &str| p.chars().count() <= s.len() && s.iter().zip(p.chars()).all(|(&a, b)| a == b);
    let ends = |s: &[char], p: &str| {
        let n = p.chars().count();
        n <= s.len() && s[s.len() - n..].iter().zip(p.chars()).all(|(&a, b)| a == b)
    };
    let replace_prefix = |s: &mut Vec<char>, n: usize, with: &str| {
        s.splice(0..n, with.chars());
    };
    if starts(&s, "MAC") {
        replace_prefix(&mut s, 3, "MCC");
    }
    if starts(&s, "KN") {
        replace_prefix(&mut s, 2, "NN");
    }
    if starts(&s, "K") {
        replace_prefix(&mut s, 1, "C");
    }
    if starts(&s, "PH") || starts(&s, "PF") {
        replace_prefix(&mut s, 2, "FF");
    }
    if starts(&s, "SCH") {
        replace_prefix(&mut s, 3, "SSS");
    }
    if ends(&s, "EE") || ends(&s, "IE") {
        let n = s.len();
        s.splice(n - 2.., ['Y']);
    }
    if ["DT", "RT", "RD", "NT", "ND"].iter().any(|p| ends(&s, p)) {
        let n = s.len();
        s.splice(n - 2.., ['D']);
    }
    let vowel = |c: char| matches!(c, 'A' | 'E' | 'I' | 'O' | 'U');
    let mut key: Vec<char> = vec![s[0]];
    let len = s.len();
    for i in 1..len {
        let next = if i < len - 1 { s[i + 1] } else { ' ' };
        let a_next = if i + 2 < len { s[i + 2] } else { ' ' };
        let (prev, curr) = (s[i - 1], s[i]);
        let t: &[char] = if curr == 'E' && next == 'V' {
            &['A', 'F']
        } else if vowel(curr) {
            &['A']
        } else if curr == 'Q' {
            &['G']
        } else if curr == 'Z' {
            &['S']
        } else if curr == 'M' {
            &['N']
        } else if curr == 'K' {
            if next == 'N' { &['N', 'N'] } else { &['C'] }
        } else if curr == 'S' && next == 'C' && a_next == 'H' {
            &['S', 'S', 'S']
        } else if curr == 'P' && next == 'H' {
            &['F', 'F']
        } else if (curr == 'H' && (!vowel(prev) || !vowel(next))) || (curr == 'W' && vowel(prev)) {
            &[prev]
        } else {
            &[curr]
        };
        let t = t.to_vec();
        s[i..i + t.len()].copy_from_slice(&t);
        if s[i] != s[i - 1] {
            key.push(s[i]);
        }
    }
    if key.len() > 1 {
        let mut last = *key.last().unwrap();
        if last == 'S' {
            key.pop();
            last = *key.last().unwrap();
        }
        if key.len() > 2 && key[key.len() - 2] == 'A' && last == 'Y' {
            let n = key.len();
            key.remove(n - 2);
        }
        if last == 'A' {
            key.pop();
        }
    }
    key.iter().take(6).collect()
}

/// A Caverphone rewrite step (`String.replaceAll` with the few regex shapes the algorithm uses).
enum Step {
    /// A literal replaced everywhere.
    All(&'static str, &'static str),
    /// `^lit`.
    Start(&'static str, &'static str),
    /// `lit$`.
    End(&'static str, &'static str),
    /// `^[aeiou]`.
    StartVowel(&'static str),
    /// `[aeiou]`.
    Vowels(&'static str),
    /// `c+`: runs of one character collapsed.
    Run(char, char),
}

fn apply_steps(source: &str, steps: &[Step], pad: usize) -> String {
    if source.is_empty() {
        return "1".repeat(pad);
    }
    let mut t: String = source.to_lowercase().chars().filter(|c| c.is_ascii_lowercase()).collect();
    let vowel = |c: char| matches!(c, 'a' | 'e' | 'i' | 'o' | 'u');
    for step in steps {
        t = match *step {
            Step::All(from, to) => t.replace(from, to),
            Step::Start(from, to) => match t.strip_prefix(from) {
                Some(rest) => format!("{to}{rest}"),
                None => t,
            },
            Step::End(from, to) => match t.strip_suffix(from) {
                Some(rest) => format!("{rest}{to}"),
                None => t,
            },
            Step::StartVowel(to) => match t.chars().next() {
                Some(c) if vowel(c) => format!("{to}{}", &t[1..]),
                _ => t,
            },
            Step::Vowels(to) => t.chars().map(|c| if vowel(c) { to.to_string() } else { c.to_string() }).collect(),
            Step::Run(c, to) => {
                let mut out = String::with_capacity(t.len());
                let mut prev_c = false;
                for x in t.chars() {
                    if x == c {
                        if !prev_c {
                            out.push(to);
                        }
                        prev_c = true;
                    } else {
                        out.push(x);
                        prev_c = false;
                    }
                }
                out
            }
        };
    }
    t.push_str(&"1".repeat(pad));
    t.chars().take(pad).collect()
}

/// `Caverphone1.encode`.
pub(crate) fn caverphone1(s: &str) -> String {
    use Step::*;
    const STEPS: &[Step] = &[
        Start("cough", "cou2f"), Start("rough", "rou2f"), Start("tough", "tou2f"), Start("enough", "enou2f"),
        Start("gn", "2n"), End("mb", "m2"), All("cq", "2q"), All("ci", "si"), All("ce", "se"), All("cy", "sy"),
        All("tch", "2ch"), All("c", "k"), All("q", "k"), All("x", "k"), All("v", "f"), All("dg", "2g"),
        All("tio", "sio"), All("tia", "sia"), All("d", "t"), All("ph", "fh"), All("b", "p"), All("sh", "s2"),
        All("z", "s"), StartVowel("A"), Vowels("3"), All("3gh3", "3kh3"), All("gh", "22"), All("g", "k"),
        Run('s', 'S'), Run('t', 'T'), Run('p', 'P'), Run('k', 'K'), Run('f', 'F'), Run('m', 'M'), Run('n', 'N'),
        All("w3", "W3"), All("wy", "Wy"), All("wh3", "Wh3"), All("why", "Why"), All("w", "2"), Start("h", "A"),
        All("h", "2"), All("r3", "R3"), All("ry", "Ry"), All("r", "2"), All("l3", "L3"), All("ly", "Ly"),
        All("l", "2"), All("j", "y"), All("y3", "Y3"), All("y", "2"), All("2", ""), All("3", ""),
    ];
    apply_steps(s, STEPS, 6)
}

/// `Caverphone2.encode` (the `caverphone` encoder too).
pub(crate) fn caverphone2(s: &str) -> String {
    use Step::*;
    const STEPS: &[Step] = &[
        End("e", ""), Start("cough", "cou2f"), Start("rough", "rou2f"), Start("tough", "tou2f"),
        Start("enough", "enou2f"), Start("trough", "trou2f"), Start("gn", "2n"), End("mb", "m2"),
        All("cq", "2q"), All("ci", "si"), All("ce", "se"), All("cy", "sy"), All("tch", "2ch"), All("c", "k"),
        All("q", "k"), All("x", "k"), All("v", "f"), All("dg", "2g"), All("tio", "sio"), All("tia", "sia"),
        All("d", "t"), All("ph", "fh"), All("b", "p"), All("sh", "s2"), All("z", "s"), StartVowel("A"),
        Vowels("3"), All("j", "y"), Start("y3", "Y3"), Start("y", "A"), All("y", "3"), All("3gh3", "3kh3"),
        All("gh", "22"), All("g", "k"), Run('s', 'S'), Run('t', 'T'), Run('p', 'P'), Run('k', 'K'),
        Run('f', 'F'), Run('m', 'M'), Run('n', 'N'), All("w3", "W3"), All("wh3", "Wh3"), End("w", "3"),
        All("w", "2"), Start("h", "A"), All("h", "2"), All("r3", "R3"), End("r", "3"), All("r", "2"),
        All("l3", "L3"), End("l", "3"), All("l", "2"), All("2", ""), End("3", "A"), All("3", ""),
    ];
    apply_steps(s, STEPS, 10)
}

/// `ColognePhonetic.colognePhonetic`.
pub(crate) fn cologne(text: &str) -> String {
    let input: Vec<char> = upper(text)
        .into_iter()
        .map(|c| match c {
            '\u{C4}' => 'A',
            '\u{DC}' => 'U',
            '\u{D6}' => 'O',
            c => c,
        })
        .collect();
    let mut out = String::new();
    let mut last_code = '/';
    let mut put = |code: char, out: &mut String| {
        if code != '-' && last_code != code && (code != '0' || out.is_empty()) {
            out.push(code);
        }
        last_code = code;
    };
    let mut last_char = '-';
    for (i, &chr) in input.iter().enumerate() {
        let next = input.get(i + 1).copied().unwrap_or('-');
        if !chr.is_ascii_uppercase() {
            continue;
        }
        let in_set = |set: &str, c: char| set.contains(c);
        if in_set("AEIJOUY", chr) {
            put('0', &mut out);
        } else if chr == 'B' || (chr == 'P' && next != 'H') {
            put('1', &mut out);
        } else if (chr == 'D' || chr == 'T') && !in_set("CSZ", next) {
            put('2', &mut out);
        } else if in_set("FPVW", chr) {
            put('3', &mut out);
        } else if in_set("GKQ", chr) {
            put('4', &mut out);
        } else if chr == 'X' && !in_set("CKQ", last_char) {
            put('4', &mut out);
            put('8', &mut out);
        } else if chr == 'S' || chr == 'Z' {
            put('8', &mut out);
        } else if chr == 'C' {
            let code = if out.is_empty() {
                if in_set("AHKLOQRUX", next) { '4' } else { '8' }
            } else if in_set("SZ", last_char) || !in_set("AHKOQUX", next) {
                '8'
            } else {
                '4'
            };
            put(code, &mut out);
        } else if in_set("DTX", chr) {
            put('8', &mut out);
        } else if chr == 'R' {
            put('7', &mut out);
        } else if chr == 'L' {
            put('5', &mut out);
        } else if chr == 'M' || chr == 'N' {
            put('6', &mut out);
        } else if chr == 'H' {
            put('-', &mut out);
        }
        last_char = chr;
    }
    out
}

/// A Daitch-Mokotoff rule (`DaitchMokotoffSoundex.Rule`).
struct DmRule {
    pattern: Vec<char>,
    at_start: Vec<String>,
    before_vowel: Vec<String>,
    default: Vec<String>,
}

struct DmRules {
    rules: HashMap<char, Vec<DmRule>>,
    foldings: HashMap<char, char>,
}

/// Java's `String.split` on a literal: trailing empty strings removed.
fn java_split(s: &str, sep: char) -> Vec<String> {
    let mut parts: Vec<String> = s.split(sep).map(str::to_string).collect();
    if parts.len() > 1 {
        while parts.len() > 1 && parts.last().is_some_and(|p| p.is_empty()) {
            parts.pop();
        }
    }
    parts
}

fn strip_quotes(s: &str) -> &str {
    let s = s.strip_prefix('"').unwrap_or(s);
    s.strip_suffix('"').unwrap_or(s)
}

fn dm_rules() -> &'static DmRules {
    static R: OnceLock<DmRules> = OnceLock::new();
    R.get_or_init(|| {
        let mut rules: HashMap<char, Vec<DmRule>> = HashMap::new();
        let mut foldings = HashMap::new();
        let mut in_comment = false;
        for raw in include_str!("data/dmrules.txt").lines() {
            if in_comment {
                if raw.ends_with("*/") {
                    in_comment = false;
                }
                continue;
            }
            if raw.starts_with("/*") {
                in_comment = true;
                continue;
            }
            let line = raw.split("//").next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            if let Some((l, r)) = line.split_once('=') {
                if let (Some(l), Some(r)) = (l.chars().next(), r.chars().next()) {
                    foldings.insert(l, r);
                }
                continue;
            }
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() != 4 {
                continue;
            }
            let pattern: Vec<char> = strip_quotes(parts[0]).chars().collect();
            let rule = DmRule {
                at_start: java_split(strip_quotes(parts[1]), '|'),
                before_vowel: java_split(strip_quotes(parts[2]), '|'),
                default: java_split(strip_quotes(parts[3]), '|'),
                pattern,
            };
            rules.entry(rule.pattern[0]).or_default().push(rule);
        }
        for list in rules.values_mut() {
            // A stable sort, longest patterns first.
            list.sort_by_key(|r| std::cmp::Reverse(r.pattern.len()));
        }
        DmRules { rules, foldings }
    })
}

/// `DaitchMokotoffSoundex.soundex(source)` with branching: the codes joined by `|`.
pub(crate) fn daitch_mokotoff(source: &str) -> String {
    const MAX: usize = 6;
    let r = dm_rules();
    let input: Vec<char> = source
        .chars()
        .filter(|&c| !unicode::is_whitespace(c))
        .map(|c| {
            let c = unicode::to_lower(c);
            r.foldings.get(&c).copied().unwrap_or(c)
        })
        .collect();
    struct Branch {
        code: String,
        last: Option<String>,
    }
    let mut branches = vec![Branch { code: String::new(), last: None }];
    let mut last_char = '\0';
    let mut index = 0;
    while index < input.len() {
        let ch = input[index];
        let context = &input[index..];
        let Some(rules) = r.rules.get(&ch) else {
            index += 1;
            continue;
        };
        for rule in rules {
            if !context.starts_with(&rule.pattern) {
                continue;
            }
            let replacements = if last_char == '\0' {
                &rule.at_start
            } else if context.get(rule.pattern.len()).is_some_and(|&c| matches!(c, 'a' | 'e' | 'i' | 'o' | 'u')) {
                &rule.before_vowel
            } else {
                &rule.default
            };
            let force = (last_char == 'm' && ch == 'n') || (last_char == 'n' && ch == 'm');
            let mut next: Vec<Branch> = Vec::with_capacity(branches.len() * replacements.len());
            for b in &branches {
                for rep in replacements {
                    let mut nb = Branch { code: b.code.clone(), last: b.last.clone() };
                    let append = nb.last.as_ref().is_none_or(|l| !l.ends_with(rep.as_str())) || force;
                    if append && nb.code.len() < MAX {
                        nb.code.push_str(rep);
                        nb.code.truncate(MAX);
                    }
                    nb.last = Some(rep.clone());
                    // `LinkedHashSet`: the first branch with a code wins.
                    if !next.iter().any(|x| x.code == nb.code) {
                        next.push(nb);
                    }
                }
            }
            branches = next;
            index += rule.pattern.len() - 1;
            break;
        }
        last_char = ch;
        index += 1;
    }
    let mut out = String::new();
    for (i, b) in branches.iter().enumerate() {
        if i > 0 {
            out.push('|');
        }
        out.push_str(&b.code);
        for _ in b.code.len()..MAX {
            out.push('0');
        }
    }
    out
}
