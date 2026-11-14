//! Elasticsearch's own German encoders, `koelnerphonetik` and `haasephonetik`
//! (`KoelnerPhonetik` and its subclass `HaasePhonetik` in the `analysis-phonetic` plugin): the
//! Kölner Phonetik over every partition of the input and its spelling variations, codes joined
//! by `_`.

use crate::unicode;

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
        while !tmp.is_empty() {
            let mut part = String::new();
            for (i, t) in tmp.iter().enumerate() {
                part.push_str(t);
                if i + 1 != n {
                    parts.push(part.clone());
                }
            }
            tmp.remove(0);
        }
        parts.iter().flat_map(|p| self.variations(p)).collect()
    }

    fn variations(self, s: &str) -> Vec<String> {
        let (patterns, replacements) = self.tables();
        let mut position = 0;
        let mut vars = vec![String::new()];
        while position < s.len() {
            let mut i = 0;
            let mut subst: Option<usize> = None;
            while subst.is_none_or(|p| p < position) && i < patterns.len() {
                for start in find_all(s, patterns[i]) {
                    if subst.is_some_and(|p| p >= position) {
                        break;
                    }
                    subst = Some(start);
                }
                i += 1;
            }
            match subst {
                Some(sp) if sp >= position => {
                    let i = i - 1;
                    let prev = &s[position..sp];
                    let mut new = Vec::with_capacity(vars.len());
                    for v in vars.iter_mut() {
                        new.push(format!("{v}{prev}{}", replacements[i]));
                        v.push_str(prev);
                        // The Java code appends the pattern's source and skips its length,
                        // anchors included.
                        v.push_str(patterns[i]);
                    }
                    vars.extend(new);
                    position = sp + patterns[i].len();
                }
                _ => {
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
