//! Elasticsearch's own German encoders, `koelnerphonetik` and `haasephonetik`
//! (`KoelnerPhonetik` and its subclass `HaasePhonetik` in the `analysis-phonetic` plugin): the
//! Kölner Phonetik over every partition of the input and its spelling variations, codes joined
//! by `_`.
//!
//! Each matched variation pattern doubles the spellings and each separator adds partitions, so
//! a token has at most `MAX_CODES` codes: past it, partitions are dropped and patterns keep their
//! original spelling (Elasticsearch has no limit; docs/search/compatibility.md).

use crate::unicode;

/// The most codes one token encodes to (ordinary names have a handful).
const MAX_CODES: usize = 256;

/// Which variant: the patterns, replacements and vowel code differ.
#[derive(Clone, Copy)]
pub(crate) enum German {
    Koelner,
    Haase,
}

const KOELNER: (&[&str], &[&str]) = (
    &["AUN", "OWN", "RB", "RW", "WSK", "RSK"],
    &["OWN", "AUN", "RW", "RB", "RSK", "WSK"],
);
const HAASE: (&[&str], &[&str]) = (
    &["OWN", "RB", "WSK", "A$", "O$", "SCH", "GLI", "EAU$", "^CH", "AUX", "EUX", "ILLE"],
    &["AUN", "RW", "RSK", "AR", "OW", "CH", "LI", "O", "SCH", "O", "O", "I"],
);

impl German {
    fn tables(self) -> (&'static [&'static str], &'static [&'static str]) {
        match self {
            German::Koelner => KOELNER,
            German::Haase => HAASE,
        }
    }

    fn vowel_code(self) -> char {
        match self {
            German::Koelner => '0',
            German::Haase => '9',
        }
    }

    pub(crate) fn encode(self, s: &str) -> String {
        let mut out = String::new();
        for (i, part) in self.partition(s).iter().enumerate() {
            if i > 0 {
                out.push('_');
            }
            out.push_str(&self.substitute(part));
        }
        out
    }

    fn partition(self, s: &str) -> Vec<String> {
        // `[^\p{L}\p{N}]` removed, then every run of the pieces between separators
        // (`[\p{Z}\p{C}\p{P}]`), as the Java loop builds them.
        let mut parts: Vec<String> = vec![s.chars().filter(|&c| is_letter_or_number(c)).collect()];
        let mut tmp: Vec<String> = s.split(is_separator).map(str::to_string).collect();
        while tmp.len() > 1 && tmp.last().is_some_and(|p| p.is_empty()) {
            tmp.pop();
        }
        if tmp.len() == 1 && tmp[0].is_empty() && !s.is_empty() {
            tmp.clear();
        }
        let n = tmp.len();
        'parts: for first in 0..n {
            let mut part = String::new();
            for (i, t) in tmp[first..].iter().enumerate() {
                part.push_str(t);
                // Java compares the index in the shrinking list with the first list's length.
                if i + 1 != n {
                    if parts.len() == MAX_CODES {
                        break 'parts;
                    }
                    parts.push(part.clone());
                }
            }
        }
        // Every part keeps at least its own spelling; the rest of the budget goes to the
        // variations of the first parts.
        let mut out = Vec::new();
        for (k, p) in parts.iter().enumerate() {
            let room = MAX_CODES - out.len() - (parts.len() - k - 1);
            out.extend(self.variations(p, room));
        }
        out
    }

    /// At most `room` (at least 1) spellings: once doubling would pass it, a pattern keeps its
    /// original spelling only.
    fn variations(self, s: &str, room: usize) -> Vec<String> {
        let (patterns, replacements) = self.tables();
        // `Matcher.find` from the start, without overlaps, for each pattern.
        let found: Vec<Vec<usize>> = patterns.iter().map(|p| find_all(s, p)).collect();
        let mut position = 0;
        let mut vars = vec![String::new()];
        while position < s.len() {
            // The first pattern (in order) with a match at or after `position`.
            let next = found.iter().enumerate().find_map(|(i, f)| {
                let k = f.partition_point(|&m| m < position);
                f.get(k).map(|&m| (i, m))
            });
            match next {
                Some((i, sp)) => {
                    let prev = &s[position..sp];
                    let double = vars.len() * 2 <= room;
                    let mut new = Vec::with_capacity(if double { vars.len() } else { 0 });
                    for v in vars.iter_mut() {
                        if double {
                            new.push(format!("{v}{prev}{}", replacements[i]));
                        }
                        v.push_str(prev);
                        // The Java code appends the pattern's source and skips its length,
                        // anchors included.
                        v.push_str(patterns[i]);
                    }
                    vars.extend(new);
                    position = sp + patterns[i].len();
                }
                None => {
                    for v in vars.iter_mut() {
                        v.push_str(&s[position..]);
                    }
                    position = s.len();
                }
            }
        }
        vars
    }

    fn substitute(self, part: &str) -> String {
        let s = remove_sequences(&expand_umlauts(&part.to_uppercase()));
        let s: Vec<char> = s.chars().collect();
        let mut sb: Vec<char> = Vec::with_capacity(s.len() * 2);
        for i in 0..s.len() {
            let current = s[i];
            let next = s.get(i + 1).copied().unwrap_or('_');
            let prev = if i > 0 { s[i - 1] } else { '_' };
            match current {
                'A' | 'E' | 'I' | 'J' | 'Y' | 'O' | 'U' => {
                    if i == 0 || (i == 1 && prev == 'H') {
                        sb.push(self.vowel_code());
                    }
                }
                'P' => {
                    if next == 'H' {
                        sb.extend(['3', '3']);
                    } else {
                        sb.push('1');
                    }
                }
                'B' => sb.push('1'),
                'D' | 'T' => sb.push(if matches!(next, 'C' | 'S' | 'Z') { '8' } else { '2' }),
                'F' | 'V' | 'W' => sb.push('3'),
                'G' | 'K' | 'Q' => sb.push('4'),
                'C' => {
                    let set = if i == 0 { "AHKLOQRUX" } else { "AOUHKXQ" };
                    sb.push(if set.contains(next) { '4' } else { '8' });
                    if sb.len() >= 2 && sb[sb.len() - 2] == '8' {
                        let n = sb.len();
                        sb[n - 1] = '8';
                    }
                }
                'X' => {
                    if i < 1 || !matches!(prev, 'C' | 'K' | 'Q') {
                        sb.extend(['4', '8']);
                    } else {
                        sb.push('8');
                    }
                }
                'L' => sb.push('5'),
                'M' | 'N' => sb.push('6'),
                'R' => sb.push('7'),
                'S' | 'Z' => sb.push('8'),
                _ => {}
            }
        }
        remove_sequences(&sb.into_iter().collect::<String>())
    }
}

