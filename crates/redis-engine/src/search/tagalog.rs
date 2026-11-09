//! The Tagalog stemmer RediSearch adds to libstemmer (`deps/stemmers`, a Snowball program): up
//! to two prefixes, partial reduplication, the `um`/`in` infixes, one suffix (`-han`, `-hin`,
//! `-in`, `-an`, a final `-ng`/`-g`), then the final `u`, `r`, `h` and a doubled first letter
//! normalized. Reproduced on Snowball's machine (a cursor, limits and a bracketed slice over the
//! UTF-8 bytes, as libstemmer's C runtime runs it).

/// The machine's state (`struct SN_env`).
struct Env {
    p: Vec<u8>,
    c: usize,
    l: usize,
    lb: usize,
    bra: usize,
    ket: usize,
    removed_prefix: bool,
    removed_infix: bool,
    removed_duplication: bool,
    removed_suffix: bool,
    likely_foreign: bool,
}

const VOWELS: &[u8] = b"aeiou";

impl Env {
    /// The character at the cursor and its width (`get_utf8`).
    fn ch_fwd(&self) -> Option<(u32, usize)> {
        if self.c >= self.l {
            return None;
        }
        let s = &self.p[self.c..self.l];
        let b0 = s[0] as u32;
        let at = |i: usize| s.get(i).map_or(0, |&b| (b & 0x3f) as u32);
        if b0 < 0xc0 || s.len() == 1 {
            return Some((b0, 1));
        }
        if b0 < 0xe0 || s.len() == 2 {
            return Some((((b0 & 0x1f) << 6) | at(1), 2));
        }
        if b0 < 0xf0 || s.len() == 3 {
            return Some((((b0 & 0x0f) << 12) | (at(1) << 6) | at(2), 3));
        }
        Some((((b0 & 0x0e) << 18) | (at(1) << 12) | (at(2) << 6) | at(3), 4))
    }

    /// The character before the cursor and its width (`get_b_utf8`).
    fn ch_bwd(&self) -> Option<(u32, usize)> {
        if self.c <= self.lb {
            return None;
        }
        let s = &self.p[self.lb..self.c];
        let n = s.len();
        let b = |i: usize| s[n - i] as u32;
        let b1 = b(1);
        if b1 < 0x80 || n == 1 {
            return Some((b1, 1));
        }
        let b2 = b(2);
        if b2 >= 0xc0 || n == 2 {
            return Some((((b2 & 0x1f) << 6) | (b1 & 0x3f), 2));
        }
        let b3 = b(3);
        if b3 >= 0xe0 || n == 3 {
            return Some((((b3 & 0x0f) << 12) | ((b2 & 0x3f) << 6) | (b1 & 0x3f), 3));
        }
        let b4 = b(4);
        Some((((b4 & 0x0e) << 18) | ((b3 & 0x3f) << 12) | ((b2 & 0x3f) << 6) | (b1 & 0x3f), 4))
    }

    fn is_vowel(ch: u32) -> bool {
        ch < 0x80 && VOWELS.contains(&(ch as u8))
    }

    /// `vowel` (`in_grouping_U`).
    fn vowel(&mut self) -> bool {
        match self.ch_fwd() {
            Some((ch, w)) if Self::is_vowel(ch) => {
                self.c += w;
                true
            }
            _ => false,
        }
    }

    /// `non-vowel` (`out_grouping_U`).
    fn non_vowel(&mut self) -> bool {
        match self.ch_fwd() {
            Some((ch, w)) if !Self::is_vowel(ch) => {
                self.c += w;
                true
            }
            _ => false,
        }
    }

    fn vowel_b(&mut self) -> bool {
        match self.ch_bwd() {
            Some((ch, w)) if Self::is_vowel(ch) => {
                self.c -= w;
                true
            }
            _ => false,
        }
    }

    fn non_vowel_b(&mut self) -> bool {
        match self.ch_bwd() {
            Some((ch, w)) if !Self::is_vowel(ch) => {
                self.c -= w;
                true
            }
            _ => false,
        }
    }

    /// `gopast vowel`.
    fn gopast_vowel(&mut self) -> bool {
        loop {
            match self.ch_fwd() {
                None => return false,
                Some((ch, w)) => {
                    self.c += w;
                    if Self::is_vowel(ch) {
                        return true;
                    }
                }
            }
        }
    }

    fn gopast_vowel_b(&mut self) -> bool {
        loop {
            match self.ch_bwd() {
                None => return false,
                Some((ch, w)) => {
                    self.c -= w;
                    if Self::is_vowel(ch) {
                        return true;
                    }
                }
            }
        }
    }

