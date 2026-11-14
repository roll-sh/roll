//! The Snowball runtime with an owned, reusable buffer, and the Snowball 2.1 algorithms the
//! `stemmer`/`snowball` filters and the language analyzers use (from the in-repo `snowball`
//! crate: the Snowball project's generated Rust, BSD-3-Clause, see below).
//!
//! The crate's runtime keeps the word in a `Cow` and builds a new `String` for every
//! replacement, so stemming allocated whenever a rule fired. Here the environment owns one
//! `String` that the stem filter swaps with the token's term buffer: replacements edit it in
//! place (`replace_range`), and the slices two algorithms save (`S_ch`, `S_x`) live in an
//! inline buffer, so stemming does not allocate once the buffer has grown to the longest word.
//! The algorithm files are the crate's, with only their `use` lines and those two slice
//! variables changed, plus the three places (in `dutch` and `serbian`) that compare a cursor
//! with a literal position, which go through [`SnowballEnv::unit_pos`] so they count UTF-16
//! units as Lucene's Java does rather than bytes.
//!
//! ```text
//! Copyright (c) 2001, Dr Martin Porter
//! Copyright (c) 2004,2005, Richard Boulton
//! Copyright (c) 2013, Yoshiki Shibukawa
//! Copyright (c) 2006,2007,2009,2010,2011,2014-2019, Olly Betts
//! All rights reserved.
//!
//! Redistribution and use in source and binary forms, with or without
//! modification, are permitted provided that the following conditions
//! are met:
//!
//!   1. Redistributions of source code must retain the above copyright notice,
//!      this list of conditions and the following disclaimer.
//!   2. Redistributions in binary form must reproduce the above copyright notice,
//!      this list of conditions and the following disclaimer in the documentation
//!      and/or other materials provided with the distribution.
//!   3. Neither the name of the Snowball project nor the names of its contributors
//!      may be used to endorse or promote products derived from this software
//!      without specific prior written permission.
//!
//! THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS" AND
//! ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE IMPLIED
//! WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE
//! DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT OWNER OR CONTRIBUTORS BE LIABLE FOR
//! ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES
//! (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES;
//! LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON
//! ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT
//! (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE OF THIS
//! SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.
//! ```

#![allow(
    clippy::all,
    clippy::pedantic,
    dead_code,
    unused_parens,
    non_snake_case,
    non_upper_case_globals,
    unused_variables,
    unused_mut,
    unused_assignments,
    unused_imports
)]

pub(crate) mod arabic;
pub(crate) mod armenian;
pub(crate) mod basque;
pub(crate) mod catalan;
pub(crate) mod danish;
pub(crate) mod dutch;
pub(crate) mod english;
pub(crate) mod finnish;
pub(crate) mod french;
pub(crate) mod greek;
pub(crate) mod hindi;
pub(crate) mod hungarian;
pub(crate) mod indonesian;
pub(crate) mod irish;
pub(crate) mod italian;
pub(crate) mod lithuanian;
pub(crate) mod nepali;
pub(crate) mod norwegian;
pub(crate) mod portuguese;
pub(crate) mod russian;
pub(crate) mod serbian;
pub(crate) mod spanish;
pub(crate) mod tamil;
pub(crate) mod turkish;
pub(crate) mod yiddish;

pub(crate) struct Among<T: 'static>(
    pub &'static str,
    pub i32,
    pub i32,
    pub Option<&'static (dyn Fn(&mut SnowballEnv, &mut T) -> bool + Sync)>,
);

/// A saved slice of the word (`slice_to`): inline up to 32 bytes.
#[derive(Default, Clone)]
pub(crate) struct SliceBuf {
    buf: [u8; 32],
    len: usize,
    heap: Option<String>,
}

