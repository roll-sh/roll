//! The Porter stemmer as Lucene implements it (`PorterStemmer`, used by `porter_stem`, the
//! `english` stemmer and the English analyzer).
//!
//! Lucene's version is Martin Porter's reference implementation (with its "Bug 1"/"Bug 2" fixes)
//! over UTF-16 units: anything but `a e i o u` (and `y` after a consonant) counts as a consonant,
//! so non-ASCII letters take part in the measure exactly as in Java. The buffer is reused.

/// Reusable Porter stemmer state.
#[derive(Default)]
pub(crate) struct Porter {
    b: Vec<u16>,
    j: isize,
    k: isize,
    k0: isize,
    dirty: bool,
}

const A: u16 = b'a' as u16;
const E: u16 = b'e' as u16;
const I: u16 = b'i' as u16;
const O: u16 = b'o' as u16;
const U: u16 = b'u' as u16;
const Y: u16 = b'y' as u16;

impl Porter {
    fn at(&self, i: isize) -> u16 {
        self.b[i as usize]
    }

    fn cons(&self, i: isize) -> bool {
        match self.at(i) {
            A | E | I | O | U => false,
            Y => {
                if i == self.k0 {
                    true
                } else {
                    !self.cons(i - 1)
                }
            }
            _ => true,
        }
    }

    fn m(&self) -> usize {
        let mut n = 0;
        let mut i = self.k0;
        loop {
            if i > self.j {
                return n;
            }
            if !self.cons(i) {
                break;
            }
            i += 1;
        }
        i += 1;
        loop {
            loop {
                if i > self.j {
                    return n;
                }
                if self.cons(i) {
                    break;
                }
                i += 1;
            }
            i += 1;
            n += 1;
            loop {
                if i > self.j {
                    return n;
                }
                if !self.cons(i) {
                    break;
                }
                i += 1;
            }
            i += 1;
        }
    }

    fn vowel_in_stem(&self) -> bool {
        (self.k0..=self.j).any(|i| !self.cons(i))
    }

    fn doublec(&self, j: isize) -> bool {
        if j < self.k0 + 1 {
            return false;
        }
        if self.at(j) != self.at(j - 1) {
            return false;
        }
        self.cons(j)
    }

    fn cvc(&self, i: isize) -> bool {
        if i < self.k0 + 2 || !self.cons(i) || self.cons(i - 1) || !self.cons(i - 2) {
            return false;
        }
        !matches!(self.at(i), 0x77..=0x79) // w, x, y
    }

    fn ends(&mut self, s: &str) -> bool {
        let l = s.len() as isize;
        let o = self.k - l + 1;
        if o < self.k0 {
            return false;
        }
        for (i, c) in s.bytes().enumerate() {
            if self.at(o + i as isize) != c as u16 {
                return false;
            }
        }
        self.j = self.k - l;
        true
    }

    fn setto(&mut self, s: &str) {
        let o = (self.j + 1) as usize;
        let need = o + s.len();
        if self.b.len() < need {
            self.b.resize(need, 0);
        }
        for (i, c) in s.bytes().enumerate() {
            self.b[o + i] = c as u16;
        }
        self.k = self.j + s.len() as isize;
        self.dirty = true;
    }

    fn r(&mut self, s: &str) {
        if self.m() > 0 {
            self.setto(s);
        }
    }

    fn step1(&mut self) {
        if self.at(self.k) == b's' as u16 {
            if self.ends("sses") {
                self.k -= 2;
            } else if self.ends("ies") {
                self.setto("i");
            } else if self.at(self.k - 1) != b's' as u16 {
                self.k -= 1;
            }
        }
        if self.ends("eed") {
            if self.m() > 0 {
                self.k -= 1;
            }
        } else if (self.ends("ed") || self.ends("ing")) && self.vowel_in_stem() {
            self.k = self.j;
            if self.ends("at") {
                self.setto("ate");
            } else if self.ends("bl") {
                self.setto("ble");
            } else if self.ends("iz") {
                self.setto("ize");
            } else if self.doublec(self.k) {
                let ch = self.at(self.k);
                self.k -= 1;
                if ch == b'l' as u16 || ch == b's' as u16 || ch == b'z' as u16 {
                    self.k += 1;
                }
            } else if self.m() == 1 && self.cvc(self.k) {
                self.setto("e");
            }
        }
    }

    fn step2(&mut self) {
        if self.ends("y") && self.vowel_in_stem() {
            let k = self.k as usize;
            self.b[k] = I;
            self.dirty = true;
        }
    }