    /// A literal (`eq_s`).
    fn eq(&mut self, s: &[u8]) -> bool {
        if self.l - self.c < s.len() || &self.p[self.c..self.c + s.len()] != s {
            return false;
        }
        self.c += s.len();
        true
    }

    fn eq_b(&mut self, s: &[u8]) -> bool {
        if self.c - self.lb < s.len() || &self.p[self.c - s.len()..self.c] != s {
            return false;
        }
        self.c -= s.len();
        true
    }

    /// `hop n` (`skip_utf8`).
    fn hop(&mut self, n: usize) -> bool {
        let mut c = self.c;
        for _ in 0..n {
            if c >= self.l {
                return false;
            }
            c += 1;
            while c < self.l && (self.p[c] & 0xc0) == 0x80 {
                c += 1;
            }
        }
        self.c = c;
        true
    }

    /// `substring` … `among`: the longest of the strings at the cursor (its index).
    fn among(&mut self, strs: &[&str]) -> Option<usize> {
        let (i, s) = strs.iter().enumerate().filter(|(_, s)| self.l - self.c >= s.len() && &self.p[self.c..self.c + s.len()] == s.as_bytes()).max_by_key(|(i, s)| (s.len(), usize::MAX - i))?;
        self.c += s.len();
        Some(i)
    }

    fn among_b(&mut self, strs: &[&str]) -> Option<usize> {
        let (i, s) = strs.iter().enumerate().filter(|(_, s)| self.c - self.lb >= s.len() && &self.p[self.c - s.len()..self.c] == s.as_bytes()).max_by_key(|(i, s)| (s.len(), usize::MAX - i))?;
        self.c -= s.len();
        Some(i)
    }

    /// `<- s` (`slice_from_s` / `replace_s`).
    fn slice_from(&mut self, s: &[u8]) {
        let (bra, ket) = (self.bra, self.ket);
        let adj = s.len() as isize - (ket - bra) as isize;
        self.p.splice(bra..ket, s.iter().copied());
        self.l = (self.l as isize + adj) as usize;
        if self.c >= ket {
            self.c = (self.c as isize + adj) as usize;
        } else if self.c > bra {
            self.c = bra;
        }
    }

    fn delete(&mut self) {
        self.slice_from(&[]);
    }

    fn slice(&self) -> Vec<u8> {
        self.p[self.bra..self.ket].to_vec()
    }

    /// `len`: the string's length in characters.
    fn len_chars(&self) -> usize {
        self.p.iter().filter(|&&b| (b & 0xc0) != 0x80).count()
    }

    /// `test C` (forward).
    fn test(&mut self, f: impl FnOnce(&mut Self) -> bool) -> bool {
        let c = self.c;
        let r = f(self);
        self.c = c;
        r
    }

    /// `test C` (backward: the cursor is kept relative to the limit).
    fn test_b(&mut self, f: impl FnOnce(&mut Self) -> bool) -> bool {
        let v = self.l - self.c;
        let r = f(self);
        self.c = self.l - v;
        r
    }

    fn any_removed(&self) -> bool {
        self.removed_prefix || self.removed_infix || self.removed_duplication
    }
}

fn special_cases(e: &mut Env) -> bool {
    e.bra = e.c;
    let Some(i) = e.among(&["araw-araw", "kanyang"]) else { return false };
    e.ket = e.c;
    if e.c != e.l {
        return false;
    }
    e.slice_from(if i == 0 { b"araw" } else { b"kanya" });
    true
}

const EXCEPTIONS: &[&str] = &[
    "alipin",
    "alitan",
    "asian",
    "baitang",
    "bakasyon",
    "baybayin",
    "birheng",
    "buhangin",
    "bulwagan",
    "dinamita",
    "dinastiya",
    "kalakal",
    "kamara",
    "kampanya",
    "kapangyarihan",
    "kapilya",
    "kapital",
    "karapatan",
    "karera",
    "kailangan",
];

fn exception1(e: &mut Env) -> bool {
    e.bra = e.c;
    if e.among(EXCEPTIONS).is_none() {
        return false;
    }
    e.ket = e.c;
    e.c == e.l
}

/// `scan_foreign`: a letter Tagalog spells only in loanwords.
fn scan_foreign(e: &mut Env) {
    let c = e.c;
    loop {
        let v = e.c;
        if [b"c", b"f", b"j", b"q", b"v", b"x", b"z"].iter().any(|s| {
            e.c = v;
            e.eq(*s)
        }) {
            e.c = v;
            e.likely_foreign = true;
            break;
        }
        e.c = v;
        if !e.hop(1) {
            break;
        }
    }
    e.c = c;
}

fn has_minimum_remainder(e: &mut Env) -> bool {
    e.l - e.c >= 3 && e.test(|e| e.gopast_vowel() && e.gopast_vowel())
}

