//! Metaphone and Double Metaphone, ported from Apache Commons Codec 1.15's `Metaphone` and
//! `DoubleMetaphone` (Apache-2.0), which Elasticsearch's `phonetic` filter runs.
//!
//! Strings are handled as `char` vectors: the encoders only look at ASCII letters, and Java's
//! UTF-16 indexing differs only for supplementary characters.

/// Java's `String.toUpperCase(Locale.ENGLISH)` as chars.
pub(super) fn upper(s: &str) -> Vec<char> {
    s.to_uppercase().chars().collect()
}

/// `Metaphone.metaphone` with the default maximum code length of 4.
pub(crate) fn metaphone(txt: &str) -> String {
    const MAX: usize = 4;
    if txt.is_empty() {
        return String::new();
    }
    let inwd = upper(txt);
    if txt.encode_utf16().count() == 1 {
        return inwd.iter().collect();
    }
    if inwd.len() < 2 {
        return inwd.iter().collect();
    }
    let mut local: Vec<char> = Vec::with_capacity(inwd.len());
    match inwd[0] {
        'K' | 'G' | 'P' | 'A' => {
            let second = if inwd[0] == 'A' { 'E' } else { 'N' };
            if inwd[1] == second {
                local.extend_from_slice(&inwd[1..]);
            } else {
                local.extend_from_slice(&inwd);
            }
        }
        'W' => {
            if inwd[1] == 'R' {
                local.extend_from_slice(&inwd[1..]);
            } else if inwd[1] == 'H' {
                local.extend_from_slice(&inwd[1..]);
                local[0] = 'W';
            } else {
                local.extend_from_slice(&inwd);
            }
        }
        'X' => {
            local.push('S');
            local.extend_from_slice(&inwd[1..]);
        }
        _ => local.extend_from_slice(&inwd),
    }
    let w = &local;
    let wdsz = w.len();
    let at = |i: usize| -> char { w.get(i).copied().unwrap_or('\0') };
    let is_vowel = |i: usize| matches!(at(i), 'A' | 'E' | 'I' | 'O' | 'U');
    let frontv = |c: char| matches!(c, 'E' | 'I' | 'Y');
    let prev_is = |i: usize, c: char| i > 0 && i < wdsz && w[i - 1] == c;
    let next_is = |i: usize, c: char| i + 1 < wdsz && w[i + 1] == c;
    let region = |i: usize, t: &str| {
        let t: Vec<char> = t.chars().collect();
        i + t.len() <= wdsz && w[i..i + t.len()] == t[..]
    };
    let last = |n: usize| n + 1 == wdsz;
    let mut code = String::with_capacity(8);
    let mut n = 0;
    while code.len() < MAX && n < wdsz {
        let symb = w[n];
        if symb != 'C' && prev_is(n, symb) {
            n += 1;
        } else {
            match symb {
                'A' | 'E' | 'I' | 'O' | 'U' => {
                    if n == 0 {
                        code.push(symb);
                    }
                }
                'B' => {
                    if !(prev_is(n, 'M') && last(n)) {
                        code.push(symb);
                    }
                }
                'C' => {
                    if prev_is(n, 'S') && !last(n) && frontv(w[n + 1]) {
                    } else if region(n, "CIA") {
                        code.push('X');
                    } else if !last(n) && frontv(w[n + 1]) {
                        code.push('S');
                    } else if prev_is(n, 'S') && next_is(n, 'H') {
                        code.push('K');
                    } else if next_is(n, 'H') {
                        if n == 0 && wdsz >= 3 && is_vowel(2) {
                            code.push('K');
                        } else {
                            code.push('X');
                        }
                    } else {
                        code.push('K');
                    }
                }
                'D' => {
                    if !last(n + 1) && next_is(n, 'G') && frontv(w[n + 2]) {
                        code.push('J');
                        n += 2;
                    } else {
                        code.push('T');
                    }
                }
                'G' => {
                    let silent = (last(n + 1) && next_is(n, 'H'))
                        || (!last(n + 1) && next_is(n, 'H') && !is_vowel(n + 2))
                        || (n > 0 && (region(n, "GN") || region(n, "GNED")));
                    if !silent {
                        let hard = prev_is(n, 'G');
                        if !last(n) && frontv(w[n + 1]) && !hard {
                            code.push('J');
                        } else {
                            code.push('K');
                        }
                    }
                }
                'H' => {
                    if !last(n)
                        && !(n > 0 && matches!(w[n - 1], 'C' | 'S' | 'P' | 'T' | 'G'))
                        && is_vowel(n + 1)
                    {
                        code.push('H');
                    }
                }
                'F' | 'J' | 'L' | 'M' | 'N' | 'R' => code.push(symb),
                'K' => {
                    if n == 0 || !prev_is(n, 'C') {
                        code.push(symb);
                    }
                }
                'P' => code.push(if next_is(n, 'H') { 'F' } else { 'P' }),
                'Q' => code.push('K'),
                'S' => {
                    if region(n, "SH") || region(n, "SIO") || region(n, "SIA") {
                        code.push('X');
                    } else {
                        code.push('S');
                    }
                }
                'T' => {
                    if region(n, "TIA") || region(n, "TIO") {
                        code.push('X');
                    } else if region(n, "TCH") {
                    } else if region(n, "TH") {
                        code.push('0');
                    } else {
                        code.push('T');
                    }
                }
                'V' => code.push('F'),
                'W' | 'Y' => {
                    if !last(n) && is_vowel(n + 1) {
                        code.push(symb);
                    }
                }
                'X' => code.push_str("KS"),
                'Z' => code.push('S'),
                _ => {}
            }
            n += 1;
        }
        if code.len() > MAX {
            code.truncate(MAX);
        }
    }
    code
}

