//! Boost.Geometry's WKT reader (`bg::from_wkt`, `io/wkt/read.hpp`) for the module's 2D points and
//! clockwise closed polygons: the `char_separator` tokenizer, `boost::lexical_cast<double>`'s
//! coordinate parsing, the points a ring's parser appends, and `read_wkt_exception`'s messages.

use super::{Geom, Poly, Pt, Ring};

/// `read_wkt_exception` thrown by the reader: its `what()`.
pub type WktResult<T> = Result<T, Vec<u8>>;

/// `boost::tokenizer<char_separator<char>>(wkt, " \n\t\r", ",()")`: the dropped delimiters split
/// tokens, the kept ones are tokens of their own; empty tokens are dropped.
fn tokenize(s: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut start = None;
    for (i, &c) in s.iter().enumerate() {
        let dropped = matches!(c, b' ' | b'\n' | b'\t' | b'\r');
        let kept = matches!(c, b',' | b'(' | b')');
        if dropped || kept {
            if let Some(st) = start.take() {
                out.push(&s[st..i]);
            }
            if kept {
                out.push(&s[i..i + 1]);
            }
        } else if start.is_none() {
            start = Some(i);
        }
    }
    if let Some(st) = start {
        out.push(&s[st..]);
    }
    out
}

/// The reader's state: the tokens, the position and the whole text (for messages).
struct Reader<'a> {
    toks: Vec<&'a [u8]>,
    i: usize,
    wkt: &'a [u8],
}

fn upto_nul(s: &[u8]) -> &[u8] {
    s.iter().position(|&c| c == 0).map_or(s, |n| &s[..n])
}

impl<'a> Reader<'a> {
    fn tok(&self) -> Option<&'a [u8]> {
        self.toks.get(self.i).copied()
    }

    fn is(&self, t: &[u8]) -> bool {
        self.tok() == Some(t)
    }

    /// `read_wkt_exception(msg, it, end, wkt)`.
    fn err(&self, msg: &str) -> Vec<u8> {
        let mut out = msg.as_bytes().to_vec();
        if let Some(t) = self.tok() {
            out.extend_from_slice(b" at '");
            out.extend_from_slice(upto_nul(t));
            out.push(b'\'');
        }
        out.extend_from_slice(b" in '");
        out.extend_from_slice(&self.wkt[..self.wkt.len().min(100)]);
        out.push(b'\'');
        out
    }

    /// `read_wkt_exception(msg, wkt)`.
    fn err2(&self, msg: &str) -> Vec<u8> {
        let mut out = msg.as_bytes().to_vec();
        out.extend_from_slice(b"' in (");
        out.extend_from_slice(&self.wkt[..self.wkt.len().min(100)]);
        out.push(b')');
        out
    }

    fn open(&mut self) -> WktResult<()> {
        if !self.is(b"(") {
            return Err(self.err("Expected '('"));
        }
        self.i += 1;
        Ok(())
    }

    fn close(&mut self) -> WktResult<()> {
        if !self.is(b")") {
            return Err(self.err("Expected ')'"));
        }
        self.i += 1;
        Ok(())
    }

    /// `parsing_assigner`: two coordinates, a missing one (at the end, `,` or `)`) zero.
    fn point(&mut self) -> WktResult<Pt> {
        let mut c = [0.0; 2];
        for v in &mut c {
            let finished = matches!(self.tok(), None | Some(b",") | Some(b")"));
            if !finished {
                *v = lexical_cast(self.tok().unwrap()).ok_or_else(|| self.err("bad lexical cast: source type value could not be interpreted as target"))?;
                self.i += 1;
            }
        }
        Ok(Pt { x: c[0], y: c[1] })
    }

    /// `initialize`: the geometry's name, then any `M`, `Z`, `EMPTY`, `MZ`, `ZM`; false when
    /// EMPTY.
    fn initialize(&mut self, name: &str) -> WktResult<bool> {
        let ok = self.tok().is_some_and(|t| t.eq_ignore_ascii_case(name.as_bytes()));
        self.i += 1;
        if !ok {
            return Err(self.err2(&format!("Should start with '{name}'")));
        }
        let (mut empty, mut z) = (false, false);
        while let Some(t) = self.tok() {
            if t.eq_ignore_ascii_case(b"M") {
            } else if t.eq_ignore_ascii_case(b"Z") || t.eq_ignore_ascii_case(b"MZ") || t.eq_ignore_ascii_case(b"ZM") {
                z = true;
            } else if t.eq_ignore_ascii_case(b"EMPTY") {
                empty = true;
            } else {
                break;
            }
            self.i += 1;
        }
        if z {
            return Err(self.err2("Z only allowed for 3 or more dimensions"));
        }
        Ok(!empty)
    }

    /// `container_appender` for a closed ring: `(x y, x y …)`.
    fn ring(&mut self, ring: &mut Ring) -> WktResult<()> {
        self.open()?;
        while self.tok().is_some_and(|t| t != b")") {
            let p = self.point()?;
            let next = self.is(b",");
            ring.push(p);
            if next {
                self.i += 1;
            }
        }
        self.close()
    }

    fn check_end(&self) -> WktResult<()> {
        if self.tok().is_some() {
            return Err(self.err("Too many tokens"));
        }
        Ok(())
    }
}

