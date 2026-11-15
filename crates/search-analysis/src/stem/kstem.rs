//! Krovetz's stemmer as Lucene implements it (`KStemmer`, behind `kstem` and the
//! `light_english` stemmer).
//!
//! KStem is dictionary-driven: inflectional and derivational endings are removed one rule at a
//! time and the candidate is accepted once it is a dictionary word, so the output is always a
//! word when one exists. The rules are ported from Lucene step by step, including how they leave
//! the working buffer when nothing matches (Lucene's `OpenStringBuilder` keeps stale characters
//! past its length, which some rules rely on). Only lowercase ASCII words of 3 to 48 letters are
//! stemmed; the dictionary is built once per process.

use std::collections::HashMap;
use std::sync::OnceLock;

use super::kstem_data::{CONFLATIONS, EXCEPTIONS, WORDS};

#[derive(Clone, Copy)]
struct Entry {
    root: Option<&'static str>,
    exception: bool,
}

fn dict() -> &'static HashMap<&'static str, Entry, foldhash::fast::RandomState> {
    static D: OnceLock<HashMap<&'static str, Entry, foldhash::fast::RandomState>> = OnceLock::new();
    D.get_or_init(|| {
        let mut d: HashMap<&'static str, Entry, foldhash::fast::RandomState> = HashMap::default();
        for &w in EXCEPTIONS {
            d.insert(
                w,
                Entry {
                    root: Some(w),
                    exception: true,
                },
            );
        }
        for &(w, r) in CONFLATIONS {
            d.entry(w).or_insert(Entry {
                root: Some(r),
                exception: false,
            });
        }
        for w in WORDS.split('\n').filter(|w| !w.is_empty()) {
            d.entry(w).or_insert(Entry {
                root: None,
                exception: false,
            });
        }
        d
    })
}

const MAX_WORD_LEN: usize = 50;

/// The stemmer's working state (Lucene's `KStemmer` fields).
pub(crate) struct KStem {
    buf: [u8; MAX_WORD_LEN + 16],
    len: usize,
    j: isize,
    k: isize,
    matched: Option<Entry>,
    /// A direct dictionary result (`result`).
    result: Option<&'static str>,
}

impl Default for KStem {
    fn default() -> Self {
        KStem {
            buf: [0; MAX_WORD_LEN + 16],
            len: 0,
            j: 0,
            k: 0,
            matched: None,
            result: None,
        }
    }
}

impl KStem {
    fn ch(&self, i: isize) -> u8 {
        self.buf[i as usize]
    }

    fn set_ch(&mut self, i: isize, c: u8) {
        self.buf[i as usize] = c;
    }

    fn set_len(&mut self, n: isize) {
        self.len = n.max(0) as usize;
    }

    fn write(&mut self, c: u8) {
        self.buf[self.len] = c;
        self.len += 1;
    }

    fn append(&mut self, s: &str) {
        for b in s.bytes() {
            self.write(b);
        }
    }

    fn word(&self) -> &str {
        // Only ASCII letters are ever written.
        std::str::from_utf8(&self.buf[..self.len]).unwrap_or("")
    }

    fn is_cons(&self, i: isize) -> bool {
        let c = self.ch(i);
        if matches!(c, b'a' | b'e' | b'i' | b'o' | b'u') {
            return false;
        }
        if c != b'y' || i == 0 {
            true
        } else {
            !self.is_cons(i - 1)
        }
    }

    fn is_vowel(&self, i: isize) -> bool {
        !self.is_cons(i)
    }

    fn penult(&self) -> u8 {
        self.ch(self.k - 1)
    }

    fn ends_in_str(&mut self, s: &str) -> bool {
        let sl = s.len() as isize;
        if sl > self.k {
            return false;
        }
        let r = self.len as isize - sl;
        self.j = self.k;
        for (i, b) in s.bytes().enumerate() {
            if b != self.ch(r + i as isize) {
                return false;
            }
        }
        self.j = r - 1;
        true
    }

