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
        if s.is_ascii() {
            // Bytes are UTF-16 units: the maps are the identity, kept empty.
            return Content { s, u16_of: Vec::new(), byte_of: Vec::new() };
        }
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

    fn ascii(&self) -> bool {
        self.byte_of.is_empty()
    }

    /// Length in UTF-16 units.
    pub fn len16(&self) -> u32 {
        if self.ascii() {
            return self.s.len() as u32;
        }
        (self.byte_of.len() - 1) as u32
    }

    pub fn u16(&self, byte: usize) -> u32 {
        if self.ascii() {
            return byte.min(self.s.len()) as u32;
        }
        self.u16_of[byte.min(self.s.len())]
    }

    pub fn byte(&self, u: u32) -> usize {
        if self.ascii() {
            return (u as usize).min(self.s.len());
        }
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
        if c.ascii() {
            // Sorted, distinct and strictly inside the text: only the ends to add.
            let mut b = Vec::with_capacity(c.s.len() / 4 + 2);
            b.push(0);
            ascii::words(c.s.as_bytes(), &mut b);
            if !c.s.is_empty() {
                b.push(c.len16());
            }
            return Boundaries { b, cur: 0 };
        }
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
        // A NUL byte is always the NUL char in UTF-8.
        let separators = c.s.bytes().enumerate().filter(|&(_, x)| x == 0).map(|(i, _)| i);
        for i in separators.chain(std::iter::once(c.s.len())) {
            let part = &c.s[start..i];
            if part.is_ascii() {
                ascii::sentences(part.as_bytes(), |j| b.push(c.u16(start + j)));
            } else {
                for (j, _) in part.split_sentence_bound_indices() {
                    b.push(c.u16(start + j));
                }
            }
            b.push(c.u16(i));
            b.push(c.u16((i + 1).min(c.s.len())));
            start = i + 1;
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
pub enum BreakIter<'c> {
    Plain(Boundaries),
    /// For value separators: `preceding` lands after a separator, `following` on one.
    Separator(Boundaries),
    Bounded(Bounded<'c>),
}

impl BreakIter<'_> {
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
pub struct Bounded<'c> {
    pub main: Boundaries,
    /// Word boundaries, built on first use: only sentences longer than `max_len` need them.
    inner: Option<Boundaries>,
    content: &'c Content<'c>,
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

impl<'c> Bounded<'c> {
    pub fn new(main: Boundaries, max_len: usize, c: &'c Content<'c>) -> Bounded<'c> {
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
            inner: None,
            content: c,
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
            let content = self.content;
            let inner = self.inner.get_or_insert_with(|| Boundaries::words(content));
            if offset - self.max_len > self.inner_start {
                self.inner_start = self.inner_start.max(inner.preceding(offset - self.max_len));
            }
            let remaining = 0i64.max(self.max_len - (offset - self.inner_start));
            if offset + remaining < self.window_end {
                let f = inner.following(offset + remaining);
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

/// UAX #29 word and sentence boundaries of ASCII text, without the generic segmenter's
/// per-char table lookups (most highlighted text is ASCII, and segmenting was a third of the
/// unified highlighter's time). Both follow the rules as `unicode-segmentation` implements
/// them for the ASCII subset, which the tests check exhaustively over short strings and on
/// random longer ones.
mod ascii {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum W {
        Cr,
        Lf,
        Newline,
        Space,
        Letter,
        Numeric,
        MidLetter,
        MidNum,
        MidNumLet,
        SingleQuote,
        ExtendNumLet,
        Other,
    }

    /// Word classes of the ASCII bytes, looked up instead of matched (a few per byte).
    const W_OF: [W; 128] = {
        let mut t = [W::Other; 128];
        let mut b = 0;
        while b < 128 {
            t[b] = w_slow(b as u8);
            b += 1;
        }
        t
    };

    #[inline]
    fn w(b: u8) -> W {
        W_OF[(b & 0x7f) as usize]
    }

    const fn w_slow(b: u8) -> W {
        match b {
            b'\r' => W::Cr,
            b'\n' => W::Lf,
            0x0b | 0x0c => W::Newline,
            b' ' => W::Space,
            b'a'..=b'z' | b'A'..=b'Z' => W::Letter,
            b'0'..=b'9' => W::Numeric,
            b':' => W::MidLetter,
            b',' | b';' => W::MidNum,
            b'.' => W::MidNumLet,
            b'\'' => W::SingleQuote,
            b'_' => W::ExtendNumLet,
            _ => W::Other,
        }
    }

    /// Whether a word boundary sits before `s[i]` (`0 < i < s.len()`).
    fn word_break(s: &[u8], i: usize) -> bool {
        let (p, c) = (w(s[i - 1]), w(s[i]));
        let before = || if i >= 2 { Some(w(s[i - 2])) } else { None };
        let after = || s.get(i + 1).map(|&b| w(b));
        let mid_letter = |x: W| matches!(x, W::MidLetter | W::MidNumLet | W::SingleQuote);
        let mid_num = |x: W| matches!(x, W::MidNum | W::MidNumLet | W::SingleQuote);
        match (p, c) {
            (W::Cr, W::Lf) => false,
            (W::Cr | W::Lf | W::Newline, _) | (_, W::Cr | W::Lf | W::Newline) => true,
            (W::Space, W::Space) => false,
            (W::Letter | W::Numeric, W::Letter | W::Numeric) => false,
            (W::Letter, m) if mid_letter(m) && after() == Some(W::Letter) => false,
            (m, W::Letter) if mid_letter(m) && before() == Some(W::Letter) => false,
            (W::Numeric, m) if mid_num(m) && after() == Some(W::Numeric) => false,
            (m, W::Numeric) if mid_num(m) && before() == Some(W::Numeric) => false,
            (W::Letter | W::Numeric | W::ExtendNumLet, W::ExtendNumLet) => false,
            (W::ExtendNumLet, W::Letter | W::Numeric) => false,
            _ => true,
        }
    }

    /// [`super::Boundaries::words`] of ASCII text: UAX #29 boundaries with Java's `$mid_word`
    /// (`-`, `"`) joining and `:` splitting between letters or digits.
    pub fn words(s: &[u8], b: &mut Vec<u32>) {
        for i in 1..s.len() {
            // Inside a run of letters and digits, or of spaces (the common positions): no
            // boundary, whatever surrounds the run.
            let (p, c) = (w(s[i - 1]), w(s[i]));
            if matches!((p, c), (W::Letter | W::Numeric, W::Letter | W::Numeric) | (W::Space, W::Space)) {
                continue;
            }
            let mid = i + 1 < s.len() && s[i - 1].is_ascii_alphanumeric() && s[i + 1].is_ascii_alphanumeric();
            let java_mid = |x: u8| x == b'-' || x == b'"';
            // `a-b`: no boundary before or after the `-`; `a:b`: both.
            let joined = (mid && java_mid(s[i])) || (i >= 2 && java_mid(s[i - 1]) && s[i - 2].is_ascii_alphanumeric() && s[i].is_ascii_alphanumeric());
            let split = (mid && s[i] == b':') || (i >= 2 && s[i - 1] == b':' && s[i - 2].is_ascii_alphanumeric() && s[i].is_ascii_alphanumeric());
            if !joined && (split || word_break(s, i)) {
                b.push(i as u32);
            }
        }
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Sb {
        Cr,
        Lf,
        Sp,
        Lower,
        Upper,
        Numeric,
        ATerm,
        STerm,
        Close,
        SContinue,
        Other,
    }

    const S_OF: [Sb; 128] = {
        let mut t = [Sb::Other; 128];
        let mut b = 0;
        while b < 128 {
            t[b] = sc_slow(b as u8);
            b += 1;
        }
        t
    };

    #[inline]
    fn sc(b: u8) -> Sb {
        S_OF[(b & 0x7f) as usize]
    }

    const fn sc_slow(b: u8) -> Sb {
        match b {
            b'\r' => Sb::Cr,
            b'\n' => Sb::Lf,
            b'\t' | 0x0b | 0x0c | b' ' => Sb::Sp,
            b'a'..=b'z' => Sb::Lower,
            b'A'..=b'Z' => Sb::Upper,
            b'0'..=b'9' => Sb::Numeric,
            b'.' => Sb::ATerm,
            b'!' | b'?' => Sb::STerm,
            b'"' | b'\'' | b'(' | b')' | b'[' | b']' | b'{' | b'}' => Sb::Close,
            b',' | b'-' | b':' | b';' => Sb::SContinue,
            _ => Sb::Other,
        }
    }

    /// Whether a sentence boundary sits before `s[i]` (`0 < i < s.len()`).
    fn sentence_break(s: &[u8], i: usize) -> bool {
        let (p, c) = (sc(s[i - 1]), sc(s[i]));
        let para = |x: Sb| matches!(x, Sb::Cr | Sb::Lf);
        if p == Sb::Cr && c == Sb::Lf {
            return false;
        }
        if para(p) {
            return true;
        }
        if p == Sb::ATerm && c == Sb::Numeric {
            return false;
        }
        if p == Sb::ATerm && c == Sb::Upper && i >= 2 && matches!(sc(s[i - 2]), Sb::Upper | Sb::Lower) {
            return false;
        }
        // `SATerm Close* Sp*` ending before `i`.
        let mut k = i;
        while k > 0 && sc(s[k - 1]) == Sb::Sp {
            k -= 1;
        }
        let spaces = k < i;
        while k > 0 && sc(s[k - 1]) == Sb::Close {
            k -= 1;
        }
        let term = match k.checked_sub(1).map(|k| sc(s[k])) {
            Some(t @ (Sb::ATerm | Sb::STerm)) => t,
            _ => return false,
        };
        if term == Sb::ATerm {
            let next = s[i..].iter().map(|&b| sc(b)).find(|x| matches!(x, Sb::Upper | Sb::Lower | Sb::Cr | Sb::Lf | Sb::ATerm | Sb::STerm));
            if next == Some(Sb::Lower) {
                return false;
            }
        }
        if matches!(c, Sb::SContinue | Sb::ATerm | Sb::STerm) {
            return false;
        }
        if !spaces && matches!(c, Sb::Close | Sb::Sp | Sb::Cr | Sb::Lf) {
            return false;
        }
        !matches!(c, Sb::Sp | Sb::Cr | Sb::Lf)
    }

    /// The start of every sentence of ASCII text (`split_sentence_bound_indices`).
    pub fn sentences(s: &[u8], mut push: impl FnMut(usize)) {
        if s.is_empty() {
            return;
        }
        push(0);
        // Every boundary follows a terminator or a paragraph separator (SB4, SB11).
        if !s.iter().any(|&b| matches!(b, b'.' | b'!' | b'?' | b'\r' | b'\n')) {
            return;
        }
        for i in 1..s.len() {
            if sentence_break(s, i) {
                push(i);
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::super::{Boundaries, Content};
        use rand::{Rng, SeedableRng, rngs::StdRng};
        use unicode_segmentation::UnicodeSegmentation;

        /// The generic path, as it ran before the ASCII fast path (the oracle).
        fn generic_words(s: &str) -> Vec<u32> {
            let mut b: Vec<u32> = s.split_word_bound_indices().map(|(i, _)| i as u32).collect();
            let chars: Vec<(usize, char)> = s.char_indices().collect();
            let mut drop = Vec::new();
            for w in chars.windows(3) {
                let ((_, a), (i, m), (j, z)) = (w[0], w[1], w[2]);
                if !(a.is_alphanumeric() && z.is_alphanumeric()) {
                    continue;
                }
                if super::super::is_java_mid_word(m) {
                    drop.push(i as u32);
                    drop.push(j as u32);
                } else if m == ':' {
                    b.push(i as u32);
                    b.push(j as u32);
                }
            }
            b.retain(|x| !drop.contains(x));
            Boundaries::new(b, s.len() as u32).b
        }

        fn generic_sentences(s: &str) -> Vec<usize> {
            s.split_sentence_bound_indices().map(|(i, _)| i).collect()
        }

        fn check(s: &str) {
            let c = Content::new(s);
            assert_eq!(Boundaries::words(&c).b, generic_words(s), "words of {s:?}");
            let mut fast = Vec::new();
            super::sentences(s.as_bytes(), |i| fast.push(i));
            assert_eq!(fast, generic_sentences(s), "sentences of {s:?}");
        }

        /// Every class of both rule sets is represented.
        const ALPHABET: &[u8] = b"aZ5 .!?:;,-'\"_()\r\n\t\x0b\x0c\0#";

        #[test]
        fn ascii_boundaries_match_the_generic_segmenter_exhaustively() {
            // Every string of up to 4 symbols of the alphabet (~390k strings).
            let n = ALPHABET.len();
            for len in 0..=4u32 {
                for mut code in 0..n.pow(len) {
                    let mut s = String::new();
                    for _ in 0..len {
                        s.push(ALPHABET[code % n] as char);
                        code /= n;
                    }
                    check(&s);
                }
            }
        }

        /// `cargo test --release -p search-index --lib bench_ascii_boundaries -- --ignored --nocapture`
        #[test]
        #[ignore]
        fn bench_ascii_boundaries() {
            let prose = "The quick brown fox jumps over the lazy dog. It was not amused! Was it? Maybe, maybe not: the dog slept on.";
            let words = "w484 w3 w506 w1 w28 w4 w0 w1480 w1539 w514 w4066 w1 w3712 w0 w670 w2813 w2 w34 w1245 w10 w18 w2570 w0 w563 w3883 w1 w2 w3650 w0 w1";
            for text in [prose, words] {
                let c = Content::new(text);
                let n = 20_000;
                let t = std::time::Instant::now();
                for _ in 0..n {
                    std::hint::black_box(Boundaries::words(std::hint::black_box(&c)));
                    std::hint::black_box(Boundaries::sentences(std::hint::black_box(&c)));
                }
                let fast = t.elapsed() / n;
                let t = std::time::Instant::now();
                for _ in 0..n {
                    std::hint::black_box(Boundaries::words(std::hint::black_box(&c)));
                }
                let w = t.elapsed() / n;
                let t = std::time::Instant::now();
                for _ in 0..n {
                    let mut b = Vec::new();
                    super::words(std::hint::black_box(text.as_bytes()), &mut b);
                    std::hint::black_box(b);
                }
                println!("words {w:?}, ascii::words {:?}", t.elapsed() / n);
                let t = std::time::Instant::now();
                for _ in 0..n {
                    std::hint::black_box(generic_words(std::hint::black_box(text)));
                    std::hint::black_box(generic_sentences(std::hint::black_box(text)));
                }
                let slow = t.elapsed() / n;
                println!("{} chars: words + sentences {fast:?} (generic segmenter {slow:?})", text.len());
            }
        }

        #[test]
        fn ascii_boundaries_match_the_generic_segmenter_on_random_text() {
            let mut rng = StdRng::seed_from_u64(29);
            for _ in 0..100_000 {
                let len = rng.gen_range(0..40);
                let s: String = (0..len)
                    .map(|_| if rng.gen_bool(0.3) { ALPHABET[rng.gen_range(0..ALPHABET.len())] as char } else { rng.gen_range(0x00u8..0x80) as char })
                    .collect();
                check(&s);
            }
            for s in ["The quick brown fox. Jumps over! The lazy dog? yes.", "e.g. this U.S.A. 3.14 sentence.\r\nNext (one.) \"Quoted.\" end", "a-b c:d e'f g.h 1,000.5 x_y"] {
                check(s);
            }
        }
    }
}