fn has_replacement_remainder(e: &mut Env) -> bool {
    e.l - e.c >= 1 && e.test(|e| e.gopast_vowel())
}

fn has_minimum_infix_remainder(e: &mut Env) -> bool {
    e.l - e.c >= 2 && e.test(|e| e.gopast_vowel() && e.gopast_vowel())
}

/// `[..] -> dup test dup delete`: the slice is repeated right after it; the first copy goes.
fn drop_repeated(e: &mut Env) -> bool {
    e.ket = e.c;
    let dup = e.slice();
    if !e.test(|e| e.eq(&dup)) {
        return false;
    }
    e.delete();
    e.removed_duplication = true;
    true
}

fn remove_partial_duplication(e: &mut Env) -> bool {
    let c = e.c;
    if e.l >= 5 {
        e.bra = e.c;
        if e.vowel() && drop_repeated(e) {
            return true;
        }
    }
    e.c = c;
    if e.l >= 6 {
        e.bra = e.c;
        if e.hop(2) && drop_repeated(e) {
            return true;
        }
    }
    e.c = c;
    if e.l >= 9 {
        e.bra = e.c;
        if e.hop(3) && drop_repeated(e) {
            return true;
        }
    }
    e.c = c;
    false
}

/// A prefix among `groups` (each with its own action: 0 delete, 1 `<- 'ka'`, 2 delete unless
/// `ny` follows).
fn remove_prefix(e: &mut Env, groups: &[(&[&str], u8)]) -> bool {
    let all: Vec<&str> = groups.iter().flat_map(|(s, _)| s.iter().copied()).collect();
    e.bra = e.c;
    let Some(i) = e.among(&all) else { return false };
    e.ket = e.c;
    let mut k = i;
    let mut action = 0;
    for (s, a) in groups {
        if k < s.len() {
            action = *a;
            break;
        }
        k -= s.len();
    }
    match action {
        1 => {
            if !has_replacement_remainder(e) {
                return false;
            }
            e.slice_from(b"ka");
        }
        2 => {
            if e.test(|e| e.eq(b"ny")) || !has_minimum_remainder(e) {
                return false;
            }
            e.delete();
        }
        _ => {
            if !has_minimum_remainder(e) {
                return false;
            }
            e.delete();
        }
    }
    e.removed_prefix = true;
    true
}

fn remove_prefix_once(e: &mut Env) -> bool {
    let tables: [&[(&[&str], u8)]; 8] = [
        &[(&["nakikipag", "pakikipag"], 0)],
        &[(&["pinakama"], 0), (&["panganga"], 1)],
        &[(&["makapag", "nakapag", "tagapag", "makipag", "nakipag", "pagpapa", "pinagka"], 0)],
        &[(&["pakiki", "napaka", "pinaka", "ipinag"], 0)],
        &[(&["magpa", "pagka", "pinag", "mapag"], 0)],
        &[(&["mapa", "taga", "ipag", "tiga", "pala", "pina", "pang", "naka", "nang", "mang", "sing"], 0)],
        &[(&["ipa", "pam", "pan", "pag", "tag", "mai", "mag", "nam", "nag", "man", "may"], 0)],
        &[(&["ma", "na", "ni", "pa"], 0), (&["ka"], 2)],
    ];
    let c = e.c;
    for t in tables {
        e.c = c;
        if remove_prefix(e, t) {
            return true;
        }
    }
    e.c = c;
    false
}

fn remove_infix(e: &mut Env) -> bool {
    if e.l < 5 || !e.non_vowel() {
        return false;
    }
    e.bra = e.c;
    let Some(i) = e.among(&["um", "in"]) else { return false };
    e.ket = e.c;
    if !e.test(|e| e.vowel()) || !has_minimum_infix_remainder(e) {
        return false;
    }
    if i == 1 {
        let c = e.c;
        // `not (atlimit 'd' goto ('i' 'y' 'a' or 'i' 't' 'a') atlimit)`.
        let inner = e.c == e.l
            && e.eq(b"d")
            && loop {
                let v = e.c;
                if e.eq(b"iya") || {
                    e.c = v;
                    e.eq(b"ita")
                } {
                    e.c = v;
                    break true;
                }
                e.c = v;
                if !e.hop(1) {
                    break false;
                }
            }
            && e.c == e.l;
        e.c = c;
        if inner {
            return false;
        }
    }
    e.delete();
    e.removed_infix = true;
    true
}

fn has_minimum_suffix_stem(e: &mut Env) -> bool {
    e.c >= 3 && e.test_b(|e| e.gopast_vowel_b() && e.gopast_vowel_b())
}

