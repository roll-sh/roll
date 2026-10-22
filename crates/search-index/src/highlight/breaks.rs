//! Break iterators with Java `BreakIterator` semantics over UTF-16 offsets: sentences and
//! words (Unicode UAX #29 boundaries, which Java's rules follow closely), the value separator
//! (`\0`, Elasticsearch's multi-value join), and Elasticsearch's `BoundedBreakIteratorScanner`
//! (sentences no longer than `fragment_size`, cut at word boundaries).

use unicode_segmentation::UnicodeSegmentation;

/// `BreakIterator.DONE`.
pub const DONE: i64 = -1;

/// The content with a byte ↔ UTF-16 offset mapping.
pub struct Content<'a> {
    pub s: &'a str,
    /// `u16_of_byte[b]` for every char start byte `b` (and `s.len()`).
    u16_of: Vec<u32>,
    /// `byte_of_u16[u]` for every UTF-16 offset `u` (and the length).
    byte_of: Vec<u32>,
}

impl<'a> Content<'a> {
    pub fn new(s: &'a str) -> Content<'a> {
        let mut u16_of = vec![0u32; s.len() + 1];
        let mut byte_of = Vec::with_capacity(s.len() + 1);
        let mut u = 0u32;
        for (b, c) in s.char_indices() {
            u16_of[b] = u;
            let n = c.len_utf16() as u32;
            for _ in 0..n {
                byte_of.push(b as u32);
            }
            u += n;
        }
        u16_of[s.len()] = u;
        byte_of.push(s.len() as u32);
        Content { s, u16_of, byte_of }
    }

    /// Length in UTF-16 units.
    pub fn len16(&self) -> u32 {
        (self.byte_of.len() - 1) as u32
    }

    pub fn u16(&self, byte: usize) -> u32 {
        self.u16_of[byte.min(self.s.len())]
    }

    pub fn byte(&self, u: u32) -> usize {
        self.byte_of[(u as usize).min(self.byte_of.len() - 1)] as usize
    }

    /// The substring between UTF-16 offsets.
    pub fn slice(&self, a: u32, b: u32) -> &'a str {
        let (x, y) = (self.byte(a), self.byte(b.max(a)));
        &self.s[x..y]
    }

    pub fn char_at(&self, u: u32) -> Option<char> {
        self.s[self.byte(u)..].chars().next()
    }
}

/// Sorted boundaries (UTF-16), including 0 and the length.
#[derive(Clone, Debug)]
pub struct Boundaries {
    pub b: Vec<u32>,
    cur: usize,
}

impl Boundaries {
    pub fn new(mut b: Vec<u32>, len: u32) -> Boundaries {
        b.push(0);
        b.push(len);
        b.sort_unstable();
        b.dedup();
        Boundaries { b, cur: 0 }
    }

    /// Word boundaries as Java's `BreakIterator.getWordInstance`: UAX #29, except that dash
    /// punctuation, soft hyphens, U+2027 and `"` between letters or digits join a word (Java's
    /// `$mid_word`) and `:` doesn't (UAX #29's MidLetter).
    pub fn words(c: &Content<'_>) -> Boundaries {
        let mut b: Vec<u32> = c.s.split_word_bound_indices().map(|(i, _)| c.u16(i)).collect();
        let chars: Vec<(usize, char)> = c.s.char_indices().collect();
        let mut drop = Vec::new();
        for w in chars.windows(3) {
            let ((_, a), (i, m), (j, z)) = (w[0], w[1], w[2]);
            if !(a.is_alphanumeric() && z.is_alphanumeric()) {
                continue;
            }
            if is_java_mid_word(m) {
                drop.push(c.u16(i));
                drop.push(c.u16(j));
            } else if m == ':' {
                b.push(c.u16(i));
                b.push(c.u16(j));
            }
        }
        if !drop.is_empty() {
            b.retain(|x| !drop.contains(x));
        }
        Boundaries::new(b, c.len16())
    }

    /// Sentences, never crossing a value separator (Lucene's `SplittingBreakIterator`: a
    /// boundary before and after every separator).
    pub fn sentences(c: &Content<'_>) -> Boundaries {
        let mut b = Vec::new();
        let mut start = 0usize;
        for (i, ch) in c.s.char_indices().chain(std::iter::once((c.s.len(), '\0'))) {
            if ch == '\0' {
                let part = &c.s[start..i];
                for (j, _) in part.split_sentence_bound_indices() {
                    b.push(c.u16(start + j));
                }
                b.push(c.u16(i));
                b.push(c.u16((i + 1).min(c.s.len())));
                start = i + 1;
            }
        }
        Boundaries::new(b, c.len16())
    }

    /// Lucene's `CustomSeparatorBreakIterator`: passages are the values.
    pub fn separators(c: &Content<'_>) -> Boundaries {
        let mut b = Vec::new();
        for (i, ch) in c.s.char_indices() {
            if ch == '\0' {
                b.push(c.u16(i));
                b.push(c.u16(i) + 1);
            }
        }
        Boundaries::new(b, c.len16())
    }

    pub fn first(&mut self) -> i64 {
        self.cur = 0;
        self.b[0] as i64
    }

    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> i64 {
        if self.cur + 1 >= self.b.len() {
            return DONE;
        }
        self.cur += 1;
        self.b[self.cur] as i64
    }

    /// Last boundary < `off`.
    pub fn preceding(&mut self, off: i64) -> i64 {
        let i = self.b.partition_point(|&x| (x as i64) < off);
        if i == 0 {
            self.cur = 0;
            return DONE;
        }
        self.cur = i - 1;
        self.b[i - 1] as i64
    }