impl SliceBuf {
    pub fn as_str(&self) -> &str {
        match &self.heap {
            Some(s) => s,
            None => std::str::from_utf8(&self.buf[..self.len]).unwrap_or(""),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.as_str().is_empty()
    }

    fn set(&mut self, s: &str) {
        if s.len() <= self.buf.len() {
            self.buf[..s.len()].copy_from_slice(s.as_bytes());
            self.len = s.len();
            self.heap = None;
        } else {
            self.heap = Some(s.to_string());
        }
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct SnowballEnv {
    pub current: String,
    pub cursor: i32,
    pub limit: i32,
    pub limit_backward: i32,
    pub bra: i32,
    pub ket: i32,
}

impl SnowballEnv {
    /// Starts on the word in `current` (which the caller has filled).
    pub fn reset(&mut self) {
        let len = self.current.len() as i32;
        self.cursor = 0;
        self.limit = len;
        self.limit_backward = 0;
        self.bra = 0;
        self.ket = len;
    }

    fn replace_s(&mut self, bra: i32, ket: i32, s: &str) -> i32 {
        let adjustment = s.len() as i32 - (ket - bra);
        self.current.replace_range(bra as usize..ket as usize, s);
        self.limit += adjustment;
        if self.cursor >= ket {
            self.cursor += adjustment;
        } else if self.cursor > bra {
            self.cursor = bra
        }
        adjustment
    }

    /// Check if s is after cursor.
    /// If so, move cursor to the end of s
    pub fn eq_s(&mut self, s: &str) -> bool {
        if self.cursor >= self.limit {
            return false;
        }
        if self.current[(self.cursor as usize)..].starts_with(s) {
            self.cursor += s.len() as i32;
            while !self.current.is_char_boundary(self.cursor as usize) {
                self.cursor += 1;
            }
            true
        } else {
            false
        }
    }

    /// Check if 's' is before cursor
    /// If so, move cursor to the beginning of s
    pub fn eq_s_b(&mut self, s: &str) -> bool {
        if (self.cursor - self.limit_backward) < s.len() as i32 {
            false
            // Check if cursor -s.len is a char boundary. if not well... return false obv
        } else if !self
            .current
            .is_char_boundary(self.cursor as usize - s.len())
            || !self.current[self.cursor as usize - s.len()..].starts_with(s)
        {
            false
        } else {
            self.cursor -= s.len() as i32;
            true
        }
    }

    /// Replace string between `bra` and `ket` with s
    pub fn slice_from(&mut self, s: &str) -> bool {
        let (bra, ket) = (self.bra, self.ket);
        self.replace_s(bra, ket, s);
        true
    }

    /// Move cursor to next character
    pub fn next_char(&mut self) {
        self.cursor += 1;
        while !self.current.is_char_boundary(self.cursor as usize) {
            self.cursor += 1;
        }
    }

    /// Move cursor to previous character
    pub fn previous_char(&mut self) {
        self.cursor -= 1;
        while !self.current.is_char_boundary(self.cursor as usize) {
            self.cursor -= 1;
        }
    }

    /// The cursor value of UTF-16 unit `n` from the start (past the end, `len + n - units`).
    /// Snowball programs compare cursors with literal positions (`$p1 < 3`), which Lucene's
    /// Java code counts in UTF-16 units and this runtime keeps as byte offsets; supplementary
    /// characters reach the stemmers as two stand-in characters, so a unit is a character.
    pub fn unit_pos(&self, n: i32) -> i32 {
        let mut left = n;
        for (i, _) in self.current.char_indices() {
            if left == 0 {
                return i as i32;
            }
            left -= 1;
        }
        self.current.len() as i32 + left
    }

    pub fn hop(&mut self, mut delta: i32) -> bool {
        let mut res = self.cursor;
        while delta > 0 {
            delta -= 1;
            if res >= self.limit {
                return false;
            }
            res += 1;
            while res < self.limit && !self.current.is_char_boundary(res as usize) {
                res += 1;
            }
        }
        self.cursor = res;
        return true;
    }

    pub fn hop_checked(&mut self, delta: i32) -> bool {
        return delta >= 0 && self.hop(delta);
    }

    pub fn hop_back(&mut self, mut delta: i32) -> bool {
        let mut res = self.cursor;
        while delta > 0 {
            delta -= 1;
            if res <= self.limit_backward {
                return false;
            }
            res -= 1;
            while res > self.limit_backward && !self.current.is_char_boundary(res as usize) {
                res -= 1;
            }
        }
        self.cursor = res;
        return true;
    }

    pub fn hop_back_checked(&mut self, delta: i32) -> bool {
        return delta >= 0 && self.hop_back(delta);
    }

    // A grouping is represented by a minimum code point, a maximum code point,
    // and a bitfield of which code points in that range are in the grouping.
    // For example, in english.sbl, valid_LI is 'cdeghkmnrt'.
    // The minimum and maximum code points are 99 and 116,
    // so every time one of these grouping functions is called for g_valid_LI,
    // min must be 99 and max must be 116. There are 18 code points within that
    // range (inclusive) so the grouping is represented with 18 bits, plus 6 bits of padding:
    //
    // cdefghij klmnopqr st
    // 11101100 10110001 01000000
    //
    // The first bit is the least significant.
    // Those three bytes become &[0b00110111, 0b10001101, 0b00000010],
    // which is &[55, 141, 2], which is how g_valid_LI is defined in english.rs.
    /// Check if the char the cursor points to is in the grouping
    pub fn in_grouping(&mut self, chars: &[u8], min: u32, max: u32) -> bool {
        if self.cursor >= self.limit {
            return false;
        }
        if let Some(chr) = self.current[self.cursor as usize..].chars().next() {
            let mut ch = chr as u32; //codepoint as integer
            if ch > max || ch < min {
                return false;
            }
            ch -= min;
            if (chars[(ch >> 3) as usize] & (0x1 << (ch & 0x7))) == 0 {
                return false;
            }
            self.next_char();
            return true;
        }
        return false;
    }

    pub fn in_grouping_b(&mut self, chars: &[u8], min: u32, max: u32) -> bool {
        if self.cursor <= self.limit_backward {
            return false;
        }
        self.previous_char();
        if let Some(chr) = self.current[self.cursor as usize..].chars().next() {
            let mut ch = chr as u32; //codepoint as integer
            self.next_char();
            if ch > max || ch < min {
                return false;
            }
            ch -= min;
            if (chars[(ch >> 3) as usize] & (0x1 << (ch & 0x7))) == 0 {
                return false;
            }
            self.previous_char();
            return true;
        }
        return false;
    }

    pub fn out_grouping(&mut self, chars: &[u8], min: u32, max: u32) -> bool {
        if self.cursor >= self.limit {
            return false;
        }
        if let Some(chr) = self.current[self.cursor as usize..].chars().next() {
            let mut ch = chr as u32; //codepoint as integer
            if ch > max || ch < min {
                self.next_char();
                return true;
            }
            ch -= min;
            if (chars[(ch >> 3) as usize] & (0x1 << (ch & 0x7))) == 0 {
                self.next_char();
                return true;
            }
        }
        return false;
    }

    pub fn out_grouping_b(&mut self, chars: &[u8], min: u32, max: u32) -> bool {
        if self.cursor <= self.limit_backward {
            return false;
        }
        self.previous_char();
        if let Some(chr) = self.current[self.cursor as usize..].chars().next() {
            let mut ch = chr as u32; //codepoint as integer
            self.next_char();
            if ch > max || ch < min {
                self.previous_char();
                return true;
            }
            ch -= min;
            if (chars[(ch >> 3) as usize] & (0x1 << (ch & 0x7))) == 0 {
                self.previous_char();
                return true;
            }
        }
        return false;
    }

    /// Helper function that removes the string slice between `bra` and `ket`
    pub fn slice_del(&mut self) -> bool {
        self.slice_from("")
    }

    pub fn insert(&mut self, bra: i32, ket: i32, s: &str) {
        let adjustment = self.replace_s(bra, ket, s);
        if bra <= self.bra {
            self.bra = self.bra + adjustment;
        }
        if bra <= self.ket {
            self.ket = self.ket + adjustment;
        }
    }

    pub fn slice_to(&mut self) -> SliceBuf {
        let mut b = SliceBuf::default();
        b.set(&self.current[self.bra as usize..self.ket as usize]);
        b
    }

    pub fn find_among<T>(&mut self, amongs: &[Among<T>], context: &mut T) -> i32 {
        use std::cmp::min;
        let mut i: i32 = 0;
        let mut j: i32 = amongs.len() as i32;

        let c = self.cursor;
        let l = self.limit;

        let mut common_i = 0i32;
        let mut common_j = 0i32;

        let mut first_key_inspected = false;
        loop {
            let k = i + ((j - i) >> 1);
            let mut diff: i32 = 0;
            let mut common = min(common_i, common_j);
            let w = &amongs[k as usize];
            for lvar in common..w.0.len() as i32 {
                if c + common == l {
                    diff = -1;
                    break;
                }
                diff = self.current.as_bytes()[(c + common) as usize] as i32
                    - w.0.as_bytes()[lvar as usize] as i32;
                if diff != 0 {
                    break;
                }
                common += 1;
            }
            if diff < 0 {
                j = k;
                common_j = common;
            } else {
                i = k;
                common_i = common;
            }
            if j - i <= 1 {
                if i > 0 {
                    break;
                }
                if j == i {
                    break;
                }
                if first_key_inspected {
                    break;
                }
                first_key_inspected = true;
            }
        }

        loop {
            let w = &amongs[i as usize];
            if common_i >= w.0.len() as i32 {
                self.cursor = c + w.0.len() as i32;
                if let Some(ref method) = w.3 {
                    let res = method(self, context);
                    self.cursor = c + w.0.len() as i32;
                    if res {
                        return w.2;
                    }
                } else {
                    return w.2;
                }
            }
            i = w.1;
            if i < 0 {
                return 0;
            }
        }
    }

    pub fn find_among_b<T>(&mut self, amongs: &[Among<T>], context: &mut T) -> i32 {
        let mut i: i32 = 0;
        let mut j: i32 = amongs.len() as i32;

        let c = self.cursor;
        let lb = self.limit_backward;

        let mut common_i = 0i32;
        let mut common_j = 0i32;

        let mut first_key_inspected = false;

        loop {
            let k = i + ((j - i) >> 1);
            let mut diff: i32 = 0;
            let mut common = if common_i < common_j {
                common_i
            } else {
                common_j
            };
            let w = &amongs[k as usize];
            for lvar in (0..w.0.len() - common as usize).rev() {
                if c - common == lb {
                    diff = -1;
                    break;
                }
                diff = self.current.as_bytes()[(c - common - 1) as usize] as i32
                    - w.0.as_bytes()[lvar] as i32;
                if diff != 0 {
                    break;
                }
                // Count up commons. But not one character but the byte width of that char
                common += 1;
            }
            if diff < 0 {
                j = k;
                common_j = common;
            } else {
                i = k;
                common_i = common;
            }
            if j - i <= 1 {
                if i > 0 {
                    break;
                }
                if j == i {
                    break;
                }
                if first_key_inspected {
                    break;
                }
                first_key_inspected = true;
            }
        }
        loop {
            let w = &amongs[i as usize];
            if common_i >= w.0.len() as i32 {
                self.cursor = c - w.0.len() as i32;
                if let Some(ref method) = w.3 {
                    let res = method(self, context);
                    self.cursor = c - w.0.len() as i32;
                    if res {
                        return w.2;
                    }
                } else {
                    return w.2;
                }
            }
            i = w.1;
            if i < 0 {
                return 0;
            }
        }
    }
}
