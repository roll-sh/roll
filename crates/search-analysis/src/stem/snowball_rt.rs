//! A Snowball runtime with Lucene's semantics (`SnowballProgram`, `Among`), for the Snowball
//! algorithms Lucene ships that the in-repo `snowball` crate lacks (Estonian, from Snowball
//! 2.2.0 as in Lucene 10). Positions are `i32` char indices like Java's; inputs are BMP text
//! (one `char` per Java `char`). The algorithm below is translated statement by statement from
//! the Snowball-generated Java (Snowball's BSD license), hence its generated shape.
#![allow(
    clippy::all,
    unused_parens,
    unused_mut,
    unused_assignments,
    dead_code,
    non_snake_case
)]

pub(crate) struct Among {
    pub s: &'static str,
    pub substring_i: i32,
    pub result: i32,
}

/// The working state of a stemmer (`SnowballProgram`).
#[derive(Default)]
pub(crate) struct Program {
    current: Vec<char>,
    pub cursor: i32,
    pub length: i32,
    pub limit: i32,
    pub limit_backward: i32,
    pub bra: i32,
    pub ket: i32,
}

fn among_len(a: &Among) -> i32 {
    a.s.chars().count() as i32
}

fn among_at(a: &Among, i: i32) -> char {
    a.s.chars().nth(i as usize).unwrap_or('\0')
}

impl Program {
    pub fn set_current(&mut self, text: &str) {
        self.current.clear();
        self.current.extend(text.chars());
        self.cursor = 0;
        self.length = self.current.len() as i32;
        self.limit = self.length;
        self.limit_backward = 0;
        self.bra = 0;
        self.ket = self.limit;
    }

    pub fn current(&self) -> impl Iterator<Item = &char> {
        self.current[..self.length as usize].iter()
    }

    fn ch(&self, i: i32) -> char {
        self.current[i as usize]
    }

    fn grouping(s: &[u8], ch: char, min: i32, max: i32) -> Option<bool> {
        let c = ch as i32;
        if c > max || c < min {
            return None;
        }
        let c = c - min;
        Some(s[(c >> 3) as usize] & (1 << (c & 7)) != 0)
    }

    pub fn in_grouping(&mut self, s: &[u8], min: i32, max: i32) -> bool {
        if self.cursor >= self.limit {
            return false;
        }
        if Self::grouping(s, self.ch(self.cursor), min, max) != Some(true) {
            return false;
        }
        self.cursor += 1;
        true
    }

    pub fn in_grouping_b(&mut self, s: &[u8], min: i32, max: i32) -> bool {
        if self.cursor <= self.limit_backward {
            return false;
        }
        if Self::grouping(s, self.ch(self.cursor - 1), min, max) != Some(true) {
            return false;
        }
        self.cursor -= 1;
        true
    }

    pub fn out_grouping(&mut self, s: &[u8], min: i32, max: i32) -> bool {
        if self.cursor >= self.limit {
            return false;
        }
        if Self::grouping(s, self.ch(self.cursor), min, max) != Some(true) {
            self.cursor += 1;
            return true;
        }
        false
    }

    pub fn out_grouping_b(&mut self, s: &[u8], min: i32, max: i32) -> bool {
        if self.cursor <= self.limit_backward {
            return false;
        }
        if Self::grouping(s, self.ch(self.cursor - 1), min, max) != Some(true) {
            self.cursor -= 1;
            return true;
        }
        false
    }

    pub fn eq_s(&mut self, s: &str) -> bool {
        let n = s.chars().count() as i32;
        if self.limit - self.cursor < n {
            return false;
        }
        for (i, c) in s.chars().enumerate() {
            if self.ch(self.cursor + i as i32) != c {
                return false;
            }
        }
        self.cursor += n;
        true
    }

    pub fn eq_s_b(&mut self, s: &str) -> bool {
        let n = s.chars().count() as i32;
        if self.cursor - self.limit_backward < n {
            return false;
        }
        for (i, c) in s.chars().enumerate() {
            if self.ch(self.cursor - n + i as i32) != c {
                return false;
            }
        }
        self.cursor -= n;
        true
    }