/// `DoubleMetaphone.doubleMetaphone(value, alternate)`; None for blank input.
pub(crate) fn double_metaphone(value: &str, alternate: bool, max: usize) -> Option<String> {
    let trimmed = value.trim_matches(|c: char| c <= ' ');
    if trimmed.is_empty() {
        return None;
    }
    let v = Dm { s: upper(trimmed) };
    let slavo = v.slavo_germanic();
    let mut r = DmResult {
        primary: String::new(),
        alternate: String::new(),
        max,
    };
    let len = v.s.len() as isize;
    let mut i: isize = if ["GN", "KN", "PN", "WR", "PS"].iter().any(|p| v.starts_with(p)) { 1 } else { 0 };
    while !r.complete() && i < len {
        i = match v.at(i) {
            'A' | 'E' | 'I' | 'O' | 'U' | 'Y' => {
                if i == 0 {
                    r.add('A');
                }
                i + 1
            }
            'B' => {
                r.add('P');
                v.skip_double(i, 'B')
            }
            '\u{C7}' => {
                r.add('S');
                i + 1
            }
            'C' => v.handle_c(&mut r, i),
            'D' => v.handle_d(&mut r, i),
            'F' => {
                r.add('F');
                v.skip_double(i, 'F')
            }
            'G' => v.handle_g(&mut r, i, slavo),
            'H' => v.handle_h(&mut r, i),
            'J' => v.handle_j(&mut r, i, slavo),
            'K' => {
                r.add('K');
                v.skip_double(i, 'K')
            }
            'L' => v.handle_l(&mut r, i),
            'M' => {
                r.add('M');
                if v.condition_m0(i) { i + 2 } else { i + 1 }
            }
            'N' => {
                r.add('N');
                v.skip_double(i, 'N')
            }
            '\u{D1}' => {
                r.add('N');
                i + 1
            }
            'P' => v.handle_p(&mut r, i),
            'Q' => {
                r.add('K');
                v.skip_double(i, 'Q')
            }
            'R' => v.handle_r(&mut r, i, slavo),
            'S' => v.handle_s(&mut r, i, slavo),
            'T' => v.handle_t(&mut r, i),
            'V' => {
                r.add('F');
                v.skip_double(i, 'V')
            }
            'W' => v.handle_w(&mut r, i),
            'X' => v.handle_x(&mut r, i),
            'Z' => v.handle_z(&mut r, i, slavo),
            _ => i + 1,
        };
    }
    Some(if alternate { r.alternate } else { r.primary })
}

struct DmResult {
    primary: String,
    alternate: String,
    max: usize,
}

