//! Streaming record splitting (CSV and JSON Lines) and CSV field parsing.

use memchr::memmem;

use crate::error::{Result, SelectError};
use crate::eval::{Fields, MAX_RECORD_BYTES};

/// What the consumer of a record wants next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Flow {
    Continue,
    /// The output buffer is full: stop after this record until it is drained.
    Pause,
    /// No more input is needed (LIMIT reached, or past the scan range).
    Done,
}

pub(crate) const BOM: &[u8] = b"\xEF\xBB\xBF";

/// Quote-aware splitting (AllowQuotedRecordDelimiter): a record delimiter inside a quoted field
/// doesn't end the record.
#[derive(Clone, Debug)]
pub(crate) struct Quoting {
    pub fd: Vec<u8>,
    pub quote: u8,
    pub esc: u8,
    pub comment: Option<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum QState {
    FieldStart,
    Unquoted,
    Quoted,
    QuotedEsc,
    /// A quote inside a quoted field: a doubled quote or the closing one.
    QuoteSeen,
    /// A comment line: only the record delimiter matters.
    Comment,
}

/// Splits a byte stream into records at a 1–8 byte delimiter, across arbitrary chunk boundaries.
/// A record is at most [`MAX_RECORD_BYTES`] (OverMaxRecordSize); only an incomplete record is
/// ever buffered.
pub(crate) struct Splitter {
    rd: Vec<u8>,
    quoting: Option<Quoting>,
    /// The delimiter is `\n`: also accept `\r\n`.
    strip_cr: bool,
    buf: Vec<u8>,
    /// Absolute offset of the next input byte.
    pos: u64,
    /// Absolute offset where the record in progress starts.
    rec_start: u64,
    qs: QState,
    /// The scan range's end: once the next record would start after it, the input is done.
    stop_after: u64,
}

fn over_max() -> SelectError {
    SelectError::over_max_record()
}

impl Splitter {
    /// `first` is the absolute offset of the first byte fed; the record in progress there is
    /// taken to start at `rec_start`.
    pub fn new(rd: &[u8], quoting: Option<Quoting>, first: u64, rec_start: u64, stop_after: u64) -> Splitter {
        Splitter {
            strip_cr: rd == b"\n",
            rd: rd.to_vec(),
            quoting,
            buf: Vec::new(),
            pos: first,
            rec_start,
            qs: QState::FieldStart,
            stop_after,
        }
    }

    fn push(&mut self, data: &[u8]) -> Result<()> {
        if self.buf.len() + data.len() > MAX_RECORD_BYTES + self.rd.len() + 1 {
            return Err(over_max());
        }
        self.buf.extend_from_slice(data);
        Ok(())
    }

