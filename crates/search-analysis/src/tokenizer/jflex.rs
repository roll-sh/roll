//! The input buffer of Lucene's JFlex scanners.
//!
//! `StandardTokenizer` and `UAX29URLEmailTokenizer` set their scanner's buffer size to
//! `max_token_length` and use JFlex's skeleton without buffer expansion, so a token can never be
//! longer than the buffer: when a match reaches the end of a full buffer, the refill reports end
//! of input and the scanner returns the longest match so far (that is how long tokens are cut).
//! The refill has one more observable quirk: when exactly one buffer slot is free and the next
//! character is a supplementary one, the high surrogate cannot be stored with its pair, so the
//! refill holds it back and reports end of input; at the start of a token that ends the whole
//! stream (with `max_token_length: 1`, the first emoji ends tokenization). This module replays
//! the buffer bookkeeping (in UTF-16 units, over the in-memory text, as a `StringReader` would
//! deliver it) so scanners stop exactly where Lucene's do.

/// What one scan produced.
pub(crate) enum Scan {
    /// The longest match: (rule, end byte, UTF-16 length).
    Token(usize, usize, usize),
    /// No rule matched the next character (the grammar's catch-all skips it).
    NoMatch,
    /// End of the stream (JFlex's `zzAtEOF`).
    Eof,
}

/// The scanner's buffer state, in absolute UTF-16 offsets of the text.
pub(crate) struct JflexBuffer {
    cap: usize,
    /// Text offset of `zzBuffer[0]`.
    buf_start: usize,
    /// Text offset of `zzEndRead`.
    end_read: usize,
    /// `zzFinalHighSurrogate`: a high surrogate read but held back past `end_read`.
    final_high: bool,
    /// The reader's position: byte of the next character to read, and whether only its low
    /// surrogate remains to be read.
    read_b: usize,
    read_mid: bool,
    at_eof: bool,
}

impl JflexBuffer {
    pub fn new(cap: usize) -> JflexBuffer {
        JflexBuffer {
            cap: cap.max(1),
            buf_start: 0,
            end_read: 0,
            final_high: false,
            read_b: 0,
            read_mid: false,
            at_eof: false,
        }
    }

    pub fn reset(&mut self) {
        *self = JflexBuffer::new(self.cap);
    }

    /// `zzRefill()` while the current token starts at text offset `start`; true means end of
    /// input for this scan.
    fn refill(&mut self, text: &str, start: usize) -> bool {
        if start > self.buf_start {
            self.end_read += self.final_high as usize;
            self.final_high = false;
            self.buf_start = start;
        }
        let used = self.end_read - self.buf_start + self.final_high as usize;
        let requested = self.cap.saturating_sub(used);
        if requested == 0 {
            return true;
        }
        let mut num = 0usize;
        let mut split = false;
        if self.read_mid {
            // The low surrogate of a held-back pair.
            num = 1;
            self.read_mid = false;
            self.read_b += 4;
        }
        let bytes = text.as_bytes();
        while num < requested && self.read_b < bytes.len() {
            let b = bytes[self.read_b];
            if b < 0x80 {
                num += 1;
                self.read_b += 1;
                continue;
            }
            let c = text[self.read_b..].chars().next().unwrap_or('\0');
            if c.len_utf16() == 2 {
                if num + 2 > requested {
                    // Only the high surrogate fits.
                    num += 1;
                    self.read_mid = true;
                    split = true;
                    break;
                }
                num += 2;
            } else {
                num += 1;
            }
            self.read_b += c.len_utf8();
        }
        if num == 0 {
            return true;
        }
        self.end_read += num;
        if split {
            self.end_read -= 1;
            self.final_high = true;
            if num == 1 {
                return true;
            }
        }
        false
    }

    /// Scans the longest match at byte `pos` / text offset `upos` with an automaton given by its
    /// transition and accept functions (state 1 is the start, 0 is dead).
    #[inline]
    pub fn scan(
        &mut self,
        text: &str,
        pos: usize,
        upos: usize,
        mut step: impl FnMut(u32, char) -> u32,
        accept: impl Fn(u32) -> Option<usize>,
    ) -> Scan {
        if self.at_eof {
            return Scan::Eof;
        }
        let bytes = text.as_bytes();
        let mut state = crate::dfa::START;
        let mut best = None;
        let (mut cb, mut cu) = (pos, upos);
        let mut eof = false;
        loop {
            if cu >= self.end_read && self.refill(text, upos) {
                eof = true;
                break;
            }
            if cb >= bytes.len() {
                eof = true;
                break;
            }
            let b = bytes[cb];
            let (c, len, ulen) = if b < 0x80 {
                (b as char, 1, 1)
            } else {
                let c = text[cb..].chars().next().unwrap_or('\u{FFFD}');
                (c, c.len_utf8(), c.len_utf16())
            };
            state = step(state, c);
            cb += len;
            cu += ulen;
            if state == crate::dfa::DEAD {
                break;
            }
            if let Some(rule) = accept(state) {
                best = Some((rule, cb, cu - upos));
            }
        }
        match best {
            Some((rule, end, ulen)) => Scan::Token(rule, end, ulen),
            None if eof && cu == upos => {
                self.at_eof = true;
                Scan::Eof
            }
            None => Scan::NoMatch,
        }
    }
}