    fn ends2(&mut self, a: u8, b: u8) -> bool {
        if 2 > self.k {
            return false;
        }
        if self.ch(self.k - 1) == a && self.ch(self.k) == b {
            self.j = self.k - 2;
            return true;
        }
        false
    }

    fn ends3(&mut self, a: u8, b: u8, c: u8) -> bool {
        if 3 > self.k {
            return false;
        }
        if self.ch(self.k - 2) == a && self.ch(self.k - 1) == b && self.ch(self.k) == c {
            self.j = self.k - 3;
            return true;
        }
        false
    }

    fn ends4(&mut self, a: u8, b: u8, c: u8, d: u8) -> bool {
        if 4 > self.k {
            return false;
        }
        if self.ch(self.k - 3) == a
            && self.ch(self.k - 2) == b
            && self.ch(self.k - 1) == c
            && self.ch(self.k) == d
        {
            self.j = self.k - 4;
            return true;
        }
        false
    }

    fn word_in_dict(&mut self) -> Option<Entry> {
        if let Some(m) = self.matched {
            return Some(m);
        }
        let e = dict().get(self.word()).copied();
        if let Some(e) = e
            && !e.exception
        {
            self.matched = Some(e);
        }
        e
    }

    fn lookup(&mut self) -> bool {
        self.matched = dict().get(self.word()).copied();
        self.matched.is_some()
    }

    fn set_suffix(&mut self, s: &str) {
        self.set_len(self.j + 1);
        self.append(s);
        self.k = self.j + s.len() as isize;
    }

    fn double_c(&self, i: isize) -> bool {
        if i < 1 {
            return false;
        }
        if self.ch(i) != self.ch(i - 1) {
            return false;
        }
        self.is_cons(i)
    }

    fn vowel_in_stem(&self) -> bool {
        (0..self.j + 1).any(|i| self.is_vowel(i))
    }

    fn plural(&mut self) {
        if self.ch(self.k) != b's' {
            return;
        }
        if self.ends3(b'i', b'e', b's') {
            self.set_len(self.j + 3);
            self.k -= 1;
            if self.lookup() {
                return;
            }
            self.k += 1;
            self.write(b's');
            self.set_suffix("y");
            self.lookup();
        } else if self.ends2(b'e', b's') {
            self.set_len(self.j + 2);
            self.k -= 1;
            let try_e = self.j > 0 && !(self.ch(self.j) == b's' && self.ch(self.j - 1) == b's');
            if try_e && self.lookup() {
                return;
            }
            self.set_len(self.j + 1);
            self.k -= 1;
            if self.lookup() {
                return;
            }
            self.write(b'e');
            self.k += 1;
            if !try_e {
                self.lookup();
            }
        } else if self.len > 3 && self.penult() != b's' && !self.ends3(b'o', b'u', b's') {
            self.set_len(self.k);
            self.k -= 1;
            self.lookup();
        }
    }

    fn past_tense(&mut self) {
        if self.len <= 4 {
            return;
        }
        if self.ends3(b'i', b'e', b'd') {
            self.set_len(self.j + 3);
            self.k -= 1;
            if self.lookup() {
                return;
            }
            self.k += 1;
            self.write(b'd');
            self.set_suffix("y");
            self.lookup();
            return;
        }
        if self.ends2(b'e', b'd') && self.vowel_in_stem() {
            self.set_len(self.j + 2);
            self.k = self.j + 1;
            if let Some(e) = self.word_in_dict()
                && !e.exception
            {
                return;
            }
            self.set_len(self.j + 1);
            self.k = self.j;
            if self.lookup() {
                return;
            }
            if self.double_c(self.k) {
                self.set_len(self.k);
                self.k -= 1;
                if self.lookup() {
                    return;
                }
                let c = self.ch(self.k);
                self.write(c);
                self.k += 1;
                self.lookup();
                return;
            }
            if self.ch(0) == b'u' && self.ch(1) == b'n' {
                self.write(b'e');
                self.write(b'd');
                self.k += 2;
                return;
            }
            self.set_len(self.j + 1);
            self.write(b'e');
            self.k = self.j + 1;
        }
    }