    pub fn find_among(&mut self, v: &[Among]) -> i32 {
        let (mut i, mut j) = (0i32, v.len() as i32);
        let c = self.cursor;
        let l = self.limit;
        let (mut common_i, mut common_j) = (0i32, 0i32);
        let mut first_key_inspected = false;
        loop {
            let k = i + ((j - i) >> 1);
            let mut diff = 0i32;
            let mut common = common_i.min(common_j);
            let w = &v[k as usize];
            let mut i2 = common;
            while i2 < among_len(w) {
                if c + common == l {
                    diff = -1;
                    break;
                }
                diff = self.ch(c + common) as i32 - among_at(w, i2) as i32;
                if diff != 0 {
                    break;
                }
                common += 1;
                i2 += 1;
            }
            if diff < 0 {
                j = k;
                common_j = common;
            } else {
                i = k;
                common_i = common;
            }
            if j - i <= 1 {
                if i > 0 || j == i || first_key_inspected {
                    break;
                }
                first_key_inspected = true;
            }
        }
        loop {
            let w = &v[i as usize];
            if common_i >= among_len(w) {
                self.cursor = c + among_len(w);
                return w.result;
            }
            i = w.substring_i;
            if i < 0 {
                return 0;
            }
        }
    }

    pub fn find_among_b(&mut self, v: &[Among]) -> i32 {
        let (mut i, mut j) = (0i32, v.len() as i32);
        let c = self.cursor;
        let lb = self.limit_backward;
        let (mut common_i, mut common_j) = (0i32, 0i32);
        let mut first_key_inspected = false;
        loop {
            let k = i + ((j - i) >> 1);
            let mut diff = 0i32;
            let mut common = common_i.min(common_j);
            let w = &v[k as usize];
            let mut i2 = among_len(w) - 1 - common;
            while i2 >= 0 {
                if c - common == lb {
                    diff = -1;
                    break;
                }
                diff = self.ch(c - 1 - common) as i32 - among_at(w, i2) as i32;
                if diff != 0 {
                    break;
                }
                common += 1;
                i2 -= 1;
            }
            if diff < 0 {
                j = k;
                common_j = common;
            } else {
                i = k;
                common_i = common;
            }
            if j - i <= 1 {
                if i > 0 || j == i || first_key_inspected {
                    break;
                }
                first_key_inspected = true;
            }
        }
        loop {
            let w = &v[i as usize];
            if common_i >= among_len(w) {
                self.cursor = c - among_len(w);
                return w.result;
            }
            i = w.substring_i;
            if i < 0 {
                return 0;
            }
        }
    }

    fn replace_s(&mut self, c_bra: i32, c_ket: i32, s: &str) -> i32 {
        let n = s.chars().count() as i32;
        let adjustment = n - (c_ket - c_bra);
        let (b, k) = (c_bra as usize, c_ket as usize);
        // In place: shift the tail, then write the replacement (no temporary buffer).
        let len = self.length as usize;
        self.current.truncate(len);
        let (n, removed) = (n as usize, k - b);
        if n > removed {
            let extra = n - removed;
            self.current.resize(len + extra, '\0');
            self.current.copy_within(k..len, k + extra);
        } else if n < removed {
            self.current.copy_within(k..len, b + n);
            self.current.truncate(len - (removed - n));
        }
        for (i, c) in s.chars().enumerate() {
            self.current[b + i] = c;
        }
        self.length += adjustment;
        self.limit += adjustment;
        if self.cursor >= c_ket {
            self.cursor += adjustment;
        } else if self.cursor > c_bra {
            self.cursor = c_bra;
        }
        adjustment
    }

    pub fn slice_from(&mut self, s: &str) {
        let (b, k) = (self.bra, self.ket);
        self.replace_s(b, k, s);
    }

    pub fn insert(&mut self, c_bra: i32, c_ket: i32, s: &str) {
        let adjustment = self.replace_s(c_bra, c_ket, s);
        if c_bra <= self.bra {
            self.bra += adjustment;
        }
        if c_bra <= self.ket {
            self.ket += adjustment;
        }
    }

    /// `insert(cursor, cursor, s)`.
    pub fn insert_at_cursor(&mut self, s: &str) {
        let c = self.cursor;
        self.insert(c, c, s);
    }

    pub fn slice_del(&mut self) {
        self.slice_from("");
    }
}