    fn strip<'a>(&self, rec: &'a [u8]) -> Result<&'a [u8]> {
        let rec = if self.strip_cr && rec.last() == Some(&b'\r') { &rec[..rec.len() - 1] } else { rec };
        if rec.len() > MAX_RECORD_BYTES {
            return Err(over_max());
        }
        Ok(rec)
    }

    /// After a record ended at absolute offset `next` (its delimiter included).
    fn after(&mut self, next: u64, flow: Flow) -> Flow {
        self.rec_start = next;
        if next > self.stop_after { Flow::Done } else { flow }
    }

    /// Feed bytes; `f` gets each complete record (without its delimiter) and its start offset.
    /// Returns how much of `data` was consumed (all of it unless `f` paused or finished).
    pub fn feed(&mut self, data: &[u8], f: &mut dyn FnMut(&[u8], u64) -> Result<Flow>) -> Result<(usize, Flow)> {
        if self.quoting.is_some() { self.feed_quoted(data, f) } else { self.feed_plain(data, f) }
    }

    fn feed_plain(&mut self, data: &[u8], f: &mut dyn FnMut(&[u8], u64) -> Result<Flow>) -> Result<(usize, Flow)> {
        let base = self.pos;
        let rdl = self.rd.len();
        let finder = memmem::Finder::new(&self.rd).into_owned();
        let mut i = 0;
        while i < data.len() {
            // A delimiter split between the buffered bytes and these.
            if !self.buf.is_empty() && rdl > 1 {
                let rest = &data[i..];
                let hit = (1..rdl).find(|&k| k <= rest.len() && self.buf.ends_with(&self.rd[..rdl - k]) && rest.starts_with(&self.rd[rdl - k..]));
                if let Some(k) = hit {
                    let len = self.buf.len() - (rdl - k);
                    let flow = f(self.strip(&self.buf[..len])?, self.rec_start)?;
                    self.buf.clear();
                    i += k;
                    let flow = self.after(base + i as u64, flow);
                    if flow != Flow::Continue {
                        self.pos = base + i as u64;
                        return Ok((i, flow));
                    }
                    continue;
                }
            }
            match finder.find(&data[i..]) {
                Some(j) => {
                    let end = i + j;
                    let flow = if self.buf.is_empty() {
                        f(self.strip(&data[i..end])?, self.rec_start)?
                    } else {
                        self.push(&data[i..end])?;
                        let flow = f(self.strip(&self.buf)?, self.rec_start)?;
                        self.buf.clear();
                        flow
                    };
                    i = end + rdl;
                    let flow = self.after(base + i as u64, flow);
                    if flow != Flow::Continue {
                        self.pos = base + i as u64;
                        return Ok((i, flow));
                    }
                }
                None => {
                    self.push(&data[i..])?;
                    i = data.len();
                }
            }
        }
        self.pos = base + data.len() as u64;
        Ok((data.len(), Flow::Continue))
    }

    fn feed_quoted(&mut self, data: &[u8], f: &mut dyn FnMut(&[u8], u64) -> Result<Flow>) -> Result<(usize, Flow)> {
        let base = self.pos;
        let q = self.quoting.clone().unwrap_or_else(|| unreachable!());
        let rdl = self.rd.len();
        for (idx, &b) in data.iter().enumerate() {
            if self.buf.len() > MAX_RECORD_BYTES + rdl {
                return Err(over_max());
            }
            self.buf.push(b);
            let unquoted = |s: &Splitter| {
                if s.buf.ends_with(&s.rd) {
                    None
                } else if s.buf.ends_with(&q.fd) {
                    Some(QState::FieldStart)
                } else {
                    Some(QState::Unquoted)
                }
            };
            let next = match self.qs {
                QState::Quoted if b == q.esc && q.esc != q.quote => Some(QState::QuotedEsc),
                QState::Quoted if b == q.quote => Some(QState::QuoteSeen),
                QState::Quoted => Some(QState::Quoted),
                QState::QuotedEsc => Some(QState::Quoted),
                QState::QuoteSeen if b == q.quote => Some(QState::Quoted),
                QState::FieldStart if b == q.quote => Some(QState::Quoted),
                QState::FieldStart if self.buf.len() == 1 && q.comment == Some(b) => Some(QState::Comment),
                QState::Comment => {
                    if self.buf.ends_with(&self.rd) {
                        None
                    } else {
                        Some(QState::Comment)
                    }
                }
                _ => unquoted(self),
            };
            match next {
                Some(s) => self.qs = s,
                None => {
                    let len = self.buf.len() - rdl;
                    let flow = f(self.strip(&self.buf[..len])?, self.rec_start)?;
                    self.buf.clear();
                    self.qs = QState::FieldStart;
                    let flow = self.after(base + idx as u64 + 1, flow);
                    if flow != Flow::Continue {
                        self.pos = base + idx as u64 + 1;
                        return Ok((idx + 1, flow));
                    }
                }
            }
        }
        self.pos = base + data.len() as u64;
        Ok((data.len(), Flow::Continue))
    }

    /// The input ended: the last record needn't end with a delimiter.
    pub fn finish(&mut self, f: &mut dyn FnMut(&[u8], u64) -> Result<Flow>) -> Result<Flow> {
        if self.buf.is_empty() {
            return Ok(Flow::Continue);
        }
        let buf = std::mem::take(&mut self.buf);
        let flow = f(self.strip(&buf)?, self.rec_start)?;
        self.rec_start = self.pos;
        Ok(flow)
    }
}