    fn aspect(&mut self) {
        if self.len <= 5 {
            return;
        }
        if self.ends3(b'i', b'n', b'g') && self.vowel_in_stem() {
            self.set_ch(self.j + 1, b'e');
            self.set_len(self.j + 2);
            self.k = self.j + 1;
            if let Some(e) = self.word_in_dict()
                && !e.exception
            {
                return;
            }
            self.set_len(self.k);
            self.k -= 1;
            if self.lookup() {
                return;
            }
            if self.double_c(self.k) {
                self.k -= 1;
                self.set_len(self.k + 1);
                if self.lookup() {
                    return;
                }
                let c = self.ch(self.k);
                self.write(c);
                self.k += 1;
                self.lookup();
                return;
            }
            if self.j > 0 && self.is_cons(self.j) && self.is_cons(self.j - 1) {
                self.k = self.j;
                self.set_len(self.k + 1);
                return;
            }
            self.set_len(self.j + 1);
            self.write(b'e');
            self.k = self.j + 1;
        }
    }

    fn ity_endings(&mut self) {
        let old_k = self.k;
        if !self.ends3(b'i', b't', b'y') {
            return;
        }
        self.set_len(self.j + 1);
        self.k = self.j;
        if self.lookup() {
            return;
        }
        self.write(b'e');
        self.k = self.j + 1;
        if self.lookup() {
            return;
        }
        self.set_ch(self.j + 1, b'i');
        self.append("ty");
        self.k = old_k;
        let j = self.j;
        if j > 0 && self.ch(j - 1) == b'i' && self.ch(j) == b'l' {
            self.set_len(j - 1);
            self.append("le");
            self.k = j;
            self.lookup();
            return;
        }
        if j > 0 && self.ch(j - 1) == b'i' && self.ch(j) == b'v' {
            self.set_len(j + 1);
            self.write(b'e');
            self.k = j + 1;
            self.lookup();
            return;
        }
        if j > 0 && self.ch(j - 1) == b'a' && self.ch(j) == b'l' {
            self.set_len(j + 1);
            self.k = j;
            self.lookup();
            return;
        }
        if self.lookup() {
            return;
        }
        self.set_len(j + 1);
        self.k = j;
    }

    fn nce_endings(&mut self) {
        let old_k = self.k;
        if self.ends3(b'n', b'c', b'e') {
            let wc = self.ch(self.j);
            if !(wc == b'e' || wc == b'a') {
                return;
            }
            self.set_len(self.j);
            self.write(b'e');
            self.k = self.j;
            if self.lookup() {
                return;
            }
            self.set_len(self.j);
            self.k = self.j - 1;
            if self.lookup() {
                return;
            }
            self.write(wc);
            self.append("nce");
            self.k = old_k;
        }
    }

    fn ness_endings(&mut self) {
        if self.ends4(b'n', b'e', b's', b's') {
            self.set_len(self.j + 1);
            self.k = self.j;
            if self.ch(self.j) == b'i' {
                self.set_ch(self.j, b'y');
            }
            self.lookup();
        }
    }

    fn ism_endings(&mut self) {
        if self.ends3(b'i', b's', b'm') {
            self.set_len(self.j + 1);
            self.k = self.j;
            self.lookup();
        }
    }

    fn ment_endings(&mut self) {
        let old_k = self.k;
        if self.ends4(b'm', b'e', b'n', b't') {
            self.set_len(self.j + 1);
            self.k = self.j;
            if self.lookup() {
                return;
            }
            self.append("ment");
            self.k = old_k;
        }
    }

    fn ize_endings(&mut self) {
        let old_k = self.k;
        if self.ends3(b'i', b'z', b'e') {
            self.set_len(self.j + 1);
            self.k = self.j;
            if self.lookup() {
                return;
            }
            self.write(b'i');
            if self.double_c(self.j) {
                self.set_len(self.j);
                self.k = self.j - 1;
                if self.lookup() {
                    return;
                }
                let c = self.ch(self.j - 1);
                self.write(c);
            }
            self.set_len(self.j + 1);
            self.write(b'e');
            self.k = self.j + 1;
            if self.lookup() {
                return;
            }
            self.set_len(self.j + 1);
            self.append("ize");
            self.k = old_k;
        }
    }