/// `boost::lexical_cast<double>`: `parse_inf_nan`, else the stream's `num_get` grammar (sign,
/// digits, one point, an exponent) consuming the whole token, out of range failing and a last
/// `e`, `E`, `+` or `-` refused.
pub fn lexical_cast(t: &[u8]) -> Option<f64> {
    if let Some(v) = parse_inf_nan(t) {
        return Some(v);
    }
    let mut i = 0;
    if i < t.len() && (t[i] == b'+' || t[i] == b'-') {
        i += 1;
    }
    let mut digits = false;
    let mut dec = false;
    let mut sci = false;
    while i < t.len() {
        let c = t[i];
        if c.is_ascii_digit() {
            digits = true;
        } else if c == b'.' && !dec && !sci {
            dec = true;
        } else if (c == b'e' || c == b'E') && !sci && digits {
            sci = true;
            if i + 1 < t.len() && (t[i + 1] == b'+' || t[i + 1] == b'-') {
                i += 1;
            }
        } else {
            return None;
        }
        i += 1;
    }
    if matches!(t.last(), Some(b'e' | b'E' | b'+' | b'-')) {
        return None;
    }
    let s = std::str::from_utf8(t).ok()?;
    // strtod's prefix must be the whole token: a mantissa, an exponent with digits.
    if !digits {
        return None;
    }
    let v: f64 = s.parse().ok()?;
    if v.is_infinite() {
        return None;
    }
    Some(v)
}

/// lexical_cast's `parse_inf_nan`: `[+-](nan[(…)]|inf|infinity)`, any case.
fn parse_inf_nan(t: &[u8]) -> Option<f64> {
    let mut b = t;
    let neg = b.first() == Some(&b'-');
    if neg || b.first() == Some(&b'+') {
        b = &b[1..];
    }
    if b.len() < 3 {
        return None;
    }
    if b[..3].eq_ignore_ascii_case(b"nan") {
        let rest = &b[3..];
        if !rest.is_empty() && (rest.len() < 2 || rest[0] != b'(' || rest[rest.len() - 1] != b')') {
            return None;
        }
        return Some(if neg { -f64::NAN } else { f64::NAN });
    }
    if (b.len() == 3 && b.eq_ignore_ascii_case(b"inf")) || (b.len() == 8 && b.eq_ignore_ascii_case(b"infinity")) {
        return Some(if neg { f64::NEG_INFINITY } else { f64::INFINITY });
    }
    None
}

/// `bg::from_wkt<point_type>`.
pub fn read_point(wkt: &[u8]) -> WktResult<Pt> {
    let mut r = Reader { toks: tokenize(wkt), i: 0, wkt };
    // `POINT EMPTY` leaves the model's (uninitialized) coordinates.
    let mut p = Pt { x: 0.0, y: 0.0 };
    if r.initialize("POINT")? {
        r.open()?;
        p = r.point()?;
        r.close()?;
    }
    r.check_end()?;
    Ok(p)
}