impl DmResult {
    fn add(&mut self, c: char) {
        self.add_primary(c);
        self.add_alternate(c);
    }
    fn add2(&mut self, p: char, a: char) {
        self.add_primary(p);
        self.add_alternate(a);
    }
    fn add_primary(&mut self, c: char) {
        if self.primary.chars().count() < self.max {
            self.primary.push(c);
        }
    }
    fn add_alternate(&mut self, c: char) {
        if self.alternate.chars().count() < self.max {
            self.alternate.push(c);
        }
    }
    fn adds(&mut self, s: &str) {
        self.adds2(s, s);
    }
    fn adds2(&mut self, p: &str, a: &str) {
        for (buf, v) in [(&mut self.primary, p), (&mut self.alternate, a)] {
            let room = self.max.saturating_sub(buf.chars().count());
            buf.extend(v.chars().take(room));
        }
    }
    fn complete(&self) -> bool {
        self.primary.chars().count() >= self.max && self.alternate.chars().count() >= self.max
    }
}

struct Dm {
    s: Vec<char>,
}

impl Dm {
    fn len(&self) -> isize {
        self.s.len() as isize
    }

    /// `charAt`: `Character.MIN_VALUE` outside the string.
    fn at(&self, i: isize) -> char {
        if i < 0 || i >= self.len() { '\0' } else { self.s[i as usize] }
    }

    fn vowel(c: char) -> bool {
        matches!(c, 'A' | 'E' | 'I' | 'O' | 'U' | 'Y')
    }

    /// `contains(value, start, length, criteria...)`.
    fn has(&self, start: isize, len: isize, criteria: &[&str]) -> bool {
        if start < 0 || start + len > self.len() {
            return false;
        }
        let target = &self.s[start as usize..(start + len) as usize];
        criteria
            .iter()
            .any(|c| c.chars().count() == target.len() && c.chars().zip(target).all(|(a, &b)| a == b))
    }

    fn starts_with(&self, p: &str) -> bool {
        self.has(0, p.chars().count() as isize, &[p])
    }

    fn index_of(&self, p: &str) -> bool {
        let p: Vec<char> = p.chars().collect();
        self.s.windows(p.len()).any(|w| w == &p[..])
    }

    fn slavo_germanic(&self) -> bool {
        self.s.contains(&'W') || self.s.contains(&'K') || self.index_of("CZ") || self.index_of("WITZ")
    }

    fn skip_double(&self, i: isize, c: char) -> isize {
        if self.at(i + 1) == c { i + 2 } else { i + 1 }
    }

    fn handle_c(&self, r: &mut DmResult, mut i: isize) -> isize {
        if self.condition_c0(i) {
            r.add('K');
            i += 2;
        } else if i == 0 && self.has(i, 6, &["CAESAR"]) {
            r.add('S');
            i += 2;
        } else if self.has(i, 2, &["CH"]) {
            i = self.handle_ch(r, i);
        } else if self.has(i, 2, &["CZ"]) && !self.has(i - 2, 4, &["WICZ"]) {
            r.add2('S', 'X');
            i += 2;
        } else if self.has(i + 1, 3, &["CIA"]) {
            r.add('X');
            i += 3;
        } else if self.has(i, 2, &["CC"]) && !(i == 1 && self.at(0) == 'M') {
            return self.handle_cc(r, i);
        } else if self.has(i, 2, &["CK", "CG", "CQ"]) {
            r.add('K');
            i += 2;
        } else if self.has(i, 2, &["CI", "CE", "CY"]) {
            if self.has(i, 3, &["CIO", "CIE", "CIA"]) {
                r.add2('S', 'X');
            } else {
                r.add('S');
            }
            i += 2;
        } else {
            r.add('K');
            if self.has(i + 1, 2, &[" C", " Q", " G"]) {
                i += 3;
            } else if self.has(i + 1, 1, &["C", "K", "Q"]) && !self.has(i + 1, 2, &["CE", "CI"]) {
                i += 2;
            } else {
                i += 1;
            }
        }
        i
    }

    fn handle_cc(&self, r: &mut DmResult, i: isize) -> isize {
        if self.has(i + 2, 1, &["I", "E", "H"]) && !self.has(i + 2, 2, &["HU"]) {
            if (i == 1 && self.at(i - 1) == 'A') || self.has(i - 1, 5, &["UCCEE", "UCCES"]) {
                r.adds("KS");
            } else {
                r.add('X');
            }
            i + 3
        } else {
            r.add('K');
            i + 2
        }
    }