    fn ncy_endings(&mut self) {
        if self.ends3(b'n', b'c', b'y') {
            if !(self.ch(self.j) == b'e' || self.ch(self.j) == b'a') {
                return;
            }
            self.set_ch(self.j + 2, b't');
            self.set_len(self.j + 3);
            self.k = self.j + 2;
            if self.lookup() {
                return;
            }
            self.set_ch(self.j + 2, b'c');
            self.write(b'e');
            self.k = self.j + 3;
            self.lookup();
        }
    }

    fn ble_endings(&mut self) {
        let old_k = self.k;
        if self.ends3(b'b', b'l', b'e') {
            if !(self.ch(self.j) == b'a' || self.ch(self.j) == b'i') {
                return;
            }
            let wc = self.ch(self.j);
            self.set_len(self.j);
            self.k = self.j - 1;
            if self.lookup() {
                return;
            }
            if self.double_c(self.k) {
                self.set_len(self.k);
                self.k -= 1;
                if self.lookup() {
                    return;
                }
                self.k += 1;
                let c = self.ch(self.k - 1);
                self.write(c);
            }
            self.set_len(self.j);
            self.write(b'e');
            self.k = self.j;
            if self.lookup() {
                return;
            }
            self.set_len(self.j);
            self.append("ate");
            self.k = self.j + 2;
            if self.lookup() {
                return;
            }
            self.set_len(self.j);
            self.write(wc);
            self.append("ble");
            self.k = old_k;
        }
    }

    fn ic_endings(&mut self) {
        if self.ends2(b'i', b'c') {
            self.set_len(self.j + 3);
            self.append("al");
            self.k = self.j + 4;
            if self.lookup() {
                return;
            }
            self.set_ch(self.j + 1, b'y');
            self.set_len(self.j + 2);
            self.k = self.j + 1;
            if self.lookup() {
                return;
            }
            self.set_ch(self.j + 1, b'e');
            if self.lookup() {
                return;
            }
            self.set_len(self.j + 1);
            self.k = self.j;
            if self.lookup() {
                return;
            }
            self.append("ic");
            self.k = self.j + 2;
        }
    }

    fn ion_endings(&mut self) {
        let old_k = self.k;
        if !self.ends3(b'i', b'o', b'n') {
            return;
        }
        if self.ends_in_str("ization") {
            self.set_len(self.j + 3);
            self.write(b'e');
            self.k = self.j + 3;
            self.lookup();
            return;
        }
        if self.ends_in_str("ition") {
            self.set_len(self.j + 1);
            self.write(b'e');
            self.k = self.j + 1;
            if self.lookup() {
                return;
            }
            self.set_len(self.j + 1);
            self.append("ition");
            self.k = old_k;
        } else if self.ends_in_str("ation") {
            self.set_len(self.j + 3);
            self.write(b'e');
            self.k = self.j + 3;
            if self.lookup() {
                return;
            }
            self.set_len(self.j + 1);
            self.write(b'e');
            self.k = self.j + 1;
            if self.lookup() {
                return;
            }
            self.set_len(self.j + 1);
            self.k = self.j;
            if self.lookup() {
                return;
            }
            self.set_len(self.j + 1);
            self.append("ation");
            self.k = old_k;
        }
        if self.ends_in_str("ication") {
            self.set_len(self.j + 1);
            self.write(b'y');
            self.k = self.j + 1;
            if self.lookup() {
                return;
            }
            self.set_len(self.j + 1);
            self.append("ication");
            self.k = old_k;
        }
        self.j = self.k - 3;
        self.set_len(self.j + 1);
        self.write(b'e');
        self.k = self.j + 1;
        if self.lookup() {
            return;
        }
        self.set_len(self.j + 1);
        self.k = self.j;
        if self.lookup() {
            return;
        }
        self.set_len(self.j + 1);
        self.append("ion");
        self.k = old_k;
    }