    /// First boundary > `off`.
    pub fn following(&mut self, off: i64) -> i64 {
        let i = self.b.partition_point(|&x| (x as i64) <= off);
        if i >= self.b.len() {
            self.cur = self.b.len() - 1;
            return DONE;
        }
        self.cur = i;
        self.b[i] as i64
    }

    pub fn current(&self) -> i64 {
        self.b[self.cur] as i64
    }
}

/// Java's `$mid_word` characters other than `'` and `.` (which UAX #29 already joins).
fn is_java_mid_word(c: char) -> bool {
    matches!(
        c,
        '-' | '"'
            | '\u{00ad}'
            | '\u{2027}'
            | '\u{058a}'
            | '\u{05be}'
            | '\u{1400}'
            | '\u{1806}'
            | '\u{2010}'..='\u{2015}'
            | '\u{2e17}'
            | '\u{2e1a}'
            | '\u{2e3a}'
            | '\u{2e3b}'
            | '\u{2e40}'
            | '\u{301c}'
            | '\u{3030}'
            | '\u{30a0}'
            | '\u{fe31}'
            | '\u{fe32}'
            | '\u{fe58}'
            | '\u{fe63}'
            | '\u{ff0d}'
    )
}

/// A break iterator as the highlighter uses it.
pub enum BreakIter {
    Plain(Boundaries),
    /// For value separators: `preceding` lands after a separator, `following` on one.
    Separator(Boundaries),
    Bounded(Bounded),
}

impl BreakIter {
    pub fn preceding(&mut self, off: i64) -> i64 {
        match self {
            BreakIter::Plain(b) => b.preceding(off),
            BreakIter::Separator(b) => b.preceding(off),
            BreakIter::Bounded(b) => b.preceding(off),
        }
    }
    pub fn following(&mut self, off: i64) -> i64 {
        match self {
            BreakIter::Plain(b) => b.following(off),
            BreakIter::Separator(b) => b.following(off),
            BreakIter::Bounded(b) => b.following(off),
        }
    }
    pub fn first(&mut self) -> i64 {
        match self {
            BreakIter::Plain(b) | BreakIter::Separator(b) => b.first(),
            BreakIter::Bounded(b) => {
                b.reset();
                b.main.first()
            }
        }
    }
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> i64 {
        match self {
            BreakIter::Plain(b) | BreakIter::Separator(b) => b.next(),
            BreakIter::Bounded(b) => {
                b.reset();
                b.main.next()
            }
        }
    }
}

/// Elasticsearch's `BoundedBreakIteratorScanner`, run per value as Lucene's
/// `SplittingBreakIterator` does: windows never cross a value separator.
pub struct Bounded {
    pub main: Boundaries,
    inner: Boundaries,
    max_len: i64,
    /// End of the current value (exclusive).
    len: i64,
    /// Value slices `[start, end)` of the content.
    slices: Vec<(i64, i64)>,
    slice: usize,
    last_preceding: i64,
    window_start: i64,
    window_end: i64,
    inner_start: i64,
    inner_end: i64,
}

impl Bounded {
    pub fn new(main: Boundaries, inner: Boundaries, max_len: usize, c: &Content<'_>) -> Bounded {
        let mut slices = Vec::new();
        let mut start = 0i64;
        for (i, ch) in c.s.char_indices() {
            if ch == '\0' {
                let at = c.u16(i) as i64;
                slices.push((start, at));
                start = at + 1;
            }
        }
        slices.push((start, c.len16() as i64));
        let len = slices[0].1;
        Bounded {
            main,
            inner,
            max_len: max_len as i64,
            len,
            slices,
            slice: 0,
            last_preceding: -1,
            window_start: -1,
            window_end: -1,
            inner_start: -1,
            inner_end: 0,
        }
    }

    fn reset(&mut self) {
        self.last_preceding = -1;
        self.window_start = -1;
        self.window_end = -1;
        self.inner_start = -1;
        self.inner_end = 0;
    }

    /// Moves to the value holding `offset`, resetting the scanner (a new `setText`).
    fn select(&mut self, offset: i64) {
        let (s, e) = self.slices[self.slice];
        if offset >= s && offset <= e {
            return;
        }
        let i = self.slices.partition_point(|&(_, e)| e < offset).min(self.slices.len() - 1);
        self.slice = i;
        self.len = self.slices[i].1;
        self.reset();
    }

    pub fn preceding(&mut self, offset: i64) -> i64 {
        self.select(offset);
        if offset > self.window_start && offset < self.window_end {
            self.inner_start = self.inner_end;
            self.inner_end = self.window_end;
        } else {
            self.inner_start = self.main.preceding(offset).max(self.slices[self.slice].0);
            let target = offset + 0i64.max(self.max_len - (offset - self.inner_start));
            if target + 1 > self.len {
                self.inner_end = self.len;
            } else {
                self.inner_end = self.main.preceding(target + 1);
            }
            if self.inner_start == self.inner_end {
                self.inner_end = self.main.following(target);
                if self.inner_end == DONE {
                    self.inner_end = self.len;
                }
            }
            self.window_start = self.inner_start;
            self.window_end = self.inner_end;
        }
        if self.inner_end - self.inner_start > self.max_len {
            if offset - self.max_len > self.inner_start {
                self.inner_start = self.inner_start.max(self.inner.preceding(offset - self.max_len));
            }
            let remaining = 0i64.max(self.max_len - (offset - self.inner_start));
            if offset + remaining < self.window_end {
                let f = self.inner.following(offset + remaining);
                self.inner_end = self.window_end.min(if f == DONE { self.len } else { f });
            }
        }
        self.last_preceding = offset - 1;
        self.inner_start
    }

    pub fn following(&mut self, offset: i64) -> i64 {
        offset.max(self.inner_end)
    }
}
