//! pkt-line framing: a 4-hex-digit length (including itself) and a payload, plus the special
//! packets `0000` (flush), `0001` (delim) and `0002` (response end). Side-band multiplexing puts
//! a band byte (1 data, 2 progress, 3 error) in front of each payload.

use bytes::{BufMut, BytesMut};

/// Largest packet, length prefix included.
pub const MAX_PKT_LEN: usize = 65520;
/// Largest payload in one packet.
pub const MAX_DATA: usize = MAX_PKT_LEN - 4;
/// Largest side-band-64k payload (one byte goes to the band number).
pub const MAX_SIDEBAND_DATA: usize = MAX_DATA - 1;

pub const BAND_DATA: u8 = 1;
pub const BAND_PROGRESS: u8 = 2;
pub const BAND_ERROR: u8 = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pkt<'a> {
    Data(&'a [u8]),
    Flush,
    Delim,
    ResponseEnd,
}

impl<'a> Pkt<'a> {
    /// The payload with one trailing newline removed; `None` for special packets.
    pub fn text(&self) -> Option<&'a [u8]> {
        match self {
            Pkt::Data(d) => Some(d.strip_suffix(b"\n").unwrap_or(d)),
            _ => None,
        }
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PktError {
    #[error("truncated pkt-line")]
    Truncated,
    #[error("invalid pkt-line length")]
    BadLength,
}

/// Appends one data packet. Panics if `data` exceeds [`MAX_DATA`]; callers split larger payloads.
pub fn write(buf: &mut BytesMut, data: &[u8]) {
    assert!(data.len() <= MAX_DATA, "pkt-line payload too large");
    buf.reserve(4 + data.len());
    buf.put_slice(format!("{:04x}", data.len() + 4).as_bytes());
    buf.put_slice(data);
}

/// Appends a text packet with a trailing newline.
pub fn write_line(buf: &mut BytesMut, line: &str) {
    let mut v = Vec::with_capacity(line.len() + 1);
    v.extend_from_slice(line.as_bytes());
    v.push(b'\n');
    write(buf, &v);
}

pub fn flush(buf: &mut BytesMut) {
    buf.put_slice(b"0000");
}

pub fn delim(buf: &mut BytesMut) {
    buf.put_slice(b"0001");
}

/// Appends `data` as side-band packets on `band`, split to fit.
pub fn write_band(buf: &mut BytesMut, band: u8, data: &[u8]) {
    for chunk in data.chunks(MAX_SIDEBAND_DATA) {
        buf.reserve(5 + chunk.len());
        buf.put_slice(format!("{:04x}", chunk.len() + 5).as_bytes());
        buf.put_u8(band);
        buf.put_slice(chunk);
    }
}

/// Reads packets from a buffered body.
pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Reader { buf, pos: 0 }
    }

    /// Bytes not yet consumed (for a pack that follows the packets).
    pub fn rest(&self) -> &'a [u8] {
        &self.buf[self.pos..]
    }

    pub fn is_empty(&self) -> bool {
        self.pos >= self.buf.len()
    }

    /// The next packet, or `None` at the end of the buffer.
    pub fn next_pkt(&mut self) -> Result<Option<Pkt<'a>>, PktError> {
        let rest = &self.buf[self.pos..];
        if rest.is_empty() {
            return Ok(None);
        }
        if rest.len() < 4 {
            return Err(PktError::Truncated);
        }
        let mut len = 0usize;
        for &c in &rest[..4] {
            let d = match c {
                b'0'..=b'9' => c - b'0',
                b'a'..=b'f' => c - b'a' + 10,
                b'A'..=b'F' => c - b'A' + 10,
                _ => return Err(PktError::BadLength),
            };
            len = (len << 4) | d as usize;
        }
        let pkt = match len {
            0 => Pkt::Flush,
            1 => Pkt::Delim,
            2 => Pkt::ResponseEnd,
            3 => return Err(PktError::BadLength),
            n if n > MAX_PKT_LEN => return Err(PktError::BadLength),
            n => {
                if rest.len() < n {
                    return Err(PktError::Truncated);
                }
                self.pos += n;
                return Ok(Some(Pkt::Data(&rest[4..n])));
            }
        };
        self.pos += 4;
        Ok(Some(pkt))
    }

    /// The next packet; the end of the buffer is an error.
    pub fn expect_pkt(&mut self) -> Result<Pkt<'a>, PktError> {
        self.next_pkt()?.ok_or(PktError::Truncated)
    }
}

