//! jsonb's on-disk bytes (convertToJsonb), from a value's JSON text: what TOAST compresses.
//! Containers carry a count-and-flags header and JEntries (every 32nd an end offset, the
//! rest lengths); object keys are sorted by length then bytes; numerics are int-aligned
//! numeric varlenas.

enum J {
    Null,
    Bool(bool),
    Num(String),
    Str(Vec<u8>),
    Arr(Vec<J>),
    Obj(Vec<(Vec<u8>, J)>),
}

struct P<'a> {
    s: &'a [u8],
    i: usize,
}

impl P<'_> {
    fn ws(&mut self) {
        while self.s.get(self.i).is_some_and(|c| c.is_ascii_whitespace()) {
            self.i += 1;
        }
    }

    fn string(&mut self) -> Vec<u8> {
        self.i += 1;
        let mut out = Vec::new();
        while let Some(&c) = self.s.get(self.i) {
            self.i += 1;
            match c {
                b'"' => break,
                b'\\' => {
                    let e = self.s.get(self.i).copied().unwrap_or(b'"');
                    self.i += 1;
                    let hex = |p: &P, at: usize| {
                        std::str::from_utf8(&p.s[at.min(p.s.len())..(at + 4).min(p.s.len())])
                            .ok()
                            .and_then(|h| u32::from_str_radix(h, 16).ok())
                            .unwrap_or(0)
                    };
                    match e {
                        b'u' => {
                            let mut cp = hex(self, self.i);
                            self.i += 4;
                            if (0xD800..0xDC00).contains(&cp) && self.s.get(self.i) == Some(&b'\\') {
                                let lo = hex(self, self.i + 2);
                                cp = 0x10000 + ((cp - 0xD800) << 10) + (lo.wrapping_sub(0xDC00) & 0x3FF);
                                self.i += 6;
                            }
                            let ch = char::from_u32(cp).unwrap_or('\u{FFFD}');
                            let mut b = [0u8; 4];
                            out.extend_from_slice(ch.encode_utf8(&mut b).as_bytes());
                        }
                        b'n' => out.push(b'\n'),
                        b't' => out.push(b'\t'),
                        b'r' => out.push(b'\r'),
                        b'b' => out.push(8),
                        b'f' => out.push(12),
                        c => out.push(c),
                    }
                }
                c => out.push(c),
            }
        }
        out
    }

    fn value(&mut self) -> J {
        self.ws();
        match self.s.get(self.i) {
            Some(b'"') => J::Str(self.string()),
            Some(b'[') => {
                self.i += 1;
                let mut items = Vec::new();
                loop {
                    self.ws();
                    match self.s.get(self.i) {
                        Some(b']') | None => {
                            self.i += 1;
                            break;
                        }
                        Some(b',') => self.i += 1,
                        _ => items.push(self.value()),
                    }
                }
                J::Arr(items)
            }
            Some(b'{') => {
                self.i += 1;
                let mut pairs: Vec<(Vec<u8>, J)> = Vec::new();
                loop {
                    self.ws();
                    match self.s.get(self.i) {
                        Some(b'}') | None => {
                            self.i += 1;
                            break;
                        }
                        Some(b',') => self.i += 1,
                        Some(b'"') => {
                            let k = self.string();
                            self.ws();
                            self.i += 1;
                            let v = self.value();
                            // (The last of duplicate keys wins.)
                            pairs.retain(|(x, _)| *x != k);
                            pairs.push((k, v));
                        }
                        _ => self.i += 1,
                    }
                }
                pairs.sort_by(|a, b| a.0.len().cmp(&b.0.len()).then_with(|| a.0.cmp(&b.0)));
                J::Obj(pairs)
            }
            Some(b't') => {
                self.i += 4;
                J::Bool(true)
            }
            Some(b'f') => {
                self.i += 5;
                J::Bool(false)
            }
            Some(b'n') => {
                self.i += 4;
                J::Null
            }
            _ => {
                let start = self.i;
                while self
                    .s
                    .get(self.i)
                    .is_some_and(|c| c.is_ascii_digit() || matches!(c, b'-' | b'+' | b'.' | b'e' | b'E'))
                {
                    self.i += 1;
                }
                J::Num(String::from_utf8_lossy(&self.s[start..self.i]).into_owned())
            }
        }
    }
}