/// Split one CSV record into fields (at most `max`). A field starting with the quote character
/// is quoted: inside it the field delimiter is data, a doubled quote is a quote, and the escape
/// character makes the next character literal; text after the closing quote is kept. A quote
/// elsewhere is data. An unterminated quoted field runs to the end of the record.
pub(crate) fn split_fields(rec: &[u8], fd: &[u8], quote: u8, esc: u8, max: usize, out: &mut Fields) {
    out.clear();
    let finder = memmem::Finder::new(fd);
    let n = rec.len();
    let mut i = 0;
    while out.len() < max {
        if i < n && rec[i] == quote {
            i += 1;
            while i < n {
                let b = rec[i];
                if b == esc && esc != quote && i + 1 < n {
                    out.data.push(rec[i + 1]);
                    i += 2;
                } else if b == quote {
                    if i + 1 < n && rec[i + 1] == quote {
                        out.data.push(quote);
                        i += 2;
                    } else {
                        i += 1;
                        break;
                    }
                } else {
                    out.data.push(b);
                    i += 1;
                }
            }
        }
        match finder.find(&rec[i..]) {
            Some(j) => {
                out.data.extend_from_slice(&rec[i..i + j]);
                out.ends.push(out.data.len());
                i += j + fd.len();
            }
            None => {
                out.data.extend_from_slice(&rec[i..]);
                out.ends.push(out.data.len());
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields(rec: &str, fd: &str, quote: u8, esc: u8) -> Vec<String> {
        let mut f = Fields::default();
        split_fields(rec.as_bytes(), fd.as_bytes(), quote, esc, usize::MAX, &mut f);
        (0..f.len()).map(|i| String::from_utf8(f.get(i).unwrap().to_vec()).unwrap()).collect()
    }

    #[test]
    fn field_splitting() {
        assert_eq!(fields("a,b,,c,", ",", b'"', b'"'), ["a", "b", "", "c", ""]);
        assert_eq!(fields("\"a,b\",\"x\"\"y\",z", ",", b'"', b'"'), ["a,b", "x\"y", "z"]);
        assert_eq!(fields("\"a\\\"b\",c", ",", b'"', b'\\'), ["a\"b", "c"]);
        assert_eq!(fields("\"a\"\"b\",c", ",", b'"', b'\\'), ["a\"b", "c"]);
        assert_eq!(fields("x\"y,z", ",", b'"', b'"'), ["x\"y", "z"]);
        assert_eq!(fields("\"ab\"cd,e", ",", b'"', b'"'), ["abcd", "e"]);
        assert_eq!(fields("\"unterminated,x", ",", b'"', b'"'), ["unterminated,x"]);
        assert_eq!(fields("a||b|c", "||", b'"', b'"'), ["a", "b|c"]);
        assert_eq!(fields("'a|b'|c", "|", b'\'', b'\''), ["a|b", "c"]);
        assert_eq!(fields("C:\\dir,x", ",", b'"', b'\\'), ["C:\\dir", "x"]);
        assert_eq!(fields("a¦b", "¦", b'"', b'"'), ["a", "b"]);
        let mut f = Fields::default();
        split_fields(b"a,b,c,d", b",", b'"', b'"', 2, &mut f);
        assert_eq!(f.len(), 2);
    }

    /// Feed `data` in chunks of `chunk` bytes; collect (record, start).
    fn split(data: &[u8], rd: &[u8], quoting: Option<Quoting>, chunk: usize) -> Vec<(String, u64)> {
        let mut s = Splitter::new(rd, quoting, 0, 0, u64::MAX);
        let mut out = Vec::new();
        let mut f = |r: &[u8], start: u64| {
            out.push((String::from_utf8(r.to_vec()).unwrap(), start));
            Ok(Flow::Continue)
        };
        for c in data.chunks(chunk.max(1)) {
            assert_eq!(s.feed(c, &mut f).unwrap(), (c.len(), Flow::Continue));
        }
        s.finish(&mut f).unwrap();
        out
    }

    #[test]
    fn splitting_in_any_chunk_size() {
        let data = b"a,b\r\nc,d\n\ne,f";
        let want = vec![("a,b".to_string(), 0), ("c,d".to_string(), 5), ("".to_string(), 9), ("e,f".to_string(), 10)];
        for chunk in 1..=data.len() {
            assert_eq!(split(data, b"\n", None, chunk), want, "chunk {chunk}");
        }
        let data = b"a\r\nbXYc\r\nXYdXY";
        for chunk in 1..=data.len() {
            assert_eq!(split(data, b"XY", None, chunk), [("a\r\nb".to_string(), 0), ("c\r\n".to_string(), 6), ("d".to_string(), 11)], "chunk {chunk}");
        }
        let q = Quoting { fd: b",".to_vec(), quote: b'"', esc: b'"', comment: Some(b'#') };
        let data = b"\"a\nb\",c\n#x\"y\nd,\"e\"\"\nf\"\nx\"y\nz";
        for chunk in 1..=data.len() {
            assert_eq!(
                split(data, b"\n", Some(q.clone()), chunk),
                [("\"a\nb\",c".to_string(), 0), ("#x\"y".to_string(), 8), ("d,\"e\"\"\nf\"".to_string(), 13), ("x\"y".to_string(), 23), ("z".to_string(), 27)],
                "chunk {chunk}"
            );
        }
    }

    #[test]
    fn records_are_capped() {
        let mut s = Splitter::new(b"\n", None, 0, 0, u64::MAX);
        let mut f = |_: &[u8], _: u64| Ok(Flow::Continue);
        let big = vec![b'x'; MAX_RECORD_BYTES + 10];
        assert_eq!(s.feed(&big, &mut f).unwrap_err().code, "OverMaxRecordSize");
        let mut s = Splitter::new(b"\n", None, 0, 0, u64::MAX);
        let mut data = vec![b'x'; MAX_RECORD_BYTES + 10];
        data.push(b'\n');
        assert_eq!(s.feed(&data, &mut f).unwrap_err().code, "OverMaxRecordSize");
        let mut s = Splitter::new(b"\n", None, 0, 0, u64::MAX);
        let mut data = vec![b'x'; MAX_RECORD_BYTES];
        data.push(b'\n');
        assert!(s.feed(&data, &mut f).is_ok());
    }

    #[test]
    fn pause_and_stop() {
        let mut s = Splitter::new(b"\n", None, 0, 0, 5);
        let mut n = 0;
        let mut f = |_: &[u8], _: u64| {
            n += 1;
            Ok(if n == 1 { Flow::Pause } else { Flow::Continue })
        };
        assert_eq!(s.feed(b"ab\ncd\nef\ngh\n", &mut f).unwrap(), (3, Flow::Pause));
        // "cd" starts at 3 (<= 5); "ef" would start at 6 > 5.
        assert_eq!(s.feed(b"cd\nef\ngh\n", &mut f).unwrap(), (3, Flow::Done));
    }
}