    fn step3(&mut self) {
        if self.k == self.k0 {
            return;
        }
        let rules: &[(&str, &str)] = match self.at(self.k - 1) {
            0x61 => &[("ational", "ate"), ("tional", "tion")],
            0x63 => &[("enci", "ence"), ("anci", "ance")],
            0x65 => &[("izer", "ize")],
            0x6C => &[
                ("bli", "ble"),
                ("alli", "al"),
                ("entli", "ent"),
                ("eli", "e"),
                ("ousli", "ous"),
            ],
            0x6F => &[("ization", "ize"), ("ation", "ate"), ("ator", "ate")],
            0x73 => &[
                ("alism", "al"),
                ("iveness", "ive"),
                ("fulness", "ful"),
                ("ousness", "ous"),
            ],
            0x74 => &[("aliti", "al"), ("iviti", "ive"), ("biliti", "ble")],
            0x67 => &[("logi", "log")],
            _ => &[],
        };
        for (suffix, rep) in rules {
            if self.ends(suffix) {
                self.r(rep);
                break;
            }
        }
    }

    fn step4(&mut self) {
        let rules: &[(&str, &str)] = match self.at(self.k) {
            0x65 => &[("icate", "ic"), ("ative", ""), ("alize", "al")],
            0x69 => &[("iciti", "ic")],
            0x6C => &[("ical", "ic"), ("ful", "")],
            0x73 => &[("ness", "")],
            _ => &[],
        };
        for (suffix, rep) in rules {
            if self.ends(suffix) {
                self.r(rep);
                break;
            }
        }
    }

    fn step5(&mut self) {
        if self.k == self.k0 {
            return;
        }
        let matched = match self.at(self.k - 1) {
            0x61 => self.ends("al"),
            0x63 => self.ends("ance") || self.ends("ence"),
            0x65 => self.ends("er"),
            0x69 => self.ends("ic"),
            0x6C => self.ends("able") || self.ends("ible"),
            0x6E => self.ends("ant") || self.ends("ement") || self.ends("ment") || self.ends("ent"),
            0x6F => {
                (self.ends("ion")
                    && self.j >= 0
                    && (self.at(self.j) == b's' as u16 || self.at(self.j) == b't' as u16))
                    || self.ends("ou")
            }
            0x73 => self.ends("ism"),
            0x74 => self.ends("ate") || self.ends("iti"),
            0x75 => self.ends("ous"),
            0x76 => self.ends("ive"),
            0x7A => self.ends("ize"),
            _ => false,
        };
        if matched && self.m() > 1 {
            self.k = self.j;
        }
    }

    fn step6(&mut self) {
        self.j = self.k;
        if self.at(self.k) == E {
            let a = self.m();
            if a > 1 || (a == 1 && !self.cvc(self.k - 1)) {
                self.k -= 1;
            }
        }
        if self.at(self.k) == b'l' as u16 && self.doublec(self.k) && self.m() > 1 {
            self.k -= 1;
        }
    }

    /// Stems `term` in place; false when it is unchanged.
    pub fn stem(&mut self, term: &mut String) -> bool {
        self.b.clear();
        self.b.extend(term.encode_utf16());
        let len = self.b.len() as isize;
        self.dirty = false;
        self.k = len - 1;
        self.k0 = 0;
        self.j = 0;
        if self.k > self.k0 + 1 {
            self.step1();
            self.step2();
            self.step3();
            self.step4();
            self.step5();
            self.step6();
        }
        if len != self.k + 1 {
            self.dirty = true;
        }
        if !self.dirty {
            return false;
        }
        let end = (self.k + 1).max(0) as usize;
        term.clear();
        term.extend(
            char::decode_utf16(self.b[..end].iter().copied()).map(|r| r.unwrap_or('\u{FFFD}')),
        );
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stem(w: &str) -> String {
        let mut p = Porter::default();
        let mut s = w.to_string();
        p.stem(&mut s);
        s
    }

    #[test]
    fn reference_words() {
        for (w, s) in [
            ("caresses", "caress"),
            ("ponies", "poni"),
            ("ties", "ti"),
            ("cats", "cat"),
            ("agreed", "agre"),
            ("disabled", "disabl"),
            ("matting", "mat"),
            ("mating", "mate"),
            ("meeting", "meet"),
            ("running", "run"),
            ("generalization", "gener"),
            ("happiness", "happi"),
            ("relational", "relat"),
            ("a", "a"),
            ("is", "is"),
            ("dogs", "dog"),
            ("jumped", "jump"),
        ] {
            assert_eq!(stem(w), s, "{w}");
        }
    }
}