/// Collects the payloads of side-band packets until a flush, by band. Errors on band 3.
#[derive(Default, Debug)]
pub struct Demuxed {
    pub data: Vec<u8>,
    pub progress: Vec<u8>,
}

#[derive(Debug, thiserror::Error)]
pub enum DemuxError {
    #[error(transparent)]
    Pkt(#[from] PktError),
    #[error("remote error: {0}")]
    Remote(String),
    #[error("invalid side-band packet")]
    BadBand,
    #[error("side-band data exceeds {0} bytes")]
    TooLarge(usize),
}

/// Reads side-band packets until a flush (or the end), bounded to `max` data bytes.
pub fn demux(r: &mut Reader<'_>, max: usize) -> Result<Demuxed, DemuxError> {
    let mut out = Demuxed::default();
    while let Some(pkt) = r.next_pkt()? {
        match pkt {
            Pkt::Flush | Pkt::ResponseEnd => break,
            Pkt::Delim => return Err(DemuxError::BadBand),
            Pkt::Data(d) => {
                let (&band, payload) = d.split_first().ok_or(DemuxError::BadBand)?;
                match band {
                    BAND_DATA => {
                        if out.data.len() + payload.len() > max {
                            return Err(DemuxError::TooLarge(max));
                        }
                        out.data.extend_from_slice(payload);
                    }
                    BAND_PROGRESS => {
                        if out.progress.len() < 1 << 20 {
                            out.progress.extend_from_slice(payload);
                        }
                    }
                    BAND_ERROR => return Err(DemuxError::Remote(String::from_utf8_lossy(payload).trim_end().to_string())),
                    _ => return Err(DemuxError::BadBand),
                }
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let mut b = BytesMut::new();
        write_line(&mut b, "hello");
        delim(&mut b);
        write(&mut b, b"");
        flush(&mut b);
        assert_eq!(&b[..], b"000ahello\n000100040000");
        let mut r = Reader::new(&b);
        assert_eq!(r.next_pkt().unwrap().unwrap().text(), Some(&b"hello"[..]));
        assert_eq!(r.next_pkt().unwrap(), Some(Pkt::Delim));
        assert_eq!(r.next_pkt().unwrap(), Some(Pkt::Data(b"")));
        assert_eq!(r.next_pkt().unwrap(), Some(Pkt::Flush));
        assert_eq!(r.next_pkt().unwrap(), None);
    }

    #[test]
    fn rejects_bad_lengths() {
        assert_eq!(Reader::new(b"0003").next_pkt(), Err(PktError::BadLength));
        assert_eq!(Reader::new(b"fff1").next_pkt(), Err(PktError::BadLength));
        assert_eq!(Reader::new(b"zzzz").next_pkt(), Err(PktError::BadLength));
        assert_eq!(Reader::new(b"0009ab").next_pkt(), Err(PktError::Truncated));
        assert_eq!(Reader::new(b"00").next_pkt(), Err(PktError::Truncated));
    }

    #[test]
    fn sideband_splits_and_demuxes() {
        let data = vec![7u8; MAX_SIDEBAND_DATA * 2 + 10];
        let mut b = BytesMut::new();
        write_band(&mut b, BAND_PROGRESS, b"working\n");
        write_band(&mut b, BAND_DATA, &data);
        flush(&mut b);
        let mut r = Reader::new(&b);
        let d = demux(&mut r, usize::MAX).unwrap();
        assert_eq!(d.data, data);
        assert_eq!(d.progress, b"working\n");
        assert!(matches!(demux(&mut Reader::new(&b), 100), Err(DemuxError::TooLarge(100))));

        let mut e = BytesMut::new();
        write_band(&mut e, BAND_ERROR, b"boom\n");
        assert!(matches!(demux(&mut Reader::new(&e), 10), Err(DemuxError::Remote(m)) if m == "boom"));
    }
}