/// Start offsets of a variation pattern's matches (`Matcher.find` from the start, without
/// overlaps): a literal, optionally anchored with `^` or `$`.
fn find_all(s: &str, pattern: &str) -> Vec<usize> {
    let start = pattern.starts_with('^');
    let end = pattern.ends_with('$');
    let lit = pattern.trim_start_matches('^').trim_end_matches('$');
    match (start, end) {
        (true, _) => s.starts_with(lit).then_some(0).into_iter().collect(),
        (false, true) => s.ends_with(lit).then(|| s.len() - lit.len()).into_iter().collect(),
        _ => s.match_indices(lit).map(|(i, _)| i).collect(),
    }
}

fn is_letter_or_number(c: char) -> bool {
    // Java's `\p{L}` and `\p{N}` by `Character.getType`.
    matches!(unicode::get_type(c), 1..=5 | 9..=11)
}

fn is_separator(c: char) -> bool {
    // `\p{Z}` (12-14), `\p{C}` (0 unassigned, 15 control, 16 format, 18 private use, 19
    // surrogate) and `\p{P}` (20-24, 29, 30).
    matches!(unicode::get_type(c), 0 | 12..=16 | 18 | 19 | 20..=24 | 29 | 30)
}

fn expand_umlauts(s: &str) -> String {
    s.replace('\u{C4}', "AE").replace('\u{D6}', "OE").replace('\u{DC}', "UE")
}

fn remove_sequences(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last = None;
    for c in s.chars() {
        if Some(c) != last {
            out.push(c);
            last = Some(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn variations_and_partitions_are_capped() {
        // Each `RB` doubles the spellings, each separator adds partitions.
        for word in ["RB".repeat(127), "a.".repeat(5000), "RB.".repeat(2000)] {
            for g in [German::Koelner, German::Haase] {
                assert!(g.encode(&word).split('_').count() <= MAX_CODES);
            }
        }
        // Up to the cap, every spelling is there.
        assert_eq!(German::Koelner.encode(&"RB".repeat(8)).split('_').count(), 256);
    }
}