    fn handle_ch(&self, r: &mut DmResult, i: isize) -> isize {
        if i > 0 && self.has(i, 4, &["CHAE"]) {
            r.add2('K', 'X');
        } else if self.condition_ch0(i) || self.condition_ch1(i) {
            r.add('K');
        } else if i > 0 {
            if self.has(0, 2, &["MC"]) {
                r.add('K');
            } else {
                r.add2('X', 'K');
            }
        } else {
            r.add('X');
        }
        i + 2
    }

    fn handle_d(&self, r: &mut DmResult, i: isize) -> isize {
        if self.has(i, 2, &["DG"]) {
            if self.has(i + 2, 1, &["I", "E", "Y"]) {
                r.add('J');
                i + 3
            } else {
                r.adds("TK");
                i + 2
            }
        } else if self.has(i, 2, &["DT", "DD"]) {
            r.add('T');
            i + 2
        } else {
            r.add('T');
            i + 1
        }
    }

    fn handle_g(&self, r: &mut DmResult, i: isize, slavo: bool) -> isize {
        if self.at(i + 1) == 'H' {
            return self.handle_gh(r, i);
        }
        if self.at(i + 1) == 'N' {
            if i == 1 && Self::vowel(self.at(0)) && !slavo {
                r.adds2("KN", "N");
            } else if !self.has(i + 2, 2, &["EY"]) && self.at(i + 1) != 'Y' && !slavo {
                r.adds2("N", "KN");
            } else {
                r.adds("KN");
            }
            return i + 2;
        }
        if self.has(i + 1, 2, &["LI"]) && !slavo {
            r.adds2("KL", "L");
            return i + 2;
        }
        if i == 0
            && (self.at(i + 1) == 'Y'
                || self.has(i + 1, 2, &["ES", "EP", "EB", "EL", "EY", "IB", "IL", "IN", "IE", "EI", "ER"]))
        {
            r.add2('K', 'J');
            return i + 2;
        }
        if (self.has(i + 1, 2, &["ER"]) || self.at(i + 1) == 'Y')
            && !self.has(0, 6, &["DANGER", "RANGER", "MANGER"])
            && !self.has(i - 1, 1, &["E", "I"])
            && !self.has(i - 1, 3, &["RGY", "OGY"])
        {
            r.add2('K', 'J');
            return i + 2;
        }
        if self.has(i + 1, 1, &["E", "I", "Y"]) || self.has(i - 1, 4, &["AGGI", "OGGI"]) {
            if self.has(0, 4, &["VAN ", "VON "]) || self.has(0, 3, &["SCH"]) || self.has(i + 1, 2, &["ET"]) {
                r.add('K');
            } else if self.has(i + 1, 3, &["IER"]) {
                r.add('J');
            } else {
                r.add2('J', 'K');
            }
            return i + 2;
        }
        if self.at(i + 1) == 'G' {
            r.add('K');
            return i + 2;
        }
        r.add('K');
        i + 1
    }

    fn handle_gh(&self, r: &mut DmResult, i: isize) -> isize {
        if i > 0 && !Self::vowel(self.at(i - 1)) {
            r.add('K');
        } else if i == 0 {
            r.add(if self.at(i + 2) == 'I' { 'J' } else { 'K' });
        } else if (i > 1 && self.has(i - 2, 1, &["B", "H", "D"]))
            || (i > 2 && self.has(i - 3, 1, &["B", "H", "D"]))
            || (i > 3 && self.has(i - 4, 1, &["B", "H"]))
        {
        } else if i > 2 && self.at(i - 1) == 'U' && self.has(i - 3, 1, &["C", "G", "L", "R", "T"]) {
            r.add('F');
        } else if i > 0 && self.at(i - 1) != 'I' {
            r.add('K');
        }
        i + 2
    }

    fn handle_h(&self, r: &mut DmResult, i: isize) -> isize {
        if (i == 0 || Self::vowel(self.at(i - 1))) && Self::vowel(self.at(i + 1)) {
            r.add('H');
            i + 2
        } else {
            i + 1
        }
    }