    fn er_or_endings(&mut self) {
        let old_k = self.k;
        if self.ch(self.k) != b'r' {
            return;
        }
        if self.ends4(b'i', b'z', b'e', b'r') {
            self.set_len(self.j + 4);
            self.k = self.j + 3;
            self.lookup();
            return;
        }
        if self.ends2(b'e', b'r') || self.ends2(b'o', b'r') {
            let wc = self.ch(self.j + 1);
            if self.double_c(self.j) {
                self.set_len(self.j);
                self.k = self.j - 1;
                if self.lookup() {
                    return;
                }
                let c = self.ch(self.j - 1);
                self.write(c);
            }
            if self.ch(self.j) == b'i' {
                self.set_ch(self.j, b'y');
                self.set_len(self.j + 1);
                self.k = self.j;
                if self.lookup() {
                    return;
                }
                self.set_ch(self.j, b'i');
                self.write(b'e');
            }
            if self.ch(self.j) == b'e' {
                self.set_len(self.j);
                self.k = self.j - 1;
                if self.lookup() {
                    return;
                }
                self.write(b'e');
            }
            self.set_len(self.j + 2);
            self.k = self.j + 1;
            if self.lookup() {
                return;
            }
            self.set_len(self.j + 1);
            self.k = self.j;
            if self.lookup() {
                return;
            }
            self.write(b'e');
            self.k = self.j + 1;
            if self.lookup() {
                return;
            }
            self.set_len(self.j + 1);
            self.write(wc);
            self.write(b'r');
            self.k = old_k;
        }
    }

    fn ly_endings(&mut self) {
        let old_k = self.k;
        if self.ends2(b'l', b'y') {
            self.set_ch(self.j + 2, b'e');
            if self.lookup() {
                return;
            }
            self.set_ch(self.j + 2, b'y');
            self.set_len(self.j + 1);
            self.k = self.j;
            if self.lookup() {
                return;
            }
            let j = self.j;
            if j > 0 && self.ch(j - 1) == b'a' && self.ch(j) == b'l' {
                return;
            }
            self.append("ly");
            self.k = old_k;
            if j > 0 && self.ch(j - 1) == b'a' && self.ch(j) == b'b' {
                self.set_ch(j + 2, b'e');
                self.k = j + 2;
                return;
            }
            if self.ch(j) == b'i' {
                self.set_len(j);
                self.write(b'y');
                self.k = j;
                if self.lookup() {
                    return;
                }
                self.set_len(j);
                self.append("ily");
                self.k = old_k;
            }
            self.set_len(j + 1);
            self.k = j;
        }
    }

    fn al_endings(&mut self) {
        let old_k = self.k;
        if self.len < 4 {
            return;
        }
        if self.ends2(b'a', b'l') {
            self.set_len(self.j + 1);
            self.k = self.j;
            if self.lookup() {
                return;
            }
            if self.double_c(self.j) {
                self.set_len(self.j);
                self.k = self.j - 1;
                if self.lookup() {
                    return;
                }
                let c = self.ch(self.j - 1);
                self.write(c);
            }
            self.set_len(self.j + 1);
            self.write(b'e');
            self.k = self.j + 1;
            if self.lookup() {
                return;
            }
            self.set_len(self.j + 1);
            self.append("um");
            self.k = self.j + 2;
            if self.lookup() {
                return;
            }
            self.set_len(self.j + 1);
            self.append("al");
            self.k = old_k;
            let j = self.j;
            if j > 0 && self.ch(j - 1) == b'i' && self.ch(j) == b'c' {
                self.set_len(j - 1);
                self.k = j - 2;
                if self.lookup() {
                    return;
                }
                self.set_len(j - 1);
                self.write(b'y');
                self.k = j - 1;
                if self.lookup() {
                    return;
                }
                self.set_len(j - 1);
                self.append("ic");
                self.k = j;
                self.lookup();
                return;
            }
            if self.ch(j) == b'i' {
                self.set_len(j);
                self.k = j - 1;
                if self.lookup() {
                    return;
                }
                self.append("ial");
                self.k = old_k;
                self.lookup();
            }
        }
    }