/// `bg::from_wkt<poly_type>`: the exterior ring, then the interior ones.
pub fn read_polygon(wkt: &[u8]) -> WktResult<Poly> {
    let mut r = Reader { toks: tokenize(wkt), i: 0, wkt };
    let mut poly = Poly::default();
    if r.initialize("POLYGON")? {
        r.open()?;
        let mut n = 0;
        while r.tok().is_some_and(|t| t != b")") {
            if n == 0 {
                r.ring(&mut poly.outer)?;
            } else {
                let mut ring = Ring::default();
                r.ring(&mut ring)?;
                poly.inners.push(ring);
            }
            n += 1;
            if r.is(b",") {
                r.i += 1;
            }
        }
        r.close()?;
    }
    r.check_end()?;
    Ok(poly)
}

/// The module's `from_wkt`: a point or a polygon by the text's first three bytes, polygons
/// corrected, then validated.
pub fn from_wkt(wkt: &[u8], cs: super::Cs) -> WktResult<Geom> {
    let g = if wkt.starts_with(b"POI") {
        Geom::Point(read_point(wkt)?)
    } else if wkt.starts_with(b"POL") {
        let mut p = read_polygon(wkt)?;
        super::algo::correct(&mut p, cs);
        Geom::Poly(p)
    } else {
        return Err(b"unknown geometry type".to_vec());
    };
    if let Err(e) = super::valid::is_valid(&g, cs) {
        let mut m = b"invalid geometry: ".to_vec();
        m.extend_from_slice(e.as_bytes());
        return Err(m);
    }
    Ok(g)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(r: WktResult<Pt>) -> String {
        String::from_utf8(r.unwrap_err()).unwrap()
    }

    #[test]
    fn points() {
        assert_eq!(read_point(b"POINT(1 2)").unwrap(), Pt { x: 1.0, y: 2.0 });
        assert_eq!(read_point(b"POINT(1)").unwrap(), Pt { x: 1.0, y: 0.0 });
        assert_eq!(read_point(b"point M (+1 -.5)").unwrap(), Pt { x: 1.0, y: -0.5 });
        assert_eq!(msg(read_point(b"POINT(1 2 3)")), "Expected ')' at '3' in 'POINT(1 2 3)'");
        assert_eq!(msg(read_point(b"POIX(1 2)")), "Should start with 'POINT'' in (POIX(1 2))");
        assert_eq!(msg(read_point(b"POINT(1 2")), "Expected ')' in 'POINT(1 2'");
        assert_eq!(msg(read_point(b"POINT(1 2) x")), "Too many tokens at 'x' in 'POINT(1 2) x'");
        assert_eq!(msg(read_point(b"POINT Z(1 2 3)")), "Z only allowed for 3 or more dimensions' in (POINT Z(1 2 3))");
        assert_eq!(
            msg(read_point(b"POINT(0x10 1)")),
            "bad lexical cast: source type value could not be interpreted as target at '0x10' in 'POINT(0x10 1)'"
        );
        for bad in ["1e", "1e999", "-1e999", "1e+", ".", "+", "1.5.3", "nanx", "e5"] {
            assert_eq!(lexical_cast(bad.as_bytes()), None, "{bad}");
        }
        assert_eq!(lexical_cast(b"1."), Some(1.0));
        assert_eq!(lexical_cast(b"-INF"), Some(f64::NEG_INFINITY));
        assert!(lexical_cast(b"NaN").unwrap().is_nan());
        assert_eq!(lexical_cast(b"1e-999"), Some(0.0));
    }

    #[test]
    fn polygons() {
        let p = read_polygon(b"POLYGON((0 0 0, 1 0 0, 1 1 0, 0 0 0))").unwrap();
        assert_eq!(p.outer.pts.len(), 8);
        let p = read_polygon(b"POLYGON((0 0, 0 1, 1 1, 1 0, 0 0),(1 1, 2 2, 3 3))").unwrap();
        assert_eq!((p.outer.pts.len(), p.outer.cap, p.inners.len()), (5, 8, 1));
        assert_eq!(
            String::from_utf8(read_polygon(b"POLYGON((0 0, 0 1, 1 1, 1 0, 0 0)))").unwrap_err()).unwrap(),
            "Too many tokens at ')' in 'POLYGON((0 0, 0 1, 1 1, 1 0, 0 0)))'"
        );
    }
}