    fn handle_j(&self, r: &mut DmResult, i: isize, slavo: bool) -> isize {
        if self.has(i, 4, &["JOSE"]) || self.has(0, 4, &["SAN "]) {
            if (i == 0 && self.at(i + 4) == ' ') || self.len() == 4 || self.has(0, 4, &["SAN "]) {
                r.add('H');
            } else {
                r.add2('J', 'H');
            }
            return i + 1;
        }
        if i == 0 && !self.has(i, 4, &["JOSE"]) {
            r.add2('J', 'A');
        } else if Self::vowel(self.at(i - 1)) && !slavo && matches!(self.at(i + 1), 'A' | 'O') {
            r.add2('J', 'H');
        } else if i == self.len() - 1 {
            r.add2('J', ' ');
        } else if !self.has(i + 1, 1, &["L", "T", "K", "S", "N", "M", "B", "Z"])
            && !self.has(i - 1, 1, &["S", "K", "L"])
        {
            r.add('J');
        }
        self.skip_double(i, 'J')
    }

    fn handle_l(&self, r: &mut DmResult, i: isize) -> isize {
        if self.at(i + 1) == 'L' {
            if self.condition_l0(i) {
                r.add_primary('L');
            } else {
                r.add('L');
            }
            i + 2
        } else {
            r.add('L');
            i + 1
        }
    }

    fn handle_p(&self, r: &mut DmResult, i: isize) -> isize {
        if self.at(i + 1) == 'H' {
            r.add('F');
            i + 2
        } else {
            r.add('P');
            if self.has(i + 1, 1, &["P", "B"]) { i + 2 } else { i + 1 }
        }
    }

    fn handle_r(&self, r: &mut DmResult, i: isize, slavo: bool) -> isize {
        if i == self.len() - 1 && !slavo && self.has(i - 2, 2, &["IE"]) && !self.has(i - 4, 2, &["ME", "MA"]) {
            r.add_alternate('R');
        } else {
            r.add('R');
        }
        self.skip_double(i, 'R')
    }

    fn handle_s(&self, r: &mut DmResult, i: isize, slavo: bool) -> isize {
        if self.has(i - 1, 3, &["ISL", "YSL"]) {
            i + 1
        } else if i == 0 && self.has(i, 5, &["SUGAR"]) {
            r.add2('X', 'S');
            i + 1
        } else if self.has(i, 2, &["SH"]) {
            if self.has(i + 1, 4, &["HEIM", "HOEK", "HOLM", "HOLZ"]) {
                r.add('S');
            } else {
                r.add('X');
            }
            i + 2
        } else if self.has(i, 3, &["SIO", "SIA"]) || self.has(i, 4, &["SIAN"]) {
            if slavo {
                r.add('S');
            } else {
                r.add2('S', 'X');
            }
            i + 3
        } else if (i == 0 && self.has(i + 1, 1, &["M", "N", "L", "W"])) || self.has(i + 1, 1, &["Z"]) {
            r.add2('S', 'X');
            if self.has(i + 1, 1, &["Z"]) { i + 2 } else { i + 1 }
        } else if self.has(i, 2, &["SC"]) {
            self.handle_sc(r, i)
        } else {
            if i == self.len() - 1 && self.has(i - 2, 2, &["AI", "OI"]) {
                r.add_alternate('S');
            } else {
                r.add('S');
            }
            if self.has(i + 1, 1, &["S", "Z"]) { i + 2 } else { i + 1 }
        }
    }

    fn handle_sc(&self, r: &mut DmResult, i: isize) -> isize {
        if self.at(i + 2) == 'H' {
            if self.has(i + 3, 2, &["OO", "ER", "EN", "UY", "ED", "EM"]) {
                if self.has(i + 3, 2, &["ER", "EN"]) {
                    r.adds2("X", "SK");
                } else {
                    r.adds("SK");
                }
            } else if i == 0 && !Self::vowel(self.at(3)) && self.at(3) != 'W' {
                r.add2('X', 'S');
            } else {
                r.add('X');
            }
        } else if self.has(i + 2, 1, &["I", "E", "Y"]) {
            r.add('S');
        } else {
            r.adds("SK");
        }
        i + 3
    }