/// numeric_in's datum (4-byte varlena header included) for a JSON number.
fn numeric(text: &str) -> Vec<u8> {
    let (neg, rest) = match text.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, text.strip_prefix('+').unwrap_or(text)),
    };
    let (mant, exp) = match rest.find(['e', 'E']) {
        Some(p) => (&rest[..p], rest[p + 1..].parse::<i64>().unwrap_or(0)),
        None => (rest, 0),
    };
    let (ip, fp) = match mant.find('.') {
        Some(p) => (&mant[..p], &mant[p + 1..]),
        None => (mant, ""),
    };
    // Decimal digits and the position of the point among them.
    let mut digits: Vec<u8> = ip.bytes().chain(fp.bytes()).map(|c| c - b'0').collect();
    let mut point = ip.len() as i64 + exp;
    let dscale = (fp.len() as i64 - exp).max(0);
    // Strip leading zeros.
    while digits.first() == Some(&0) {
        digits.remove(0);
        point -= 1;
    }
    while digits.last() == Some(&0) {
        digits.pop();
    }
    let mut groups: Vec<i16> = Vec::new();
    let mut weight: i64 = 0;
    if !digits.is_empty() {
        // Pad so the point falls on a base-10000 group boundary.
        let lead = point.rem_euclid(4);
        let lead = if lead == 0 { 0 } else { 4 - lead };
        let mut padded = vec![0u8; lead as usize];
        padded.extend_from_slice(&digits);
        while !padded.len().is_multiple_of(4) {
            padded.push(0);
        }
        weight = (point + lead) / 4 - 1;
        groups = padded
            .chunks(4)
            .map(|c| c.iter().fold(0i16, |a, &d| a * 10 + d as i16))
            .collect();
        while groups.first() == Some(&0) {
            groups.remove(0);
            weight -= 1;
        }
        while groups.last() == Some(&0) {
            groups.pop();
        }
    }
    let neg = neg && !groups.is_empty();
    let mut body = Vec::new();
    if dscale <= 63 && (-64..=63).contains(&weight) {
        let h: u16 = 0x8000
            | if neg { 0x2000 } else { 0 }
            | ((dscale as u16) << 7)
            | if weight < 0 { 0x0040 } else { 0 }
            | (weight as u16 & 0x003F);
        body.extend_from_slice(&h.to_le_bytes());
    } else {
        let sd: u16 = if neg { 0x4000 } else { 0 } | (dscale as u16 & 0x3FFF);
        body.extend_from_slice(&sd.to_le_bytes());
        body.extend_from_slice(&(weight as i16).to_le_bytes());
    }
    for g in groups {
        body.extend_from_slice(&g.to_le_bytes());
    }
    let mut out = (((body.len() + 4) as u32) << 2).to_le_bytes().to_vec();
    out.extend(body);
    out
}

const STRIDE: usize = 32;
const HAS_OFF: u32 = 0x8000_0000;
const IS_NUMERIC: u32 = 0x1000_0000;
const IS_FALSE: u32 = 0x2000_0000;
const IS_TRUE: u32 = 0x3000_0000;
const IS_NULL: u32 = 0x4000_0000;
const IS_CONTAINER: u32 = 0x5000_0000;

fn pad4(buf: &mut Vec<u8>) -> usize {
    let p = (4 - buf.len() % 4) % 4;
    buf.resize(buf.len() + p, 0);
    p
}

/// convertJsonbValue: appends the value's data, returns its JEntry (type and length).
fn value(buf: &mut Vec<u8>, v: &J) -> u32 {
    match v {
        J::Null => IS_NULL,
        J::Bool(true) => IS_TRUE,
        J::Bool(false) => IS_FALSE,
        J::Str(s) => {
            buf.extend_from_slice(s);
            s.len() as u32
        }
        J::Num(n) => {
            let pad = pad4(buf);
            let d = numeric(n);
            buf.extend_from_slice(&d);
            IS_NUMERIC | (pad + d.len()) as u32
        }
        J::Arr(_) | J::Obj(_) => {
            let pad = pad4(buf);
            let start = buf.len();
            container(buf, v, false);
            IS_CONTAINER | (pad + buf.len() - start) as u32
        }
    }
}

fn container(buf: &mut Vec<u8>, v: &J, scalar: bool) {
    let elems: Vec<&J> = match v {
        J::Arr(items) => items.iter().collect(),
        J::Obj(pairs) => pairs.iter().map(|(_, v)| v).collect(),
        other => vec![other],
    };
    let keys: Vec<&[u8]> = match v {
        J::Obj(pairs) => pairs.iter().map(|(k, _)| k.as_slice()).collect(),
        _ => Vec::new(),
    };
    let obj = matches!(v, J::Obj(_));
    let n = elems.len() as u32;
    let header = n | if obj { 0x2000_0000 } else { 0x4000_0000 } | if scalar { 0x1000_0000 } else { 0 };
    buf.extend_from_slice(&header.to_le_bytes());
    let nentries = keys.len() + elems.len();
    let meta_at = buf.len();
    buf.resize(buf.len() + 4 * nentries, 0);
    let mut total = 0u32;
    let mut i = 0usize;
    let put = |buf: &mut Vec<u8>, meta: u32, i: usize, total: &mut u32| {
        *total += meta & 0x0FFF_FFFF;
        let meta = if i.is_multiple_of(STRIDE) { (meta & 0x7000_0000) | *total | HAS_OFF } else { meta };
        buf[meta_at + 4 * i..meta_at + 4 * i + 4].copy_from_slice(&meta.to_le_bytes());
    };
    for k in &keys {
        buf.extend_from_slice(k);
        put(buf, k.len() as u32, i, &mut total);
        i += 1;
    }
    for e in elems {
        let meta = value(buf, e);
        put(buf, meta, i, &mut total);
        i += 1;
    }
}

/// The bytes of a jsonb value after its varlena header.
pub fn jsonb_bytes(text: &str) -> Vec<u8> {
    let mut p = P { s: text.as_bytes(), i: 0 };
    let v = p.value();
    let mut buf = Vec::new();
    match v {
        a @ (J::Arr(_) | J::Obj(_)) => container(&mut buf, &a, false),
        other => container(&mut buf, &J::Arr(vec![other]), true),
    }
    buf
}