    fn ive_endings(&mut self) {
        let old_k = self.k;
        if self.ends3(b'i', b'v', b'e') {
            self.set_len(self.j + 1);
            self.k = self.j;
            if self.lookup() {
                return;
            }
            self.write(b'e');
            self.k = self.j + 1;
            if self.lookup() {
                return;
            }
            self.set_len(self.j + 1);
            self.append("ive");
            let j = self.j;
            if j > 0 && self.ch(j - 1) == b'a' && self.ch(j) == b't' {
                self.set_ch(j - 1, b'e');
                self.set_len(j);
                self.k = j - 1;
                if self.lookup() {
                    return;
                }
                self.set_len(j - 1);
                if self.lookup() {
                    return;
                }
                self.append("ative");
                self.k = old_k;
            }
            self.set_ch(j + 2, b'o');
            self.set_ch(j + 3, b'n');
            if self.lookup() {
                return;
            }
            self.set_ch(j + 2, b'v');
            self.set_ch(j + 3, b'e');
            self.k = old_k;
        }
    }

    /// Stems `term` in place (lowercase ASCII words only); false when unchanged.
    pub fn stem(&mut self, term: &mut String) -> bool {
        self.result = None;
        let len = term.len();
        self.k = len as isize - 1;
        if self.k <= 1 || self.k >= MAX_WORD_LEN as isize - 1 {
            return false;
        }
        if let Some(e) = dict().get(term.as_str()) {
            return match e.root {
                Some(r) if r != term => {
                    term.clear();
                    term.push_str(r);
                    true
                }
                _ => false,
            };
        }
        if !term.bytes().all(|b| b.is_ascii_lowercase()) {
            return false;
        }
        self.len = 0;
        for b in term.bytes() {
            self.write(b);
        }
        self.matched = None;
        let steps: [fn(&mut KStem); 8] = [
            KStem::plural,
            KStem::past_tense,
            KStem::aspect,
            KStem::ity_endings,
            KStem::ness_endings,
            KStem::ion_endings,
            KStem::er_or_endings,
            KStem::ly_endings,
        ];
        let rest: [fn(&mut KStem); 8] = [
            KStem::ive_endings,
            KStem::ize_endings,
            KStem::ment_endings,
            KStem::ble_endings,
            KStem::ism_endings,
            KStem::ic_endings,
            KStem::ncy_endings,
            KStem::nce_endings,
        ];
        'steps: {
            for s in steps {
                s(self);
                if self.matched.is_some() {
                    break 'steps;
                }
            }
            self.al_endings();
            if self.matched.is_some() {
                break 'steps;
            }
            let _ = self.word_in_dict();
            for s in rest {
                s(self);
                if self.matched.is_some() {
                    break 'steps;
                }
            }
        }
        if let Some(e) = self.matched
            && let Some(r) = e.root
        {
            self.result = Some(r);
        }
        let out = match self.result {
            Some(r) => r,
            None => self.word(),
        };
        if out == term {
            return false;
        }
        term.clear();
        term.push_str(out);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stem(w: &str) -> String {
        let mut k = KStem::default();
        let mut s = w.to_string();
        k.stem(&mut s);
        s
    }

    #[test]
    fn krovetz() {
        // Expected outputs captured from Elasticsearch (dictionary words are kept as they are).
        for (w, s) in [
            ("running", "running"),
            ("jumping", "jump"),
            ("dogs", "dogs"),
            ("jumped", "jump"),
            ("aging", "age"),
            ("calories", "calorie"),
            ("ponies", "pony"),
            ("happiness", "happiness"),
            ("definition", "definition"),
            ("abilities", "abilities"),
            ("italians", "italy"),
            ("brown", "brown"),
        ] {
            assert_eq!(stem(w), s, "{w}");
        }
    }
}