fn remove_suffix(e: &mut Env) -> bool {
    let v = e.l - e.c;
    // `['g'] test ('n' 'o' ('y' or 'i' or 't'))`.
    e.ket = e.c;
    if e.eq_b(b"g") {
        e.bra = e.c;
        let ok = e.test_b(|e| {
            e.eq_b(b"n") && e.eq_b(b"o") && {
                let w = e.l - e.c;
                e.eq_b(b"y") || {
                    e.c = e.l - w;
                    e.eq_b(b"i")
                } || {
                    e.c = e.l - w;
                    e.eq_b(b"t")
                }
            }
        });
        if ok && has_minimum_suffix_stem(e) {
            e.delete();
            e.removed_suffix = true;
            return true;
        }
    }
    e.c = e.l - v;
    if e.len_chars() >= 7 {
        e.ket = e.c;
        if e.eq_b(b"g") && e.eq_b(b"n") {
            e.bra = e.c;
            if e.test_b(|e| e.vowel_b()) && has_minimum_suffix_stem(e) {
                e.delete();
                e.removed_suffix = true;
                return true;
            }
        }
    }
    e.c = e.l - v;
    e.ket = e.c;
    let Some(i) = e.among_b(&["han", "hin", "in", "an"]) else { return false };
    e.bra = e.c;
    let ok = match i {
        0 | 1 => has_minimum_suffix_stem(e),
        2 => e.any_removed() && has_minimum_suffix_stem(e),
        _ => {
            !e.likely_foreign
                && has_minimum_suffix_stem(e)
                && (e.any_removed() || (e.c >= 5 && e.test_b(|e| e.non_vowel_b())))
                && e.c >= 3
        }
    };
    if !ok {
        return false;
    }
    e.delete();
    e.removed_suffix = true;
    true
}

fn normalize_suffix(e: &mut Env) -> bool {
    if !(e.any_removed() || e.removed_suffix) {
        return false;
    }
    for (from, to) in [(&b"u"[..], &b"o"[..]), (b"r", b"d")] {
        let v = e.l - e.c;
        e.ket = e.c;
        if e.eq_b(from) {
            e.bra = e.c;
            if e.c >= 3 {
                e.slice_from(to);
            }
        }
        e.c = e.l - v;
    }
    let v = e.l - e.c;
    e.ket = e.c;
    if e.eq_b(b"h") {
        e.bra = e.c;
        if e.c >= 3 && e.test_b(|e| e.vowel_b()) {
            e.delete();
        }
    }
    e.c = e.l - v;
    true
}

fn normalize_initial(e: &mut Env) -> bool {
    if !(e.any_removed() || e.removed_suffix) || e.l < 4 {
        return false;
    }
    e.bra = e.c;
    if !e.hop(1) {
        return false;
    }
    e.ket = e.c;
    let dup = e.slice();
    if !e.test(|e| e.eq(&dup)) {
        return false;
    }
    e.delete();
    true
}

/// The stem of `word`.
pub fn stem(word: &str) -> String {
    let mut e = Env {
        p: word.as_bytes().to_vec(),
        c: 0,
        l: word.len(),
        lb: 0,
        bra: 0,
        ket: word.len(),
        removed_prefix: false,
        removed_infix: false,
        removed_duplication: false,
        removed_suffix: false,
        likely_foreign: false,
    };
    scan_foreign(&mut e);
    e.c = 0;
    if !special_cases(&mut e) {
        e.c = 0;
        if !exception1(&mut e) {
            e.c = 0;
            let start = 0;
            remove_prefix_once(&mut e);
            e.c = start;
            remove_prefix_once(&mut e);
            e.c = start;
            remove_partial_duplication(&mut e);
            e.c = start;
            remove_infix(&mut e);
            e.c = start;
            remove_partial_duplication(&mut e);
            // `backwards ( do remove_suffix do normalize_suffix )`.
            e.lb = e.c;
            e.c = e.l;
            let v = e.l - e.c;
            remove_suffix(&mut e);
            e.c = e.l - v;
            let v = e.l - e.c;
            normalize_suffix(&mut e);
            e.c = e.l - v;
            e.c = e.lb;
            e.c = start;
            normalize_initial(&mut e);
        }
    }
    String::from_utf8_lossy(&e.p).into_owned()
}

#[cfg(test)]
mod tests {
    use super::stem;

    #[test]
    fn stems() {
        assert_eq!(stem("araw-araw"), "araw");
        assert_eq!(stem("kailangan"), "kailangan");
        assert_eq!(stem("sumulat"), "sulat");
        assert_eq!(stem("manunulat"), "unulat");
        assert_eq!(stem("tumatakbo"), "takbo");
    }
}