    fn handle_t(&self, r: &mut DmResult, i: isize) -> isize {
        if self.has(i, 4, &["TION"]) || self.has(i, 3, &["TIA", "TCH"]) {
            r.add('X');
            i + 3
        } else if self.has(i, 2, &["TH"]) || self.has(i, 3, &["TTH"]) {
            if self.has(i + 2, 2, &["OM", "AM"]) || self.has(0, 4, &["VAN ", "VON "]) || self.has(0, 3, &["SCH"]) {
                r.add('T');
            } else {
                r.add2('0', 'T');
            }
            i + 2
        } else {
            r.add('T');
            if self.has(i + 1, 1, &["T", "D"]) { i + 2 } else { i + 1 }
        }
    }

    fn handle_w(&self, r: &mut DmResult, i: isize) -> isize {
        if self.has(i, 2, &["WR"]) {
            r.add('R');
            return i + 2;
        }
        if i == 0 && (Self::vowel(self.at(i + 1)) || self.has(i, 2, &["WH"])) {
            if Self::vowel(self.at(i + 1)) {
                r.add2('A', 'F');
            } else {
                r.add('A');
            }
            i + 1
        } else if (i == self.len() - 1 && Self::vowel(self.at(i - 1)))
            || self.has(i - 1, 5, &["EWSKI", "EWSKY", "OWSKI", "OWSKY"])
            || self.has(0, 3, &["SCH"])
        {
            r.add_alternate('F');
            i + 1
        } else if self.has(i, 4, &["WICZ", "WITZ"]) {
            r.adds2("TS", "FX");
            i + 4
        } else {
            i + 1
        }
    }

    fn handle_x(&self, r: &mut DmResult, i: isize) -> isize {
        if i == 0 {
            r.add('S');
            return i + 1;
        }
        if !(i == self.len() - 1 && (self.has(i - 3, 3, &["IAU", "EAU"]) || self.has(i - 2, 2, &["AU", "OU"]))) {
            r.adds("KS");
        }
        if self.has(i + 1, 1, &["C", "X"]) { i + 2 } else { i + 1 }
    }

    fn handle_z(&self, r: &mut DmResult, i: isize, slavo: bool) -> isize {
        if self.at(i + 1) == 'H' {
            r.add('J');
            return i + 2;
        }
        if self.has(i + 1, 2, &["ZO", "ZI", "ZA"]) || (slavo && i > 0 && self.at(i - 1) != 'T') {
            r.adds2("S", "TS");
        } else {
            r.add('S');
        }
        self.skip_double(i, 'Z')
    }

    fn condition_c0(&self, i: isize) -> bool {
        if self.has(i, 4, &["CHIA"]) {
            true
        } else if i <= 1 || Self::vowel(self.at(i - 2)) || !self.has(i - 1, 3, &["ACH"]) {
            false
        } else {
            let c = self.at(i + 2);
            (c != 'I' && c != 'E') || self.has(i - 2, 6, &["BACHER", "MACHER"])
        }
    }

    fn condition_ch0(&self, i: isize) -> bool {
        i == 0
            && (self.has(i + 1, 5, &["HARAC", "HARIS"]) || self.has(i + 1, 3, &["HOR", "HYM", "HIA", "HEM"]))
            && !self.has(0, 5, &["CHORE"])
    }

    fn condition_ch1(&self, i: isize) -> bool {
        self.has(0, 4, &["VAN ", "VON "])
            || self.has(0, 3, &["SCH"])
            || self.has(i - 2, 6, &["ORCHES", "ARCHIT", "ORCHID"])
            || self.has(i + 2, 1, &["T", "S"])
            || ((self.has(i - 1, 1, &["A", "O", "U", "E"]) || i == 0)
                && (self.has(i + 2, 1, &["L", "R", "N", "M", "B", "H", "F", "V", "W", " "]) || i + 1 == self.len() - 1))
    }

    fn condition_l0(&self, i: isize) -> bool {
        let n = self.len();
        (i == n - 3 && self.has(i - 1, 4, &["ILLO", "ILLA", "ALLE"]))
            || ((self.has(n - 2, 2, &["AS", "OS"]) || self.has(n - 1, 1, &["A", "O"])) && self.has(i - 1, 4, &["ALLE"]))
    }

    fn condition_m0(&self, i: isize) -> bool {
        self.at(i + 1) == 'M' || (self.has(i - 1, 3, &["UMB"]) && (i + 1 == self.len() - 1 || self.has(i + 2, 2, &["ER"])))
    }
}
